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

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_imap::extensions::idle::IdleResponse;
use tokio::runtime::Runtime;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::account::{Account, AccountId};
use crate::client::{Event, Mailin};
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
    Shutdown,
}

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
        let account = match state.mailbox.account(&account_id) {
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

        let started = Instant::now();
        match run_session(&state, &account, &password, &mut commands, &mut pending).await {
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
    password: &SecretString,
    commands: &mut mpsc::UnboundedReceiver<WorkerCommand>,
    pending: &mut VecDeque<WorkerCommand>,
) -> Result<()> {
    let tls = crate::tls::client_config()?;
    let mut session = ImapSession::connect(&account.imap, password.expose_secret(), tls).await?;
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
        ImapSession::from_session(inner, condstore, idle),
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
    let account = state
        .mailbox
        .account(&entry.account_id)?
        .ok_or(Error::AccountNotFound(entry.account_id))?;
    let password = state.mailbox.password(&entry.account_id)?;
    let envelope: OutgoingEnvelope = serde_json::from_str(&entry.envelope_json)?;

    state
        .mailbox
        .store()
        .set_outbox_state(entry.id, OutboxState::Sending, None)?;
    state.mailbox.publish(Event::OutboxChanged);

    crate::smtp::send(
        &account.smtp,
        password.expose_secret(),
        &entry.raw,
        &envelope,
    )
    .await?;

    // Mark the message as sent before the best-effort Sent copy: a server
    // without a usable Sent folder must not cause a second SMTP submission.
    state
        .mailbox
        .store()
        .set_outbox_state(entry.id, OutboxState::Sent, None)?;
    state.mailbox.publish(Event::OutboxChanged);

    append_to_sent(state, &account, password.expose_secret(), &entry.raw).await;
    Ok(())
}

async fn append_to_sent(state: &Arc<EngineState>, account: &Account, password: &str, raw: &[u8]) {
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
    match ImapSession::connect(&account.imap, password, tls).await {
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
