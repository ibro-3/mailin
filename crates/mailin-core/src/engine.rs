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
use std::sync::Arc;
use std::time::Duration;

use async_imap::extensions::idle::IdleResponse;
use tokio::runtime::Runtime;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::account::{Account, AccountId, AuthMethod};
use crate::client::{Event, Mailin};
use crate::compose::OutgoingMessage;
use crate::error::{Error, Result};
use crate::imap::ImapSession;
use crate::model::{
    Folder, FolderId, FolderRole, MessageId, OutboxEntry, OutboxState, OutgoingEnvelope,
};
use crate::sync::{self, SyncMode};
use secrecy::{ExposeSecret, SecretString};

const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
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

        let state = Arc::new(EngineState {
            mailbox,
            outbox: Arc::new(Notify::new()),
        });
        let (commands, receiver) = mpsc::unbounded_channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("mailin-net")
            .build()
            .map_err(|e| Error::Config(format!("could not start the network runtime: {e}")))?;
        runtime.spawn(supervise(state.clone(), receiver));

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
        self.send(Command::RetryOutbox { account_id, entry_id });
    }

    /// Cancel (delete) an outbox entry.
    pub fn cancel_outbox(&self, account_id: AccountId, entry_id: i64) {
        self.send(Command::CancelOutbox { account_id, entry_id });
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
    Stop,
}

async fn supervise(state: Arc<EngineState>, mut commands: mpsc::UnboundedReceiver<Command>) {
    let mut workers: HashMap<AccountId, mpsc::UnboundedSender<WorkerCommand>> = HashMap::new();
    let sender = tokio::spawn(outbox_sender(state.clone()));

    // Start the workers of accounts that want background synchronization. A
    // worker connects and syncs the inbox on its own.
    if let Ok(accounts) = state.mailbox.list_accounts() {
        for account in accounts {
            if account.sync.background_sync {
                ensure_worker(&mut workers, &state, account.id);
            }
        }
    }

    while let Some(command) = commands.recv().await {
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
                message: error.to_string(),
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
                        message: error.to_string(),
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
                state.mailbox.publish(Event::AccountError {
                    account_id,
                    message: error.to_string(),
                });
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

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// Refresh the OAuth2 access token in place when it is about to expire.
///
/// A no-op for password accounts, accounts without a refresh token, and tokens
/// that are still valid. New tokens are written back to the store so they
/// survive a restart.
async fn refresh_oauth_tokens(state: &Arc<EngineState>, account: &mut Account) -> Result<()> {
    if account.imap.auth_method != AuthMethod::OAuth2 {
        return Ok(());
    }
    let tokens = &account.imap.oauth2;
    if tokens.refresh_token.is_empty() || !tokens.is_expired() {
        return Ok(());
    }

    let provider = tokens.provider;
    let client_id = tokens.client_id.clone();
    let refresh_token = tokens.refresh_token.clone();
    let refreshed = tokio::task::spawn_blocking(move || {
        crate::oauth::refresh(provider, &client_id, &refresh_token)
    })
    .await
    .map_err(|error| Error::OAuth(format!("token refresh task failed: {error}")))??;

    account.imap.oauth2 = refreshed.clone();
    account.smtp.oauth2 = refreshed;
    state.mailbox.save_account(account, None)?;
    Ok(())
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
    let remote = session.list_folders().await?;
    sync::refresh_folders(&state.mailbox, account.id, &remote)?;

    let poll = Duration::from_secs(account.sync.poll_interval_secs.clamp(15, 1800));
    let idle_capable = session.supports_idle();

    if let Some(inbox) = inbox_folder(state, account) {
        let mode = initial_mode(&inbox);
        sync_folder_logged(&mut session, state, inbox.id, mode).await;
    }

    let mut last_full = Instant::now();

    loop {
        // Pending commands were queued while the worker was reconnecting.
        if let Some(command) = pending.pop_front() {
            if handle_command(&mut session, state, account, command).await? {
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
                if handle_command(&mut session, state, account, command).await? {
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
            if handle_command(&mut session, state, account, command).await? {
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
        }
        WorkerCommand::CancelOutbox { entry_id } => {
            state.mailbox.cancel_outbox(entry_id)?;
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

    let (_, body) = crate::imap::parse::parse_message(&raw);
    let store = state.mailbox.store();
    if store.set_body(
        message_id,
        body.text.as_deref(),
        body.html.as_deref(),
        body.snippet.as_deref(),
    )? {
        store.replace_attachments(message_id, &body.attachments)?;
        state.mailbox.publish(Event::BodyReady { message_id });
        state.mailbox.publish(Event::MessagesChanged {
            folder_id: folder.id,
        });
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

        let destination = match target {
            MoveTarget::Folder(folder_id) => folders.iter().find(|folder| folder.id == folder_id),
            // Deleting a message that already sits in Trash is permanent.
            MoveTarget::Trash if source.role == FolderRole::Trash => None,
            MoveTarget::Trash => find_role(&folders, FolderRole::Trash),
            MoveTarget::Archive => find_role(&folders, FolderRole::Archive),
            MoveTarget::Junk => find_role(&folders, FolderRole::Junk),
            MoveTarget::Inbox => find_role(&folders, FolderRole::Inbox),
            MoveTarget::Permanent => None,
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
                    tokio::time::sleep(Duration::from_secs(30)).await;
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

    // Mark the message as sent before the best-effort Sent copy: a server
    // without a usable Sent folder must not cause a second SMTP submission.
    state
        .mailbox
        .store()
        .set_outbox_state(entry.id, OutboxState::Sent, None)?;
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
