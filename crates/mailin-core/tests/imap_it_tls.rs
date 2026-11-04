//! Integration tests for IMAP/SMTP over implicit TLS (IMAPS 3993, SMTPS 3465).
//!
//! These tests require the GreenMail instance configured with the local test
//! CA. `it-up-tls` generates it (idempotent) and starts the container:
//!
//! ```text
//! cargo xtask it-up-tls
//! SSL_CERT_FILE=target/it-certs/ca.crt cargo test -p mailin-core --test imap_it_tls -- --ignored
//! cargo xtask it-down-tls
//! ```
//!
//! The certificate is verified by the same platform verifier the app uses in
//! production; rustls-native-certs honours SSL_CERT_FILE, so no system store
//! changes are needed.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mailin_core::config::Paths;
use mailin_core::{
    Account, AccountId, AuthMethod, Config, Engine, Mailin, MemoryCredentials, MessageHeader,
    OAuth2Tokens, OutgoingEnvelope, Security, ServerSettings, Store,
};
use secrecy::SecretString;

const ADDRESS: &str = "ada@example.com";
const LOGIN: &str = "ada";
const PASSWORD: &str = "correct-horse";
const SUBJECT: &str = "Mailin TLS integration test";

fn password() -> SecretString {
    SecretString::new(PASSWORD.into())
}

fn addr(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_owned())
}

fn host_and_port(value: &str) -> (String, u16) {
    let (host, port) = value.rsplit_once(':').expect("host:port");
    (host.to_owned(), port.parse().expect("port number"))
}

fn client() -> (Arc<Store>, Arc<Mailin>) {
    let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
    let paths = Paths::with_root(tempfile::tempdir().expect("temp dir").path());
    let config = Config::default();
    let config_cache = Arc::new(std::sync::RwLock::new(Some(config)));
    let config_paths = Arc::new(paths);
    let mailin = Arc::new(Mailin::with_parts(
        store.clone(),
        Arc::new(MemoryCredentials::new()),
        config_cache,
        config_paths,
    ));
    (store, mailin)
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn find_inbox(mailin: &Mailin, account: &AccountId) -> Option<Vec<MessageHeader>> {
    let folders = mailin.list_folders(account).ok()?;
    let inbox = folders
        .into_iter()
        .find(|folder| folder.role == mailin_core::FolderRole::Inbox)?;
    mailin.list_messages(inbox.id, 0, 100).ok()
}

/// Build a blocking TLS stream to `host`, validating against the platform
/// verifier (which honours SSL_CERT_FILE) — the same configuration the
/// production engine uses.
fn tls_stream(addr: &str, host: &str) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    use rustls_platform_verifier::ConfigVerifierExt;
    use std::sync::Arc;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tcp = TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect to {addr}: {e}"));
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let config =
        rustls::ClientConfig::with_platform_verifier().expect("platform certificate verifier");
    let server_name =
        rustls::pki_types::ServerName::try_from(host.to_owned()).expect("valid server name");
    let conn =
        rustls::ClientConnection::new(Arc::new(config), server_name).expect("client connection");
    rustls::StreamOwned::new(conn, tcp)
}

/// Read from the stream until a CRLF-terminated line has arrived.
fn read_line<R: std::io::Read>(reader: &mut R) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        reader.read_exact(&mut byte).expect("read byte");
        line.push(byte[0]);
        if line.ends_with(b"\n") {
            break;
        }
    }
    String::from_utf8(line).expect("utf-8 protocol line")
}

#[test]
#[ignore = "requires GreenMail with the test CA: cargo xtask it-up-tls"]
fn greenmail_speaks_imaps() {
    let addr = addr("MAILIN_IT_IMAPS_ADDR", "127.0.0.1:3993");
    let mut stream = tls_stream(&addr, "localhost");

    let greeting = read_line(&mut stream);
    assert!(
        greeting.starts_with("* OK"),
        "unexpected greeting: {greeting:?}"
    );

    std::io::Write::write_all(
        &mut stream,
        format!("a1 LOGIN {LOGIN} {PASSWORD}\r\n").as_bytes(),
    )
    .expect("login");
    let mut response = String::new();
    loop {
        response.clear();
        response.push_str(&read_line(&mut stream));
        if response.starts_with("a1 ") {
            break;
        }
    }
    assert!(response.starts_with("a1 OK"), "login failed: {response:?}");

    std::io::Write::write_all(&mut stream, b"a2 LOGOUT\r\n").expect("logout");
}

