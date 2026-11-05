//! The façade the front-ends use.
//!
//! [`Mailin`] owns the store and the credential store and hands out a
//! broadcast channel for change notifications. Synchronization workers will
//! publish through the same channel in the next milestone, so the UIs never
//! need to know whether an update came from the network or from a local edit.

use std::fmt;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

use crate::account::{Account, AccountId};
use crate::config::Paths;
use crate::credentials::CredentialsStore;
use crate::error::{Error, Result};
use crate::model::{
    Folder, FolderId, MessageHeader, MessageId, OutboxEntry, OutboxState, PgpKey, PgpMime, Plugin,
    SignatureStatus, VirtualFolderKind,
};
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
    /// `message_ids` lists the messages that were created by this sync so
    /// notification actions can target exactly those messages.
    NewMessages {
        account_id: AccountId,
        folder_id: FolderId,
        count: u32,
        message_ids: Vec<MessageId>,
    },
    BodyReady {
        message_id: MessageId,
    },
    OutboxChanged,
    ScheduledActionRan {
        account_id: AccountId,
        action_id: i64,
        kind: String,
    },
    AccountError {
        account_id: AccountId,
        message: String,
    },
    /// A user-initiated action (archive, delete, send…) failed. The message
    /// is user-facing (see [`crate::error::Error::friendly`]); the technical
    /// details are only in the log.
    ActionFailed {
        account_id: AccountId,
        message: String,
    },
    /// Sync status changed for an account.
    SyncStatusChanged {
        account_id: AccountId,
        status: crate::model::SyncStatus,
    },
}

/// Backward compatibility alias for front-ends that imported `client::Event as ClientEvent`.
pub type ClientEvent = Event;

pub struct Mailin {
    store: Arc<Store>,
    credentials: Arc<dyn CredentialsStore>,
    events: broadcast::Sender<Event>,
    plugin_system: Arc<RwLock<Option<Arc<crate::plugins::PluginSystem>>>>,
    config_cache: Arc<RwLock<Option<crate::Config>>>,
    config_paths: Arc<Paths>,
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

        let config = crate::Config::load(paths)?;
        let config_cache = Arc::new(RwLock::new(Some(config)));
        let config_paths = Arc::new(paths.clone());

