//! Background network engine: IMAP synchronization, on-demand body fetches,
//! flag updates and the SMTP outbox.
//!
//! The engine owns one Tokio runtime. Each account has a worker task with a
//! single IMAP connection: commands, IDLE watch-ups and periodic polls are
//! serialized through that connection, which keeps the protocol state (selected
//! mailbox, UIDVALIDITY) coherent without locking.
//!
//! All communication with the rest of the application happens through
//! [`Mailin`]: rows are written to the store and [`Event`]s tell the UIs to
//! re-read them.

use either::Either;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_imap::extensions::idle::IdleResponse;
use tokio::runtime::Runtime;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::account::{Account, AccountId, AuthMethod, OAuth2Tokens};
use crate::client::{ClientEvent, Event, Mailin};
use crate::compose::OutgoingMessage;
use crate::error::{Error, Result};
use crate::imap::ImapSession;
use crate::model::{
    Folder, FolderId, FolderRole, MessageId, OutboxEntry, OutboxState, OutgoingEnvelope,
    ScheduledAction, SnoozePayload, SyncStatus,
};
use crate::sync::{self, SyncMode};
use secrecy::{ExposeSecret, SecretString};

const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// A snoozed-message batch grouped by source folder.
/// (message id, uid, source folder id, cached Message-ID header)
type SnoozeGroups = HashMap<FolderId, Vec<(MessageId, u32, FolderId, Option<String>)>>;
/// Re-run a full sync of the inbox occasionally to notice deletions and flag
/// changes made by other clients.
const FULL_SYNC_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Handle to the network engine.
///
/// Dropping the handle stops the runtime and all workers. The UI keeps one of
/// these for the lifetime of the process.
pub struct Engine {
    state: Arc<EngineState>,
    commands: mpsc::UnboundedSender<Command>,
    runtime: Option<Runtime>,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("mailbox", &self.state.mailbox)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Start the runtime and the background workers of all accounts that have
    /// background synchronization enabled.
    pub fn start(mailbox: Arc<Mailin>) -> Result<Self> {
        crate::tls::install_crypto_provider();

        let (scheduled_actions_sender, scheduled_actions_receiver) = mpsc::unbounded_channel();

        let mut state = Arc::new(EngineState {
            mailbox: Arc::clone(&mailbox),
            outbox: Arc::new(Notify::new()),
            sync_status: Arc::new(RwLock::new(HashMap::new())),
            last_sync_at: Arc::new(RwLock::new(HashMap::new())),
            plugin_system: None,
            scheduled_actions_sender,
            scheduled_actions_receiver: Some(scheduled_actions_receiver),
        });
        let (commands, receiver) = mpsc::unbounded_channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("mailin-net")
            .build()
            .map_err(|e| Error::Config(format!("could not start the network runtime: {e}")))?;

        // Initialize plugin system
        let data_dir = mailbox.store().data_dir();
        let plugin_system = runtime.block_on(async {
            match crate::plugins::init_plugin_system(mailbox.store(), &data_dir).await {
                Ok(sys) => Some(sys),
                Err(e) => {
                    tracing::warn!(%e, "Failed to initialize plugin system");
                    None
                }
            }
        });

        // Set plugin system on Mailin
        if let Some(ref sys) = plugin_system {
            runtime.block_on(mailbox.set_plugin_system(Arc::clone(sys)));
        }

        // Store plugin system in state
        let state_mut = Arc::get_mut(&mut state).ok_or_else(|| {
            Error::Config("engine state is unexpectedly shared during startup".to_string())
        })?;
        state_mut.plugin_system = plugin_system;

        // Take the receiver for supervise
        let scheduled_actions_receiver = state_mut
            .scheduled_actions_receiver
            .take()
            .expect("scheduled-actions receiver exists until taken");
        runtime.spawn(supervise(
            state.clone(),
            receiver,
            scheduled_actions_receiver,
        ));
        runtime.spawn(scheduled_worker(state.clone()));

        Ok(Self {
            state,
            commands,
            runtime: Some(runtime),
        })
    }

    /// Synchronize every folder of an account. Also starts the account worker
    /// if it is not running yet.
    pub fn sync_account(&self, account_id: AccountId) {
        self.send(Command::SyncAccount(account_id));
    }

    /// Synchronize one folder, for example when the user opens it.
    pub fn sync_folder(&self, folder_id: FolderId) {
        self.send(Command::SyncFolder(folder_id));
    }

    /// Download and parse the complete body of a message. Attachment bytes are
    /// cached alongside it.
    pub fn fetch_body(&self, message_id: MessageId) {
        self.send(Command::FetchBody(message_id));
    }

    /// Download a message again even when its body is already cached. Used to
    /// fill in attachment bytes for messages cached before milestone 4.
    pub fn refresh_body(&self, message_id: MessageId) {
        self.send(Command::RefreshBody(message_id));
    }

    /// Add and remove IMAP flags both on the server and in the cache.
    pub fn set_flags(&self, message_id: MessageId, add: Vec<String>, remove: Vec<String>) {
        self.send(Command::SetFlags {
            message_id,
            add,
            remove,
        });
    }

    /// Add and remove IMAP flags for many messages at once, batching one
    /// `UID STORE` per source folder instead of one round-trip per message.
    pub fn bulk_set_flags(&self, message_ids: &[MessageId], add: Vec<String>, remove: Vec<String>) {
        self.send(Command::BulkSetFlags {
            message_ids: message_ids.to_vec(),
            add,
            remove,
        });
    }

    /// Move messages into a specific folder.
    pub fn move_messages(&self, message_ids: &[MessageId], target: FolderId) {
        self.send(Command::MoveMessages {
            message_ids: message_ids.to_vec(),
            target: MoveTarget::Folder(target),
        });
    }

    /// Move messages to Trash, or delete them permanently when they are
    /// already in Trash.
    pub fn delete_messages(&self, message_ids: &[MessageId]) {
        self.send(Command::MoveMessages {
            message_ids: message_ids.to_vec(),
            target: MoveTarget::Trash,
        });
    }

    /// Delete messages permanently, without moving them to Trash.
    pub fn permanently_delete_messages(&self, message_ids: &[MessageId]) {
        self.send(Command::MoveMessages {
            message_ids: message_ids.to_vec(),
            target: MoveTarget::Permanent,
        });
    }

    pub fn archive_messages(&self, message_ids: &[MessageId]) {
        self.send(Command::MoveMessages {
            message_ids: message_ids.to_vec(),
            target: MoveTarget::Archive,
        });
    }

    /// Move messages to Junk (`junk = true`) or back to Inbox.
    pub fn mark_messages_junk(&self, message_ids: &[MessageId], junk: bool) {
        self.send(Command::MoveMessages {
            message_ids: message_ids.to_vec(),
            target: if junk {
                MoveTarget::Junk
            } else {
                MoveTarget::Inbox
            },
        });
    }

    /// Permanently delete every message in the account's Trash folder.
    pub fn empty_trash(&self, account_id: AccountId) {
        self.send(Command::EmptyTrash(account_id));
    }

    /// Sync calendars for an account using CalDAV.
    pub fn sync_calendars(&self, account_id: AccountId) {
        self.send(Command::SyncCalendars(account_id));
    }

    /// Append or replace a draft in the account's Drafts folder. Reusing
    /// `draft.draft_key` means auto-saves overwrite the previous copy.
    pub fn save_draft(&self, account_id: AccountId, draft: OutgoingMessage) {
        self.send(Command::SaveDraft {
            account_id,
            draft: Box::new(draft),
        });
    }

    /// Remove the server-side draft identified by its Message-ID. Called after
    /// a draft is sent or explicitly discarded.
    pub fn discard_draft(&self, account_id: AccountId, draft_key: impl Into<String>) {
        self.send(Command::DiscardDraft {
            account_id,
            draft_key: draft_key.into(),
        });
    }

    /// Retry a failed outbox entry (resets state to Queued).
    pub fn retry_outbox(&self, account_id: AccountId, entry_id: i64) {
        self.send(Command::RetryOutbox {
            account_id,
            entry_id,
        });
    }

    /// Cancel (delete) an outbox entry.
    pub fn cancel_outbox(&self, account_id: AccountId, entry_id: i64) {
        self.send(Command::CancelOutbox {
            account_id,
            entry_id,
        });
    }

    /// Snooze messages until the specified time (Unix timestamp).
    /// Messages are moved to the Snoozed folder and will be moved back at due_at.
    pub fn snooze_messages(&self, message_ids: &[MessageId], due_at: i64) {
        self.send(Command::SnoozeMessages {
            message_ids: message_ids.to_vec(),
            due_at,
        });
    }

    /// Unsnooze messages immediately, moving them back to their original folders.
    pub fn unsnooze_messages(&self, message_ids: &[MessageId]) {
        self.send(Command::UnsnoozeMessages {
            message_ids: message_ids.to_vec(),
        });
    }

    /// Apply rules to messages in a folder (used after body fetch).
    pub fn apply_rules(&self, folder_id: FolderId, message_ids: &[MessageId]) {
        self.send(Command::ApplyRules {
            folder_id,
            message_ids: message_ids.to_vec(),
        });
    }

    /// Get the current sync status for an account.
    pub fn sync_status(&self, account_id: &AccountId) -> SyncStatus {
        self.state
            .sync_status
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .get(account_id)
            .copied()
            .unwrap_or(SyncStatus::Idle)
    }

    /// Get the last successful sync timestamp for an account.
    pub fn last_sync_at(&self, account_id: &AccountId) -> Option<i64> {
        self.state
            .last_sync_at
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .get(account_id)
            .copied()
    }

    /// Store a message in the outbox and wake the sender.
    pub fn queue_and_send(
        &self,
        account_id: AccountId,
        raw: Vec<u8>,
        envelope: OutgoingEnvelope,
    ) -> Result<i64> {
        envelope.validate()?;
        let envelope_json = serde_json::to_string(&envelope)?;
        let id = self
            .state
            .mailbox
            .queue_outgoing(&account_id, &raw, &envelope_json)?;
        self.state.outbox.notify_one();
        Ok(id)
    }

    // Folder management

