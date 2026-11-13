//! Hermetic tests for the engine wake paths (Phase 2 X11/X12, Phase 5 C3).
//!
//! These tests exercise the real `Engine` against an in-memory store. The SMTP
//! server is `127.0.0.1:1`, which refuses connections, so delivery fails fast —
//! if the outbox sender were not woken by `queue_and_send`, the entry would
//! sit out the 60-second poll and this test would time out.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mailin_core::config::Paths;
use mailin_core::{
    Account, AuthMethod, Config, Engine, Mailin, MemoryCredentials, OAuth2Tokens, OutboxState,
    OutgoingEnvelope, Security, ServerSettings, Store,
};
use secrecy::SecretString;

const ADDRESS: &str = "ada@example.com";

fn mailin_with_store(store: Arc<Store>) -> Arc<Mailin> {
    let paths = Paths::with_root(tempfile::tempdir().expect("temp dir").path());
    let config_cache = Arc::new(std::sync::RwLock::new(Some(Config::default())));
    Arc::new(Mailin::with_parts(
        store,
        Arc::new(MemoryCredentials::new()),
        config_cache,
        Arc::new(paths),
    ))
}

fn unreachable_settings(username: &str) -> ServerSettings {
    ServerSettings {
        host: "127.0.0.1".to_owned(),
        port: 1,
        security: Security::None,
        username: username.to_owned(),
        auth_method: AuthMethod::default(),
        oauth2: OAuth2Tokens::default(),
    }
}

/// `queue_and_send` must wake the outbox sender: the message is delivered (or,
/// here, fails) without waiting for the 60-second poll interval.
#[test]
fn queue_and_send_wakes_outbox_sender() {
    let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
    let mailin = mailin_with_store(store.clone());

    let account = Account::new(
        "Ada",
        ADDRESS,
        unreachable_settings(ADDRESS),
        unreachable_settings(ADDRESS),
    );
    mailin
        .save_account(&account, Some(&SecretString::new("secret".into())))
        .expect("save account");

    let engine = Engine::start(mailin.clone()).expect("start engine");

    let raw = format!(
        "From: <{ADDRESS}>\r\n\
         To: <{ADDRESS}>\r\n\
         Subject: wake test\r\n\
         Message-ID: <wake-{}@mailin.test>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         hello\r\n",
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

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let entry = store.outbox_entry(outbox_id).expect("read outbox entry");
        let entry = entry.expect("outbox entry exists");
        if entry.state == OutboxState::Failed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "outbox entry was not processed within 20 s (state: {:?}) — the outbox sender was not woken",
            entry.state
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
