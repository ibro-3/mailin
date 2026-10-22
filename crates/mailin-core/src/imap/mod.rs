//! IMAP client: connection setup, mailbox listing, header/body fetches and
//! flag updates. The protocol crate is contained in this module; the rest of
//! the core only sees Mailin types.

pub(crate) mod parse;

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_imap::Session;
use async_imap::types::{Flag, Mailbox, Name, NameAttribute};
use futures_util::TryStreamExt;
use rustls::ClientConfig;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use crate::account::{Security, ServerSettings};
use crate::error::{Error, Result};
use crate::model::FolderRole;

use self::parse::ParsedHeader;

/// Abstraction over the two stream flavours used for IMAP so that
/// `async_imap::Session` can be a single type.
#[derive(Debug)]
pub(crate) enum ImapStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for ImapStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ImapStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

impl ImapStream {
    fn into_plain(self) -> Result<TcpStream> {
        match self {
            Self::Plain(stream) => Ok(stream),
            Self::Tls(_) => Err(Error::Protocol(
                "STARTTLS was requested on an already encrypted stream".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteFolder {
    pub name: String,
    pub delimiter: String,
    pub attributes: Vec<String>,
    pub role: FolderRole,
}

impl RemoteFolder {
    pub fn is_selectable(&self) -> bool {
        !self
            .attributes
            .iter()
            .any(|a| a.eq_ignore_ascii_case("\\Noselect"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteHeader {
    pub uid: u32,
    pub flags: Vec<String>,
    pub internal_date: Option<i64>,
    pub size: Option<u32>,
    pub header: Option<Vec<u8>>,
    pub parsed: ParsedHeader,
}

pub(crate) struct ImapSession {
    inner: Session<ImapStream>,
    condstore: bool,
    idle: bool,
}

impl fmt::Debug for ImapSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapSession")
            .field("condstore", &self.condstore)
            .field("idle", &self.idle)
            .finish_non_exhaustive()
    }
}

impl ImapSession {
    pub async fn connect(
        settings: &ServerSettings,
        password: &str,
        tls: Arc<ClientConfig>,
    ) -> Result<Self> {
        let tcp = TcpStream::connect((settings.host.as_str(), settings.port))
            .await
            .map_err(|e| {
                Error::Imap(format!(
                    "could not connect to {}:{}: {e}",
                    settings.host, settings.port
                ))
            })?;
        let _ = tcp.set_nodelay(true);

        let stream = match settings.security {
            Security::None => ImapStream::Plain(tcp),
            Security::Tls => ImapStream::Tls(Box::new(
                crate::tls::connect(tls, &settings.host, tcp).await?,
            )),
            Security::StartTls => {
                let mut client = async_imap::Client::new(ImapStream::Plain(tcp));
                // Consume the server greeting before sending STARTTLS.
                client
                    .read_response()
                    .await
                    .map_err(|e| Error::Imap(format!("could not read greeting: {e}")))?;
                client
                    .run_command_and_check_ok("STARTTLS", None)
                    .await
                    .map_err(|e| Error::Imap(format!("STARTTLS was refused: {e}")))?;
                let tcp = client.into_inner().into_plain()?;
                ImapStream::Tls(Box::new(
                    crate::tls::connect(tls, &settings.host, tcp).await?,
                ))
            }
        };

        let client = async_imap::Client::new(stream);
        let mut session = client
            .login(&settings.username, password)
            .await
            .map_err(|(e, _)| Error::Imap(format!("login failed: {e}")))?;

        let capabilities = session.capabilities().await.ok();
        let condstore = capabilities
            .as_ref()
            .is_some_and(|caps| caps.has_str("CONDSTORE"));
        let idle = capabilities
            .as_ref()
            .is_some_and(|caps| caps.has_str("IDLE"));

        Ok(Self {
            inner: session,
            condstore,
            idle,
        })
    }

    pub fn supports_condstore(&self) -> bool {
        self.condstore
    }

    pub fn supports_idle(&self) -> bool {
        self.idle
    }

    pub(crate) fn into_session(self) -> Session<ImapStream> {
        self.inner
    }

    pub(crate) fn from_session(inner: Session<ImapStream>, condstore: bool, idle: bool) -> Self {
        Self {
            inner,
            condstore,
            idle,
        }
    }

    pub async fn logout(mut self) {
        let _ = self.inner.logout().await;
    }

    pub async fn list_folders(&mut self) -> Result<Vec<RemoteFolder>> {
        let mut stream = self
            .inner
            .list(Some(""), Some("*"))
            .await
            .map_err(|e| Error::Imap(format!("LIST failed: {e}")))?;
        let mut folders = Vec::new();
        loop {
            let name = stream
                .try_next()
                .await
                .map_err(|e| Error::Imap(format!("LIST failed: {e}")))?;
            let Some(name) = name else { break };
            folders.push(remote_folder(&name));
        }
        Ok(folders)
    }

    pub async fn select(&mut self, mailbox: &str) -> Result<Mailbox> {
        let result = if self.condstore {
            self.inner.select_condstore(mailbox).await
        } else {
            self.inner.select(mailbox).await
        };
        result.map_err(|e| Error::Imap(format!("SELECT {mailbox} failed: {e}")))
    }

    /// Search by UID, e.g. `ALL` or `UID 42:*`.
    pub async fn search(&mut self, query: &str) -> Result<HashSet<u32>> {
        self.inner
            .uid_search(query)
            .await
            .map_err(|e| Error::Imap(format!("UID SEARCH {query} failed: {e}")))
    }

    pub async fn fetch_headers(&mut self, uids: &[u32]) -> Result<Vec<RemoteHeader>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let set = uid_set(uids);
        let mut stream = self
            .inner
            .uid_fetch(
                &set,
                "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER])",
            )
            .await
            .map_err(|e| Error::Imap(format!("UID FETCH (headers) failed: {e}")))?;
        let mut headers = Vec::with_capacity(uids.len());
        loop {
            let fetch = stream
                .try_next()
                .await
                .map_err(|e| Error::Imap(format!("UID FETCH (headers) failed: {e}")))?;
            let Some(fetch) = fetch else { break };
            let Some(uid) = fetch.uid else { continue };
            let raw = fetch.header().map(<[u8]>::to_vec);
            let parsed = raw.as_deref().map(parse::parse_header).unwrap_or_default();
            headers.push(RemoteHeader {
                uid,
                flags: fetch.flags().map(flag_to_string).collect(),
                internal_date: fetch.internal_date().map(|d| d.timestamp()),
                size: fetch.size,
                header: raw,
                parsed,
            });
        }
        Ok(headers)
    }

    /// Flags for every message in the selected folder, used by a full sync to
    /// notice changes made by other clients.
    pub async fn fetch_all_flags(&mut self) -> Result<Vec<(u32, Vec<String>)>> {
        let mut stream = self
            .inner
            .uid_fetch("1:*", "(UID FLAGS)")
            .await
            .map_err(|e| Error::Imap(format!("UID FETCH (flags) failed: {e}")))?;
        let mut flags = Vec::new();
        loop {
            let fetch = stream
                .try_next()
                .await
                .map_err(|e| Error::Imap(format!("UID FETCH (flags) failed: {e}")))?;
            let Some(fetch) = fetch else { break };
            if let Some(uid) = fetch.uid {
                flags.push((uid, fetch.flags().map(flag_to_string).collect()));
            }
        }
        Ok(flags)
    }

    pub async fn fetch_body(&mut self, uid: u32) -> Result<Option<Vec<u8>>> {
        let mut stream = self
            .inner
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .await
            .map_err(|e| Error::Imap(format!("UID FETCH (body) failed: {e}")))?;
        while let Some(fetch) = stream
            .try_next()
            .await
            .map_err(|e| Error::Imap(format!("UID FETCH (body) failed: {e}")))?
        {
            if let Some(body) = fetch.body() {
                return Ok(Some(body.to_vec()));
            }
        }
        Ok(None)
    }

    /// Add and remove flags on the server. Both lists may be empty.
    pub async fn store_flags(&mut self, uid: u32, add: &[String], remove: &[String]) -> Result<()> {
        if !add.is_empty() {
            self.store_flags_command(uid, "+", add).await?;
        }
        if !remove.is_empty() {
            self.store_flags_command(uid, "-", remove).await?;
        }
        Ok(())
    }

    async fn store_flags_command(&mut self, uid: u32, mode: &str, flags: &[String]) -> Result<()> {
        let query = format!("{mode}FLAGS.SILENT ({})", flags.join(" "));
        let mut stream = self
            .inner
            .uid_store(uid.to_string(), &query)
            .await
            .map_err(|e| Error::Imap(format!("UID STORE failed: {e}")))?;
        while stream
            .try_next()
            .await
            .map_err(|e| Error::Imap(format!("UID STORE failed: {e}")))?
            .is_some()
        {}
        Ok(())
    }

    /// Append a raw message, for example a copy of an outgoing message to the
    /// Sent folder.
    pub async fn append(&mut self, mailbox: &str, flags: &str, raw: &[u8]) -> Result<()> {
        self.inner
            .append(mailbox, Some(flags), None, raw)
            .await
            .map_err(|e| Error::Imap(format!("APPEND to {mailbox} failed: {e}")))
    }
}

fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn remote_folder(name: &Name) -> RemoteFolder {
    let attributes: Vec<String> = name.attributes().iter().map(attribute_string).collect();
    RemoteFolder {
        name: name.name().to_owned(),
        delimiter: name.delimiter().unwrap_or("/").to_owned(),
        role: FolderRole::from_imap(name.name(), &attributes),
        attributes,
    }
}

fn attribute_string(attribute: &NameAttribute<'_>) -> String {
    match attribute {
        NameAttribute::NoInferiors => "\\Noinferiors".into(),
        NameAttribute::NoSelect => "\\Noselect".into(),
        NameAttribute::Marked => "\\Marked".into(),
        NameAttribute::Unmarked => "\\Unmarked".into(),
        NameAttribute::All => "\\All".into(),
        NameAttribute::Archive => "\\Archive".into(),
        NameAttribute::Drafts => "\\Drafts".into(),
        NameAttribute::Flagged => "\\Flagged".into(),
        NameAttribute::Junk => "\\Junk".into(),
        NameAttribute::Sent => "\\Sent".into(),
        NameAttribute::Trash => "\\Trash".into(),
        NameAttribute::Extension(value) => value.to_string(),
        other => format!("{other:?}"),
    }
}

fn flag_to_string(flag: Flag<'_>) -> String {
    match flag {
        Flag::Seen => "\\Seen".into(),
        Flag::Answered => "\\Answered".into(),
        Flag::Flagged => "\\Flagged".into(),
        Flag::Deleted => "\\Deleted".into(),
        Flag::Draft => "\\Draft".into(),
        Flag::Recent => "\\Recent".into(),
        Flag::MayCreate => "\\*".into(),
        Flag::Custom(value) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_set_is_a_comma_separated_list() {
        assert_eq!(uid_set(&[1, 2, 3]), "1,2,3");
        assert_eq!(uid_set(&[]), "");
    }

    #[test]
    fn noselect_folders_are_not_selectable() {
        let folder = RemoteFolder {
            name: "Archives".into(),
            delimiter: "/".into(),
            attributes: vec!["\\Noselect".into(), "\\HasChildren".into()],
            role: FolderRole::Other,
        };
        assert!(!folder.is_selectable());
    }

    #[test]
    fn special_use_attributes_map_to_roles() {
        let make = |name: &str, attrs: &[&str]| RemoteFolder {
            name: name.into(),
            delimiter: "/".into(),
            attributes: attrs.iter().map(|a| (*a).to_owned()).collect(),
            role: FolderRole::from_imap(
                name,
                &attrs.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>(),
            ),
        };
        assert_eq!(make("INBOX", &[]).role, FolderRole::Inbox);
        assert_eq!(make("Gesendet", &["\\Sent"]).role, FolderRole::Sent);
        assert_eq!(make("Papierkorb", &["\\Trash"]).role, FolderRole::Trash);
        assert_eq!(make("custom", &["\\Noselect"]).role, FolderRole::Other);
    }
}