    /// Create a new folder on the server and locally.
    pub fn create_folder(
        &self,
        account_id: AccountId,
        name: String,
        delimiter: String,
        role: FolderRole,
    ) {
        self.send(Command::CreateFolder {
            account_id,
            name,
            delimiter,
            role,
        });
    }

    /// Rename a folder on the server and locally.
    pub fn rename_folder(&self, folder_id: FolderId, new_name: String) {
        self.send(Command::RenameFolder {
            folder_id,
            new_name,
        });
    }

    /// Delete a folder on the server and locally.
    pub fn delete_folder(&self, folder_id: FolderId) {
        self.send(Command::DeleteFolder(folder_id));
    }

    /// Subscribe or unsubscribe from a folder.
    pub fn subscribe_folder(&self, folder_id: FolderId, subscribe: bool) {
        self.send(Command::SubscribeFolder {
            folder_id,
            subscribe,
        });
    }

    /// Stop the workers and wait briefly for them to finish.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            tracing::debug!("network engine is not running");
        }
    }

    fn shutdown(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct EngineState {
    mailbox: Arc<Mailin>,
    outbox: Arc<Notify>,
    /// Per-account sync status. Workers update this; UIs read via Mailin.
    sync_status: Arc<RwLock<HashMap<AccountId, SyncStatus>>>,
    /// Timestamp of the last successful sync per account.
    last_sync_at: Arc<RwLock<HashMap<AccountId, i64>>>,
    /// Plugin system for Lua-based extensions.
    plugin_system: Option<Arc<crate::plugins::PluginSystem>>,
    /// Channel for sending scheduled actions from scheduled_worker to supervise.
    scheduled_actions_sender: mpsc::UnboundedSender<(AccountId, ScheduledAction)>,
    /// Receiver for supervise to forward scheduled actions to account workers.
    scheduled_actions_receiver: Option<mpsc::UnboundedReceiver<(AccountId, ScheduledAction)>>,
}

enum Command {
    SyncAccount(AccountId),
    SyncFolder(FolderId),
    FetchBody(MessageId),
    RefreshBody(MessageId),
    SetFlags {
        message_id: MessageId,
        add: Vec<String>,
        remove: Vec<String>,
    },
    BulkSetFlags {
        message_ids: Vec<MessageId>,
        add: Vec<String>,
        remove: Vec<String>,
    },
    MoveMessages {
        message_ids: Vec<MessageId>,
        target: MoveTarget,
    },
    EmptyTrash(AccountId),
    SaveDraft {
        account_id: AccountId,
        draft: Box<OutgoingMessage>,
    },
    DiscardDraft {
        account_id: AccountId,
        draft_key: String,
    },
    RetryOutbox {
        account_id: AccountId,
        entry_id: i64,
    },
    CancelOutbox {
        account_id: AccountId,
        entry_id: i64,
    },
    SnoozeMessages {
        message_ids: Vec<MessageId>,
        due_at: i64,
    },
    UnsnoozeMessages {
        message_ids: Vec<MessageId>,
    },
    ApplyRules {
        folder_id: FolderId,
        message_ids: Vec<MessageId>,
    },
    CreateFolder {
        account_id: AccountId,
        name: String,
        delimiter: String,
        role: FolderRole,
    },
    RenameFolder {
        folder_id: FolderId,
        new_name: String,
    },
    DeleteFolder(FolderId),
    SubscribeFolder {
        folder_id: FolderId,
        subscribe: bool,
    },
    SyncCalendars(AccountId),
    Shutdown,
}

/// Where a batch of messages should go. Role-based targets are resolved per
/// account so the UIs never need to know folder names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveTarget {
    Folder(FolderId),
    Trash,
    Archive,
    Junk,
    Inbox,
    /// Delete permanently instead of moving to Trash.
    Permanent,
}

/// Messages grouped by source folder and optional destination folder.
type MoveGroups = HashMap<(FolderId, Option<FolderId>), Vec<(MessageId, u32)>>;

#[derive(Debug)]
enum WorkerCommand {
    SyncAccount,
    SyncFolder(FolderId),
    FetchBody(MessageId),
    RefreshBody(MessageId),
    SetFlags {
        message_id: MessageId,
        add: Vec<String>,
        remove: Vec<String>,
    },
    BulkSetFlags {
        message_ids: Vec<MessageId>,
        add: Vec<String>,
        remove: Vec<String>,
    },
    MoveMessages {
        message_ids: Vec<MessageId>,
        target: MoveTarget,
    },
    EmptyTrash,
    SaveDraft {
        draft: Box<OutgoingMessage>,
    },
    DiscardDraft {
        draft_key: String,
    },
    RetryOutbox {
        entry_id: i64,
    },
    CancelOutbox {
        entry_id: i64,
    },
    SnoozeMessages {
        message_ids: Vec<MessageId>,
        due_at: i64,
    },
    UnsnoozeMessages {
        message_ids: Vec<MessageId>,
    },
    ApplyRules {
        folder_id: FolderId,
        message_ids: Vec<MessageId>,
    },
    CreateFolder {
        account_id: AccountId,
        name: String,
        delimiter: String,
        role: FolderRole,
    },
    RenameFolder {
        folder_id: FolderId,
        new_name: String,
    },
    DeleteFolder(FolderId),
    SubscribeFolder {
        folder_id: FolderId,
        subscribe: bool,
    },
    SyncCalendars,
    ExecuteScheduledAction {
        action: ScheduledAction,
    },
    Stop,
}

async fn supervise(
    state: Arc<EngineState>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut scheduled_actions: mpsc::UnboundedReceiver<(AccountId, ScheduledAction)>,
) {
    let mut workers: HashMap<AccountId, mpsc::UnboundedSender<WorkerCommand>> = HashMap::new();
    let sender = tokio::spawn(outbox_sender(state.clone()));
    // scheduled_worker is now spawned in Engine::start

    // Start the workers of accounts that want background synchronization. A
    // worker connects and syncs the inbox on its own.
    if let Ok(accounts) = state.mailbox.list_accounts() {
        for account in accounts {
            if account.sync.background_sync {
                ensure_worker(&mut workers, &state, account.id);
            }
        }
    }

    loop {
        tokio::select! {
            command = commands.recv() => {
                if let Some(command) = command {
                    match command {
                        Command::Shutdown => break,
                        Command::SyncAccount(account_id) => {
                            let worker = ensure_worker(&mut workers, &state, account_id);
                            let _ = worker.send(WorkerCommand::SyncAccount);
                        }
                        Command::SyncFolder(folder_id) => {
                            if let Some(account_id) = account_for_folder(&state, folder_id) {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::SyncFolder(folder_id));
                            }
                        }
                        Command::SyncCalendars(account_id) => {
                            let worker = ensure_worker(&mut workers, &state, account_id);
                            let _ = worker.send(WorkerCommand::SyncCalendars);
                        }
                        Command::FetchBody(message_id) => {
                            if let Some(account_id) = account_for_message(&state, message_id) {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::FetchBody(message_id));
                            }
                        }
                        Command::RefreshBody(message_id) => {
                            if let Some(account_id) = account_for_message(&state, message_id) {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::RefreshBody(message_id));
                            }
                        }
                        Command::SetFlags {
                            message_id,
                            add,
                            remove,
                        } => {
                            if let Some(account_id) = account_for_message(&state, message_id) {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::SetFlags {
                                    message_id,
                                    add,
                                    remove,
                                });
                            }
                        }
                        Command::BulkSetFlags {
                            message_ids,
                            add,
                            remove,
                        } => {
                            let mut by_account: HashMap<AccountId, Vec<MessageId>> = HashMap::new();
                            for message_id in message_ids {
                                if let Some(account_id) = account_for_message(&state, message_id) {
                                    by_account.entry(account_id).or_default().push(message_id);
                                }
                            }
                            for (account_id, message_ids) in by_account {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::BulkSetFlags {
                                    message_ids,
                                    add: add.clone(),
                                    remove: remove.clone(),
                                });
                            }
                        }
                        Command::MoveMessages {
                            message_ids,
                            target,
                        } => {
                            let mut by_account: HashMap<AccountId, Vec<MessageId>> = HashMap::new();
                            for message_id in message_ids {
                                if let Some(account_id) = account_for_message(&state, message_id) {
                                    by_account.entry(account_id).or_default().push(message_id);
                                }
                            }
                            for (account_id, message_ids) in by_account {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::MoveMessages {
                                    message_ids,
                                    target,
                                });
                            }
                        }
                        Command::EmptyTrash(account_id) => {
                            let worker = ensure_worker(&mut workers, &state, account_id);
                            let _ = worker.send(WorkerCommand::EmptyTrash);
                        }
                        Command::SaveDraft { account_id, draft } => {
                            let worker = ensure_worker(&mut workers, &state, account_id);
                            let _ = worker.send(WorkerCommand::SaveDraft { draft });
                        }
                        Command::DiscardDraft {
                            account_id,
                            draft_key,
                        } => {
                            let worker = ensure_worker(&mut workers, &state, account_id);
                            let _ = worker.send(WorkerCommand::DiscardDraft { draft_key });
                        }
                        Command::RetryOutbox {
                            account_id,
                            entry_id,
                        } => {
                            let worker = ensure_worker(&mut workers, &state, account_id);
                            let _ = worker.send(WorkerCommand::RetryOutbox { entry_id });
                        }
                        Command::CancelOutbox {
                            account_id,
                            entry_id,
                        } => {
                            let worker = ensure_worker(&mut workers, &state, account_id);
                            let _ = worker.send(WorkerCommand::CancelOutbox { entry_id });
                        }
                        Command::SnoozeMessages {
                            message_ids,
                            due_at,
                        } => {
                            let mut by_account: HashMap<AccountId, Vec<MessageId>> = HashMap::new();
                            for message_id in message_ids {
                                if let Some(account_id) = account_for_message(&state, message_id) {
                                    by_account.entry(account_id).or_default().push(message_id);
                                }
                            }
                            for (account_id, message_ids) in by_account {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::SnoozeMessages { message_ids, due_at });
                            }
                        }
                        Command::UnsnoozeMessages {
                            message_ids,
                        } => {
                            let mut by_account: HashMap<AccountId, Vec<MessageId>> = HashMap::new();
                            for message_id in message_ids {
                                if let Some(account_id) = account_for_message(&state, message_id) {
                                    by_account.entry(account_id).or_default().push(message_id);
                                }
                            }
                            for (account_id, message_ids) in by_account {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::UnsnoozeMessages { message_ids });
                            }
                        }
                        Command::ApplyRules {
                            folder_id,
                            message_ids,
                        } => {
                            if let Some(account_id) = account_for_folder(&state, folder_id) {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::ApplyRules { folder_id, message_ids });
                            }
                        }
                        Command::CreateFolder {
                            account_id,
                            name,
                            delimiter,
                            role,
                        } => {
                            let worker = ensure_worker(&mut workers, &state, account_id);
                            let _ = worker.send(WorkerCommand::CreateFolder {
                                account_id,
                                name,
                                delimiter,
                                role,
                            });
                        }
                        Command::RenameFolder {
                            folder_id,
                            new_name,
                        } => {
                            if let Some(account_id) = account_for_folder(&state, folder_id) {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::RenameFolder { folder_id, new_name });
                            }
                        }
                        Command::DeleteFolder(folder_id) => {
                            if let Some(account_id) = account_for_folder(&state, folder_id) {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::DeleteFolder(folder_id));
                            }
                        }
                        Command::SubscribeFolder {
                            folder_id,
                            subscribe,
                        } => {
                            if let Some(account_id) = account_for_folder(&state, folder_id) {
                                let worker = ensure_worker(&mut workers, &state, account_id);
                                let _ = worker.send(WorkerCommand::SubscribeFolder { folder_id, subscribe });
                            }
                        }
                    }
                } else {
                    break;
                }
            }
            scheduled_action = scheduled_actions.recv() => {
                if let Some((account_id, action)) = scheduled_action {
                    let worker = ensure_worker(&mut workers, &state, account_id);
                    let _ = worker.send(WorkerCommand::ExecuteScheduledAction { action });
                }
            }
        }
    }

    for worker in workers.values() {
        let _ = worker.send(WorkerCommand::Stop);
    }
    sender.abort();
    let _ = sender.await;
}

