//! The façade the front-ends use.
//!
//! [`Mailin`] owns the store and the credential store and hands out a
//! broadcast channel for change notifications. Synchronization workers will
//! publish through the same channel in the next milestone, so the UIs never
//! need to know whether an update came from the network or from a local edit.

use std::fmt;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::account::{Account, AccountId};
use crate::config::Paths;
use crate::credentials::CredentialsStore;
use crate::error::{Error, Result};
use crate::model::{Folder, FolderId, MessageHeader, MessageId, OutboxEntry, OutboxState};
use crate::store::Store;
use secrecy::{ExposeSecret, SecretString};

/// Prefix that distinguishes an OAuth2 refresh token from a password in the
/// keyring entry shared by both credential types.
const OAUTH2_PREFIX: &str = "oauth2-refresh:";

fn is_oauth2_secret(secret: &SecretString) -> bool {
    secret.expose_secret().starts_with(OAUTH2_PREFIX)
}

fn refresh_token_from_secret(secret: &SecretString) -> Option<String> {
    secret
        .expose_secret()
        .strip_prefix(OAUTH2_PREFIX)
        .map(str::to_owned)
}

/// Everything a UI needs to react to. Events are advisory: after receiving one
/// the UI re-reads the affected rows from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    AccountsChanged,
    FoldersChanged {
        account_id: AccountId,
    },
    MessagesChanged {
        folder_id: FolderId,
    },
    /// New messages arrived in the background. Front-ends use this for desktop
    /// notifications; the rows are also covered by [`Event::MessagesChanged`].
    NewMessages {
        account_id: AccountId,
        folder_id: FolderId,
        count: u32,
    },
    BodyReady {
        message_id: MessageId,
    },
    OutboxChanged,
    AccountError {
        account_id: AccountId,
        message: String,
    },
}

pub struct Mailin {
    store: Arc<Store>,
    credentials: Arc<dyn CredentialsStore>,
    events: broadcast::Sender<Event>,
}