#[test]
#[ignore = "requires GreenMail with the test CA: cargo xtask it-up-tls"]
fn greenmail_speaks_smtps() {
    let addr = addr("MAILIN_IT_SMTPS_ADDR", "127.0.0.1:3465");
    let mut stream = tls_stream(&addr, "localhost");

    let line = read_line(&mut stream);
    assert!(line.starts_with("220"), "unexpected greeting: {line:?}");

    std::io::Write::write_all(&mut stream, b"EHLO mailin.test\r\n").expect("ehlo");
    loop {
        let line = read_line(&mut stream);
        if line.starts_with("250 ") {
            break;
        }
    }
    std::io::Write::write_all(&mut stream, b"QUIT\r\n").expect("quit");
}

#[test]
#[ignore = "requires GreenMail with the test CA: cargo xtask it-up-tls"]
fn sync_send_and_fetch_body_round_trip_tls() {
    let (imap_host, imap_port) = host_and_port(&addr("MAILIN_IT_IMAPS_ADDR", "127.0.0.1:3993"));
    let (smtp_host, smtp_port) = host_and_port(&addr("MAILIN_IT_SMTPS_ADDR", "127.0.0.1:3465"));

    let (store, mailin) = client();
    let account = Account::new(
        "Ada",
        ADDRESS,
        ServerSettings {
            host: imap_host,
            port: imap_port,
            security: Security::Tls,
            username: LOGIN.to_owned(),
            auth_method: AuthMethod::default(),
            oauth2: OAuth2Tokens::default(),
        },
        ServerSettings {
            host: smtp_host,
            port: smtp_port,
            security: Security::Tls,
            username: LOGIN.to_owned(),
            auth_method: AuthMethod::default(),
            oauth2: OAuth2Tokens::default(),
        },
    );
    mailin
        .save_account(&account, Some(&password()))
        .expect("save account");

    let engine = Engine::start(mailin.clone()).expect("start engine");

    let raw = format!(
        "From: Ada <{ADDRESS}>\r\n\
         To: Ada <{ADDRESS}>\r\n\
         Subject: {SUBJECT}\r\n\
         Date: Tue, 1 Jul 2025 10:30:00 +0000\r\n\
         Message-ID: <integration-tls-{}@mailin.test>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         hello from the TLS integration test\r\n",
        std::process::id()
    );
    let outbox_id = engine
        .queue_and_send(
            account.id,
            raw.into_bytes(),
            OutgoingEnvelope {
                from: ADDRESS.to_owned(),
                to: vec![ADDRESS.to_owned()],
                ..OutgoingEnvelope::default()
            },
        )
        .expect("queue outgoing message");

    if !wait_until(Duration::from_secs(60), || {
        store.outbox_len().unwrap_or(1) == 0
    }) {
        let entry = store.outbox_entry(outbox_id).expect("read outbox entry");
        panic!("the outgoing message was not accepted by SMTP: {entry:?}");
    }

    engine.sync_account(account.id);

    let message = {
        let mut found = None;
        let appeared = wait_until(Duration::from_secs(60), || {
            found = find_inbox(&mailin, &account.id)
                .unwrap_or_default()
                .into_iter()
                .find(|message| message.subject == SUBJECT);
            found.is_some()
        });
        assert!(appeared, "the message did not show up in the inbox");
        found.expect("message")
    };

    engine.fetch_body(message.id);
    assert!(
        wait_until(Duration::from_secs(60), || {
            mailin
                .message(message.id)
                .ok()
                .flatten()
                .is_some_and(|message| message.body_fetched)
        }),
        "the body was not fetched"
    );
    let (text, _) = mailin
        .message_body(message.id)
        .expect("read body")
        .expect("stored body");
    assert!(
        text.unwrap_or_default()
            .contains("hello from the TLS integration test"),
        "unexpected body"
    );

    engine.set_flags(message.id, vec!["\\Seen".to_owned()], Vec::new());
    assert!(
        wait_until(Duration::from_secs(60), || {
            mailin
                .message(message.id)
                .ok()
                .flatten()
                .is_some_and(|message| message.is_seen())
        }),
        "the \\Seen flag was not applied"
    );

    engine.stop();
}
