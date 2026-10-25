//! Integration harness against GreenMail (see `docker-compose.it.yml`).
//!
//! These tests are ignored by default because they need the container:
//!
//! ```text
//! cargo xtask it-up
//! cargo test -p mailin-core --test imap_it -- --ignored
//! cargo xtask it-down
//! ```

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mailin_core::{
    Account, AccountId, Engine, Mailin, MemoryCredentials, MessageHeader, OutgoingEnvelope,
    Security, ServerSettings, Store,
};
use secrecy::SecretString;

const ADDRESS: &str = "ada@example.com";
const LOGIN: &str = "ada";
const PASSWORD: &str = "correct-horse";
const SUBJECT: &str = "Mailin integration test";

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
    let mailin = Arc::new(Mailin::with_parts(
        store.clone(),
        Arc::new(MemoryCredentials::new()),
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

#[test]
#[ignore = "requires GreenMail: cargo xtask it-up"]
fn greenmail_speaks_imap() {
    let addr = addr("MAILIN_IT_IMAP_ADDR", "127.0.0.1:3143");
    let mut stream = TcpStream::connect(&addr).expect("connect to GreenMail IMAP");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");

    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut greeting = String::new();
    reader.read_line(&mut greeting).expect("greeting");
    assert!(
        greeting.starts_with("* OK"),
        "unexpected greeting: {greeting:?}"
    );

    stream
        .write_all(format!("a1 LOGIN {LOGIN} {PASSWORD}\r\n").as_bytes())
        .expect("login");
    let mut response = String::new();
    loop {
        response.clear();
        reader.read_line(&mut response).expect("read");
        if response.starts_with("a1 ") {
            break;
        }
    }
    assert!(response.starts_with("a1 OK"), "login failed: {response:?}");

    stream.write_all(b"a2 LOGOUT\r\n").expect("logout");
}

#[test]
#[ignore = "requires GreenMail: cargo xtask it-up"]
fn greenmail_speaks_smtp() {
    let addr = addr("MAILIN_IT_SMTP_ADDR", "127.0.0.1:3025");
    let mut stream = TcpStream::connect(&addr).expect("connect to GreenMail SMTP");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");

    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("greeting");
    assert!(line.starts_with("220"), "unexpected greeting: {line:?}");

    stream.write_all(b"EHLO mailin.test\r\n").expect("ehlo");
    loop {
        line.clear();
        reader.read_line(&mut line).expect("read");
        if line.starts_with("250 ") {
            break;
        }
    }
    stream.write_all(b"QUIT\r\n").expect("quit");
}

#[test]
#[ignore = "requires GreenMail: cargo xtask it-up"]
fn sync_send_and_fetch_body_round_trip() {
    let (imap_host, imap_port) = host_and_port(&addr("MAILIN_IT_IMAP_ADDR", "127.0.0.1:3143"));
    let (smtp_host, smtp_port) = host_and_port(&addr("MAILIN_IT_SMTP_ADDR", "127.0.0.1:3025"));

    let (store, mailin) = client();
    let account = Account::new(
        "Ada",
        ADDRESS,
        ServerSettings {
            host: imap_host,
            port: imap_port,
            security: Security::None,
            username: LOGIN.to_owned(),
        },
        ServerSettings {
            host: smtp_host,
            port: smtp_port,
            security: Security::None,
            username: LOGIN.to_owned(),
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
         Message-ID: <integration-{}@mailin.test>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         hello from the integration test\r\n",
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
            .contains("hello from the integration test"),
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