impl fmt::Debug for Mailin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mailin")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl Mailin {
    /// Open the database below `paths` and use the OS keyring for passwords.
    pub fn open(paths: &Paths) -> Result<Self> {
        paths.ensure()?;
        let store = Arc::new(Store::open(paths.database_file())?);

        #[cfg(feature = "keyring")]
        let credentials: Arc<dyn CredentialsStore> = Arc::new(crate::KeyringCredentials::new());
        #[cfg(not(feature = "keyring"))]
        let credentials: Arc<dyn CredentialsStore> =
            Arc::new(crate::credentials::MemoryCredentials::new());

        Ok(Self::with_parts(store, credentials))
    }

    pub fn with_parts(store: Arc<Store>, credentials: Arc<dyn CredentialsStore>) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            store,
            credentials,
            events,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Used by the sync workers and by the UIs after local edits.
    pub fn publish(&self, event: Event) {
        // A send error only means nobody is listening.
        let _ = self.events.send(event);
    }

    // ---------------------------------------------------------------- accounts

    /// Create or update an account. The password is stored in the keyring.
    ///
    /// OAuth2 refresh tokens are also kept in the keyring: the account row
    /// stores the short-lived access token and metadata, while the long-lived
    /// refresh token never touches the SQLite file.
    pub fn save_account(&self, account: &Account, password: Option<&SecretString>) -> Result<()> {
        account.validate()?;

        let mut stored = account.clone();
        let refresh_token = stored.imap.oauth2.refresh_token.clone();
        if !refresh_token.is_empty() {
            let secret = SecretString::new(format!("{OAUTH2_PREFIX}{refresh_token}").into());
            self.credentials.set(&account.id, &secret)?;
            stored.imap.oauth2.refresh_token.clear();
            stored.smtp.oauth2.refresh_token.clear();
        }

        self.store.upsert_account(&stored)?;
        if let Some(password) = password {
            self.credentials.set(&account.id, password)?;
        }
        self.publish(Event::AccountsChanged);
        self.publish(Event::FoldersChanged {
            account_id: account.id,
        });
        Ok(())
    }

    pub fn remove_account(&self, id: &AccountId) -> Result<()> {
        let removed = self.store.delete_account(id)?;
        if !removed {
            return Err(Error::AccountNotFound(*id));
        }
        if let Err(error) = self.credentials.delete(id) {
            tracing::warn!(%id, %error, "could not remove the stored password");
        }
        self.publish(Event::AccountsChanged);
        Ok(())
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        let accounts = self.store.list_accounts()?;
        accounts
            .into_iter()
            .map(|account| self.hydrate_oauth2(account))
            .collect()
    }

    pub fn account(&self, id: &AccountId) -> Result<Option<Account>> {
        self.store
            .account(id)?
            .map(|account| self.hydrate_oauth2(account))
            .transpose()
    }

    pub fn has_password(&self, id: &AccountId) -> Result<bool> {
        Ok(self
            .credentials
            .get(id)?
            .is_some_and(|secret| !is_oauth2_secret(&secret)))
    }

    pub fn set_password(&self, id: &AccountId, password: &SecretString) -> Result<()> {
        self.credentials.set(id, password)
    }

    /// Password for a connection attempt. The UI should only call this from a
    /// worker, never from a widget callback that renders it.
    pub fn password(&self, id: &AccountId) -> Result<SecretString> {
        self.credentials
            .get(id)?
            .filter(|secret| !is_oauth2_secret(secret))
            .ok_or_else(|| Error::Credentials("no password is stored for this account".into()))
    }

    /// Add the refresh token from the keyring to an account loaded from the
    /// store. Missing keyring entries are not an error: the account may be a
    /// password account, or the user may be editing it before signing in.
    fn hydrate_oauth2(&self, mut account: Account) -> Result<Account> {
        let Some(secret) = self.credentials.get(&account.id)? else {
            return Ok(account);
        };
        let Some(refresh_token) = refresh_token_from_secret(&secret) else {
            return Ok(account);
        };
        account.imap.oauth2.refresh_token = refresh_token.clone();
        account.smtp.oauth2.refresh_token = refresh_token;
        Ok(account)
    }

    /// Test both IMAP and SMTP connections for the given account settings.
    /// This does not require the account to be saved in the database.
    pub async fn test_connection(
        &self,
        imap: &crate::account::ServerSettings,
        smtp: &crate::account::ServerSettings,
        password: &str,
    ) -> Result<()> {
        crate::client::test_connection(imap, smtp, password).await
    }

    // ----------------------------------------------------------------- folders

    pub fn list_folders(&self, account: &AccountId) -> Result<Vec<Folder>> {
        self.store.list_folders(account)
    }

    pub fn folder(&self, id: FolderId) -> Result<Option<Folder>> {
        self.store.folder(id)
    }

    // ---------------------------------------------------------------- messages

    pub fn list_messages(
        &self,
        folder: FolderId,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<MessageHeader>> {
        self.store
            .list_messages(folder, offset, limit.clamp(1, 1000))
    }

    pub fn message(&self, id: MessageId) -> Result<Option<MessageHeader>> {
        self.store.message(id)
    }

    pub fn message_body(&self, id: MessageId) -> Result<Option<(Option<String>, Option<String>)>> {
        self.store.body(id)
    }

    pub fn list_attachments(&self, id: MessageId) -> Result<Vec<crate::model::AttachmentMeta>> {
        self.store.list_attachments(id)
    }

    /// Attachment metadata and bytes. `data` is `None` when only metadata is
    /// cached; call [`crate::Engine::refresh_body`] to download it again.
    pub fn attachment(
        &self,
        id: MessageId,
        part_id: &str,
    ) -> Result<Option<crate::model::AttachmentData>> {
        self.store.attachment(id, part_id)
    }

    pub fn search(
        &self,
        account: &AccountId,
        query: &str,
        limit: u32,
    ) -> Result<Vec<MessageHeader>> {
        self.store.search(account, query, limit)
    }

    pub fn unread_total(&self, account: &AccountId) -> Result<u32> {
        self.store.unread_total(account)
    }

    // ------------------------------------------------------------------ outbox

    pub fn queue_outgoing(
        &self,
        account: &AccountId,
        raw: &[u8],
        envelope_json: &str,
    ) -> Result<i64> {
        let id = self.store.enqueue_outbox(account, raw, envelope_json)?;
        self.publish(Event::OutboxChanged);
        Ok(id)
    }

    pub fn next_outgoing(&self) -> Result<Option<OutboxEntry>> {
        self.store.next_queued_outbox()
    }

    pub fn outbox_len(&self) -> Result<u32> {
        self.store.outbox_len()
    }

    pub fn list_outbox(&self, account: &AccountId) -> Result<Vec<OutboxEntry>> {
        self.store.list_outbox(account)
    }

    pub fn retry_outbox(&self, entry_id: i64) -> Result<bool> {
        let changed = self.store.set_outbox_state(entry_id, OutboxState::Queued, None)?;
        if changed {
            self.publish(Event::OutboxChanged);
        }
        Ok(changed)
    }

    pub fn cancel_outbox(&self, entry_id: i64) -> Result<bool> {
        let changed = self.store.delete_outbox(entry_id)?;
        if changed {
            self.publish(Event::OutboxChanged);
        }
        Ok(changed)
    }

    pub fn outbox_entry(&self, entry_id: i64) -> Result<Option<OutboxEntry>> {
        self.store.outbox_entry(entry_id)
    }
}