fn ensure_worker(
    workers: &mut HashMap<AccountId, mpsc::UnboundedSender<WorkerCommand>>,
    state: &Arc<EngineState>,
    account_id: AccountId,
) -> mpsc::UnboundedSender<WorkerCommand> {
    if let Some(sender) = workers.get(&account_id) {
        if !sender.is_closed() {
            return sender.clone();
        }
        // The task exited (for example because the account was removed);
        // replace it so the command is not dropped.
        workers.remove(&account_id);
    }

    let (sender, receiver) = mpsc::unbounded_channel();
    let state = state.clone();
    tokio::spawn(async move {
        account_worker(state, account_id, receiver).await;
    });
    workers.insert(account_id, sender.clone());
    sender
}

fn account_for_folder(state: &EngineState, folder_id: FolderId) -> Option<AccountId> {
    state
        .mailbox
        .folder(folder_id)
        .ok()
        .flatten()
        .map(|folder| folder.account_id)
}

fn account_for_message(state: &EngineState, message_id: MessageId) -> Option<AccountId> {
    let message = state.mailbox.message(message_id).ok().flatten()?;
    account_for_folder(state, message.folder_id)
}

async fn account_worker(
    state: Arc<EngineState>,
    account_id: AccountId,
    mut commands: mpsc::UnboundedReceiver<WorkerCommand>,
) {
    let mut pending = VecDeque::new();
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let mut account = match state.mailbox.account(&account_id) {
            Ok(Some(account)) => account,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%account_id, %error, "could not load the account");
                if !wait_with_backoff(&mut commands, &mut pending, backoff).await {
                    return;
                }
                backoff = next_backoff(backoff);
                continue;
            }
        };

        // Refresh an expiring OAuth2 access token before connecting. The
        // refreshed token is persisted so a restart does not need another
        // round-trip.
        if let Err(error) = refresh_oauth_tokens(&state, &mut account).await {
            state.mailbox.publish(Event::AccountError {
                account_id,
                message: error.friendly(),
            });
            tracing::warn!(%account_id, %error, "could not refresh OAuth2 tokens");
            if !wait_with_backoff(&mut commands, &mut pending, backoff).await {
                return;
            }
            backoff = next_backoff(backoff);
            continue;
        }

        // For OAuth2 accounts, we need the access token instead of a password
        let auth_creds = if account.imap.auth_method == AuthMethod::OAuth2 {
            if account.imap.oauth2.access_token.is_empty() {
                state.mailbox.publish(Event::AccountError {
                    account_id,
                    message: "OAuth2 access token not configured".into(),
                });
                if !wait_with_backoff(&mut commands, &mut pending, backoff).await {
                    return;
                }
                backoff = next_backoff(backoff);
                continue;
            }
            // Create a secret string with the access token for OAuth2
            Either::Right(secrecy::SecretString::new(
                account.imap.oauth2.access_token.clone().into(),
            ))
        } else {
            let password = match state.mailbox.password(&account_id) {
                Ok(password) => password,
                Err(error) => {
                    state.mailbox.publish(Event::AccountError {
                        account_id,
                        message: error.friendly(),
                    });
                    if !wait_with_backoff(&mut commands, &mut pending, backoff).await {
                        return;
                    }
                    backoff = next_backoff(backoff);
                    continue;
                }
            };
            Either::Left(password)
        };

        let started = Instant::now();
        match run_session(&state, &account, auth_creds, &mut commands, &mut pending).await {
            Ok(()) => return,
            Err(error) => {
                match &error {
                    // A command the user asked for failed: say what didn't
                    // work, without suggesting a full sync as "Retry".
                    Error::ActionFailed { .. } => {
                        state.mailbox.publish(Event::ActionFailed {
                            account_id,
                            message: error.friendly(),
                        });
                    }
                    _ => {
                        state.mailbox.publish(Event::AccountError {
                            account_id,
                            message: error.friendly(),
                        });
                    }
                }
                // Network/connection failures -> Offline, others -> Error.
                // Match the structured io::ErrorKind first; narrow, explicit
                // phrases are used for string-wrapped network errors so
                // server-side failures such as "connection limit exceeded"
                // are not misreported as the client being offline.
                let is_offline = is_offline_error(&error);
                let status = if is_offline {
                    SyncStatus::Offline
                } else {
                    SyncStatus::Error
                };
                set_sync_status(&state, account_id, status);
                tracing::warn!(%account_id, %error, "account session failed");
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = INITIAL_BACKOFF;
                }
                if !wait_with_backoff(&mut commands, &mut pending, backoff).await {
                    return;
                }
                backoff = next_backoff(backoff);
            }
        }
    }
}

/// Whether an error from a sync/session indicates a connectivity problem
/// rather than a protocol, auth or configuration failure.
///
/// Structured `io::Error` kinds are trusted; everything else is matched
/// against specific network phrases (not the bare word "connection", so
/// server-side errors like "connection limit exceeded" classify as `Error`).
fn worker_action_name(command: &WorkerCommand) -> &'static str {
    match command {
        WorkerCommand::MoveMessages { target, .. } => match target {
            MoveTarget::Archive => "archive the messages",
            MoveTarget::Trash => "move the messages to Trash",
            MoveTarget::Junk => "mark the messages as Junk",
            MoveTarget::Inbox => "move the messages to Inbox",
            MoveTarget::Permanent => "permanently delete the messages",
            MoveTarget::Folder(_) => "move the messages",
        },
        WorkerCommand::EmptyTrash => "empty Trash",
        WorkerCommand::SaveDraft { .. } => "save the draft",
        WorkerCommand::DiscardDraft { .. } => "discard the draft",
        WorkerCommand::RetryOutbox { .. } => "retry sending the message",
        WorkerCommand::CancelOutbox { .. } => "cancel the message",
        WorkerCommand::SetFlags { .. } | WorkerCommand::BulkSetFlags { .. } => {
            "update the messages"
        }
        WorkerCommand::SnoozeMessages { .. } => "snooze the messages",
        WorkerCommand::UnsnoozeMessages { .. } => "unsnooze the messages",
        WorkerCommand::ApplyRules { .. } => "apply the rules",
        _ => "run a command",
    }
}

/// Attach the operation a worker was performing to its failure, so the UI can
/// tell the user what didn't work instead of a bare transport error.
fn action_failed(action: &'static str, error: Error) -> Error {
    Error::ActionFailed {
        action: action.into(),
        reason: error.to_string(),
    }
}

fn is_offline_error(error: &Error) -> bool {
    if let Error::Io(io) = error {
        return matches!(
            io.kind(),
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::NetworkUnreachable
                | std::io::ErrorKind::HostUnreachable
                | std::io::ErrorKind::AddrNotAvailable
        );
    }
    let text = error.to_string().to_lowercase();
    [
        "connection refused",
        "connection reset",
        "connection timed out",
        "connection closed",
        "connection aborted",
        "dns failure",
        "dns resolution",
        "name or service not known",
        "network unreachable",
        "host unreachable",
        "no route to host",
        "timed out",
        "broken pipe",
        "not connected",
        "eof",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// Refresh the OAuth2 access tokens in place when they are about to expire.
///
/// A no-op for password accounts, accounts without a refresh token, and tokens
/// that are still valid. New tokens are written back to the store so they
/// survive a restart.
///
/// IMAP and SMTP are refreshed independently when they use different grants
/// (different provider, client id or refresh token — or only one side is an
/// OAuth2 account). When both sides share the same grant, a single refresh
/// covers both; refreshing twice would break providers that rotate refresh
/// tokens on every exchange.
async fn refresh_oauth_tokens(state: &Arc<EngineState>, account: &mut Account) -> Result<()> {
    let imap_needs =
        account.imap.auth_method == AuthMethod::OAuth2 && needs_refresh(&account.imap.oauth2);
    let smtp_needs =
        account.smtp.auth_method == AuthMethod::OAuth2 && needs_refresh(&account.smtp.oauth2);
    if !imap_needs && !smtp_needs {
        return Ok(());
    }

    let shared_grant = account.imap.auth_method == AuthMethod::OAuth2
        && account.smtp.auth_method == AuthMethod::OAuth2
        && account.imap.oauth2.provider == account.smtp.oauth2.provider
        && account.imap.oauth2.client_id == account.smtp.oauth2.client_id
        && account.imap.oauth2.refresh_token == account.smtp.oauth2.refresh_token;

    if shared_grant {
        // One login, one refresh. Assign the result to both sides so the
        // shared grant stays consistent even if only one side is expired.
        let refreshed = refresh_tokens(&account.imap.oauth2).await?;
        account.imap.oauth2 = refreshed.clone();
        account.smtp.oauth2 = refreshed;
    } else {
        if imap_needs {
            account.imap.oauth2 = refresh_tokens(&account.imap.oauth2).await?;
        }
        if smtp_needs {
            account.smtp.oauth2 = refresh_tokens(&account.smtp.oauth2).await?;
        }
    }

    state.mailbox.save_account(account, None)?;
    Ok(())
}

fn needs_refresh(tokens: &OAuth2Tokens) -> bool {
    !tokens.refresh_token.is_empty() && tokens.is_expired()
}

async fn refresh_tokens(tokens: &OAuth2Tokens) -> Result<OAuth2Tokens> {
    let provider = tokens.provider;
    let client_id = tokens.client_id.clone();
    let refresh_token = tokens.refresh_token.clone();
    tokio::task::spawn_blocking(move || crate::oauth::refresh(provider, &client_id, &refresh_token))
        .await
        .map_err(|error| Error::OAuth(format!("token refresh task failed: {error}")))?
}

/// Wait before reconnecting, but keep accepting commands. Returns `false` when
/// the worker should stop.
async fn wait_with_backoff(
    commands: &mut mpsc::UnboundedReceiver<WorkerCommand>,
    pending: &mut VecDeque<WorkerCommand>,
    duration: Duration,
) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            command = commands.recv() => match command {
                Some(WorkerCommand::Stop) | None => return false,
                Some(command) => pending.push_back(command),
            },
        }
    }
}

