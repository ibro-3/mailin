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
use crate::model::{Folder, FolderId, MessageHeader, MessageId, OutboxEntry};
use crate::store::Store;

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
    pub fn save_account(&self, account: &Account, password: Option<&str>) -> Result<()> {
        account.validate()?;
        self.store.upsert_account(account)?;
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
        self.store.list_accounts()
    }

    pub fn account(&self, id: &AccountId) -> Result<Option<Account>> {
        self.store.account(id)
    }

    pub fn has_password(&self, id: &AccountId) -> Result<bool> {
        Ok(self.credentials.get(id)?.is_some())
    }

    pub fn set_password(&self, id: &AccountId, password: &str) -> Result<()> {
        self.credentials.set(id, password)
    }

    /// Password for a connection attempt. The UI should only call this from a
    /// worker, never from a widget callback that renders it.
    pub fn password(&self, id: &AccountId) -> Result<String> {
        self.credentials
            .get(id)?
            .ok_or_else(|| Error::Credentials("no password is stored for this account".into()))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::ServerSettings;
    use crate::credentials::MemoryCredentials;

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
        client.save_account(&acc, Some("secret")).expect("save");

        assert_eq!(client.list_accounts().expect("list"), vec![acc.clone()]);
        assert!(client.has_password(&acc.id).expect("has"));
        assert_eq!(client.password(&acc.id).expect("password"), "secret");
        assert_eq!(events.try_recv().expect("event"), Event::AccountsChanged);
    }

    #[test]
    fn removing_an_account_forgets_the_password() {
        let client = client();
        let acc = account();
        client.save_account(&acc, Some("secret")).expect("save");
        client.remove_account(&acc.id).expect("remove");
        assert!(client.remove_account(&acc.id).is_err());
        assert!(!client.has_password(&acc.id).expect("has"));
        assert!(client.list_accounts().expect("list").is_empty());
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