/// Test both IMAP and SMTP connections for an account.
///
/// This attempts to connect to both servers and authenticate. Returns a descriptive
/// error message if either connection fails.
pub async fn test_connection(
    imap: &crate::account::ServerSettings,
    smtp: &crate::account::ServerSettings,
    password: &str,
) -> Result<()> {
    // Test IMAP connection
    let tls = crate::tls::client_config()?;
    let imap_session = crate::imap::ImapSession::connect(imap, password, tls.clone())
        .await
        .map_err(|e| crate::Error::Imap(format!("IMAP connection failed: {e}")))?;
    imap_session.logout().await;

    // Test SMTP connection
    crate::smtp::test_connection(smtp, password)
        .await
        .map_err(|e| crate::Error::Smtp(format!("SMTP connection failed: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::ServerSettings;
    use crate::credentials::MemoryCredentials;
    use secrecy::{ExposeSecret, SecretString};

    fn account() -> Account {
        Account::new(
            "Ada",
            "ada@example.com",
            ServerSettings::imap("imap.example.com", "ada@example.com"),
            ServerSettings::smtp("smtp.example.com", "ada@example.com"),
        )
    }

    fn client() -> Mailin {
        Mailin::with_parts(
            Arc::new(Store::open_in_memory().expect("store")),
            Arc::new(MemoryCredentials::new()),
        )
    }

    #[test]
    fn saving_an_account_stores_password_and_emits_events() {
        let client = client();
        let mut events = client.subscribe();
        let acc = account();
        client
            .save_account(&acc, Some(&SecretString::new("secret".into())))
            .expect("save");

        assert_eq!(client.list_accounts().expect("list"), vec![acc.clone()]);
        assert!(client.has_password(&acc.id).expect("has"));
        assert_eq!(
            client.password(&acc.id).expect("password").expose_secret(),
            "secret"
        );
        assert_eq!(events.try_recv().expect("event"), Event::AccountsChanged);
    }

    #[test]
    fn removing_an_account_forgets_the_password() {
        let client = client();
        let acc = account();
        client
            .save_account(&acc, Some(&SecretString::new("secret".into())))
            .expect("save");
        client.remove_account(&acc.id).expect("remove");
        assert!(client.remove_account(&acc.id).is_err());
        assert!(!client.has_password(&acc.id).expect("has"));
        assert!(client.list_accounts().expect("list").is_empty());
    }

    #[test]
    fn oauth2_refresh_tokens_are_kept_out_of_the_database() {
        use crate::account::{AuthMethod, OAuth2Tokens};

        let client = client();
        let mut acc = account();
        let tokens = OAuth2Tokens {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 42,
            ..OAuth2Tokens::default()
        };
        acc.imap.auth_method = AuthMethod::OAuth2;
        acc.imap.oauth2 = tokens.clone();
        acc.smtp.auth_method = AuthMethod::OAuth2;
        acc.smtp.oauth2 = tokens;

        client.save_account(&acc, None).expect("save");

        let stored = client.store.account(&acc.id).expect("read").expect("row");
        assert!(stored.imap.oauth2.refresh_token.is_empty());
        assert_eq!(stored.imap.oauth2.access_token, "access");

        let loaded = client.account(&acc.id).expect("load").expect("account");
        assert_eq!(loaded.imap.oauth2.refresh_token, "refresh");
        assert_eq!(loaded.smtp.oauth2.refresh_token, "refresh");
        assert!(!client.has_password(&acc.id).expect("has password"));
        assert!(client.password(&acc.id).is_err());
    }

    #[test]
    fn invalid_accounts_are_rejected_before_touching_storage() {
        let client = client();
        let mut acc = account();
        acc.email = "broken".into();
        assert!(client.save_account(&acc, None).is_err());
        assert!(client.list_accounts().expect("list").is_empty());
    }
}