/// Run one IMAP connection until it fails or a stop is requested. `Ok(())`
/// means the worker was asked to stop.
async fn run_session(
    state: &Arc<EngineState>,
    account: &Account,
    auth_creds: Either<SecretString, SecretString>,
    commands: &mut mpsc::UnboundedReceiver<WorkerCommand>,
    pending: &mut VecDeque<WorkerCommand>,
) -> Result<()> {
    let tls = crate::tls::client_config()?;
    let password_or_token = match &auth_creds {
        Either::Left(p) => p.expose_secret(),
        Either::Right(t) => t.expose_secret(),
    };
    let mut session = ImapSession::connect(&account.imap, password_or_token, tls).await?;
    set_sync_status(state, account.id, SyncStatus::Syncing);
    let remote = session.list_folders().await?;
    sync::refresh_folders(&state.mailbox, account.id, &remote)?;

    let poll = Duration::from_secs(account.sync.poll_interval_secs.clamp(15, 1800));
    let idle_capable = session.supports_idle();

    if let Some(inbox) = inbox_folder(state, account) {
        let mode = initial_mode(&inbox);
        sync_folder_logged(&mut session, state, inbox.id, mode).await;
    }
    set_sync_status(state, account.id, SyncStatus::Connected);
    set_last_sync_at(state, account.id, crate::now_unix());

    let mut last_full = Instant::now();

    loop {
        // Pending commands were queued while the worker was reconnecting.
        if let Some(command) = pending.pop_front() {
            let action = worker_action_name(&command);
            if handle_command(&mut session, state, account, command)
                .await
                .map_err(|error| action_failed(action, error))?
            {
                session.logout().await;
                return Ok(());
            }
            continue;
        }
        match commands.try_recv() {
            Ok(WorkerCommand::Stop) => {
                session.logout().await;
                return Ok(());
            }
            Ok(command) => {
                let action = worker_action_name(&command);
                if handle_command(&mut session, state, account, command)
                    .await
                    .map_err(|error| action_failed(action, error))?
                {
                    session.logout().await;
                    return Ok(());
                }
                continue;
            }
            Err(mpsc::error::TryRecvError::Empty) => {}
            Err(mpsc::error::TryRecvError::Disconnected) => {
                session.logout().await;
                return Ok(());
            }
        }

        let command;

        if idle_capable {
            if let Some(inbox) = inbox_folder(state, account) {
                session.select(&inbox.name).await?;
                let (restored, received, new_data) = idle_wait(session, poll, commands).await?;
                session = restored;
                command = received;
                // A wake-up means new mail; a timeout is our polling tick.
                if new_data || command.is_none() {
                    sync_folder_logged(&mut session, state, inbox.id, SyncMode::Incremental).await;
                }
            } else {
                command = wait_for_command(poll, commands).await;
            }
        } else {
            command = wait_for_command(poll, commands).await;
            // Polling ticks also refresh the inbox, but a stop request should
            // not wait for a network round trip.
            let inbox = if matches!(command, Some(WorkerCommand::Stop)) {
                None
            } else {
                inbox_folder(state, account)
            };
            if let Some(inbox) = inbox {
                sync_folder_logged(&mut session, state, inbox.id, SyncMode::Incremental).await;
            }
        }

        if let Some(command) = command {
            let action = worker_action_name(&command);
            if handle_command(&mut session, state, account, command)
                .await
                .map_err(|error| action_failed(action, error))?
            {
                session.logout().await;
                return Ok(());
            }
        }

        if last_full.elapsed() >= FULL_SYNC_INTERVAL {
            if let Some(inbox) = inbox_folder(state, account) {
                sync_folder_logged(&mut session, state, inbox.id, SyncMode::Full).await;
            }
            last_full = Instant::now();
        }
    }
}

async fn wait_for_command(
    wait: Duration,
    commands: &mut mpsc::UnboundedReceiver<WorkerCommand>,
) -> Option<WorkerCommand> {
    match tokio::time::timeout(wait, commands.recv()).await {
        Ok(Some(command)) => Some(command),
        Ok(None) => Some(WorkerCommand::Stop),
        Err(_) => None,
    }
}

/// Enter IDLE and wait for server activity, a timeout or a queued command.
///
/// Returns the restored session, the command that interrupted the wait (if
/// any) and whether the server announced new data.
async fn idle_wait(
    session: ImapSession,
    wait: Duration,
    commands: &mut mpsc::UnboundedReceiver<WorkerCommand>,
) -> Result<(ImapSession, Option<WorkerCommand>, bool)> {
    let condstore = session.supports_condstore();
    let idle = session.supports_idle();
    let supports_move = session.supports_move();
    let supports_uidplus = session.supports_uidplus();
    let mut handle = session.into_session().idle();
    handle
        .init()
        .await
        .map_err(|e| Error::Imap(format!("IDLE failed: {e}")))?;

    let (future, interrupt) = handle.wait_with_timeout(wait);
    let mut future = Box::pin(future);
    let mut command = None;
    let mut new_data = false;

    tokio::select! {
        result = &mut future => {
            match result.map_err(|e| Error::Imap(format!("IDLE failed: {e}")))? {
                IdleResponse::NewData(_) => new_data = true,
                IdleResponse::Timeout | IdleResponse::ManualInterrupt => {}
            }
        }
        received = commands.recv() => {
            command = Some(received.unwrap_or(WorkerCommand::Stop));
            // Dropping the stop source interrupts the idle stream.
            drop(interrupt);
            let _ = (&mut future).await;
        }
    }

    drop(future);
    let inner = handle
        .done()
        .await
        .map_err(|e| Error::Imap(format!("IDLE failed: {e}")))?;
    Ok((
        ImapSession::from_session(inner, condstore, idle, supports_move, supports_uidplus),
        command,
        new_data,
    ))
}

/// Handle one command. Returns `true` when the worker should stop.
async fn handle_command(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    account: &Account,
    command: WorkerCommand,
) -> Result<bool> {
    match command {
        WorkerCommand::Stop => return Ok(true),
        WorkerCommand::SyncAccount => {
            let remote = session.list_folders().await?;
            let folders = sync::refresh_folders(&state.mailbox, account.id, &remote)?;
            for folder in folders
                .into_iter()
                .filter(|folder| folder_allowed(account, folder))
            {
                sync_folder_logged(session, state, folder.id, SyncMode::Full).await;
            }
        }
        WorkerCommand::SyncFolder(folder_id) => {
            if let Some(folder) = state.mailbox.folder(folder_id)? {
                let mode = initial_mode(&folder);
                sync_folder_logged(session, state, folder_id, mode).await;
            }
        }
        WorkerCommand::SyncCalendars => {
            sync_calendars(session, state, account).await?;
        }
        WorkerCommand::FetchBody(message_id) => {
            fetch_body(session, state, message_id, false).await?;
        }
        WorkerCommand::RefreshBody(message_id) => {
            fetch_body(session, state, message_id, true).await?;
        }
        WorkerCommand::SetFlags {
            message_id,
            add,
            remove,
        } => {
            set_flags(session, state, message_id, &add, &remove).await?;
        }
        WorkerCommand::BulkSetFlags {
            message_ids,
            add,
            remove,
        } => {
            bulk_set_flags(session, state, &message_ids, &add, &remove).await?;
        }
        WorkerCommand::MoveMessages {
            message_ids,
            target,
        } => {
            move_messages(session, state, account, &message_ids, target).await?;
        }
        WorkerCommand::EmptyTrash => {
            empty_trash(session, state, account).await?;
        }
        WorkerCommand::SaveDraft { draft } => {
            save_draft(session, state, account, *draft).await?;
        }
        WorkerCommand::DiscardDraft { draft_key } => {
            discard_draft(session, state, account, &draft_key).await?;
        }
        WorkerCommand::RetryOutbox { entry_id } => {
            state.mailbox.retry_outbox(entry_id)?;
            // The entry is queued again: wake the sender so it does not sit
            // out the rest of its 60-second wait.
            state.outbox.notify_one();
        }
        WorkerCommand::CancelOutbox { entry_id } => {
            state.mailbox.cancel_outbox(entry_id)?;
        }
        WorkerCommand::SnoozeMessages {
            message_ids,
            due_at,
        } => {
            snooze_messages(session, state, account, &message_ids, due_at).await?;
        }
        WorkerCommand::UnsnoozeMessages { message_ids } => {
            unsnooze_messages(session, state, account, &message_ids).await?;
        }
        WorkerCommand::ApplyRules {
            folder_id,
            message_ids,
        } => {
            apply_rules(session, state, account, folder_id, &message_ids).await?;
        }
        WorkerCommand::CreateFolder {
            account_id,
            name,
            delimiter,
            role,
        } => {
            create_folder_imap(session, state, account_id, &name, &delimiter, role).await?;
        }
        WorkerCommand::RenameFolder {
            folder_id,
            new_name,
        } => {
            rename_folder_imap(session, state, folder_id, &new_name).await?;
        }
        WorkerCommand::DeleteFolder(folder_id) => {
            delete_folder_imap(session, state, folder_id).await?;
        }
        WorkerCommand::SubscribeFolder {
            folder_id,
            subscribe,
        } => {
            subscribe_folder_imap(session, state, folder_id, subscribe).await?;
        }
        WorkerCommand::ExecuteScheduledAction { action } => {
            // The worker path must record the terminal state itself; otherwise
            // `due_actions()` keeps returning the action and it is re-dispatched
            // every 60 seconds forever.
            match execute_scheduled_action_with_session(session, state, account, &action).await {
                Ok(()) => {
                    let _ = state
                        .mailbox
                        .store()
                        .set_action_state(action.id, "done", None);
                }
                Err(e) => {
                    tracing::warn!(action_id = action.id, %e, "scheduled action failed");
                    let _ = state.mailbox.store().set_action_state(
                        action.id,
                        "failed",
                        Some(&e.to_string()),
                    );
                }
            }
        }
    }
    Ok(false)
}