        Ok(Self::with_parts(
            store,
            credentials,
            config_cache,
            config_paths,
        ))
    }

    pub fn with_parts(
        store: Arc<Store>,
        credentials: Arc<dyn CredentialsStore>,
        config_cache: Arc<RwLock<Option<crate::Config>>>,
        config_paths: Arc<Paths>,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            store,
            credentials,
            events,
            plugin_system: Arc::new(RwLock::new(None)),
            config_cache,
            config_paths,
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

    pub fn list_messages_virtual(
        &self,
        kind: VirtualFolderKind,
        account_filter: &[AccountId],
        limit: u32,
        offset: u32,
    ) -> Result<Vec<(MessageHeader, FolderId, AccountId)>> {
        self.store
            .list_messages_virtual(kind, account_filter, limit.clamp(1, 1000), offset)
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

    /// Enhanced search with structured query parsing and optional folder scope.
    pub fn search_messages(
        &self,
        account: &AccountId,
        folder_id: Option<FolderId>,
        query: &str,
        limit: u32,
    ) -> Result<Vec<MessageHeader>> {
        self.store.search_messages(account, folder_id, query, limit)
    }

    /// Get search history for an account.
    pub fn search_history(&self, account: &AccountId) -> Vec<crate::config::SearchHistoryEntry> {
        self.config_cache
            .read()
            .ok()
            .and_then(|c| c.clone())
            .map(|c| c.search.get(account))
            .unwrap_or_default()
    }

    /// Add a search query to history.
    pub fn add_search_history(
        &self,
        account: &AccountId,
        query: String,
        folder_id: Option<FolderId>,
    ) -> Result<()> {
        let mut cache = self.config_cache.write().map_err(|_| Error::LockPoisoned)?;
        if let Some(config) = cache.as_mut() {
            config.search.add(*account, query, folder_id);
            config.save(&self.config_paths)?;
        }
        Ok(())
    }

    pub fn unread_total(&self, account: &AccountId) -> Result<u32> {
        self.store.unread_total(account)
    }

    pub fn unread_total_all(&self) -> Result<u32> {
        let accounts = self.list_accounts()?;
        let mut total = 0;
        for account in accounts {
            total += self.store.unread_total(&account.id)?;
        }
        Ok(total)
    }

    // ----------------------------------------------------------------- sync status

    /// Get the current sync status for an account.
    pub fn sync_status(&self, _account: &AccountId) -> crate::model::SyncStatus {
        // This is a synchronous read; the engine updates the store asynchronously.
        // For now, return Idle; the Engine's sync_status method provides the real value.
        crate::model::SyncStatus::Idle
    }

    /// Get the last successful sync timestamp for an account.
    pub fn last_sync_at(&self, _account: &AccountId) -> Option<i64> {
        None
    }

    // ---------------------------------------------------------------------- pgp

    /// All public keys cached for an account.
    pub fn list_pgp_keys(&self, account: &AccountId) -> Result<Vec<PgpKey>> {
        self.store.pgp_keys_for_account(account)
    }

    /// Import an armored public key and cache it for `account`.
    pub fn import_pgp_public_key(&self, account: &AccountId, armored: &str) -> Result<PgpKey> {
        let cert = crate::pgp::import_public_key(armored)?;
        let public_key = crate::pgp::export_public_key(&cert)?;
        self.cache_pgp_key(account, &cert, &public_key)
    }

    /// Import an armored secret key: the public part goes to the database, the
    /// secret part to the OS keyring.
    pub fn import_pgp_secret_key(&self, account: &AccountId, armored: &str) -> Result<PgpKey> {
        let cert = crate::pgp::import_secret_key(armored)?;
        if !cert.is_tsk() {
            return Err(Error::Protocol(
                "No secret key material found in the imported key".into(),
            ));
        }
        let fingerprint = crate::pgp::key_fingerprint(&cert);
        let public_key = crate::pgp::export_public_key(&cert)?;
        let secret = crate::pgp::export_secret_key(&cert)?;
        self.credentials.set_pgp_secret(
            account,
            &fingerprint,
            &SecretString::new(secret.into()),
        )?;
        self.cache_pgp_key(account, &cert, &public_key)
    }

    /// Generate a fresh key pair for `user_id`, cache the public key and store
    /// the secret key in the keyring.
    pub fn generate_pgp_key(&self, account: &AccountId, user_id: &str) -> Result<PgpKey> {
        let (public_key, secret_key) = crate::pgp::generate_keypair(user_id)?;
        let cert = crate::pgp::import_secret_key(&secret_key)?;
        let fingerprint = crate::pgp::key_fingerprint(&cert);
        self.credentials.set_pgp_secret(
            account,
            &fingerprint,
            &SecretString::new(secret_key.into()),
        )?;
        let stored = self.store.upsert_pgp_key(
            account,
            &fingerprint,
            &crate::pgp::key_user_ids(&cert),
            &public_key,
        )?;
        self.publish(Event::AccountsChanged);
        Ok(PgpKey {
            id: stored,
            account_id: *account,
            fingerprint,
            user_ids: crate::pgp::key_user_ids(&cert),
            public_key,
            created_at: crate::now_unix(),
        })
    }

    /// Remove a key from the database and the keyring.
    pub fn delete_pgp_key(&self, account: &AccountId, fingerprint: &str) -> Result<bool> {
        self.credentials.delete_pgp_secret(account, fingerprint)?;
        let deleted = self.store.delete_pgp_key(account, fingerprint)?;
        if deleted {
            self.publish(Event::AccountsChanged);
        }
        Ok(deleted)
    }

    /// The stored key whose user IDs mention `address`, if any.
    pub fn pgp_key_for_address(
        &self,
        account: &AccountId,
        address: &str,
    ) -> Result<Option<PgpKey>> {
        let address = address.trim().to_lowercase();
        if address.is_empty() {
            return Ok(None);
        }
        Ok(self
            .store
            .pgp_keys_for_account(account)?
            .into_iter()
            // Exact match on the addr-spec of each user ID; never a substring
            // match, which would let lookalike addresses select the wrong key.
            .find(|key| {
                key.user_ids
                    .iter()
                    .any(|uid| crate::pgp::user_id_email(uid).eq_ignore_ascii_case(&address))
            }))
    }

    /// `true` when a secret key for `fingerprint` is available to this account.
    pub fn has_pgp_secret(&self, account: &AccountId, fingerprint: &str) -> bool {
        matches!(
            self.credentials.get_pgp_secret(account, fingerprint),
            Ok(Some(_))
        )
    }

    /// `true` when the account has any key pair that can sign.
    pub fn has_signing_key(&self, account: &AccountId) -> bool {
        self.store
            .pgp_keys_for_account(account)
            .map(|keys| {
                keys.iter()
                    .any(|key| self.has_pgp_secret(account, &key.fingerprint))
            })
            .unwrap_or(false)
    }

    /// Store the CalDAV password for an account in the keyring.
    pub fn set_caldav_password(&self, account: &AccountId, password: &str) -> Result<()> {
        self.credentials
            .set_caldav_password(account, &SecretString::new(password.into()))
    }

    /// Load the CalDAV password for an account from the keyring.
    pub fn get_caldav_password(&self, account: &AccountId) -> Result<Option<String>> {
        self.credentials
            .get_caldav_password(account)
            .map(|opt| opt.map(|s| s.expose_secret().to_owned()))
    }

    /// Remove the CalDAV password for an account from the keyring.
    pub fn delete_caldav_password(&self, account: &AccountId) -> Result<()> {
        self.credentials.delete_caldav_password(account)
    }

    /// Get the plugin system for managing Lua plugins.
    pub async fn plugin_system(&self) -> Option<Arc<crate::plugins::PluginSystem>> {
        self.plugin_system.read().ok().and_then(|c| c.clone())
    }

    /// Set the plugin system (called by Engine after initialization).
    pub async fn set_plugin_system(&self, system: Arc<crate::plugins::PluginSystem>) {
        if let Ok(mut guard) = self.plugin_system.write() {
            *guard = Some(system);
        }
    }

    /// List all loaded plugins.
    pub async fn list_plugins(&self) -> Vec<Plugin> {
        if let Some(sys) = self.plugin_system().await {
            sys.list_plugins().await
        } else {
            Vec::new()
        }
    }

    /// Enable/disable a plugin.
    pub async fn set_plugin_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        if let Some(sys) = self.plugin_system().await {
            sys.set_enabled(name, enabled).await
        } else {
            Ok(())
        }
    }

    /// Unload a plugin.
    pub async fn unload_plugin(&self, name: &str) -> Result<()> {
        if let Some(sys) = self.plugin_system().await {
            sys.unload(name).await
        } else {
            Ok(())
        }
    }

    /// List all loaded plugins (blocking version).
    pub fn list_plugins_blocking(&self) -> Result<Vec<Plugin>> {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Config(format!("could not start the plugin runtime: {e}")))?;
        Ok(runtime.block_on(self.list_plugins()))
    }

    /// Enable/disable a plugin (blocking version).
    pub fn set_plugin_enabled_blocking(&self, name: &str, enabled: bool) -> Result<()> {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Config(format!("could not start the plugin runtime: {e}")))?;
        runtime.block_on(self.set_plugin_enabled(name, enabled))
    }

    /// Build an outgoing message, applying PGP options. Returns the wire bytes
    /// and the SMTP envelope.
    pub fn build_outgoing(
        &self,
        account: &AccountId,
        message: &crate::compose::OutgoingMessage,
        pgp: crate::compose::PgpOptions,
    ) -> Result<(Vec<u8>, crate::model::OutgoingEnvelope)> {
        let (raw, envelope) = message.build()?;
        if !pgp.is_enabled() {
            return Ok((raw, envelope));
        }

        let entity = crate::compose::mime_entity(&raw)?;
        if pgp.encrypt {
            let mut recipients = envelope.to.clone();
            recipients.extend(envelope.cc.iter().cloned());
            recipients.extend(envelope.bcc.iter().cloned());
            let armored = self.pgp_encrypt(account, &entity, &recipients, pgp.sign)?;
            return Ok((crate::compose::wrap_encrypted(&raw, &armored)?, envelope));
        }

        let signature = self.pgp_sign_detached(account, entity.as_bytes())?;
        Ok((crate::compose::wrap_signed(&raw, &signature)?, envelope))
    }

    /// Detached signature over arbitrary bytes using the account's own key.
    pub fn pgp_sign_detached(&self, account: &AccountId, data: &[u8]) -> Result<String> {
        let (cert, secret) = self
            .pgp_signer(account)?
            .ok_or_else(|| Error::Protocol("No secret key available for signing".into()))?;
        crate::pgp::sign_detached(data, &cert, &secret)
    }

    /// The OpenPGP/MIME payload stored for a message, if any.
    pub fn pgp_mime(&self, message: MessageId) -> Result<Option<PgpMime>> {
        self.store.pgp_mime(message)
    }

    /// Decrypt or verify an OpenPGP/MIME message on demand.
    ///
    /// Encrypted messages are decrypted with the account's secret keys.
    /// Signed messages are verified against the sender's cached public key
    /// (unknown senders yield [`SignatureStatus::UnknownKey`]).
    pub fn decrypt_message(&self, message: MessageId) -> Result<(String, SignatureStatus)> {
        let payload = self
            .store
            .pgp_mime(message)?
            .ok_or_else(|| Error::Protocol("message is not OpenPGP protected".into()))?;
        let header = self
            .store
            .message(message)?
            .ok_or_else(|| Error::Protocol("message not found".into()))?;
        let folder = self
            .store
            .folder(header.folder_id)?
            .ok_or_else(|| Error::FolderNotFound(header.folder_id.0))?;
        let account = folder.account_id;

        match payload.kind {
            crate::model::PgpMimeKind::Encrypted => self.pgp_decrypt(&account, &payload.payload),
            crate::model::PgpMimeKind::Signed => {
                let signature = payload
                    .signature
                    .ok_or_else(|| Error::Protocol("signed message has no signature".into()))?;
                let Some(sender) = header.from_addr.as_deref() else {
                    return Ok((payload.payload, SignatureStatus::UnknownKey));
                };
                let Some(key) = self.pgp_key_for_address(&account, sender)? else {
                    return Ok((payload.payload, SignatureStatus::UnknownKey));
                };
                let cert = crate::pgp::import_public_key(&key.public_key)?;
                let status =
                    crate::pgp::verify_detached(payload.payload.as_bytes(), &signature, &cert)?;
                Ok((payload.payload, status))
            }
        }
    }

    /// Decrypt/verify a message and parse the resulting MIME entity for display.
    /// Returns `(text, html, signature_status)`.
    pub fn decrypted_body(
        &self,
        message: MessageId,
    ) -> Result<(Option<String>, Option<String>, SignatureStatus)> {
        let (entity, status) = self.decrypt_message(message)?;
        let body = crate::imap::parse::parse_entity(entity.as_bytes());
        Ok((body.text, body.html, status))
    }

    /// Addresses (from `recipients`) that have no cached public key.
    pub fn missing_pgp_keys(
        &self,
        account: &AccountId,
        recipients: &[String],
    ) -> Result<Vec<String>> {
        let mut missing = Vec::new();
        for address in recipients {
            if self.pgp_key_for_address(account, address)?.is_none() {
                missing.push(address.clone());
            }
        }
        Ok(missing)
    }

    /// Encrypt `plaintext` for `recipients`; signs with the account's own key
    /// when `sign` is set. Returns armored ciphertext.
    pub fn pgp_encrypt(
        &self,
        account: &AccountId,
        plaintext: &str,
        recipients: &[String],
        sign: bool,
    ) -> Result<String> {
        let mut certs = Vec::new();
        let mut missing = Vec::new();
        for address in recipients {
            match self.pgp_key_for_address(account, address)? {
                Some(key) => match crate::pgp::import_public_key(&key.public_key) {
                    Ok(cert) => certs.push(cert),
                    Err(_) => missing.push(address.clone()),
                },
                None => missing.push(address.clone()),
            }
        }
        if !missing.is_empty() {
            return Err(Error::Protocol(format!(
                "No public key for: {}",
                missing.join(", ")
            )));
        }
        let signer = if sign {
            self.pgp_signer(account)?
        } else {
            None
        };
        let (signer_cert, signer_secret) = match signer {
            Some((cert, secret)) => (Some(cert), Some(secret)),
            None => (None, None),
        };
        crate::pgp::encrypt_message(
            plaintext,
            &certs,
            signer_cert.as_ref(),
            signer_secret.as_deref(),
        )
    }

    /// Sign `plaintext` as a cleartext signature using the account's own key.
    pub fn pgp_sign(&self, account: &AccountId, plaintext: &str) -> Result<String> {
        let (cert, secret) = self
            .pgp_signer(account)?
            .ok_or_else(|| Error::Protocol("No secret key available for signing".into()))?;
        crate::pgp::sign_message(plaintext, &cert, &secret)
    }

    /// Decrypt armored ciphertext with any secret key of the account and report
    /// the signature status.
    pub fn pgp_decrypt(
        &self,
        account: &AccountId,
        ciphertext: &str,
    ) -> Result<(String, SignatureStatus)> {
        let mut secret_certs = Vec::new();
        for key in self.store.pgp_keys_for_account(account)? {
            if let Some(secret) = self.credentials.get_pgp_secret(account, &key.fingerprint)? {
                if let Ok(cert) = crate::pgp::import_secret_key(secret.expose_secret()) {
                    secret_certs.push(cert);
                }
            }
        }
        if secret_certs.is_empty() {
            return Err(Error::Protocol(
                "No secret key available to decrypt this message".into(),
            ));
        }
        crate::pgp::decrypt_message(ciphertext, &secret_certs)
    }

    fn cache_pgp_key(
        &self,
        account: &AccountId,
        cert: &sequoia_openpgp::Cert,
        public_key: &str,
    ) -> Result<PgpKey> {
        let fingerprint = crate::pgp::key_fingerprint(cert);
        let user_ids = crate::pgp::key_user_ids(cert);
        let id = self
            .store
            .upsert_pgp_key(account, &fingerprint, &user_ids, public_key)?;
        self.publish(Event::AccountsChanged);
        Ok(PgpKey {
            id,
            account_id: *account,
            fingerprint,
            user_ids,
            public_key: public_key.to_owned(),
            created_at: crate::now_unix(),
        })
    }

    /// The first key of `account` that has secret key material in the keyring.
    fn pgp_signer(&self, account: &AccountId) -> Result<Option<(sequoia_openpgp::Cert, String)>> {
        for key in self.store.pgp_keys_for_account(account)? {
            if let Some(secret) = self.credentials.get_pgp_secret(account, &key.fingerprint)? {
                if let Ok(cert) = crate::pgp::import_secret_key(secret.expose_secret()) {
                    if cert.is_tsk() {
                        return Ok(Some((cert, secret.expose_secret().to_owned())));
                    }
                }
            }
        }
        Ok(None)
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
        let changed = self
            .store
            .set_outbox_state(entry_id, OutboxState::Queued, None)?;
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
    use crate::config::Paths;
    use crate::credentials::MemoryCredentials;
    use secrecy::{ExposeSecret, SecretString};
    use tempfile;

    fn account() -> Account {
        Account::new(
            "Ada",
            "ada@example.com",
            ServerSettings::imap("imap.example.com", "ada@example.com"),
            ServerSettings::smtp("smtp.example.com", "ada@example.com"),
        )
    }

    fn client() -> Mailin {
        let paths = Paths::with_root(tempfile::tempdir().expect("temp dir").path());
        let config = crate::Config::default();
        let config_cache = Arc::new(RwLock::new(Some(config)));
        let config_paths = Arc::new(paths);
        Mailin::with_parts(
            Arc::new(Store::open_in_memory().expect("store")),
            Arc::new(MemoryCredentials::new()),
            config_cache,
            config_paths,
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
        use crate::account::{AuthMethod, OAuth2Tokens, OAuthProvider};

        let client = client();
        let mut acc = account();
        let tokens = OAuth2Tokens {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 42,
            provider: OAuthProvider::Google,
            client_id: "test-client-id".into(),
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

    #[test]
    fn pgp_keys_round_trip_through_store_and_keyring() {
        let client = client();
        let acc = account();
        client.save_account(&acc, None).expect("save");
        let key = client
            .generate_pgp_key(&acc.id, "Alice <alice@example.com>")
            .expect("generate");

        let keys = client.list_pgp_keys(&acc.id).expect("list");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].fingerprint, key.fingerprint);
        assert!(client.has_pgp_secret(&acc.id, &key.fingerprint));

        let found = client
            .pgp_key_for_address(&acc.id, "ALICE@example.com")
            .expect("lookup")
            .expect("key");
        assert_eq!(found.fingerprint, key.fingerprint);
        assert!(
            client
                .pgp_key_for_address(&acc.id, "bob@example.com")
                .expect("lookup")
                .is_none()
        );

        assert!(
            client
                .delete_pgp_key(&acc.id, &key.fingerprint)
                .expect("delete")
        );
        assert!(client.list_pgp_keys(&acc.id).expect("list").is_empty());
        assert!(!client.has_pgp_secret(&acc.id, &key.fingerprint));
    }

    #[test]
    fn pgp_encrypt_decrypt_and_sign_through_the_client() {
        let client = client();
        let acc = account();
        client.save_account(&acc, None).expect("save");
        let key = client
            .generate_pgp_key(&acc.id, "Alice <alice@example.com>")
            .expect("generate");
        assert!(
            client
                .missing_pgp_keys(&acc.id, &["alice@example.com".into()])
                .expect("missing")
                .is_empty()
        );
        assert_eq!(
            client
                .missing_pgp_keys(&acc.id, &["bob@example.com".into()])
                .expect("missing"),
            vec!["bob@example.com".to_owned()]
        );

        let recipients = vec!["alice@example.com".to_owned()];
        let encrypted = client
            .pgp_encrypt(&acc.id, "top secret", &recipients, true)
            .expect("encrypt");
        let (plaintext, status) = client.pgp_decrypt(&acc.id, &encrypted).expect("decrypt");
        assert_eq!(plaintext, "top secret");
        assert_eq!(status, SignatureStatus::Valid);

        let signed = client.pgp_sign(&acc.id, "hello").expect("sign");
        let cert = crate::pgp::import_public_key(&key.public_key).expect("cert");
        assert_eq!(
            crate::pgp::verify_signature(&signed, &cert).expect("verify"),
            SignatureStatus::Valid
        );
    }

    #[test]
    fn pgp_encrypt_fails_without_recipient_key() {
        let client = client();
        let acc = account();
        client.save_account(&acc, None).expect("save");
        client
            .generate_pgp_key(&acc.id, "Alice <alice@example.com>")
            .expect("generate");
        let error = client
            .pgp_encrypt(&acc.id, "x", &["bob@example.com".into()], false)
            .expect_err("must fail");
        assert!(
            error
                .to_string()
                .contains("No public key for: bob@example.com")
        );
    }

    #[test]
    fn build_outgoing_encrypts_and_signs_for_recipients() {
        use crate::compose::PgpOptions;
        let client = client();
        let acc = account();
        client.save_account(&acc, None).expect("save");
        client
            .generate_pgp_key(&acc.id, "Alice <alice@example.com>")
            .expect("generate");

        let message = crate::compose::OutgoingMessage {
            from_name: "Alice".into(),
            from_addr: "alice@example.com".into(),
            to: "Alice <alice@example.com>".into(),
            subject: "Secret".into(),
            body: "hello".into(),
            ..crate::compose::OutgoingMessage::default()
        };
        let (raw, envelope) = client
            .build_outgoing(
                &acc.id,
                &message,
                PgpOptions {
                    encrypt: true,
                    sign: true,
                },
            )
            .expect("build");
        let text = String::from_utf8(raw).expect("utf-8");
        assert!(text.contains("multipart/encrypted"));
        assert_eq!(envelope.to, vec!["alice@example.com".to_owned()]);

        let start = text.find("-----BEGIN PGP MESSAGE-----").expect("begin");
        let end = text.find("-----END PGP MESSAGE-----").expect("end")
            + "-----END PGP MESSAGE-----".len();
        let (entity, status) = client
            .pgp_decrypt(&acc.id, &text[start..end])
            .expect("decrypt");
        assert_eq!(status, SignatureStatus::Valid);
        assert!(entity.contains("Content-Type: text/plain"));
        assert!(!entity.contains("Subject:"));
    }

    #[test]
    fn decrypt_message_reads_the_stored_payload() {
        use crate::model::{FolderRole, NewMessage, PgpMimeKind};
        use crate::store::FolderUpsert;

        let client = client();
        let acc = account();
        client.save_account(&acc, None).expect("save");
        client
            .generate_pgp_key(&acc.id, "Alice <alice@example.com>")
            .expect("generate");

        let mut folder = FolderUpsert::new(acc.id, "INBOX");
        folder.role = FolderRole::Inbox;
        folder.delimiter = "/".into();
        let folder_id = client.store().upsert_folder(&folder).expect("folder");

        let mut outgoing = crate::compose::OutgoingMessage {
            from_name: "Alice".into(),
            from_addr: "alice@example.com".into(),
            to: "Alice <alice@example.com>".into(),
            subject: "Encrypted".into(),
            body: "hello there".into(),
            ..crate::compose::OutgoingMessage::default()
        };
        outgoing.draft_key = None;
        let (raw, _) = outgoing.build().expect("build");
        let entity = crate::compose::mime_entity(&raw).expect("entity");
        let armored = client
            .pgp_encrypt(&acc.id, &entity, &["alice@example.com".into()], true)
            .expect("encrypt");

        let mut message = NewMessage::new(folder_id, 1);
        message.subject = "Encrypted".into();
        message.from_name = Some("Alice".into());
        message.from_addr = Some("alice@example.com".into());
        message.to_addrs = "alice@example.com".into();
        message.received_at = 0;
        let stored = client.store().upsert_message(&message).expect("insert");
        client
            .store()
            .upsert_pgp_mime(stored.id, PgpMimeKind::Encrypted, &armored, None)
            .expect("payload");

        assert!(client.pgp_mime(stored.id).expect("mime").is_some());
        let (text, html, status) = client.decrypted_body(stored.id).expect("decrypt");
        assert_eq!(status, SignatureStatus::Valid);
        assert_eq!(text.as_deref(), Some("hello there"));
        assert!(
            html.as_deref()
                .is_none_or(|html| html.contains("hello there"))
        );
    }

    #[test]
    fn build_outgoing_signs_without_encrypting() {
        use crate::compose::PgpOptions;
        let client = client();
        let acc = account();
        client.save_account(&acc, None).expect("save");
        let key = client
            .generate_pgp_key(&acc.id, "Alice <alice@example.com>")
            .expect("generate");

        let message = crate::compose::OutgoingMessage {
            from_name: "Alice".into(),
            from_addr: "alice@example.com".into(),
            to: "Bob <bob@example.com>".into(),
            subject: "Signed".into(),
            body: "hello".into(),
            ..crate::compose::OutgoingMessage::default()
        };
        let (raw, _) = client
            .build_outgoing(
                &acc.id,
                &message,
                PgpOptions {
                    encrypt: false,
                    sign: true,
                },
            )
            .expect("build");
        let text = String::from_utf8(raw).expect("utf-8");
        assert!(text.contains("multipart/signed"));

        let boundary = text
            .split_once("boundary=\"")
            .expect("boundary")
            .1
            .split('"')
            .next()
            .expect("boundary value");
        let marker = format!("--{boundary}");
        let body = text.split_once("\r\n\r\n").expect("body").1;
        let parts: Vec<&str> = body.split(&marker).collect();
        // parts[0] is the preamble; parts[1] is the signed entity.
        let entity = parts[1].strip_prefix("\r\n").expect("entity start");
        let signature = parts[2];
        let sig_start = signature
            .find("-----BEGIN PGP SIGNATURE-----")
            .expect("sig begin");
        let sig_end = signature
            .find("-----END PGP SIGNATURE-----")
            .expect("sig end")
            + "-----END PGP SIGNATURE-----".len();
        let cert = crate::pgp::import_public_key(&key.public_key).expect("cert");
        let status =
            crate::pgp::verify_detached(entity.as_bytes(), &signature[sig_start..sig_end], &cert)
                .expect("verify");
        assert_eq!(status, SignatureStatus::Valid);
    }
}