async fn fetch_body(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    message_id: MessageId,
    force: bool,
) -> Result<()> {
    let Some(message) = state.mailbox.message(message_id)? else {
        return Ok(());
    };
    if message.body_fetched && !force {
        return Ok(());
    }
    let Some(folder) = state.mailbox.folder(message.folder_id)? else {
        return Ok(());
    };
    session.select(&folder.name).await?;
    let Some(raw) = session.fetch_body(message.uid).await? else {
        return Ok(());
    };

    let (header, body) = crate::imap::parse::parse_message(&raw);
    let store = state.mailbox.store();
    if store.set_body(
        message_id,
        body.text.as_deref(),
        body.html.as_deref(),
        body.snippet.as_deref(),
    )? {
        store.replace_attachments(message_id, &body.attachments)?;
        if let Some(pgp) = &body.pgp {
            store.upsert_pgp_mime(message_id, pgp.kind, &pgp.payload, pgp.signature.as_deref())?;
        }
        state.mailbox.publish(Event::BodyReady { message_id });
        state.mailbox.publish(Event::MessagesChanged {
            folder_id: folder.id,
        });
        // Apply rules that depend on body content. Route through the engine's
        // rule path: `Mailin::apply_rules`/`Store::apply_rules` is a no-op, so
        // calling it here silently drops every body-dependent rule.
        if let Some(account) = state.mailbox.account(&folder.account_id)? {
            apply_rules(session, state, &account, folder.id, &[message_id]).await?;
        }
    }
    // Seed contacts from full message headers
    let account_id = folder.account_id;
    if let Some(addr) = &header.from_addr {
        let name = header.from_name.as_deref().unwrap_or("");
        let _ = store.upsert_contact(&account_id, addr, name, false);
    }
    for addr in crate::sync::parse_address_list(&header.to_addrs) {
        let _ = store.upsert_contact(&account_id, &addr, "", false);
    }
    for addr in crate::sync::parse_address_list(&header.cc_addrs) {
        let _ = store.upsert_contact(&account_id, &addr, "", false);
    }
    Ok(())
}

async fn set_flags(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    message_id: MessageId,
    add: &[String],
    remove: &[String],
) -> Result<()> {
    let Some(message) = state.mailbox.message(message_id)? else {
        return Ok(());
    };
    let Some(folder) = state.mailbox.folder(message.folder_id)? else {
        return Ok(());
    };
    session.select(&folder.name).await?;
    session.store_flags(message.uid, add, remove).await?;

    let mut flags = message.flags.clone();
    for flag in add {
        if !flags
            .iter()
            .any(|current| current.eq_ignore_ascii_case(flag))
        {
            flags.push(flag.clone());
        }
    }
    flags.retain(|current| {
        !remove
            .iter()
            .any(|removed| current.eq_ignore_ascii_case(removed))
    });
    state
        .mailbox
        .store()
        .set_flags(folder.id, message.uid, &flags)?;
    state.mailbox.publish(Event::MessagesChanged {
        folder_id: folder.id,
    });
    Ok(())
}

/// Apply flag changes to many messages at once. Messages are grouped by
/// source folder so a bulk action costs one `SELECT` and at most two
/// `UID STORE` commands per folder instead of a round-trip per message.
async fn bulk_set_flags(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    message_ids: &[MessageId],
    add: &[String],
    remove: &[String],
) -> Result<()> {
    if message_ids.is_empty() || (add.is_empty() && remove.is_empty()) {
        return Ok(());
    }

    let mut groups: HashMap<FolderId, Vec<_>> = HashMap::new();
    for message_id in message_ids {
        let Some(message) = state.mailbox.message(*message_id)? else {
            continue;
        };
        groups.entry(message.folder_id).or_default().push(message);
    }

    for (folder_id, messages) in groups {
        let Some(folder) = state.mailbox.folder(folder_id)? else {
            continue;
        };
        let uids: Vec<u32> = messages.iter().map(|message| message.uid).collect();
        session.select(&folder.name).await?;
        session.store_flags_many(&uids, add, remove).await?;

        let updates: Vec<(u32, Vec<String>)> = messages
            .iter()
            .map(|message| {
                let mut flags = message.flags.clone();
                for flag in add {
                    if !flags
                        .iter()
                        .any(|current| current.eq_ignore_ascii_case(flag))
                    {
                        flags.push(flag.clone());
                    }
                }
                flags.retain(|current| {
                    !remove
                        .iter()
                        .any(|removed| current.eq_ignore_ascii_case(removed))
                });
                (message.uid, flags)
            })
            .collect();
        state.mailbox.store().set_flags_many(folder_id, &updates)?;
        state.mailbox.publish(Event::MessagesChanged { folder_id });
    }
    Ok(())
}

async fn move_messages(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    account: &Account,
    message_ids: &[MessageId],
    target: MoveTarget,
) -> Result<()> {
    let folders = state.mailbox.list_folders(&account.id)?;

    // Group by source folder and destination so a multi-select move uses one
    // UID command per folder instead of one connection round-trip per message.
    let mut groups: MoveGroups = HashMap::new();
    for message_id in message_ids {
        let Some(message) = state.mailbox.message(*message_id)? else {
            continue;
        };
        let Some(source) = folders.iter().find(|folder| folder.id == message.folder_id) else {
            continue;
        };

        let (destination, permanent_in_trash) = match target {
            MoveTarget::Folder(folder_id) => {
                (folders.iter().find(|folder| folder.id == folder_id), false)
            }
            // Deleting a message that already sits in Trash is permanent. This
            // is an intentional `None` destination, not a missing Trash folder.
            MoveTarget::Trash if source.role == FolderRole::Trash => (None, true),
            MoveTarget::Trash => (find_role(&folders, FolderRole::Trash), false),
            MoveTarget::Archive => (find_role(&folders, FolderRole::Archive), false),
            MoveTarget::Junk => (find_role(&folders, FolderRole::Junk), false),
            MoveTarget::Inbox => (find_role(&folders, FolderRole::Inbox), false),
            MoveTarget::Permanent => (None, true),
        };

        if matches!(target, MoveTarget::Folder(_)) && destination.is_none() {
            return Err(Error::FolderNotFound(match target {
                MoveTarget::Folder(folder_id) => folder_id.0,
                _ => 0,
            }));
        }
        if matches!(
            target,
            MoveTarget::Trash | MoveTarget::Archive | MoveTarget::Junk | MoveTarget::Inbox
        ) && destination.is_none()
            && !permanent_in_trash
        {
            return Err(Error::Config(format!(
                "the account has no {} folder",
                match target {
                    MoveTarget::Trash => "Trash",
                    MoveTarget::Archive => "Archive",
                    MoveTarget::Junk => "Junk",
                    _ => "Inbox",
                }
            )));
        }

        let destination = destination.map(|folder| folder.id);
        if destination == Some(source.id) {
            continue;
        }
        groups
            .entry((source.id, destination))
            .or_default()
            .push((*message_id, message.uid));
    }

    let mut destinations = Vec::new();
    for ((source_id, destination), items) in groups {
        let Some(source) = folders.iter().find(|folder| folder.id == source_id) else {
            continue;
        };
        let uids: Vec<u32> = items.iter().map(|(_, uid)| *uid).collect();
        session.select(&source.name).await?;

        if let Some(destination) = destination {
            let Some(target_folder) = folders.iter().find(|folder| folder.id == destination) else {
                return Err(Error::FolderNotFound(destination.0));
            };
            session.move_messages(&uids, &target_folder.name).await?;
            destinations.push(destination);
        } else {
            session.delete_messages(&uids).await?;
        }

        for (message_id, _) in &items {
            let _ = state.mailbox.store().delete_message_by_id(*message_id)?;
        }
        state.mailbox.publish(Event::MessagesChanged {
            folder_id: source_id,
        });
    }

    // Pull the moved messages into the destination cache immediately rather
    // than waiting for the next background poll.
    destinations.sort();
    destinations.dedup();
    for folder_id in destinations {
        sync_folder_logged(session, state, folder_id, SyncMode::Full).await;
    }
    Ok(())
}

async fn snooze_messages(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    account: &Account,
    message_ids: &[MessageId],
    due_at: i64,
) -> Result<()> {
    let folders = state.mailbox.list_folders(&account.id)?;
    let Some(snoozed) = find_role(&folders, FolderRole::Snoozed) else {
        return Err(Error::Config(
            "the account has no Snoozed folder; create one on the server first".into(),
        ));
    };

    // Group by source folder
    let mut groups: SnoozeGroups = HashMap::new();
    for message_id in message_ids {
        let Some(message) = state.mailbox.message(*message_id)? else {
            continue;
        };
        groups.entry(message.folder_id).or_default().push((
            *message_id,
            message.uid,
            message.folder_id,
            message.message_id.clone(),
        ));
    }

    for (source_id, items) in groups {
        let Some(source) = folders.iter().find(|f| f.id == source_id) else {
            continue;
        };
        let uids: Vec<u32> = items.iter().map(|(_, uid, _, _)| *uid).collect();
        session.select(&source.name).await?;
        session.move_messages(&uids, &snoozed.name).await?;

        for (message_id, _, return_to_folder_id, rfc_message_id) in items {
            // Remove from source cache
            let _ = state.mailbox.store().delete_message_by_id(message_id)?;
            // Schedule unsnooze
            let payload = crate::model::SnoozePayload {
                message_id: message_id.0,
                return_to_folder_id: return_to_folder_id.0,
                snoozed_folder_id: snoozed.id.0,
                rfc_message_id,
            };
            let payload_json = serde_json::to_string(&payload)?;
            state.mailbox.store().schedule_action(
                &account.id,
                due_at,
                "snooze_message",
                &payload_json,
            )?;
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: source_id,
            });
        }
    }

    // Sync the Snoozed folder to pull in the moved messages
    sync_folder_logged(session, state, snoozed.id, SyncMode::Full).await;
    Ok(())
}

async fn unsnooze_messages(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    account: &Account,
    message_ids: &[MessageId],
) -> Result<()> {
    let folders = state.mailbox.list_folders(&account.id)?;
    let Some(snoozed) = find_role(&folders, FolderRole::Snoozed) else {
        return Ok(()); // No snoozed folder, nothing to do
    };

    let store = state.mailbox.store();

    for message_id in message_ids {
        let Some(message) = state.mailbox.message(*message_id)? else {
            continue;
        };
        if message.folder_id != snoozed.id {
            continue;
        };

        // Find the scheduled action to get return_to_folder_id. Match by the
        // RFC 5322 `Message-ID` first: it survives the move into the Snoozed
        // folder and the re-sync that re-inserts the row under a new local
        // row id/UID. The recorded `message_id` (a local row id) only matches
        // when the folder was never re-synced, so use it as a fallback.
        let action = match message.message_id.as_deref() {
            Some(rfc_id) => store.find_scheduled_action_by_payload(
                "snooze_message",
                "rfc_message_id",
                rfc_id,
            )?,
            None => None,
        };
        let action = match action {
            Some(action) => Some(action),
            None => store.find_scheduled_action_by_payload(
                "snooze_message",
                "message_id",
                &message_id.0.to_string(),
            )?,
        };

        let return_folder_id = action
            .as_ref()
            .and_then(|a| {
                serde_json::from_str::<crate::model::SnoozePayload>(&a.payload_json)
                    .ok()
                    .map(|p| FolderId(p.return_to_folder_id))
            })
            .or_else(|| find_role(&folders, FolderRole::Inbox).map(|f| f.id));

        if let Some(return_folder_id) = return_folder_id {
            if let Some(return_folder) = folders.iter().find(|f| f.id == return_folder_id) {
                let uids = vec![message.uid];
                session.select(&snoozed.name).await?;
                session.move_messages(&uids, &return_folder.name).await?;
                let _ = store.delete_message_by_id(*message_id)?;
                if let Some(a) = action {
                    store.set_action_state(a.id, "done", None)?;
                }
                state.mailbox.publish(Event::MessagesChanged {
                    folder_id: snoozed.id,
                });
                state.mailbox.publish(Event::MessagesChanged {
                    folder_id: return_folder_id,
                });
            }
        }
    }

    Ok(())
}

async fn apply_rules(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    account: &Account,
    _folder_id: FolderId,
    message_ids: &[MessageId],
) -> Result<()> {
    use crate::rules::{RegexCache, eval_rule, parse_actions};

    let store = state.mailbox.store();
    let rules = store.list_rules(&account.id)?;
    if rules.is_empty() {
        return Ok(());
    }

    let mut regex_cache = RegexCache::default();

    for message_id in message_ids {
        let Some(message) = state.mailbox.message(*message_id)? else {
            continue;
        };

        // Get body if available for body conditions
        let body_text = if message.body_fetched {
            store.body(message.id)?.and_then(|(text, _html)| text)
        } else {
            None
        };

        for rule in &rules {
            if eval_rule(rule, &message, body_text.as_deref(), &mut regex_cache) {
                let actions = parse_actions(rule);
                for action in actions {
                    execute_rule_action(session, state, account, &action, message_id).await?;
                }
                if rule.stop {
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn execute_rule_action(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    account: &Account,
    action: &crate::model::RuleAction,
    message_id: &MessageId,
) -> Result<()> {
    use crate::model::FolderRole;

    let folders = state.mailbox.list_folders(&account.id)?;
    let Some(message) = state.mailbox.message(*message_id)? else {
        return Ok(());
    };

    match action.kind.as_str() {
        "move" => {
            // Value is the target folder role or folder ID
            let target_folder = if let Ok(folder_id) = action.value.parse::<i64>() {
                folders.iter().find(|f| f.id.0 == folder_id)
            } else {
                find_role(&folders, action.value.parse().unwrap_or(FolderRole::Other))
            };

            if let Some(target) = target_folder {
                let uids = vec![message.uid];
                let Some(source) = folders.iter().find(|f| f.id == message.folder_id) else {
                    return Ok(());
                };
                session.select(&source.name).await?;
                session.move_messages(&uids, &target.name).await?;
                let _ = state.mailbox.store().delete_message_by_id(*message_id)?;
                state.mailbox.publish(Event::MessagesChanged {
                    folder_id: source.id,
                });
                state.mailbox.publish(Event::MessagesChanged {
                    folder_id: target.id,
                });
            }
        }
        "copy" => {
            // Similar to move but without deleting from source
            let target_folder = if let Ok(folder_id) = action.value.parse::<i64>() {
                folders.iter().find(|f| f.id.0 == folder_id)
            } else {
                find_role(&folders, action.value.parse().unwrap_or(FolderRole::Other))
            };

            if let Some(target) = target_folder {
                let uids = vec![message.uid];
                let Some(source) = folders.iter().find(|f| f.id == message.folder_id) else {
                    return Ok(());
                };
                session.select(&source.name).await?;
                session.copy_messages(&uids, &target.name).await?;
                // Note: we don't delete from source for copy
                state.mailbox.publish(Event::MessagesChanged {
                    folder_id: target.id,
                });
            }
        }
        "flag" => {
            // Value is the flag to add
            let flags = vec![action.value.clone()];
            let Some(source) = folders.iter().find(|f| f.id == message.folder_id) else {
                return Ok(());
            };
            session.select(&source.name).await?;
            session.store_flags(message.uid, &flags, &[]).await?;
            let mut msg_flags = message.flags.clone();
            if !msg_flags
                .iter()
                .any(|f| f.eq_ignore_ascii_case(&action.value))
            {
                msg_flags.push(action.value.clone());
            }
            state
                .mailbox
                .store()
                .set_flags(source.id, message.uid, &msg_flags)?;
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: source.id,
            });
        }
        "unflag" => {
            // Value is the flag to remove
            let flags = vec![action.value.clone()];
            let Some(source) = folders.iter().find(|f| f.id == message.folder_id) else {
                return Ok(());
            };
            session.select(&source.name).await?;
            session.store_flags(message.uid, &[], &flags).await?;
            let mut msg_flags = message.flags.clone();
            msg_flags.retain(|f| !f.eq_ignore_ascii_case(&action.value));
            state
                .mailbox
                .store()
                .set_flags(source.id, message.uid, &msg_flags)?;
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: source.id,
            });
        }
        "mark_read" => {
            let flags = vec!["\\Seen".to_string()];
            let Some(source) = folders.iter().find(|f| f.id == message.folder_id) else {
                return Ok(());
            };
            session.select(&source.name).await?;
            session.store_flags(message.uid, &flags, &[]).await?;
            let mut msg_flags = message.flags.clone();
            if !msg_flags.iter().any(|f| f.eq_ignore_ascii_case("\\Seen")) {
                msg_flags.push("\\Seen".to_string());
            }
            state
                .mailbox
                .store()
                .set_flags(source.id, message.uid, &msg_flags)?;
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: source.id,
            });
        }
        "mark_unread" => {
            let flags = vec!["\\Seen".to_string()];
            let Some(source) = folders.iter().find(|f| f.id == message.folder_id) else {
                return Ok(());
            };
            session.select(&source.name).await?;
            session.store_flags(message.uid, &[], &flags).await?;
            let mut msg_flags = message.flags.clone();
            msg_flags.retain(|f| !f.eq_ignore_ascii_case("\\Seen"));
            state
                .mailbox
                .store()
                .set_flags(source.id, message.uid, &msg_flags)?;
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: source.id,
            });
        }
        "trash" => {
            let Some(trash) = find_role(&folders, FolderRole::Trash) else {
                return Err(Error::Config("no Trash folder".into()));
            };
            let Some(source) = folders.iter().find(|f| f.id == message.folder_id) else {
                return Ok(());
            };
            if source.id == trash.id {
                return Ok(()); // Already in trash
            }
            let uids = vec![message.uid];
            session.select(&source.name).await?;
            session.move_messages(&uids, &trash.name).await?;
            let _ = state.mailbox.store().delete_message_by_id(*message_id)?;
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: source.id,
            });
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: trash.id,
            });
        }
        "archive" => {
            let Some(archive) = find_role(&folders, FolderRole::Archive) else {
                return Err(Error::Config("no Archive folder".into()));
            };
            let Some(source) = folders.iter().find(|f| f.id == message.folder_id) else {
                return Ok(());
            };
            let uids = vec![message.uid];
            session.select(&source.name).await?;
            session.move_messages(&uids, &archive.name).await?;
            let _ = state.mailbox.store().delete_message_by_id(*message_id)?;
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: source.id,
            });
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: archive.id,
            });
        }
        "junk" => {
            let Some(junk) = find_role(&folders, FolderRole::Junk) else {
                return Err(Error::Config("no Junk folder".into()));
            };
            let Some(source) = folders.iter().find(|f| f.id == message.folder_id) else {
                return Ok(());
            };
            let uids = vec![message.uid];
            session.select(&source.name).await?;
            session.move_messages(&uids, &junk.name).await?;
            let _ = state.mailbox.store().delete_message_by_id(*message_id)?;
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: source.id,
            });
            state
                .mailbox
                .publish(Event::MessagesChanged { folder_id: junk.id });
        }
        "snooze" => {
            // Value is due_at timestamp
            if let Ok(due_at) = action.value.parse::<i64>() {
                let Some(snoozed) = find_role(&folders, FolderRole::Snoozed) else {
                    return Err(Error::Config("no Snoozed folder".into()));
                };
                state.mailbox.store().schedule_action(
                    &account.id,
                    due_at,
                    "snooze_message",
                    &serde_json::to_string(&crate::model::SnoozePayload {
                        message_id: message_id.0,
                        return_to_folder_id: message.folder_id.0,
                        snoozed_folder_id: snoozed.id.0,
                        rfc_message_id: message.message_id.clone(),
                    })?,
                )?;
                let Some(source) = folders.iter().find(|f| f.id == message.folder_id) else {
                    return Ok(());
                };
                let uids = vec![message.uid];
                session.select(&source.name).await?;
                session.move_messages(&uids, &snoozed.name).await?;
                let _ = state.mailbox.store().delete_message_by_id(*message_id)?;
                state.mailbox.publish(Event::MessagesChanged {
                    folder_id: source.id,
                });
                state.mailbox.publish(Event::MessagesChanged {
                    folder_id: snoozed.id,
                });
            }
        }
        "stop" => {
            // Stop processing further rules
            return Ok(());
        }
        _ => {}
    }
    Ok(())
}

async fn empty_trash(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    account: &Account,
) -> Result<()> {
    let folders = state.mailbox.list_folders(&account.id)?;
    let Some(trash) = find_role(&folders, FolderRole::Trash) else {
        return Err(Error::Config("the account has no Trash folder".into()));
    };
    session.select(&trash.name).await?;
    let mut uids: Vec<u32> = session.search("ALL").await?.into_iter().collect();
    uids.sort_unstable();
    if !uids.is_empty() {
        session.delete_messages(&uids).await?;
    }
    state.mailbox.store().clear_folder_messages(trash.id)?;
    state.mailbox.publish(Event::MessagesChanged {
        folder_id: trash.id,
    });
    Ok(())
}

fn find_role(folders: &[Folder], role: FolderRole) -> Option<&Folder> {
    folders.iter().find(|folder| folder.role == role)
}

async fn save_draft(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    account: &Account,
    mut draft: OutgoingMessage,
) -> Result<()> {
    let folders = state.mailbox.list_folders(&account.id)?;
    let Some(drafts) = find_role(&folders, FolderRole::Drafts).cloned() else {
        return Err(Error::Config(
            "the account has no Drafts folder; create one on the server first".into(),
        ));
    };
    if draft.draft_key.is_none() {
        draft.draft_key = Some(crate::compose::new_draft_key(&draft.from_addr));
    }
    let raw = draft.build_draft()?;

    session.select(&drafts.name).await?;
    if let Some(key) = draft.draft_key.clone() {
        discard_draft_key(session, state, &drafts, &key).await?;
    }
    session
        .append(&drafts.name, "(\\Draft \\Seen)", &raw)
        .await?;
    sync_folder_logged(session, state, drafts.id, SyncMode::Full).await;
    Ok(())
}

async fn discard_draft(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    account: &Account,
    draft_key: &str,
) -> Result<()> {
    let folders = state.mailbox.list_folders(&account.id)?;
    let Some(drafts) = find_role(&folders, FolderRole::Drafts).cloned() else {
        return Ok(());
    };
    session.select(&drafts.name).await?;
    discard_draft_key(session, state, &drafts, draft_key).await
}

async fn discard_draft_key(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    drafts: &Folder,
    draft_key: &str,
) -> Result<()> {
    let key = draft_key
        .trim()
        .trim_matches(['<', '>'])
        .replace(['"', '\r', '\n'], "");
    if key.is_empty() {
        return Ok(());
    }
    let query = format!("HEADER Message-ID \"<{key}>\"");
    let mut uids: Vec<u32> = session.search(&query).await?.into_iter().collect();
    uids.sort_unstable();
    if uids.is_empty() {
        return Ok(());
    }
    session.delete_messages(&uids).await?;
    for uid in uids {
        let _ = state.mailbox.store().delete_message(drafts.id, uid)?;
    }
    state.mailbox.publish(Event::MessagesChanged {
        folder_id: drafts.id,
    });
    Ok(())
}

/// Create a new folder via IMAP and locally.
async fn create_folder_imap(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    account_id: AccountId,
    name: &str,
    _delimiter: &str,
    _role: FolderRole,
) -> Result<()> {
    session.create_folder(name).await?;
    let remote = session.list_folders().await?;
    sync::refresh_folders(&state.mailbox, account_id, &remote)?;
    Ok(())
}

/// Rename a folder via IMAP and locally.
async fn rename_folder_imap(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    folder_id: FolderId,
    new_name: &str,
) -> Result<()> {
    let Some(folder) = state.mailbox.folder(folder_id)? else {
        return Ok(());
    };
    session.rename_folder(&folder.name, new_name).await?;
    let remote = session.list_folders().await?;
    sync::refresh_folders(&state.mailbox, folder.account_id, &remote)?;
    Ok(())
}

/// Delete a folder via IMAP and locally.
async fn delete_folder_imap(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    folder_id: FolderId,
) -> Result<()> {
    let Some(folder) = state.mailbox.folder(folder_id)? else {
        return Ok(());
    };
    session.delete_folder(&folder.name).await?;
    let _ = state.mailbox.store().delete_folder(folder_id)?;
    Ok(())
}

/// Subscribe or unsubscribe from a folder via IMAP.
async fn subscribe_folder_imap(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    folder_id: FolderId,
    subscribe: bool,
) -> Result<()> {
    let Some(folder) = state.mailbox.folder(folder_id)? else {
        return Ok(());
    };
    if subscribe {
        session.subscribe_folder(&folder.name).await?;
    } else {
        session.unsubscribe_folder(&folder.name).await?;
    }
    // Persist the user's intent (M3a-4). `upsert_folder` preserves subscribed
    // on conflict, so refresh_folders will not clobber this.
    state
        .mailbox
        .store()
        .set_folder_subscribed(folder_id, subscribe)?;
    let remote = session.list_folders().await?;
    sync::refresh_folders(&state.mailbox, folder.account_id, &remote)?;
    Ok(())
}

async fn sync_folder_logged(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    folder_id: FolderId,
    mode: SyncMode,
) {
    let folder = match state.mailbox.folder(folder_id) {
        Ok(Some(folder)) => folder,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%folder_id, %error, "could not load the folder");
            return;
        }
    };
    if let Err(error) = sync::sync_folder(session, &state.mailbox, &folder, mode).await {
        tracing::warn!(folder = %folder.name, %error, "folder synchronization failed");
        return;
    }

    // Draft bodies are needed as soon as the user resumes one, and there are
    // usually only a handful, so fetch them eagerly.
    if folder.role == FolderRole::Drafts
        && let Ok(messages) = state.mailbox.store().list_messages(folder.id, 0, 200)
    {
        for message in messages {
            if !message.body_fetched
                && let Err(error) = fetch_body(session, state, message.id, false).await
            {
                tracing::debug!(%error, "could not fetch a draft body");
            }
        }
    }
}

fn inbox_folder(state: &EngineState, account: &Account) -> Option<Folder> {
    state
        .mailbox
        .list_folders(&account.id)
        .ok()?
        .into_iter()
        .find(|folder| folder.role == FolderRole::Inbox && folder_allowed(account, folder))
}

fn folder_allowed(account: &Account, folder: &Folder) -> bool {
    account.sync.folder_allowlist.is_empty()
        || account
            .sync
            .folder_allowlist
            .iter()
            .any(|name| name == &folder.name)
}

fn initial_mode(folder: &Folder) -> SyncMode {
    if folder.last_synced_at.is_none() {
        SyncMode::Full
    } else {
        SyncMode::Incremental
    }
}

/// Update the sync status for an account and emit the change event.
fn set_sync_status(state: &Arc<EngineState>, account_id: AccountId, status: SyncStatus) {
    let mut guard = state
        .sync_status
        .write()
        .unwrap_or_else(|err| err.into_inner());
    let changed = guard.get(&account_id).is_none_or(|old| *old != status);
    if changed {
        guard.insert(account_id, status);
        state
            .mailbox
            .publish(ClientEvent::SyncStatusChanged { account_id, status });
    }
}

/// Update the last successful sync timestamp for an account.
fn set_last_sync_at(state: &Arc<EngineState>, account_id: AccountId, timestamp: i64) {
    state
        .last_sync_at
        .write()
        .unwrap_or_else(|err| err.into_inner())
        .insert(account_id, timestamp);
}

/// Sync calendars for an account using CalDAV.
async fn sync_calendars(
    _session: &mut ImapSession,
    state: &Arc<EngineState>,
    account: &Account,
) -> Result<()> {
    use crate::caldav::{CalDavClient, parse_ical_event};

    // Check if the account has CalDAV settings
    let caldav_url = account
        .caldav_url
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let caldav_username = account
        .caldav_username
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let caldav_password = account
        .caldav_password
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());

    let (Some(url), Some(username), Some(password)) =
        (caldav_url, caldav_username, caldav_password)
    else {
        // No CalDAV configured, skip silently
        return Ok(());
    };

    let client = CalDavClient::new(url, username, password);

    // Discover calendar home
    let home_url = match client.discover_calendar_home() {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(account = %account.email, %error, "CalDAV calendar home discovery failed");
            return Ok(());
        }
    };

    // List calendars
    let calendars = match client.list_calendars(&home_url) {
        Ok(calendars) => calendars,
        Err(error) => {
            tracing::warn!(account = %account.email, %error, "CalDAV calendar listing failed");
            return Ok(());
        }
    };

    for calendar in calendars {
        // Get or create calendar in local store
        let cal_id = match state.mailbox.store().upsert_calendar(
            &account.id,
            &calendar.href,
            &calendar.display_name,
            calendar.color.as_deref(),
            calendar.ctag.as_deref(),
        ) {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(account = %account.email, %error, "could not upsert calendar");
                continue;
            }
        };

        // Sync events
        let events = match client.sync_calendar(&calendar.href, calendar.ctag.as_deref()) {
            Ok(events) => events,
            Err(error) => {
                tracing::warn!(account = %account.email, calendar = %calendar.display_name, %error, "CalDAV calendar sync failed");
                continue;
            }
        };

        for event_info in events {
            let event = match parse_ical_event(&event_info.ical, cal_id) {
                Ok(event) => event,
                Err(error) => {
                    tracing::debug!(%error, "could not parse iCal event");
                    continue;
                }
            };

            let mut event = event;
            event.etag = Some(event_info.etag);

            if let Err(error) = state.mailbox.store().upsert_calendar_event(&event) {
                tracing::warn!(%error, "could not upsert calendar event");
            }
        }

        // Update ctag if we got one from the sync
        if let Some(ref new_ctag) = calendar.ctag {
            if let Err(error) = state.mailbox.store().update_calendar_ctag(cal_id, new_ctag) {
                tracing::debug!(%error, "could not update calendar ctag");
            }
        }
    }

    Ok(())
}

async fn outbox_sender(state: Arc<EngineState>) {
    if let Err(error) = state.mailbox.store().reset_outbox_sending() {
        tracing::warn!(%error, "could not reset interrupted outbox entries");
    }

    loop {
        match state.mailbox.next_outgoing() {
            Ok(Some(entry)) => {
                if let Err(error) = deliver(&state, &entry).await {
                    tracing::warn!(outbox = entry.id, %error, "sending failed");
                    let _ = state.mailbox.store().set_outbox_state(
                        entry.id,
                        OutboxState::Failed,
                        Some(&error.to_string()),
                    );
                    state.mailbox.publish(Event::OutboxChanged);
                    // Back off before the next attempt, but wake up at once
                    // when the user retries an entry in the meantime.
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                        _ = state.outbox.notified() => {}
                    }
                }
            }
            Ok(None) => {
                let _ =
                    tokio::time::timeout(Duration::from_secs(60), state.outbox.notified()).await;
            }
            Err(error) => {
                tracing::warn!(%error, "could not read the outbox");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

/// Background worker that executes scheduled actions (snooze, send-later, etc.)
async fn scheduled_worker(state: Arc<EngineState>) {
    if let Err(error) = state.mailbox.store().reset_running_scheduled_actions() {
        tracing::warn!(%error, "could not reset interrupted scheduled actions");
    }

    let notify = Arc::new(tokio::sync::Notify::new());
    let notify_clone = notify.clone();

    loop {
        // Process all due actions
        let actions = match state.mailbox.store().due_actions() {
            Ok(actions) => actions,
            Err(e) => {
                tracing::warn!(%e, "failed to fetch due scheduled actions");
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
        };

        for action in actions {
            // Claim the action before dispatching so a slow execution can
            // never be re-dispatched on the next 60-second tick. Exactly one
            // caller flips `pending` → `running`; everyone else skips it.
            match state.mailbox.store().claim_scheduled_action(action.id) {
                Ok(true) => {}
                Ok(false) => continue, // Already claimed or finished elsewhere.
                Err(error) => {
                    tracing::warn!(
                        action_id = action.id,
                        %error,
                        "could not claim scheduled action"
                    );
                    continue;
                }
            }

            // Try to send to account worker first
            let account_id = action.account_id;
            let action_clone = action.clone();
            if state
                .scheduled_actions_sender
                .send((account_id, action))
                .is_err()
            {
                // Worker not running, execute directly as fallback (creates new connection)
                let state = state.clone();
                tokio::spawn(async move {
                    let result = execute_scheduled_action(&state, &action_clone).await;
                    if let Err(e) = result {
                        tracing::warn!(action_id = action_clone.id, %e, "scheduled action failed (fallback)");
                        let _ = state.mailbox.store().set_action_state(
                            action_clone.id,
                            "failed",
                            Some(&e.to_string()),
                        );
                    } else {
                        let _ =
                            state
                                .mailbox
                                .store()
                                .set_action_state(action_clone.id, "done", None);
                    }
                });
            }
        }

        // Wait for next check: either 60 seconds or until notified
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            _ = notify_clone.notified() => {}
        }
    }
}

async fn execute_scheduled_action_with_session(
    session: &mut ImapSession,
    state: &Arc<EngineState>,
    _account: &Account,
    action: &ScheduledAction,
) -> Result<()> {
    match action.kind.as_str() {
        "snooze_message" => {
            let payload: SnoozePayload = serde_json::from_str(&action.payload_json)?;
            let return_folder_id = FolderId(payload.return_to_folder_id);

            // Move message back to original folder
            let folders = state.mailbox.list_folders(&action.account_id)?;
            let Some(snoozed) = find_role(&folders, FolderRole::Snoozed) else {
                return Err(Error::Config("no Snoozed folder".into()));
            };
            let Some(return_folder) = folders.iter().find(|f| f.id == return_folder_id) else {
                return Err(Error::Config("return folder not found".into()));
            };

            // The local row id in the payload points at the row that was
            // deleted when the message was moved into Snoozed. Syncing the
            // Snoozed folder re-inserts it under a *new* row id (and possibly a
            // new UID), so resolve it by its stable RFC 5322 Message-ID first,
            // falling back to the recorded row id only if the header is absent.
            let message = match payload.rfc_message_id.as_deref() {
                Some(rfc_id) => state
                    .mailbox
                    .store()
                    .message_by_rfc_id(snoozed.id, rfc_id)?,
                None => None,
            };
            let message = match message {
                Some(message) => Some(message),
                None if payload.rfc_message_id.is_none() => {
                    state.mailbox.message(MessageId(payload.message_id))?
                }
                None => None,
            };

            let Some(message) = message else {
                return Ok(()); // Already gone
            };

            if message.folder_id != snoozed.id {
                return Ok(()); // Not in snoozed folder anymore
            }

            let uids = vec![message.uid];
            session.select(&snoozed.name).await?;
            session.move_messages(&uids, &return_folder.name).await?;

            let _ = state.mailbox.store().delete_message_by_id(message.id)?;
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: snoozed.id,
            });
            state.mailbox.publish(Event::MessagesChanged {
                folder_id: return_folder_id,
            });
            Ok(())
        }
        _ => Err(Error::Config(format!(
            "unknown scheduled action kind: {}",
            action.kind
        ))),
    }
}

// Keep the old function for any direct calls (e.g., from tests), but it creates a new connection
async fn execute_scheduled_action(
    state: &Arc<EngineState>,
    action: &ScheduledAction,
) -> Result<()> {
    // This is the fallback for when there's no worker running
    // It creates a new connection (not ideal but works for manual triggers)
    let account = state
        .mailbox
        .account(&action.account_id)?
        .ok_or(Error::AccountNotFound(action.account_id))?;
    let password = state.mailbox.password(&action.account_id)?;
    let mut session = crate::imap::ImapSession::connect(
        &account.imap,
        password.expose_secret(),
        crate::tls::client_config()?,
    )
    .await?;
    execute_scheduled_action_with_session(&mut session, state, &account, action).await
}

async fn deliver(state: &Arc<EngineState>, entry: &OutboxEntry) -> Result<()> {
    let mut account = state
        .mailbox
        .account(&entry.account_id)?
        .ok_or(Error::AccountNotFound(entry.account_id))?;

    // SMTP may be used before the IMAP worker has refreshed an expiring token.
    refresh_oauth_tokens(state, &mut account).await?;

    // For OAuth2 accounts, use the access token
    let auth_creds = if account.smtp.auth_method == AuthMethod::OAuth2 {
        if account.smtp.oauth2.access_token.is_empty() {
            return Err(Error::Config(
                "OAuth2 access token not configured for SMTP".into(),
            ));
        }
        Either::Right(secrecy::SecretString::new(
            account.smtp.oauth2.access_token.clone().into(),
        ))
    } else {
        let password = state.mailbox.password(&entry.account_id)?;
        Either::Left(password)
    };

    let password_or_token = match &auth_creds {
        Either::Left(p) => p.expose_secret(),
        Either::Right(t) => t.expose_secret(),
    };

    let envelope: OutgoingEnvelope = serde_json::from_str(&entry.envelope_json)?;

    state
        .mailbox
        .store()
        .set_outbox_state(entry.id, OutboxState::Sending, None)?;
    state.mailbox.publish(Event::OutboxChanged);

    crate::smtp::send(&account.smtp, password_or_token, &entry.raw, &envelope).await?;

    // The message is on its way. From here on, a failure to bookkeep must
    // never become a re-send: the entry stays in `sending` (not `failed`), so
    // it is not picked up by `next_queued_outbox` again this session. Marking
    // it `Sent` here is only a local convenience for the outbox view; a lost
    // marker is an accounting problem, not a delivery problem.
    if let Err(error) = state
        .mailbox
        .store()
        .set_outbox_state(entry.id, OutboxState::Sent, None)
    {
        tracing::error!(
            outbox = entry.id,
            %error,
            "message was sent but the outbox entry could not be marked as sent"
        );
        state.mailbox.publish(Event::OutboxChanged);
        return Ok(());
    }
    state.mailbox.publish(Event::OutboxChanged);

    append_to_sent(state, &account, password_or_token, &entry.raw).await;
    Ok(())
}

async fn append_to_sent(
    state: &Arc<EngineState>,
    account: &Account,
    password_or_token: &str,
    raw: &[u8],
) {
    let sent = match state.mailbox.list_folders(&account.id) {
        Ok(folders) => folders
            .into_iter()
            .find(|folder| folder.role == FolderRole::Sent),
        Err(error) => {
            tracing::warn!(%error, "could not list folders for the Sent copy");
            return;
        }
    };
    let Some(sent) = sent else { return };

    let tls = match crate::tls::client_config() {
        Ok(tls) => tls,
        Err(error) => {
            tracing::warn!(%error, "could not prepare TLS for the Sent copy");
            return;
        }
    };
    match ImapSession::connect(&account.imap, password_or_token, tls).await {
        Ok(mut session) => {
            if let Err(error) = session.append(&sent.name, "(\\Seen)", raw).await {
                tracing::warn!(folder = %sent.name, %error, "could not append to Sent");
            }
            session.logout().await;
        }
        Err(error) => {
            tracing::warn!(%error, "could not connect to store a copy in Sent");
        }
    }
}
