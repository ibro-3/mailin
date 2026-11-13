//! The three-pane mail window: folders, message list and reader.
//!
//! Like the GTK client, the window owns no mail state of its own: everything is
//! read from the core store. A `DispatcherQueueTimer` drains the core event
//! channel on the UI thread and refreshes the panes that changed. Widgets are
//! composed from Rust; there is no XAML compiler in a plain Cargo build.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use mailin_core::{
    Account, AccountId, AttachmentMeta, Event, Folder, FolderId, FolderRole, MessageHeader,
    MessageId, MessageListDensity, OutboxEntry, OutboxState, OutgoingEnvelope, OutgoingMessage,
    html,
};
use mailin_winui_sys::{
    Application, AutomationProperties, Button, CheckBox, ComboBox, ContentDialog,
    ContentDialogButton, DispatcherQueue, FrameworkElement, Grid, GridUnitType, IFrameworkElement,
    IPanel, IToggleButton, IUIElement, IXmlNodeSerializer, KeyboardAccelerator, MenuFlyout,
    MenuFlyoutItem, Orientation, ResourceDictionary, ScrollViewer, StackPanel, TextBlock, TextBox,
    ToastNotification, ToastNotificationManager, VerticalAlignment, VirtualKey,
    VirtualKeyModifiers, WebView2, Window, XamlRoot,
};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;
use windows_core::{EventRevoker, Interface};

use crate::account_dialog;
use crate::app::AppState;
use crate::compose;
use crate::design_tokens;
use crate::format;
use crate::move_dialog;
use crate::platform;
use crate::ui;

const SEARCH_LIMIT: u32 = 200;
const MAX_PAGE: u32 = 1_000;
const EVENT_POLL_MILLIS: i64 = 200;

/// Responsive breakpoints (in effective pixels).
const BREAKPOINT_MOBILE: f64 = 720.0;
const BREAKPOINT_TABLET: f64 = 1024.0;
const MIN_SIDEBAR_WIDTH: f64 = 200.0;
const MIN_LIST_WIDTH: f64 = 300.0;

#[derive(Debug, Default)]
struct Selection {
    account: Option<AccountId>,
    folder: Option<FolderId>,
    message: Option<MessageId>,
    query: String,
    search: bool,
    limit: u32,
}

/// An attachment whose bytes are needed before the save dialog can open.
#[derive(Debug, Clone)]
struct PendingAttachment {
    message_id: MessageId,
    attachment: AttachmentMeta,
}

#[derive(Default)]
struct State {
    selection: Selection,
    accounts: Vec<Account>,
    folders: Vec<Folder>,
    messages: Vec<MessageHeader>,
    /// Rows ticked for bulk actions. Always a subset of `messages`, so the
    /// count shown in the bulk bar matches what is on screen.
    selected: BTreeSet<MessageId>,
    outbox_entries: Vec<OutboxEntry>,
    /// The outbox entry the user picked in the list. The outbox actions work
    /// on this entry, never on an arbitrary one.
    outbox_selected: Option<i64>,
    show_outbox: bool,
    remote_allowed: Option<MessageId>,
    pending_attachment: Option<PendingAttachment>,
    /// Expanded thread roots (thread_root_key) for threaded view.
    expanded_threads: BTreeSet<String>,
}

struct Widgets {
    window: Window,
    title: TextBlock,
    banner: TextBlock,
    account_combo: ComboBox,
    add_account: Button,
    edit_account: Button,
    compose: Button,
    sync: Button,
    search: TextBox,
    search_button: Button,
    welcome: StackPanel,
    welcome_add: Button,
    mail: Grid,
    folder_list: StackPanel,
    folder_empty: TextBlock,
    list_title: TextBlock,
    bulk_bar: StackPanel,
    bulk_count: TextBlock,
    bulk_actions: ComboBox,
    message_scroll: ScrollViewer,
    message_list: StackPanel,
    message_empty: TextBlock,
    load_more: Button,
    reader_empty: StackPanel,
    reader_content: Grid,
    reader_subject: TextBlock,
    reader_from: TextBlock,
    reader_to: TextBlock,
    reader_cc: TextBlock,
    reader_date: TextBlock,
    remote_panel: StackPanel,
    remote_once: Button,
    remote_trust: Button,
    remote_dismiss: Button,
    pgp_panel: StackPanel,
    pgp_status: TextBlock,
    pgp_action: Button,
    attachments_panel: StackPanel,
    body_plain: ScrollViewer,
    body_text: TextBlock,
    body_html: WebView2,
    body_notice: TextBlock,
    html_button: Button,
    reply_button: Button,
    reply_all_button: Button,
    forward_button: Button,
    flag_button: Button,
    read_button: Button,
    archive_button: Button,
    trash_button: Button,
    junk_button: Button,
    outbox_retry_button: Button,
    outbox_discard_button: Button,
    outbox_edit_button: Button,
    /// Sync status indicator (colored dot + text)
    sync_status: TextBlock,
    /// Undo banner for move operations (shown for 5 seconds after a move)
    undo_banner: StackPanel,
    undo_button: Button,
    /// Keyboard shortcuts help window (created on demand)
    shortcuts_window: std::cell::RefCell<Option<ContentDialog>>,
}

pub struct MainWindow {
    app: Arc<AppState>,
    state: Mutex<State>,
    programmatic: AtomicBool,
    html_mode: AtomicBool,
    /// Whether the current OpenPGP message has been unwrapped.
    pgp_shown: AtomicBool,
    closed: AtomicBool,
    /// Undo state for move operations: (message_id, source_folder_id) pairs,
    /// one per message, so mixed selections restore each message to where it
    /// came from.
    last_move: Mutex<Option<Vec<(MessageId, FolderId)>>>,
    /// The auto-hide timer for the undo banner; exactly one at a time.
    undo_timer: std::rc::Rc<std::cell::RefCell<Option<mailin_winui_sys::DispatcherQueueTimer>>>,
    events: ui::Revokers,
    folder_events: ui::Revokers,
    message_events: ui::Revokers,
    attachment_events: ui::Revokers,
    /// The modal dialogs that are open. The wrapper owns the only strong
    /// reference the front-end holds on the XAML object, so it has to
    /// outlive the call that opened the dialog.
    dialogs: std::cell::RefCell<Vec<ContentDialog>>,
    /// The event subscriptions of the open dialogs. They are revoked when
    /// the dialog closes.
    dialog_events: ui::Revokers,
    /// The row checkboxes, parallel to `State::messages`, used to tick or
    /// untick the whole list at once (Ctrl+A, Escape, bulk actions).
    checkboxes: std::cell::RefCell<Vec<CheckBox>>,
    /// M3b-1 prep: checkbox lookup by message ID for future incremental reconcile.
    /// Kept in sync with `checkboxes`/`State::messages` on full rebuilds.
    /// Runtime incremental apply/scroll/focus preservation: Windows CI.
    checkboxes_by_id: std::cell::RefCell<HashMap<MessageId, CheckBox>>,
    widgets: Widgets,
}

impl MainWindow {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn show_banner(&self, text: &str) {
        let _ = self.widgets.banner.SetText(text);
        let _ = ui::set_visible(&self.widgets.banner, !text.is_empty());
    }

    /// Show the undo banner for a folder-moving action and remember the
    /// per-message source folders. Undo is a single fixed button registered
    /// once at creation time; this only records what undo should do.
    fn show_undo_banner(&self, text: &str, pairs: Vec<(MessageId, FolderId)>) {
        let _ = ui::set_visible(&self.widgets.undo_banner, true);
        // Update the text in the undo banner (first child is the text block)
        if let Ok(children) = self
            .widgets
            .undo_banner
            .cast::<IPanel>()
            .and_then(|p| p.Children())
            && children.Size().unwrap_or(0) > 0
            && let Ok(child) = children.GetAt(0)
            && let Ok(text_block) = child.cast::<TextBlock>()
        {
            let _ = text_block.SetText(text);
        }
        *self.last_move.lock().unwrap_or_else(|err| err.into_inner()) = Some(pairs);

        // Auto-hide after 5 seconds. A newer banner drops the previous timer,
        // so only the newest one can hide it.
        let Ok(queue) = DispatcherQueue::GetForCurrentThread() else {
            return;
        };
        let Ok(timer) = queue.CreateTimer() else {
            return;
        };
        if timer
            .SetInterval(windows_time::TimeSpan {
                duration: 5 * 10_000_000,
            })
            .is_err()
        {
            return;
        }
        let _ = timer.SetIsRepeating(false);
        let banner = self.widgets.undo_banner.clone();
        let slot = self.undo_timer.clone();
        let _ = timer.Tick(move |_, _| {
            let _ = ui::set_visible(&banner, false);
            *slot.borrow_mut() = None;
        });
        let _ = timer.Start();
        // Dropping the previous timer (if any) cancels its pending hide.
        *self.undo_timer.borrow_mut() = Some(timer);
    }

    /// (message_id, source_folder) pairs for the given ids. Messages outside
    /// the current view fall back to the selected folder.
    fn undo_pairs(&self, ids: &[MessageId]) -> Vec<(MessageId, FolderId)> {
        let state = self.state();
        let fallback = state.selection.folder;
        ids.iter()
            .filter_map(|id| {
                state
                    .messages
                    .iter()
                    .find(|message| message.id == *id)
                    .map(|message| (message.id, message.folder_id))
                    .or_else(|| fallback.map(|folder| (*id, folder)))
            })
            .collect()
    }

    /// Restore whatever the last undoable action moved, grouped by the
    /// folder each message came from.
    fn undo_last_move(self: &Rc<Self>) {
        let pending = self
            .last_move
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        let Some(pairs) = pending else {
            let _ = ui::set_visible(&self.widgets.undo_banner, false);
            return;
        };
        let mut groups: std::collections::HashMap<FolderId, Vec<MessageId>> =
            std::collections::HashMap::new();
        for (id, folder) in pairs {
            groups.entry(folder).or_default().push(id);
        }
        for (folder, ids) in groups {
            self.app.engine.move_messages(&ids, folder);
        }
        self.refresh_folders();
        self.refresh_messages();
        self.show_banner("Move undone");
        let _ = ui::set_visible(&self.widgets.undo_banner, false);
    }

    /// True when the selected folder is Trash (deletes there are permanent).
    fn current_folder_is_trash(&self) -> bool {
        // Copy the folder id out of the state first: the store read must not
        // run while the state is locked.
        let folder_id = self.state().selection.folder;
        folder_id
            .and_then(|id| self.app.mailin.folder(id).ok().flatten())
            .is_some_and(|folder| folder.role == FolderRole::Trash)
    }

    fn show_error(&self, message: &str) {
        let mut text = message.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().count() > 180 {
            text = text.chars().take(179).collect();
            text.push('…');
        }
        self.show_banner(&format!("Problem: {text}"));
        tracing::debug!(%message, "account error surfaced in the UI");
    }

    /// Unwrap a UI-building result, surfacing the failure through
    /// [`Self::show_error`] and giving the caller an early exit instead of
    /// swallowing it.
    fn require<T, E: std::fmt::Display>(&self, result: Result<T, E>, what: &str) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.show_error(&format!("Could not {what}: {error}"));
                None
            }
        }
    }

    /// Keep a modal dialog alive while it is shown. Only one dialog is open
    /// at a time — a `ContentDialog` is modal — so opening a new one retires
    /// the previous one.
    fn keep_dialog(&self, dialog: ContentDialog) {
        let mut dialogs = self.dialogs.borrow_mut();
        dialogs.clear();
        dialogs.push(dialog);
    }

    /// Show a simple toast notification with no action buttons.
    fn show_simple_toast(&self, title: &str, body: &str) {
        if !self.app.config().ui.notifications {
            return;
        }
        let Some(notifier) = &self.app.toast_notifier else {
            return;
        };
        let xml = match ToastNotificationManager::GetTemplateContent(
            mailin_winui_sys::TOAST_TEMPLATE_GENERIC,
        ) {
            Ok(xml) => xml,
            Err(error) => {
                tracing::warn!(%error, "failed to get the toast template");
                return;
            }
        };
        let text_nodes = match xml.GetElementsByTagName("text") {
            Ok(nodes) => nodes,
            Err(error) => {
                tracing::warn!(%error, "failed to get the toast text elements");
                return;
            }
        };
        let set_text = |index: u32, value: &str| -> windows_core::Result<()> {
            let node: IXmlNodeSerializer = text_nodes.Item(index)?.cast()?;
            node.SetInnerText(value)
        };
        let _ = set_text(0, title);
        let _ = set_text(1, body);
        let toast = match ToastNotification::CreateToastNotification(&xml) {
            Ok(toast) => toast,
            Err(error) => {
                tracing::warn!(%error, "failed to create the toast notification");
                return;
            }
        };
        if let Err(error) = notifier.Show(&toast) {
            tracing::warn!(%error, "failed to show the toast notification");
        }
    }

    fn show_toast_notification(
        self: &Rc<Self>,
        account_id: AccountId,
        text: &str,
        message_ids: &[MessageId],
    ) {
        // Check if notifications are enabled in config
        if !self.app.config().ui.notifications {
            return;
        }
        // Only show toast if we have a toast notifier
        let Some(notifier) = &self.app.toast_notifier else {
            return;
        };
        // Use ToastGeneric template to support custom action buttons
        let xml = match ToastNotificationManager::GetTemplateContent(
            mailin_winui_sys::TOAST_TEMPLATE_GENERIC,
        ) {
            Ok(xml) => xml,
            Err(error) => {
                tracing::warn!(%error, "failed to get the toast template");
                return;
            }
        };

        // Set up the visual content (title and body)
        let text_nodes = match xml.GetElementsByTagName("text") {
            Ok(nodes) => nodes,
            Err(error) => {
                tracing::warn!(%error, "failed to get the toast text elements");
                return;
            }
        };
        let set_text = |index: u32, value: &str| -> windows_core::Result<()> {
            let node: IXmlNodeSerializer = text_nodes.Item(index)?.cast()?;
            node.SetInnerText(value)
        };
        let _ = set_text(0, "New mail");
        let _ = set_text(1, text);

        // Add action buttons using XML: "Mark as read", "Archive" and "Open".
        // The arguments carry exactly the message IDs this toast is about, so
        // a button never sweeps unrelated unread mail.
        let ids_param = message_ids
            .iter()
            .map(|id| id.0.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let open_id = message_ids.first().map(|id| id.0).unwrap_or_default();
        let actions_xml = format!(
            r#"<actions>
                <action content="Mark as read" arguments="mark-read:{account_id}:{ids_param}" activationType="foreground"/>
                <action content="Archive" arguments="archive:{account_id}:{ids_param}" activationType="foreground"/>
                <action content="Open" arguments="open:{account_id}:{open_id}" activationType="foreground"/>
            </actions>"#
        );

        // Parse the actions XML and append to the toast XML
        if let Ok(actions_doc) = mailin_winui_sys::activate::<
            mailin_winui_sys::XmlDocument,
            mailin_winui_sys::XmlDocument,
        >() {
            // `LoadXml` lives on `IXmlDocumentIO`, one interface up from the
            // document's primary interface.
            if let Ok(io) = actions_doc.cast::<mailin_winui_sys::IXmlDocumentIO>() {
                let _ = io.LoadXml(&actions_xml);
            }
            if let Ok(actions_element) = actions_doc.DocumentElement() {
                match xml.DocumentElement() {
                    Ok(doc_element) => {
                        // `AppendChild` takes an `IXmlNode`.
                        if let (Ok(doc_node), Ok(action_node)) = (
                            doc_element.cast::<mailin_winui_sys::IXmlNode>(),
                            actions_element.cast::<mailin_winui_sys::IXmlNode>(),
                        ) {
                            let _ = doc_node.AppendChild(&action_node);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to read the toast document root")
                    }
                }
            }
        }

        let toast = match ToastNotification::CreateToastNotification(&xml) {
            Ok(toast) => toast,
            Err(error) => {
                tracing::warn!(%error, "failed to create the toast notification");
                return;
            }
        };

        // Handle toast activation (button clicks). The notification raises
        // this on its own thread, so everything stateful — banner, engine
        // calls, window activation — is enqueued onto the UI thread first.
        let this = Rc::clone(self);
        let queue = DispatcherQueue::GetForCurrentThread().ok();
        let _activated_token = toast.Activated(move |_sender, args| {
            fn parse_action(prefix: &str, argument: &str) -> Option<(AccountId, Vec<MessageId>)> {
                let rest = argument.strip_prefix(prefix)?;
                let mut parts = rest.splitn(2, ':');
                let account_id: AccountId = parts.next()?.parse().ok()?;
                let ids: Vec<MessageId> = parts
                    .next()
                    .map(|list| {
                        list.split(',')
                            .filter_map(|s| s.parse::<i64>().ok().map(MessageId))
                            .collect()
                    })
                    .unwrap_or_default();
                Some((account_id, ids))
            }
            // The activation carries the arguments interface behind the
            // plain `IInspectable` the handler receives.
            let activated = args
                .as_ref()
                .and_then(|args| {
                    args.cast::<mailin_winui_sys::IToastNotificationActivatedEventArgs>()
                        .ok()
                })
                .and_then(|toast_args| toast_args.Argument().ok());
            let Some(argument) = activated else {
                return;
            };
            let this = Rc::clone(&this);
            let dispatch = move || {
                if let Some((account_id, ids)) = parse_action("mark-read:", &argument) {
                    this.notification_mark_read(account_id, ids);
                } else if let Some((account_id, ids)) = parse_action("archive:", &argument) {
                    this.notification_archive(account_id, ids);
                } else if let Some((account_id, ids)) = parse_action("open:", &argument) {
                    this.notification_open(account_id, ids);
                }
            };
            // Toasts are raised on the UI thread, so the queue exists; when it
            // does not, dispatch inline instead of dropping the activation.
            match &queue {
                Some(queue) => {
                    let handler = mailin_winui_sys::DispatcherQueueHandler::new(dispatch);
                    let _ = queue.TryEnqueue(handler.to_ref());
                }
                None => dispatch(),
            }
        });

        if let Err(error) = notifier.Show(&toast) {
            tracing::warn!(%error, "failed to show the toast notification");
        }
    }

    // ------------------------------------------------------------- accounts

    fn refresh_accounts(self: &Rc<Self>) {
        let accounts = match self.app.mailin.list_accounts() {
            Ok(accounts) => accounts,
            Err(error) => {
                self.show_error(&error.to_string());
                return;
            }
        };
        let current = self.state().selection.account;
        let keep = current.filter(|id| accounts.iter().any(|account| account.id == *id));

        self.programmatic.store(true, Ordering::Relaxed);
        let _ = ui::clear_items(&self.widgets.account_combo);
        for account in &accounts {
            let _ = ui::add_combo_item(
                &self.widgets.account_combo,
                &format!("{} <{}>", account.display_name, account.email),
            );
        }
        if let Some(index) =
            keep.and_then(|id| accounts.iter().position(|account| account.id == id))
        {
            ui::select_index(&self.widgets.account_combo, index);
        }
        self.programmatic.store(false, Ordering::Relaxed);

        self.state().accounts = accounts;
        let empty = self.state().accounts.is_empty();
        if empty {
            {
                let mut state = self.state();
                state.selection = Selection::default();
                state.folders.clear();
                state.messages.clear();
            }
            let _ = self.widgets.title.SetText("Mailin");
            let _ = ui::set_visible(&self.widgets.welcome, true);
            let _ = ui::set_visible(&self.widgets.mail, false);
            let _ = self.widgets.list_title.SetText("No messages");
            self.clear_messages();
            return;
        }

        let _ = ui::set_visible(&self.widgets.welcome, false);
        let _ = ui::set_visible(&self.widgets.mail, true);
        match keep {
            Some(_) => {
                self.update_title();
                self.refresh_folders();
            }
            None => {
                let Some(first) = self.state().accounts.first().map(|account| account.id) else {
                    return;
                };
                self.select_account(first);
            }
        }
    }

    fn select_account(self: &Rc<Self>, id: AccountId) {
        {
            let mut state = self.state();
            if state.selection.account != Some(id) {
                state.selection.folder = None;
                state.selection.message = None;
                state.selection.search = false;
                state.selection.query.clear();
            }
            state.selection.account = Some(id);
            if state.selection.limit == 0 {
                state.selection.limit = self.page_size();
            }
        }
        self.programmatic.store(true, Ordering::Relaxed);
        // Look the index up in a scoped block: the guard must not be held
        // while the combo box is updated, which can re-enter Rust code that
        // locks the state again.
        let index = {
            let state = self.state();
            state.accounts.iter().position(|account| account.id == id)
        };
        if let Some(index) = index {
            ui::select_index(&self.widgets.account_combo, index);
        }
        self.programmatic.store(false, Ordering::Relaxed);
        self.update_title();
        self.refresh_folders();
    }

    fn update_title(&self) {
        let account = self.state().selection.account;
        let subtitle = account
            .and_then(|id| {
                self.state()
                    .accounts
                    .iter()
                    .find(|account| account.id == id)
                    .map(|account| account.email.clone())
            })
            .unwrap_or_else(|| "No account".to_owned());
        let _ = self.widgets.title.SetText(&format!("Mailin — {subtitle}"));
    }

    // -------------------------------------------------------------- folders

    fn refresh_folders(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            self.clear_folders();
            return;
        };
        let mut folders = match self.app.mailin.list_folders(&account_id) {
            Ok(folders) => folders,
            Err(error) => {
                self.show_error(&error.to_string());
                return;
            }
        };

        // Add virtual Outbox folder at the end
        let outbox_len = self.app.mailin.outbox_len().unwrap_or(0);
        if outbox_len > 0 {
            folders.push(Folder {
                id: FolderId::OUTBOX, // UI virtual: Outbox
                account_id,
                name: "Outbox".to_owned(),
                delimiter: "/".to_owned(),
                role: FolderRole::Other,
                uid_validity: None,
                uid_next: None,
                highest_modseq: None,
                last_synced_at: None,
                subscribed: true,
                message_count: outbox_len,
                unread_count: outbox_len,
            });
        }

        let selected_id = self.state().selection.folder;
        let index = selected_id
            .and_then(|id| folders.iter().position(|folder| folder.id == id))
            .or_else(|| {
                folders
                    .iter()
                    .position(|folder| folder.role == FolderRole::Inbox)
            })
            .or_else(|| (!folders.is_empty()).then_some(0));

        let _ = ui::clear(&self.widgets.folder_list);
        self.folder_events.replace();
        for folder in &folders {
            if let Ok(row) = folder_row(folder, self) {
                let this = Rc::clone(self);
                let folder = folder.clone();
                if let Ok(revoker) = ui::on_click(&row, move || this.select_folder(folder.clone()))
                {
                    self.folder_events.keep(revoker);
                }
                let _ = ui::append(&self.widgets.folder_list, &row);
            }
        }
        let _ = ui::set_visible(&self.widgets.folder_empty, folders.is_empty());
        self.state().folders = folders;

        match index {
            Some(index) => {
                // Look the folder up without indexing: the list may have
                // changed in the meantime.
                let Some(folder) = self.state().folders.get(index).cloned() else {
                    return;
                };
                if self.state().selection.folder != Some(folder.id) {
                    self.select_folder(folder);
                }
            }
            None => {
                let _ = self.widgets.list_title.SetText("No folders");
                self.clear_messages();
            }
        }
    }

    fn clear_folders(self: &Rc<Self>) {
        self.folder_events.replace();
        let _ = ui::clear(&self.widgets.folder_list);
        let _ = ui::set_visible(&self.widgets.folder_empty, true);
        self.state().folders.clear();
        self.clear_messages();
    }

    fn select_folder(self: &Rc<Self>, folder: Folder) {
        let is_outbox = folder.id.is_outbox();
        {
            let mut state = self.state();
            if state.selection.folder == Some(folder.id)
                && !state.selection.search
                && state.show_outbox == is_outbox
            {
                return;
            }
            state.selection.folder = Some(folder.id);
            state.selection.message = None;
            state.selection.search = false;
            state.selection.query.clear();
            state.show_outbox = is_outbox;
            state.selection.limit = self.page_size();
        }
        let _ = self.widgets.search.SetText("");
        let _ = self.widgets.list_title.SetText(&folder_title(&folder));
        self.show_reader_empty();
        self.refresh_messages();
        if !is_outbox {
            let stale = folder
                .last_synced_at
                .is_none_or(|synced| format::now_unix().saturating_sub(synced) >= 60);
            if stale {
                self.app.engine.sync_folder(folder.id);
            }
        }
    }

    // ------------------------------------------------------------- messages

    /// Apply a freshly saved configuration to the widgets that cache values
    /// from it: the reader (HTML preference) and the message list (density,
    /// page size).
    fn apply_config(self: &Rc<Self>) {
        let html_enabled = self.app.config().ui.html_enabled;
        self.html_mode.store(html_enabled, Ordering::Relaxed);
        self.refresh_messages();
        let message_id = self.state().selection.message;
        if let Some(message_id) = message_id {
            self.render_body(message_id);
        }
    }

    fn page_size(&self) -> u32 {
        self.app.config().ui.page_size.clamp(1, MAX_PAGE)
    }

    fn refresh_messages(self: &Rc<Self>) {
        let (search, query, folder_id, limit, show_outbox, account_id) = {
            let state = self.state();
            (
                state.selection.search,
                state.selection.query.clone(),
                state.selection.folder,
                state.selection.limit,
                state.show_outbox,
                state.selection.account,
            )
        };

        if search {
            let Some(account_id) = account_id else {
                return;
            };
            match self.app.mailin.search(&account_id, &query, SEARCH_LIMIT) {
                Ok(results) => {
                    self.show_messages(results, format!("Results for “{query}”"), false);
                }
                Err(error) => self.show_error(&error.to_string()),
            }
            return;
        }

        if show_outbox {
            let Some(account_id) = account_id else {
                self.clear_messages();
                return;
            };
            self.refresh_outbox(account_id);
            return;
        }

        let Some(folder_id) = folder_id else {
            self.clear_messages();
            return;
        };
        let limit = limit.max(self.page_size());
        let thread_mode = self.app.config().ui.thread_mode;
        if thread_mode != mailin_core::config::ThreadMode::Off {
            let title = self
                .app
                .mailin
                .folder(folder_id)
                .ok()
                .flatten()
                .map(|folder| folder_title(&folder))
                .unwrap_or_else(|| "Messages".to_owned());
            let more = false;
            self.show_threaded_messages(folder_id, title, more);
            return;
        }
        match self.app.mailin.list_messages(folder_id, 0, limit) {
            Ok(messages) => {
                let title = self
                    .app
                    .mailin
                    .folder(folder_id)
                    .ok()
                    .flatten()
                    .map(|folder| folder_title(&folder))
                    .unwrap_or_else(|| "Messages".to_owned());
                let more = messages.len() as u32 >= limit && limit < MAX_PAGE;
                self.show_messages(messages, title, more);
            }
            Err(error) => self.show_error(&error.to_string()),
        }
    }

    fn refresh_outbox(self: &Rc<Self>, account_id: AccountId) {
        match self.app.mailin.list_outbox(&account_id) {
            Ok(entries) => {
                let title = format!("Outbox ({})", entries.len());
                self.show_outbox_entries(entries, title);
            }
            Err(error) => self.show_error(&error.to_string()),
        }
    }

    fn show_outbox_entries(self: &Rc<Self>, entries: Vec<OutboxEntry>, title: String) {
        let now = format::now_unix();
        let empty = entries.is_empty();
        self.message_events.replace();
        let _ = ui::clear(&self.widgets.message_list);
        // Outbox rows are not messages; drop any message selection behind them.
        self.reset_bulk_selection();
        for entry in &entries {
            if let Ok(row) = outbox_entry_row(entry, now) {
                let this = Rc::clone(self);
                let entry = entry.clone();
                if let Ok(revoker) =
                    ui::on_click(&row, move || this.select_outbox_entry(entry.clone()))
                {
                    self.message_events.keep(revoker);
                }
                let _ = ui::append(&self.widgets.message_list, &row);
            }
        }
        self.state().outbox_entries = entries;
        let _ = self.widgets.list_title.SetText(&title);
        let empty_text = if empty {
            "Outbox is empty"
        } else {
            "Select an entry to see actions"
        };
        let _ = self.widgets.message_empty.SetText(empty_text);
        let _ = ui::set_visible(&self.widgets.message_empty, empty);
        let _ = ui::set_visible(&self.widgets.load_more, false);
        self.show_reader_empty();
        // Show outbox action buttons, hide regular message buttons
        let _ = ui::set_visible(&self.widgets.outbox_retry_button, true);
        let _ = ui::set_visible(&self.widgets.outbox_discard_button, true);
        let _ = ui::set_visible(&self.widgets.outbox_edit_button, true);
        let _ = ui::set_visible(&self.widgets.archive_button, false);
        let _ = ui::set_visible(&self.widgets.trash_button, false);
        let _ = ui::set_visible(&self.widgets.junk_button, false);
        // The selection survives a refresh as long as the entry is still in
        // the list; an entry that disappeared is not selected any more.
        {
            let mut state = self.state();
            let selected = state.outbox_selected;
            let still_there =
                selected.is_some_and(|id| state.outbox_entries.iter().any(|entry| entry.id == id));
            state.outbox_selected = if still_there { selected } else { None };
        }
        self.update_outbox_actions();
    }

    fn show_messages(self: &Rc<Self>, messages: Vec<MessageHeader>, title: String, more: bool) {
        let now = format::now_unix();
        let empty = messages.is_empty();
        // M3b-1 prep: compute structural diff for future incremental reconcile.
        // Full row reuse requires retaining row/control refs and event revokers
        // per message ID; runtime behavior must be verified on Windows CI.
        let _list_diff = {
            let old = self
                .state()
                .messages
                .iter()
                .map(|m| m.id)
                .collect::<Vec<_>>();
            let new_ids = messages.iter().map(|m| m.id).collect::<Vec<_>>();
            mailin_core::list_diff::diff_by_id(&old, &new_ids, |id| *id)
        };
        let identical_ids = _list_diff.removed_desc.is_empty()
            && _list_diff.inserted_count() == 0
            && _list_diff
                .reuse
                .iter()
                .enumerate()
                .all(|(i, o)| *o == Some(i));
        let _ = identical_ids; // reserved for incremental fast-path (Windows CI)

        self.message_events.replace();
        let _ = ui::clear(&self.widgets.message_list);
        self.checkboxes.borrow_mut().clear();
        // The ticked set never outlives the rows that are on screen.
        let visible: BTreeSet<MessageId> = messages.iter().map(|message| message.id).collect();
        let selected = {
            let mut state = self.state();
            state.selected.retain(|id| visible.contains(id));
            state.selected.clone()
        };
        let quick_actions_enabled = self.app.config().ui.quick_actions;
        let quick_action_items = self.app.config().ui.quick_action_items.clone();
        for message in &messages {
            let ticked = selected.contains(&message.id);
            let density = self.app.config().ui.message_list_density;
            if let Ok((row, button, checkbox, actions_bar)) =
                message_row(message, now, density, ticked)
            {
                let this = Rc::clone(self);
                let open = message.clone();
                if let Ok(revoker) = ui::on_click(&button, move || this.open_message(open.clone()))
                {
                    self.message_events.keep(revoker);
                }
                let id = message.id;
                if let Ok(toggle) = checkbox.cast::<IToggleButton>() {
                    if let Ok(revoker) = toggle.Checked({
                        let this = Rc::clone(self);
                        move |_, _| this.set_selected(id, true)
                    }) {
                        self.message_events.keep(revoker);
                    }
                    if let Ok(revoker) = toggle.Unchecked({
                        let this = Rc::clone(self);
                        move |_, _| this.set_selected(id, false)
                    }) {
                        self.message_events.keep(revoker);
                    }
                }
                // Quick actions: populate the bar and wire hover handlers.
                if quick_actions_enabled && let Some(actions_bar) = &actions_bar {
                    for action in &quick_action_items {
                        if let Ok(btn) =
                            ui::button(action.label(), Some(action.label()), Some(action.label()))
                        {
                            let this = Rc::clone(self);
                            let action = *action;
                            let mid = id;
                            if let Ok(revoker) = ui::on_click(&btn, move || {
                                this.run_quick_action(action, mid);
                            }) {
                                self.message_events.keep(revoker);
                            }
                            let _ = ui::append(actions_bar, &btn);
                        }
                    }
                    // PointerEntered → show actions bar.
                    if let Ok(element) = row.cast::<mailin_winui_sys::IUIElement>() {
                        let bar = actions_bar.clone();
                        if let Ok(revoker) = element.PointerEntered(move |_, _| {
                            let _ = ui::set_visible(&bar, true);
                        }) {
                            self.message_events.keep(revoker);
                        }
                        // PointerExited → hide actions bar.
                        let bar = actions_bar.clone();
                        if let Ok(revoker) = element.PointerExited(move |_, _| {
                            let _ = ui::set_visible(&bar, false);
                        }) {
                            self.message_events.keep(revoker);
                        }
                    }
                }
                self.checkboxes_by_id
                    .borrow_mut()
                    .insert(id, checkbox.clone());
                self.checkboxes.borrow_mut().push(checkbox);
                let _ = ui::append(&self.widgets.message_list, &row);
            }
        }
        self.state().messages = messages;
        let _ = self.widgets.list_title.SetText(&title);
        let searching = self.state().selection.search;
        let empty_text = if empty && searching {
            "No results. Try a different search term."
        } else {
            "This folder is empty, or has not been synchronized yet."
        };
        let _ = self.widgets.message_empty.SetText(empty_text);
        let _ = ui::set_visible(&self.widgets.message_empty, empty);
        let _ = ui::set_visible(&self.widgets.message_scroll, !empty);
        let _ = ui::set_visible(&self.widgets.load_more, more);
        // Show regular message buttons, hide outbox buttons
        let _ = ui::set_visible(&self.widgets.outbox_retry_button, false);
        let _ = ui::set_visible(&self.widgets.outbox_discard_button, false);
        let _ = ui::set_visible(&self.widgets.outbox_edit_button, false);
        let _ = ui::set_visible(&self.widgets.archive_button, true);
        let _ = ui::set_visible(&self.widgets.trash_button, true);
        let _ = ui::set_visible(&self.widgets.junk_button, true);
        self.update_bulk_bar();
    }

    /// Show messages with threaded/conversation view.
    fn show_threaded_messages(self: &Rc<Self>, folder_id: FolderId, title: String, more: bool) {
        let thread_mode = self.app.config().ui.thread_mode;
        if thread_mode == mailin_core::config::ThreadMode::Off {
            // Fall back to flat view
            let limit = self.state().selection.limit.max(self.page_size());
            if let Ok(messages) = self.app.mailin.list_messages(folder_id, 0, limit) {
                self.show_messages(messages, title, more);
            }
            return;
        }

        let now = format::now_unix();

        // Fetch thread roots
        let limit = self.state().selection.limit.max(self.page_size());
        let roots = match self.app.mailin.store().get_thread_roots(folder_id, limit) {
            Ok(r) => r,
            Err(e) => {
                self.show_error(&e.to_string());
                return;
            }
        };

        // Build flat list: roots + expanded replies
        let mut flat_messages = Vec::new();
        let expanded = self.state().expanded_threads.clone();

        for root in &roots {
            flat_messages.push(root.clone());
            let root_key = root.thread_root_key.as_ref().or(root.message_id.as_ref());
            if let Some(key) = root_key
                && expanded.contains(key)
                && let Ok(replies) = self.app.mailin.store().list_thread_replies(key)
            {
                // Fetch replies for this thread
                for mut reply in replies {
                    reply.thread_depth = 1; // All replies at depth 1 for flat view
                    flat_messages.push(reply);
                }
            }
        }

        let empty = flat_messages.is_empty();

        self.message_events.replace();
        let _ = ui::clear(&self.widgets.message_list);
        self.checkboxes.borrow_mut().clear();
        self.checkboxes_by_id.borrow_mut().clear();
        // The ticked set never outlives the rows that are on screen.
        let visible: BTreeSet<MessageId> = flat_messages.iter().map(|message| message.id).collect();
        let selected = {
            let mut state = self.state();
            state.selected.retain(|id| visible.contains(id));
            state.selected.clone()
        };
        let quick_actions_enabled = self.app.config().ui.quick_actions;
        let quick_action_items = self.app.config().ui.quick_action_items.clone();
        for message in &flat_messages {
            let ticked = selected.contains(&message.id);
            let density = self.app.config().ui.message_list_density;
            if let Ok((row, button, checkbox, actions_bar)) =
                message_row(message, now, density, ticked)
            {
                let this = Rc::clone(self);
                let open = message.clone();
                if let Ok(revoker) = ui::on_click(&button, move || this.open_message(open.clone()))
                {
                    self.message_events.keep(revoker);
                }
                let id = message.id;
                if let Ok(toggle) = checkbox.cast::<IToggleButton>() {
                    if let Ok(revoker) = toggle.Checked({
                        let this = Rc::clone(self);
                        move |_, _| this.set_selected(id, true)
                    }) {
                        self.message_events.keep(revoker);
                    }
                    if let Ok(revoker) = toggle.Unchecked({
                        let this = Rc::clone(self);
                        move |_, _| this.set_selected(id, false)
                    }) {
                        self.message_events.keep(revoker);
                    }
                }
                // Quick actions: populate the bar and wire hover handlers.
                if quick_actions_enabled && let Some(actions_bar) = &actions_bar {
                    for action in &quick_action_items {
                        if let Ok(btn) =
                            ui::button(action.label(), Some(action.label()), Some(action.label()))
                        {
                            let this = Rc::clone(self);
                            let action = *action;
                            let mid = id;
                            if let Ok(revoker) = ui::on_click(&btn, move || {
                                this.run_quick_action(action, mid);
                            }) {
                                self.message_events.keep(revoker);
                            }
                            let _ = ui::append(actions_bar, &btn);
                        }
                    }
                    // PointerEntered → show actions bar.
                    if let Ok(element) = row.cast::<mailin_winui_sys::IUIElement>() {
                        let bar = actions_bar.clone();
                        if let Ok(revoker) = element.PointerEntered(move |_, _| {
                            let _ = ui::set_visible(&bar, true);
                        }) {
                            self.message_events.keep(revoker);
                        }
                        // PointerExited → hide actions bar.
                        let bar = actions_bar.clone();
                        if let Ok(revoker) = element.PointerExited(move |_, _| {
                            let _ = ui::set_visible(&bar, false);
                        }) {
                            self.message_events.keep(revoker);
                        }
                    }
                }
                self.checkboxes_by_id
                    .borrow_mut()
                    .insert(message.id, checkbox.clone());
                self.checkboxes.borrow_mut().push(checkbox);
                let _ = ui::append(&self.widgets.message_list, &row);
            }
        }
        self.state().messages = flat_messages;
        let _ = self.widgets.list_title.SetText(&title);
        let searching = self.state().selection.search;
        let empty_text = if empty && searching {
            "No results. Try a different search term."
        } else {
            "This folder is empty, or has not been synchronized yet."
        };
        let _ = self.widgets.message_empty.SetText(empty_text);
        let _ = ui::set_visible(&self.widgets.message_empty, empty);
        let _ = ui::set_visible(&self.widgets.message_scroll, !empty);
        let _ = ui::set_visible(&self.widgets.load_more, more);
        // Show regular message buttons, hide outbox buttons
        let _ = ui::set_visible(&self.widgets.outbox_retry_button, false);
        let _ = ui::set_visible(&self.widgets.outbox_discard_button, false);
        let _ = ui::set_visible(&self.widgets.outbox_edit_button, false);
        let _ = ui::set_visible(&self.widgets.archive_button, true);
        let _ = ui::set_visible(&self.widgets.trash_button, true);
        let _ = ui::set_visible(&self.widgets.junk_button, true);
        self.update_bulk_bar();
    }

    fn clear_messages(self: &Rc<Self>) {
        self.message_events.replace();
        let _ = ui::clear(&self.widgets.message_list);
        self.reset_bulk_selection();
        {
            let mut state = self.state();
            state.messages.clear();
            state.selection.message = None;
        }
        let _ = self.widgets.message_empty.SetText("No messages");
        let _ = ui::set_visible(&self.widgets.message_empty, true);
        let _ = ui::set_visible(&self.widgets.message_scroll, false);
        let _ = ui::set_visible(&self.widgets.load_more, false);
        self.show_reader_empty();
    }

    fn request_more(self: &Rc<Self>) {
        {
            let mut state = self.state();
            if state.selection.search {
                return;
            }
            state.selection.limit = (state.selection.limit + self.page_size()).min(MAX_PAGE);
        }
        self.refresh_messages();
    }

    fn run_search(self: &Rc<Self>) {
        let query = ui::text_value(&self.widgets.search).trim().to_owned();
        if query.is_empty() {
            self.clear_search();
            return;
        }
        {
            let mut state = self.state();
            state.selection.search = true;
            state.selection.query = query;
        }
        self.refresh_messages();
    }

    fn clear_search(self: &Rc<Self>) {
        let was_searching = {
            let mut state = self.state();
            let was_searching = state.selection.search;
            state.selection.search = false;
            state.selection.query.clear();
            was_searching
        };
        if was_searching {
            self.refresh_messages();
        }
    }

    // --------------------------------------------------------------- reader

    fn open_message(self: &Rc<Self>, message: MessageHeader) {
        let is_draft = message.is_draft()
            || self
                .state()
                .folders
                .iter()
                .find(|folder| folder.id == message.folder_id)
                .is_some_and(|folder| folder.role == FolderRole::Drafts);
        if is_draft {
            let body = self
                .app
                .mailin
                .message_body(message.id)
                .ok()
                .flatten()
                .and_then(|(text, _)| text);
            let draft = mailin_core::compose::draft_from_header(&message, body.as_deref());
            self.open_compose(draft);
            return;
        }

        {
            let mut state = self.state();
            state.selection.message = Some(message.id);
            if state.remote_allowed != Some(message.id) {
                state.remote_allowed = None;
            }
        }
        let _ = ui::set_visible(&self.widgets.reader_empty, false);
        let _ = ui::set_visible(&self.widgets.reader_content, true);
        self.pgp_shown.store(false, Ordering::Relaxed);
        self.render_headers(&message);
        self.render_attachments(message.id);
        self.render_body(message.id);
        self.update_actions(&message);
        if self.app.config().ui.mark_read_on_open && !message.is_seen() {
            self.app
                .engine
                .set_flags(message.id, vec!["\\Seen".to_owned()], Vec::new());
        }
    }

    // ----------------------------------------------------------- outbox actions

    /// Remember the picked outbox entry and enable the actions that work on
    /// it. The entry id identifies the entry, so the selection survives a
    /// refresh as long as the entry is still in the list.
    fn select_outbox_entry(self: &Rc<Self>, entry: OutboxEntry) {
        {
            let mut state = self.state();
            state.outbox_selected = Some(entry.id);
        }
        self.update_outbox_actions();
    }

    /// Enable or disable the outbox actions. They only make sense with a
    /// selected entry: acting on an arbitrary one would be a guess.
    fn update_outbox_actions(&self) {
        let selected = self.state().outbox_selected.is_some();
        let _ = ui::set_enabled(&self.widgets.outbox_retry_button, selected);
        let _ = ui::set_enabled(&self.widgets.outbox_discard_button, selected);
        let _ = ui::set_enabled(&self.widgets.outbox_edit_button, selected);
    }

    /// The outbox entry the user picked, if it is still in the list.
    fn selected_outbox_entry(&self) -> Option<OutboxEntry> {
        let id = self.state().outbox_selected?;
        self.state()
            .outbox_entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
    }

    fn outbox_retry_selected(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let Some(entry) = self.selected_outbox_entry() else {
            return;
        };
        self.app.engine.retry_outbox(account_id, entry.id);
        self.show_banner("Retrying…");
    }

    /// Discarding a queued message cannot be undone, so the user has to
    /// confirm it — and the confirmation names the entry that is about to go.
    fn outbox_discard_selected(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let Some(entry) = self.selected_outbox_entry() else {
            return;
        };
        let Ok(xaml_root) = ui::window_xaml_root(&self.widgets.window) else {
            self.show_error("The window is not ready yet");
            return;
        };
        let Some(dialog) = self.require(ContentDialog::new(), "create the confirmation") else {
            return;
        };
        let Some(title) = self.require(ui::text("Discard queued message?"), "write the title")
        else {
            return;
        };
        let _ = dialog.SetTitle(&title);
        let Some(body) = self.require(
            ui::text(&format!(
                "The queued message to {} will be removed from the outbox. This cannot be undone.",
                outbox_entry_recipient(&entry)
            )),
            "write the confirmation text",
        ) else {
            return;
        };
        let _ = ui::set_content(&dialog, &body);
        let _ = dialog.SetPrimaryButtonText("Discard");
        let _ = dialog.SetCloseButtonText("Cancel");
        let _ = dialog.SetDefaultButton(ContentDialogButton::Close);
        // The previous dialog is closed by now: its subscriptions can go.
        self.dialog_events.replace();
        let this = Rc::clone(self);
        let entry_id = entry.id;
        if let Ok(revoker) = dialog.PrimaryButtonClick(move |_, _| {
            this.app.engine.cancel_outbox(account_id, entry_id);
            this.show_banner("Discarding…");
            // The entry is gone, so the selection must not point at it any
            // more.
            this.clear_outbox_selection();
        }) {
            self.dialog_events.keep(revoker);
        }
        let _ = ui::set_xaml_root(&dialog, &xaml_root);
        // The dialog object is shared with the front-end after this call:
        // keep it alive until the next one replaces it.
        self.keep_dialog(dialog.clone());
        let _ = dialog.ShowAsync();
    }

    fn outbox_edit_selected(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let Some(entry) = self.selected_outbox_entry() else {
            return;
        };
        let Ok(env) = serde_json::from_str::<OutgoingEnvelope>(&entry.envelope_json) else {
            self.show_error("The queued message has no readable envelope");
            return;
        };
        let mut draft = OutgoingMessage {
            from_name: String::new(),
            from_addr: env.from,
            to: env.to.join(", "),
            cc: env.cc.join(", "),
            bcc: env.bcc.join(", "),
            subject: String::new(),
            body: String::new(),
            html_body: None,
            in_reply_to: None,
            references: Vec::new(),
            draft_key: None,
            attachments: Vec::new(),
        };
        // Parse the raw message to get subject and body
        if let Some(parsed) = mail_parser::MessageParser::default().parse(&entry.raw) {
            draft.subject = parsed.subject().unwrap_or_default().to_owned();
            if let Some(text) = parsed.body_text(0) {
                draft.body = text.to_string();
            }
        }
        // The composer is opened first: cancelling the queued entry before
        // the draft is on screen would destroy the message whenever opening
        // the composer fails.
        if self.open_compose_checked(draft) {
            self.app.engine.cancel_outbox(account_id, entry.id);
            self.clear_outbox_selection();
        }
    }

    /// Forget the picked outbox entry and disable the actions that need one.
    fn clear_outbox_selection(&self) {
        {
            let mut state = self.state();
            state.outbox_selected = None;
        }
        self.update_outbox_actions();
    }

    fn render_headers(&self, message: &MessageHeader) {
        let _ = self
            .widgets
            .reader_subject
            .SetText(&format::subject_label(message));
        let _ = self.widgets.reader_from.SetText(&message.from_display());

        if message.to_addrs.is_empty() {
            let _ = ui::set_visible(&self.widgets.reader_to, false);
        } else {
            let _ = self
                .widgets
                .reader_to
                .SetText(&format!("To: {}", message.to_addrs));
            let _ = ui::set_visible(&self.widgets.reader_to, true);
        }
        if message.cc_addrs.is_empty() {
            let _ = ui::set_visible(&self.widgets.reader_cc, false);
        } else {
            let _ = self
                .widgets
                .reader_cc
                .SetText(&format!("Cc: {}", message.cc_addrs));
            let _ = ui::set_visible(&self.widgets.reader_cc, true);
        }

        let timestamp = message.sent_at.unwrap_or(message.received_at);
        let _ = self
            .widgets
            .reader_date
            .SetText(&format::full_date(timestamp));
    }

    // Helper: find the header for a message, preferring cached list over store lookup
    fn header_for(&self, message_id: MessageId) -> Option<MessageHeader> {
        // The cached lookup must not hold the state lock while the store is
        // read, so the list is searched in a scoped block.
        let cached = {
            let state = self.state();
            state
                .messages
                .iter()
                .find(|message| message.id == message_id)
                .cloned()
        };
        cached.or_else(|| self.app.mailin.message(message_id).ok().flatten())
    }

    // Whether the account behind the current selection trusts `domain` for
    // remote content. Reads from the store so changes via the account dialog
    // or the banner's trust button are picked up immediately.
    fn sender_domain_allowed(&self, domain: &str) -> bool {
        // Copy the account id out of the state first: the store read must
        // not run while the state is locked.
        let account = self.state().selection.account;
        match account {
            Some(id) => self
                .app
                .mailin
                .account(&id)
                .ok()
                .flatten()
                .is_some_and(|account| account.remote_domain_allowed(domain)),
            None => false,
        }
    }

    // Persist "always allow" for the sender's domain on the current account
    // and re-render the message with remote content enabled.
    fn trust_current_sender(self: &Rc<Self>) {
        let (message_id, account_id) = {
            let state = self.state();
            (state.selection.message, state.selection.account)
        };
        let (Some(message_id), Some(account_id)) = (message_id, account_id) else {
            return;
        };
        let Some(domain) = self
            .header_for(message_id)
            .and_then(|header| header.from_domain().map(str::to_owned))
        else {
            return;
        };
        let Some(mut account) = self.app.mailin.account(&account_id).ok().flatten() else {
            return;
        };
        if account.allow_remote_domain(&domain)
            && let Err(error) = self.app.mailin.save_account(&account, None)
        {
            tracing::error!(%error, "failed to save trusted domain");
            return;
        }
        self.state().remote_allowed = Some(message_id);
        let _ = ui::set_visible(&self.widgets.remote_panel, false);
        self.render_body(message_id);
    }

    fn render_body(self: &Rc<Self>, message_id: MessageId) {
        let html_mode = self.html_mode.load(Ordering::Relaxed);
        let _ = ui::set_visible(&self.widgets.body_plain, false);
        let _ = ui::set_visible(&self.widgets.body_html, false);
        let _ = ui::set_visible(&self.widgets.remote_panel, false);
        // M4-D: the inline-image notice is recomputed for every message.
        let _ = ui::set_visible(&self.widgets.body_notice, false);

        // OpenPGP messages are unwrapped on demand.
        if let Ok(Some(pgp)) = self.app.mailin.pgp_mime(message_id) {
            if self.pgp_shown.load(Ordering::Relaxed) {
                self.decrypt_pgp_body(message_id);
                return;
            }
            let encrypted = pgp.kind == mailin_core::PgpMimeKind::Encrypted;
            let _ = self.widgets.pgp_status.SetText(if encrypted {
                "This message is encrypted with OpenPGP. Decrypt it to read the content."
            } else {
                "This message is signed with OpenPGP. Verify it to check the signature and read the content."
            });
            let _ = ui::set_button_label(
                &self.widgets.pgp_action,
                if encrypted { "Decrypt" } else { "Verify" },
            );
            let _ = ui::set_enabled(&self.widgets.pgp_action, true);
            let _ = ui::set_visible(&self.widgets.pgp_panel, true);
            let _ = ui::set_enabled(&self.widgets.html_button, false);
            return;
        }
        let _ = ui::set_visible(&self.widgets.pgp_panel, false);

        let Ok(body) = self.app.mailin.message_body(message_id) else {
            let _ = self
                .widgets
                .body_text
                .SetText("Could not read this message.");
            let _ = ui::set_visible(&self.widgets.body_plain, true);
            return;
        };
        match body {
            Some((text, html)) => {
                let has_text = text.as_deref().is_some_and(|text| !text.trim().is_empty());
                let has_html = html.as_deref().is_some_and(|html| !html.trim().is_empty());
                let want_html = has_html && html_mode;

                let _ = ui::set_enabled(&self.widgets.html_button, has_html);
                let label = if want_html {
                    "Show plain text"
                } else {
                    "Show HTML"
                };
                let _ = ui::set_button_label(&self.widgets.html_button, label);

                if want_html {
                    let html_body = html.as_deref().unwrap_or_default();
                    // Determine sender domain for per-sender trust check
                    let sender_domain = self
                        .header_for(message_id)
                        .and_then(|header| header.from_domain().map(str::to_owned));
                    let trusted = sender_domain
                        .as_deref()
                        .is_some_and(|domain| self.sender_domain_allowed(domain));
                    let allow_remote = self.app.config().ui.remote_content_enabled
                        || self.state().remote_allowed == Some(message_id)
                        || trusted;
                    let images = self.inline_images(message_id);
                    let document = html::document(html_body, allow_remote, &images);
                    let _ = self.widgets.body_html.NavigateToString(&document);
                    let _ = ui::set_visible(&self.widgets.body_html, true);
                    // M4-D: report inline images whose `cid:` part is missing.
                    let unresolved = mailin_core::cid::references(&document);
                    if !unresolved.is_empty() {
                        let _ = self.widgets.body_notice.SetText(&format!(
                            "{} couldn't be loaded.",
                            mailin_core::plural(unresolved.len(), "inline image", "inline images")
                        ));
                        let _ = ui::set_visible(&self.widgets.body_notice, true);
                    }
                    if !allow_remote && html::has_remote_content(html_body) {
                        // Show/hide trust button based on whether we have a domain
                        match sender_domain {
                            Some(domain) => {
                                let _ = ui::set_button_label(
                                    &self.widgets.remote_trust,
                                    &format!("Always allow @{domain}"),
                                );
                                let _ = ui::set_visible(&self.widgets.remote_trust, true);
                            }
                            None => {
                                let _ = ui::set_visible(&self.widgets.remote_trust, false);
                            }
                        }
                        let _ = ui::set_visible(&self.widgets.remote_panel, true);
                    }
                } else if has_text {
                    let _ = self
                        .widgets
                        .body_text
                        .SetText(text.as_deref().unwrap_or_default());
                    let _ = ui::set_visible(&self.widgets.body_plain, true);
                } else if has_html {
                    let _ = self.widgets.body_text.SetText(
                        "This message only provides an HTML body. Use the HTML button to show it.",
                    );
                    let _ = ui::set_visible(&self.widgets.body_plain, true);
                } else {
                    let _ = self
                        .widgets
                        .body_text
                        .SetText("This message has no readable body.");
                    let _ = ui::set_visible(&self.widgets.body_plain, true);
                }
            }
            None => {
                let fetched = self
                    .app
                    .mailin
                    .message(message_id)
                    .ok()
                    .flatten()
                    .is_some_and(|message| message.body_fetched);
                if fetched {
                    let _ = self
                        .widgets
                        .body_text
                        .SetText("This message has no readable body.");
                } else {
                    let _ = self.widgets.body_text.SetText("Loading message…");
                    self.app.engine.fetch_body(message_id);
                }
                let _ = ui::set_visible(&self.widgets.body_plain, true);
            }
        }
    }

    /// Decrypt or verify the selected message and show the unwrapped body.
    fn decrypt_pgp_body(self: &Rc<Self>, message_id: MessageId) {
        let html_mode = self.html_mode.load(Ordering::Relaxed);
        let _ = ui::set_enabled(&self.widgets.pgp_action, false);
        match self.app.mailin.decrypted_body(message_id) {
            Ok((text, html, status)) => {
                self.pgp_shown.store(true, Ordering::Relaxed);
                let _ = self
                    .widgets
                    .pgp_status
                    .SetText(&format!("OpenPGP signature: {}", signature_label(status)));
                let _ = ui::set_visible(&self.widgets.pgp_action, false);

                let has_text = text.as_deref().is_some_and(|text| !text.trim().is_empty());
                let has_html = html.as_deref().is_some_and(|html| !html.trim().is_empty());
                let want_html = has_html && html_mode;
                let _ = ui::set_enabled(&self.widgets.html_button, has_html);
                let _ = ui::set_button_label(
                    &self.widgets.html_button,
                    if want_html {
                        "Show plain text"
                    } else {
                        "Show HTML"
                    },
                );

                if want_html {
                    let html_body = html.as_deref().unwrap_or_default();
                    let document = html::document(html_body, false, &[]);
                    let _ = self.widgets.body_html.NavigateToString(&document);
                    let _ = ui::set_visible(&self.widgets.body_html, true);
                } else if has_text {
                    let _ = self
                        .widgets
                        .body_text
                        .SetText(text.as_deref().unwrap_or_default());
                    let _ = ui::set_visible(&self.widgets.body_plain, true);
                } else if has_html {
                    let _ = self.widgets.body_text.SetText(
                        "This message only provides an HTML body. Use the HTML button to show it.",
                    );
                    let _ = ui::set_visible(&self.widgets.body_plain, true);
                } else {
                    let _ = self
                        .widgets
                        .body_text
                        .SetText("The decrypted message has no readable body.");
                    let _ = ui::set_visible(&self.widgets.body_plain, true);
                }
            }
            Err(error) => {
                let _ = self
                    .widgets
                    .pgp_status
                    .SetText(&format!("Could not open the message: {error}"));
                let _ = ui::set_enabled(&self.widgets.pgp_action, true);
            }
        }
    }

    /// `cid:` images from inline attachment data, ready for the HTML viewer.
    fn inline_images(&self, message_id: MessageId) -> Vec<(String, String)> {
        let attachments = self
            .app
            .mailin
            .list_attachments(message_id)
            .unwrap_or_default();
        attachments
            .into_iter()
            .filter(|attachment| attachment.is_inline)
            .filter_map(|attachment| {
                let content_id = attachment.content_id?;
                let data = self
                    .app
                    .mailin
                    .attachment(message_id, &attachment.part_id)
                    .ok()
                    .flatten()?
                    .data?;
                Some((content_id, html::data_url(&attachment.mime_type, &data)))
            })
            .collect()
    }

    fn render_attachments(self: &Rc<Self>, message_id: MessageId) {
        self.attachment_events.replace();
        let _ = ui::clear(&self.widgets.attachments_panel);
        let attachments = self
            .app
            .mailin
            .list_attachments(message_id)
            .unwrap_or_default();
        let visible: Vec<_> = attachments
            .into_iter()
            .filter(|attachment| !attachment.is_inline)
            .collect();

        // "Download all" button when multiple attachments
        if visible.len() > 1
            && let Ok(download_all_row) = ui::stack(Orientation::Horizontal, 8.0)
        {
            let build_download_all = || -> anyhow::Result<()> {
                ui::padding(&download_all_row, 4.0, 4.0, 4.0, 4.0)?;
                let download_all = ui::button(
                    "Download all",
                    Some("Download all"),
                    Some("Download all attachments"),
                )?;
                let this = Rc::clone(self);
                let mid = message_id;
                let visible_clone = visible.clone();
                let revoker = ui::on_click(&download_all, move || {
                    for att in &visible_clone {
                        this.save_attachment(mid, att.clone());
                    }
                })?;
                self.attachment_events.keep(revoker);
                ui::append(&download_all_row, &download_all)?;
                let _ = ui::append(&self.widgets.attachments_panel, &download_all_row);
                Ok(())
            };
            if let Err(error) = build_download_all() {
                tracing::warn!(%error, "could not build the download-all attachment row");
            }
        }

        for attachment in &visible {
            if let Ok(row) = self.attachment_row(message_id, attachment.clone()) {
                let _ = ui::append(&self.widgets.attachments_panel, &row);
            }
        }
        let _ = ui::set_visible(&self.widgets.attachments_panel, !visible.is_empty());
    }

    fn attachment_row(
        self: &Rc<Self>,
        message_id: MessageId,
        attachment: AttachmentMeta,
    ) -> Result<StackPanel> {
        let is_image = attachment.mime_type.starts_with("image/");
        let is_pdf = attachment.mime_type == "application/pdf";
        let is_large = attachment.size.map(|s| s > 40_000_000).unwrap_or(false);

        let row = ui::stack(Orientation::Horizontal, 8.0)?;
        ui::padding(&row, 4.0, 4.0, 4.0, 4.0)?;

        // Preview for images and PDFs
        if (is_image || is_pdf)
            && let Ok(preview) = self.attachment_preview(message_id, attachment.clone())
        {
            ui::append(&row, &preview)?;
        }

        let name = ui::text(&attachment.display_name())?;
        ui::stretch_horizontal(&name)?;
        ui::append(&row, &name)?;

        if let Some(size) = attachment.size {
            let size_text = format::file_size(size);
            let size_block = ui::text(&size_text)?;
            if is_large {
                let _ = size_block
                    .SetForeground(&ui::solid_brush(design_tokens::Colors::default().error)?);
            }
            ui::append(&row, &size_block)?;
        }

        if is_large {
            let warning = ui::text("⚠ Large file")?;
            let _ =
                warning.SetForeground(&ui::solid_brush(design_tokens::Colors::default().error)?);
            ui::append(&row, &warning)?;
        }

        let save = ui::button(
            "Save",
            Some(&format!("Save {}", attachment.display_name())),
            Some(&format!("Save attachment {}", attachment.display_name())),
        )?;
        let this = Rc::clone(self);
        let attachment = attachment.clone();
        let revoker = ui::on_click(&save, move || {
            this.save_attachment(message_id, attachment.clone())
        })?;
        self.attachment_events.keep(revoker);
        ui::append(&row, &save)?;
        Ok(row)
    }

    fn attachment_preview(
        self: &Rc<Self>,
        message_id: MessageId,
        attachment: AttachmentMeta,
    ) -> Result<StackPanel> {
        let container = ui::stack(Orientation::Vertical, 2.0)?;
        ui::min_width(&container, 60.0)?;
        ui::min_height(&container, 60.0)?;

        let is_image = attachment.mime_type.starts_with("image/");
        let is_pdf = attachment.mime_type == "application/pdf";

        if is_image {
            // Image preview - show thumbnail
            if let Ok(Some(data)) = self.app.mailin.attachment(message_id, &attachment.part_id)
                && let Some(bytes) = data.data
                && let Ok(bitmap) = Self::bytes_to_bitmap(&bytes)
            {
                let image =
                    mailin_winui_sys::activate::<mailin_winui_sys::Image, mailin_winui_sys::Image>(
                    )?;
                image.SetSource(&bitmap)?;
                image.SetStretch(mailin_winui_sys::Stretch::Uniform)?;
                // `MaxWidth`/`MaxHeight` live on `IFrameworkElement`,
                // one interface up from the image's primary interface.
                if let Ok(element) = image.cast::<IFrameworkElement>() {
                    element.SetMaxWidth(80.0)?;
                    element.SetMaxHeight(80.0)?;
                }
                ui::append(&container, &image)?;
            }
        } else if is_pdf {
            // PDF preview - show icon with page count
            let pdf_icon = ui::text("📄")?;
            pdf_icon.SetFontSize(32.0)?;
            ui::append(&container, &pdf_icon)?;
            let pdf_label = ui::text("PDF")?;
            pdf_label.SetFontSize(10.0)?;
            ui::append(&container, &pdf_label)?;
        }

        Ok(container)
    }

    fn bytes_to_bitmap(bytes: &[u8]) -> Result<mailin_winui_sys::BitmapImage> {
        use mailin_winui_sys::{
            BitmapImage, IBitmapSource, IOutputStream, InMemoryRandomAccessStream,
        };
        use windows_core::Interface;

        // The image bytes go straight into an in-memory stream: the write
        // side of `IRandomAccessStream` is `IOutputStream`.
        let stream =
            mailin_winui_sys::activate::<InMemoryRandomAccessStream, InMemoryRandomAccessStream>()?;
        let output: IOutputStream = stream.cast()?;
        // `IOutputStream` only offers asynchronous writes; `DataWriter` is
        // the synchronous writer on top of it.
        let writer = mailin_winui_sys::DataWriter::CreateDataWriter(&output)?;
        writer.WriteBytes(bytes)?;
        // Detaching flushes the bytes into the underlying stream.
        let _ = writer.DetachStream()?;
        stream.Seek(0)?;

        let bitmap = mailin_winui_sys::activate::<BitmapImage, BitmapImage>()?;
        let source: IBitmapSource = bitmap.cast()?;
        source.SetSource(&stream)?;
        Ok(bitmap)
    }

    fn save_attachment(self: &Rc<Self>, message_id: MessageId, attachment: AttachmentMeta) {
        let result = self.app.mailin.attachment(message_id, &attachment.part_id);
        match result {
            Ok(Some(data)) => match data.data {
                Some(bytes) => self.write_attachment(&attachment, bytes),
                None => self.request_attachment(message_id, attachment),
            },
            Ok(None) => self.request_attachment(message_id, attachment),
            Err(error) => {
                self.show_error(&format!("Could not read the attachment: {error}"));
            }
        }
    }

    fn request_attachment(&self, message_id: MessageId, attachment: AttachmentMeta) {
        self.state().pending_attachment = Some(PendingAttachment {
            message_id,
            attachment,
        });
        self.app.engine.refresh_body(message_id);
        self.show_banner("Downloading attachment…");
    }

    fn resolve_pending_attachment(&self, message_id: MessageId) {
        let pending = {
            let mut state = self.state();
            if state
                .pending_attachment
                .as_ref()
                .is_some_and(|pending| pending.message_id == message_id)
            {
                state.pending_attachment.take()
            } else {
                None
            }
        };
        let Some(pending) = pending else {
            return;
        };
        match self
            .app
            .mailin
            .attachment(message_id, &pending.attachment.part_id)
            .ok()
            .flatten()
            .and_then(|data| data.data)
        {
            Some(bytes) => self.write_attachment(&pending.attachment, bytes),
            None => {
                self.show_error("Could not load the attachment; it may no longer be available.")
            }
        }
    }

    fn write_attachment(&self, attachment: &AttachmentMeta, bytes: Vec<u8>) {
        let Some(path) = platform::save_file("Save attachment", &attachment.display_name()) else {
            return;
        };
        match platform::save_attachment(&path, &bytes) {
            Ok(()) => self.show_banner("Attachment saved"),
            Err(message) => self.show_error(&message),
        }
    }

    fn update_actions(&self, message: &MessageHeader) {
        let seen = message.is_seen();
        let flagged = message.is_flagged();
        let _ = ui::set_button_label(
            &self.widgets.read_button,
            if seen { "Mark unread" } else { "Mark read" },
        );
        let _ = ui::set_button_label(
            &self.widgets.flag_button,
            if flagged { "Unstar" } else { "Star" },
        );
    }

    fn show_reader_empty(&self) {
        let _ = ui::set_visible(&self.widgets.reader_empty, true);
        let _ = ui::set_visible(&self.widgets.reader_content, false);
    }

    fn selected_message(&self) -> Option<MessageHeader> {
        let id = self.state().selection.message?;
        self.app.mailin.message(id).ok().flatten()
    }

    fn toggle_seen(&self) {
        let Some(message) = self.selected_message() else {
            return;
        };
        if message.is_seen() {
            self.app
                .engine
                .set_flags(message.id, Vec::new(), vec!["\\Seen".to_owned()]);
        } else {
            self.app
                .engine
                .set_flags(message.id, vec!["\\Seen".to_owned()], Vec::new());
        }
    }

    fn toggle_flagged(&self) {
        let Some(message) = self.selected_message() else {
            return;
        };
        if message.is_flagged() {
            self.app
                .engine
                .set_flags(message.id, Vec::new(), vec!["\\Flagged".to_owned()]);
        } else {
            self.app
                .engine
                .set_flags(message.id, vec!["\\Flagged".to_owned()], Vec::new());
        }
    }

    fn archive_selected(&self) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let ids = vec![message.id];
        let pairs = self.undo_pairs(&ids);
        self.app.engine.archive_messages(&ids);
        self.show_undo_banner("Archived 1 message", pairs);
    }

    fn delete_selected(&self) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let ids = vec![message.id];
        if self.current_folder_is_trash() {
            self.app.engine.permanently_delete_messages(&ids);
            self.show_banner("Deleting permanently…");
            return;
        }
        let pairs = self.undo_pairs(&ids);
        self.app.engine.delete_messages(&ids);
        self.show_undo_banner("Moved 1 message to Trash", pairs);
    }

    fn mark_selected_junk(&self, junk: bool) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let ids = vec![message.id];
        let pairs = self.undo_pairs(&ids);
        self.app.engine.mark_messages_junk(&ids, junk);
        self.show_undo_banner(
            if junk {
                "Marked 1 message as Junk"
            } else {
                "Moved 1 message to Inbox"
            },
            pairs,
        );
    }

    /// Run a quick action on a specific message (from hover/swipe).
    fn run_quick_action(&self, action: mailin_core::config::QuickAction, message_id: MessageId) {
        use mailin_core::config::QuickAction;
        let ids = vec![message_id];
        match action {
            QuickAction::Archive => {
                let pairs = self.undo_pairs(&ids);
                self.app.engine.archive_messages(&ids);
                self.show_undo_banner("Archived 1 message", pairs);
            }
            QuickAction::Delete => {
                if self.current_folder_is_trash() {
                    self.app.engine.permanently_delete_messages(&ids);
                    self.show_banner("Deleting permanently…");
                    return;
                }
                let pairs = self.undo_pairs(&ids);
                self.app.engine.delete_messages(&ids);
                self.show_undo_banner("Moved 1 message to Trash", pairs);
            }
            QuickAction::Junk => {
                let pairs = self.undo_pairs(&ids);
                self.app.engine.mark_messages_junk(&ids, true);
                self.show_undo_banner("Marked 1 message as Junk", pairs);
            }
            QuickAction::Flag => {
                self.app
                    .engine
                    .bulk_set_flags(&ids, vec!["\\Flagged".to_owned()], Vec::new());
            }
            QuickAction::Unflag => {
                self.app
                    .engine
                    .bulk_set_flags(&ids, Vec::new(), vec!["\\Flagged".to_owned()]);
            }
            QuickAction::MarkRead => {
                self.app
                    .engine
                    .bulk_set_flags(&ids, vec!["\\Seen".to_owned()], Vec::new());
            }
            QuickAction::MarkUnread => {
                self.app
                    .engine
                    .bulk_set_flags(&ids, Vec::new(), vec!["\\Seen".to_owned()]);
            }
        }
    }

    /// Handle "Mark as read" action from a notification.
    ///
    /// `ids` are the message IDs the toast was about; when it is empty the
    /// action falls back to the account's unread inbox messages.
    fn notification_mark_read(&self, account_id: AccountId, ids: Vec<MessageId>) {
        let ids = if ids.is_empty() {
            self.get_unread_inbox_messages(account_id)
        } else {
            ids
        };
        if ids.is_empty() {
            return;
        }
        self.app
            .engine
            .bulk_set_flags(&ids, vec!["\\Seen".to_owned()], Vec::new());
        self.show_banner("Marked as read");
    }

    /// Handle "Archive" action from a notification.
    fn notification_archive(&self, account_id: AccountId, ids: Vec<MessageId>) {
        let ids = if ids.is_empty() {
            self.get_unread_inbox_messages(account_id)
        } else {
            ids
        };
        if ids.is_empty() {
            return;
        }
        self.app.engine.archive_messages(&ids);
        self.show_banner("Archiving…");
    }

    /// Handle the "Open" action from a notification: bring the window
    /// forward, switch to the account the toast was about and show the
    /// newest message it mentioned.
    fn notification_open(self: &Rc<Self>, account_id: AccountId, ids: Vec<MessageId>) {
        let _ = self.widgets.window.Activate();
        if self.state().selection.account != Some(account_id) {
            self.select_account(account_id);
        }
        let Some(message_id) = ids.first().copied() else {
            // Nothing specific to open: the inbox of the selected account is
            // on screen.
            return;
        };
        let Ok(Some(header)) = self.app.mailin.message(message_id) else {
            self.show_error("Could not open the message");
            return;
        };
        self.open_message(header);
    }

    /// Get unread message IDs from the inbox folder for an account.
    fn get_unread_inbox_messages(&self, account_id: AccountId) -> Vec<MessageId> {
        let Ok(folders) = self.app.mailin.list_folders(&account_id) else {
            return Vec::new();
        };
        let inbox_folder = folders.iter().find(|f| f.role == FolderRole::Inbox);
        let Some(folder) = inbox_folder else {
            return Vec::new();
        };
        let Ok(messages) = self.app.mailin.list_messages(folder.id, 0, 100) else {
            return Vec::new();
        };
        messages
            .into_iter()
            .filter(|m| !m.is_seen())
            .map(|m| m.id)
            .collect()
    }

    // --------------------------------------------------------- bulk actions

    /// Record or drop a row's tick and refresh the bulk bar.
    ///
    /// Called from the row checkbox handlers; the state lock is released
    /// before the bar is updated because `update_bulk_bar` re-locks it.
    fn set_selected(&self, id: MessageId, selected: bool) {
        {
            let mut state = self.state();
            if selected {
                state.selected.insert(id);
            } else {
                state.selected.remove(&id);
            }
        }
        self.update_bulk_bar();
    }

    /// Show or hide the bulk action bar for the current selection.
    fn update_bulk_bar(&self) {
        let count = self.state().selected.len();
        let _ = self
            .widgets
            .bulk_count
            .SetText(&format!("{count} selected"));
        let visible = count > 0;
        let _ = ui::set_visible(&self.widgets.bulk_bar, visible);
        if !visible {
            // Park the dropdown on its placeholder so the next selection
            // starts from a clean slate. Re-entry sees index 0 and returns.
            ui::select_index(&self.widgets.bulk_actions, 0);
        }
    }

    /// The ids of the ticked rows, in list order.
    fn selected_ids(&self) -> Vec<MessageId> {
        let state = self.state();
        state
            .messages
            .iter()
            .filter(|message| state.selected.contains(&message.id))
            .map(|message| message.id)
            .collect()
    }

    fn bulk_mark_seen(&self, seen: bool) {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return;
        }
        if seen {
            self.app
                .engine
                .bulk_set_flags(&ids, vec!["\\Seen".to_owned()], Vec::new());
        } else {
            self.app
                .engine
                .bulk_set_flags(&ids, Vec::new(), vec!["\\Seen".to_owned()]);
        }
    }

    fn bulk_mark_flagged(&self, flagged: bool) {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return;
        }
        if flagged {
            self.app
                .engine
                .bulk_set_flags(&ids, vec!["\\Flagged".to_owned()], Vec::new());
        } else {
            self.app
                .engine
                .bulk_set_flags(&ids, Vec::new(), vec!["\\Flagged".to_owned()]);
        }
    }

    fn bulk_archive(&self) {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return;
        }
        let count = ids.len();
        let pairs = self.undo_pairs(&ids);
        self.app.engine.archive_messages(&ids);
        self.show_undo_banner(
            &format!(
                "Archived {}",
                mailin_core::plural(count, "message", "messages")
            ),
            pairs,
        );
        self.clear_bulk_selection();
    }

    fn bulk_delete(&self) {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return;
        }
        let count = ids.len();
        if self.current_folder_is_trash() {
            self.app.engine.permanently_delete_messages(&ids);
            self.show_banner(&format!(
                "Deleted {} permanently",
                mailin_core::plural(count, "message", "messages")
            ));
            self.clear_bulk_selection();
            return;
        }
        let pairs = self.undo_pairs(&ids);
        self.app.engine.delete_messages(&ids);
        self.show_undo_banner(
            &format!(
                "Moved {} to Trash",
                mailin_core::plural(count, "message", "messages")
            ),
            pairs,
        );
        self.clear_bulk_selection();
    }

    /// Ask for a destination folder and move every ticked row there.
    fn bulk_move(self: &Rc<Self>) {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return;
        }
        let folders = self.state().folders.clone();
        let exclude = self.state().selection.folder;
        let xaml_root = match ui::window_xaml_root(&self.widgets.window) {
            Ok(root) => root,
            Err(error) => {
                tracing::error!(%error, "could not get the window's XAML root");
                return;
            }
        };
        let app = Arc::clone(&self.app);
        let this = Rc::downgrade(self);
        let ids_clone = ids.clone();
        let picked = move |target: FolderId| {
            app.engine.move_messages(&ids, target);
            if let Some(this) = this.upgrade() {
                let count = ids_clone.len();
                let pairs = this.undo_pairs(&ids_clone);
                this.show_undo_banner(
                    &format!(
                        "Moved {}",
                        mailin_core::plural(count, "message", "messages")
                    ),
                    pairs,
                );
                this.clear_bulk_selection();
            }
        };
        if let Err(error) = move_dialog::show(&xaml_root, &self.events, &folders, exclude, picked) {
            self.show_error(&format!("Could not open the folder picker: {error:#}"));
        }
    }

    /// Move a message to a folder without showing the undo banner (used for undo).
    /// Dispatch the action chosen in the bulk bar's dropdown.
    ///
    /// The dropdown is parked on its placeholder first; that fires
    /// `SelectionChanged` again, but the re-entrant call reads index 0 and
    /// returns immediately.
    fn run_bulk_action(self: &Rc<Self>) {
        let index = ui::selected_index(&self.widgets.bulk_actions);
        if index == 0 {
            return;
        }
        ui::select_index(&self.widgets.bulk_actions, 0);
        match index {
            1 => self.bulk_mark_seen(true),
            2 => self.bulk_mark_seen(false),
            3 => self.bulk_mark_flagged(true),
            4 => self.bulk_mark_flagged(false),
            5 => self.bulk_move(),
            6 => self.bulk_archive(),
            7 => self.bulk_delete(),
            8 => self.clear_bulk_selection(),
            _ => {}
        }
    }

    /// Tick every row currently in the list (Ctrl+A).
    fn select_all(&self) {
        let ids: BTreeSet<MessageId> = {
            let state = self.state();
            // The outbox pane shows entries, not messages; its stale message
            // list must not be ticked.
            if state.show_outbox {
                return;
            }
            state.messages.iter().map(|message| message.id).collect()
        };
        self.state().selected = ids;
        let checkboxes = self.checkboxes.borrow().clone();
        for checkbox in &checkboxes {
            if let Ok(toggle) = checkbox.cast::<IToggleButton>() {
                let _ = toggle.SetIsChecked(Some(true));
            }
        }
        self.update_bulk_bar();
    }

    /// Untick every row while keeping the rows themselves on screen
    /// (Escape, "Clear selection", and after archive/delete/move).
    fn clear_bulk_selection(&self) {
        self.state().selected.clear();
        let checkboxes = self.checkboxes.borrow().clone();
        for checkbox in &checkboxes {
            if let Ok(toggle) = checkbox.cast::<IToggleButton>() {
                let _ = toggle.SetIsChecked(Some(false));
            }
        }
        self.update_bulk_bar();
    }

    /// Forget the selection entirely, including the checkbox widgets; used
    /// when the list is replaced or the view switches to the outbox.
    fn reset_bulk_selection(&self) {
        self.state().selected.clear();
        self.checkboxes.borrow_mut().clear();
        self.checkboxes_by_id.borrow_mut().clear();
        self.update_bulk_bar();
    }

    /// Move selection to the previous message (Up arrow).
    fn select_previous_message(&self) {
        let new_idx = {
            let state = self.state();
            if state.show_outbox || state.messages.is_empty() {
                return;
            }
            if let Some(&last) = state.selected.iter().last() {
                // Find the index of the last selected message and go to previous
                state
                    .messages
                    .iter()
                    .position(|m| m.id == last)
                    .map(|i| i.saturating_sub(1))
                    .unwrap_or(0)
            } else {
                // Nothing selected, select the first message
                0
            }
        };
        let mut state = self.state();
        if let Some(msg_id) = state.messages.get(new_idx).map(|message| message.id) {
            state.selected.clear();
            state.selected.insert(msg_id);
            drop(state);
            // Update checkbox UI
            let checkboxes = self.checkboxes.borrow().clone();
            for (i, checkbox) in checkboxes.iter().enumerate() {
                if let Ok(toggle) = checkbox.cast::<IToggleButton>() {
                    let _ = toggle.SetIsChecked(Some(i == new_idx));
                }
            }
            self.update_bulk_bar();
        }
    }

    /// Move selection to the next message (Down arrow).
    fn select_next_message(&self) {
        let new_idx = {
            let state = self.state();
            if state.show_outbox || state.messages.is_empty() {
                return;
            }
            if let Some(&last) = state.selected.iter().last() {
                // Find the index of the last selected message and go to next
                state
                    .messages
                    .iter()
                    .position(|m| m.id == last)
                    .map(|i| (i + 1).min(state.messages.len().saturating_sub(1)))
                    .unwrap_or(0)
            } else {
                // Nothing selected, select the first message
                0
            }
        };
        let mut state = self.state();
        if let Some(msg_id) = state.messages.get(new_idx).map(|message| message.id) {
            state.selected.clear();
            state.selected.insert(msg_id);
            drop(state);
            // Update checkbox UI
            let checkboxes = self.checkboxes.borrow().clone();
            for (i, checkbox) in checkboxes.iter().enumerate() {
                if let Ok(toggle) = checkbox.cast::<IToggleButton>() {
                    let _ = toggle.SetIsChecked(Some(i == new_idx));
                }
            }
            self.update_bulk_bar();
        }
    }

    /// Toggle selection of the currently focused message (Space).
    fn toggle_current_selection(&self) {
        // Resolve the focused row first and drop the state guard before the
        // mutation below: update_bulk_bar() locks `state` again, which would
        // deadlock if the guard were still held.
        let target = {
            let state = self.state();
            if state.show_outbox || state.messages.is_empty() {
                return;
            }
            // Find the currently focused message (the one with checked checkbox)
            let checkboxes = self.checkboxes.borrow().clone();
            let mut target: Option<(usize, MessageId, bool)> = None;
            for (i, checkbox) in checkboxes.iter().enumerate() {
                if let Ok(toggle) = checkbox.cast::<IToggleButton>()
                    && let Ok(true) = toggle.IsChecked()
                {
                    if i < state.messages.len() {
                        let msg = &state.messages[i];
                        target = Some((i, msg.id, state.selected.contains(&msg.id)));
                    }
                    break;
                }
            }
            target
        };
        let Some((index, id, selected_before)) = target else {
            return;
        };
        {
            let mut state = self.state();
            if selected_before {
                state.selected.remove(&id);
            } else {
                state.selected.insert(id);
            }
        }
        // Release the borrow before touching the control: `SetIsChecked`
        // raises an event that can rebuild the rows.
        let checkbox = {
            let checkboxes = self.checkboxes.borrow();
            checkboxes.get(index).cloned()
        };
        if let Some(checkbox) = checkbox
            && let Ok(toggle) = checkbox.cast::<IToggleButton>()
        {
            let _ = toggle.SetIsChecked(Some(!selected_before));
        }
        self.update_bulk_bar();
    }

    /// Open the currently selected message (Enter).
    fn open_selected_message(self: &Rc<Self>) {
        // Copy the target message out and drop the state guard first:
        // open_message() locks `state` again, which would deadlock here.
        let target = {
            let state = self.state();
            if state.show_outbox {
                None
            } else {
                state
                    .selected
                    .iter()
                    .next()
                    .and_then(|msg_id| state.messages.iter().find(|m| m.id == *msg_id).cloned())
            }
        };
        if let Some(message) = target {
            self.open_message(message);
        }
    }

    // --------------------------------------------------------------- compose

    fn compose_reply(self: &Rc<Self>, all: bool) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let Some(account) = self.app.mailin.account(&account_id).ok().flatten() else {
            return;
        };
        let body = self
            .app
            .mailin
            .message_body(message.id)
            .ok()
            .flatten()
            .and_then(|(text, _)| text)
            .filter(|text| !text.trim().is_empty())
            .or_else(|| message.snippet.clone());
        let draft = if all {
            compose::reply_all(&account, &message, body.as_deref())
        } else {
            compose::reply(&account, &message, body.as_deref())
        };
        self.open_compose(draft);
    }

    fn compose_forward(self: &Rc<Self>) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let Some(account) = self.app.mailin.account(&account_id).ok().flatten() else {
            return;
        };
        let body = self
            .app
            .mailin
            .message_body(message.id)
            .ok()
            .flatten()
            .and_then(|(text, _)| text)
            .or_else(|| message.snippet.clone());
        let draft = compose::forward(&account, &message, body.as_deref());
        self.open_compose(draft);
    }

    /// Draft for a brand-new message: an empty body that already carries the
    /// selected account's signature, so the caret starts above it.
    fn new_draft(&self) -> OutgoingMessage {
        // Copy the account id out of the state first: the store read must not
        // run while the state is locked.
        let account_id = self.state().selection.account;
        let signature = account_id
            .and_then(|account_id| self.app.mailin.account(&account_id).ok().flatten())
            .map(|account| mailin_core::compose::signature_block(&account.signature))
            .unwrap_or_default();
        OutgoingMessage {
            body: signature,
            ..OutgoingMessage::default()
        }
    }

    fn open_compose(self: &Rc<Self>, draft: OutgoingMessage) {
        self.open_compose_checked(draft);
    }

    /// Open the composer and report whether it opened. Callers that move a
    /// message (the outbox "Edit" action) need the answer: the queued entry
    /// must only be cancelled once the draft is on screen.
    fn open_compose_checked(self: &Rc<Self>, draft: OutgoingMessage) -> bool {
        let Some(account_id) = self.state().selection.account else {
            self.show_banner("Add an account before composing mail");
            return false;
        };
        let this = Rc::clone(self);
        match compose::present(Arc::clone(&self.app), account_id, draft, move || {
            // Show a real toast so the user sees confirmation even if the
            // main window is behind other windows.
            this.show_simple_toast("Mailin", "Message queued in the outbox");
        }) {
            Ok(_) => true,
            Err(error) => {
                self.show_error(&format!("Could not open the composer: {error:#}"));
                false
            }
        }
    }

    // M3b-3: the plain reply action (not reply-all) for the Ctrl+R accelerator.
    fn open_reply(self: &Rc<Self>) {
        self.compose_reply(false);
    }

    // Folder management actions (for context menu)
    fn new_folder_action(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let this = Rc::clone(self);
        let xaml_root = match ui::window_xaml_root(&self.widgets.window) {
            Ok(root) => root,
            Err(error) => {
                self.show_error(&format!("Could not get window root: {error:#}"));
                return;
            }
        };
        // Use a simple text input dialog for folder name
        let Some(dialog) = self.require(mailin_winui_sys::ContentDialog::new(), "create dialog")
        else {
            return;
        };
        let Some(title) = self.require(ui::text("New Folder"), "build dialog text") else {
            return;
        };
        let _ = dialog.SetTitle(&title);
        let Some(panel) = self.require(
            ui::stack_sm(mailin_winui_sys::Orientation::Vertical),
            "lay out dialog",
        ) else {
            return;
        };
        ui::margin_lg(&panel).unwrap_or_else(|error| {
            self.show_error(&format!("Could not lay out dialog: {error}"));
        });
        let Some(input) = self.require(
            ui::text_box("Folder name", Some("Folder name")),
            "create text box",
        ) else {
            return;
        };
        ui::append(&panel, &input).unwrap_or_else(|error| {
            self.show_error(&format!("Could not lay out dialog: {error}"));
        });
        let _ = ui::set_content(&dialog, &panel);
        let _ = dialog.SetPrimaryButtonText("Create");
        let _ = dialog.SetCloseButtonText("Cancel");
        let this_clone = this.clone();
        let _ = dialog.PrimaryButtonClick(move |_, _| {
            let name = ui::text_value(&input).trim().to_string();
            if !name.is_empty() {
                this_clone.app.engine.create_folder(
                    account_id,
                    name,
                    "/".to_owned(),
                    FolderRole::Other,
                );
                this_clone.refresh_folders();
            }
        });
        let _ = ui::set_xaml_root(&dialog, &xaml_root);
        let _ = dialog.ShowAsync();
    }

    fn rename_folder_action(self: &Rc<Self>, folder_id_str: &str) {
        let Ok(folder_id) = folder_id_str.parse::<i64>() else {
            return;
        };
        let this = Rc::clone(self);
        let xaml_root = match ui::window_xaml_root(&self.widgets.window) {
            Ok(root) => root,
            Err(error) => {
                self.show_error(&format!("Could not get window root: {error:#}"));
                return;
            }
        };
        // Find current folder name
        let folders = this.state().folders.clone();
        let Some(folder) = folders.iter().find(|f| f.id.0 == folder_id) else {
            return;
        };
        let Some(dialog) = self.require(mailin_winui_sys::ContentDialog::new(), "create dialog")
        else {
            return;
        };
        let Some(title) = self.require(ui::text("Rename Folder"), "build dialog text") else {
            return;
        };
        let _ = dialog.SetTitle(&title);
        let Some(panel) = self.require(
            ui::stack_sm(mailin_winui_sys::Orientation::Vertical),
            "lay out dialog",
        ) else {
            return;
        };
        ui::margin_lg(&panel).unwrap_or_else(|error| {
            self.show_error(&format!("Could not lay out dialog: {error}"));
        });
        let Some(input) = self.require(
            ui::text_box("New folder name", Some("New folder name")),
            "create text box",
        ) else {
            return;
        };
        input.SetText(&folder.name).unwrap_or_else(|error| {
            self.show_error(&format!("Could not fill dialog text box: {error}"));
        });
        ui::append(&panel, &input).unwrap_or_else(|error| {
            self.show_error(&format!("Could not lay out dialog: {error}"));
        });
        let _ = ui::set_content(&dialog, &panel);
        let _ = dialog.SetPrimaryButtonText("Rename");
        let _ = dialog.SetCloseButtonText("Cancel");
        let this_clone = this.clone();
        let _ = dialog.PrimaryButtonClick(move |_, _| {
            let name = ui::text_value(&input).trim().to_string();
            if !name.is_empty() {
                this_clone
                    .app
                    .engine
                    .rename_folder(FolderId(folder_id), name);
                this_clone.refresh_folders();
            }
        });
        let _ = ui::set_xaml_root(&dialog, &xaml_root);
        let _ = dialog.ShowAsync();
    }

    fn delete_folder_action(self: &Rc<Self>, folder_id_str: &str) {
        let Ok(folder_id) = folder_id_str.parse::<i64>() else {
            return;
        };
        let this = Rc::clone(self);
        let xaml_root = match ui::window_xaml_root(&self.widgets.window) {
            Ok(root) => root,
            Err(error) => {
                self.show_error(&format!("Could not get window root: {error:#}"));
                return;
            }
        };
        let Some(dialog) = self.require(mailin_winui_sys::ContentDialog::new(), "create dialog")
        else {
            return;
        };
        let Some(title) = self.require(ui::text("Delete Folder"), "build dialog text") else {
            return;
        };
        let _ = dialog.SetTitle(&title);
        let Some(panel) = self.require(
            ui::stack_sm(mailin_winui_sys::Orientation::Vertical),
            "lay out dialog",
        ) else {
            return;
        };
        ui::margin_lg(&panel).unwrap_or_else(|error| {
            self.show_error(&format!("Could not lay out dialog: {error}"));
        });
        let Some(msg) = self.require(
            ui::text("Are you sure you want to delete this folder? This action cannot be undone."),
            "build dialog text",
        ) else {
            return;
        };
        ui::append(&panel, &msg).unwrap_or_else(|error| {
            self.show_error(&format!("Could not lay out dialog: {error}"));
        });
        let _ = ui::set_content(&dialog, &panel);
        let _ = dialog.SetPrimaryButtonText("Delete");
        let _ = dialog.SetCloseButtonText("Cancel");
        let _ = dialog.SetDefaultButton(mailin_winui_sys::ContentDialogButton::Close);
        let this_clone = this.clone();
        let _ = dialog.PrimaryButtonClick(move |_, _| {
            this_clone.app.engine.delete_folder(FolderId(folder_id));
            this_clone.refresh_folders();
        });
        let _ = ui::set_xaml_root(&dialog, &xaml_root);
        let _ = dialog.ShowAsync();
    }

    fn subscribe_folder_action(self: &Rc<Self>, folder_id_str: &str, subscribe: bool) {
        let Ok(folder_id) = folder_id_str.parse::<i64>() else {
            return;
        };
        let this = Rc::clone(self);
        this.app
            .engine
            .subscribe_folder(FolderId(folder_id), subscribe);
        this.refresh_folders();
    }

    fn folder_properties_action(self: &Rc<Self>, folder_id_str: &str) {
        let Ok(folder_id) = folder_id_str.parse::<i64>() else {
            return;
        };
        let this = Rc::clone(self);
        let xaml_root = match ui::window_xaml_root(&self.widgets.window) {
            Ok(root) => root,
            Err(error) => {
                self.show_error(&format!("Could not get window root: {error:#}"));
                return;
            }
        };
        let folders = this.state().folders.clone();
        let Some(folder) = folders.iter().find(|f| f.id.0 == folder_id) else {
            return;
        };
        let Some(dialog) = self.require(mailin_winui_sys::ContentDialog::new(), "create dialog")
        else {
            return;
        };
        let Some(title) = self.require(ui::text("Folder Properties"), "build dialog text") else {
            return;
        };
        let _ = dialog.SetTitle(&title);
        let Some(panel) = self.require(
            ui::stack_sm(mailin_winui_sys::Orientation::Vertical),
            "lay out dialog",
        ) else {
            return;
        };
        ui::margin_lg(&panel).unwrap_or_else(|error| {
            self.show_error(&format!("Could not lay out dialog: {error}"));
        });
        let info = format!(
            "Name: {}\nRole: {:?}\nSubscribed: {}\nUnread: {}\nTotal messages: {}",
            folder.name, folder.role, folder.subscribed, folder.unread_count, folder.message_count
        );
        let Some(msg) = self.require(ui::text(&info), "build dialog text") else {
            return;
        };
        ui::append(&panel, &msg).unwrap_or_else(|error| {
            self.show_error(&format!("Could not lay out dialog: {error}"));
        });
        let _ = ui::set_content(&dialog, &panel);
        let _ = dialog.SetCloseButtonText("Close");
        let _ = ui::set_xaml_root(&dialog, &xaml_root);
        let _ = dialog.ShowAsync();
    }

    fn add_account(self: &Rc<Self>) {
        let this = Rc::clone(self);
        match account_dialog::present(
            Arc::clone(&self.app),
            account_dialog::AccountDialogMode::Add,
            None,
            move |id| {
                this.refresh_accounts();
                this.select_account(id);
            },
        ) {
            Ok(_) => {}
            Err(error) => self.show_error(&format!("Could not open the account wizard: {error:#}")),
        }
    }

    fn edit_account(self: &Rc<Self>) {
        let this = Rc::clone(self);
        let account = self.state().selection.account;
        let Some(account_id) = account else { return };
        let Some(account) = self
            .state()
            .accounts
            .iter()
            .find(|a| a.id == account_id)
            .cloned()
        else {
            return;
        };
        match account_dialog::present(
            Arc::clone(&self.app),
            account_dialog::AccountDialogMode::Edit,
            Some(account),
            move |id| {
                this.refresh_accounts();
                this.select_account(id);
            },
        ) {
            Ok(_) => {}
            Err(error) => self.show_error(&format!("Could not open the account wizard: {error:#}")),
        }
    }

    /// Remove the selected account in the background.
    ///
    /// The account wizard's *Remove Account* button is today's removal path;
    /// this direct entry point is kept for parity with the GTK account menu.
    #[allow(dead_code)]
    fn remove_account(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let Some(account) = self
            .state()
            .accounts
            .iter()
            .find(|account| account.id == account_id)
            .cloned()
        else {
            return;
        };
        // Deleting the account publishes `AccountsChanged`, which the event
        // pump turns into a pane refresh on the UI thread.
        let mailin = Arc::clone(&self.app.mailin);
        std::thread::spawn(move || {
            if let Err(error) = mailin.remove_account(&account.id) {
                tracing::warn!(%error, "could not remove the account");
            }
        });
    }

    /// Show the keyboard shortcuts help dialog.
    fn show_shortcuts_help(self: &Rc<Self>) {
        // Re-activate the help dialog when it is already open. The dialog is
        // cloned out of the cell first: showing it can close it again, and
        // the close handler needs the cell mutably.
        let existing = self.widgets.shortcuts_window.borrow().clone();
        if let Some(existing) = existing {
            let _ = existing.ShowAsync();
            return;
        }

        let xaml_root = match ui::window_xaml_root(&self.widgets.window) {
            Ok(root) => root,
            Err(error) => {
                tracing::error!(%error, "could not get the window's XAML root");
                return;
            }
        };

        let shortcuts_dialog = match build_shortcuts_window(&xaml_root) {
            Ok(dialog) => dialog,
            Err(error) => {
                tracing::error!(%error, "could not create the shortcuts dialog");
                return;
            }
        };

        *self.widgets.shortcuts_window.borrow_mut() = Some(shortcuts_dialog.clone());

        // Drop the cached reference once the dialog closes so the next F1
        // opens a fresh one.
        let this = Rc::clone(self);
        match shortcuts_dialog.Closed(move |_, _| {
            *this.widgets.shortcuts_window.borrow_mut() = None;
        }) {
            Ok(revoker) => self.events.keep(revoker),
            Err(error) => {
                tracing::warn!(%error, "could not watch the shortcuts dialog for closing")
            }
        }

        let _ = shortcuts_dialog.ShowAsync();
    }

    fn show_preferences(self: &Rc<Self>) {
        let xaml_root = match ui::window_xaml_root(&self.widgets.window) {
            Ok(root) => root,
            Err(error) => {
                tracing::error!(%error, "could not get the window's XAML root");
                return;
            }
        };
        let this = Rc::clone(self);
        if let Err(error) =
            crate::preferences::show_preferences(&self.app, &xaml_root, &self.events, move || {
                this.apply_config()
            })
        {
            tracing::error!(%error, "could not show the preferences dialog");
        }
    }

    // ---------------------------------------------------------------- events

    fn drain_events(self: &Rc<Self>, events: &Mutex<broadcast::Receiver<Event>>) {
        let mut accounts_changed = false;
        let mut folders_changed = false;
        let mut messages_changed = false;
        let mut body_ready = None;
        let mut account_error = None;
        let mut new_messages = Vec::new();
        let mut outbox_changed = false;
        let mut sync_status_update = None;

        {
            let mut events = events.lock().unwrap_or_else(|error| error.into_inner());
            loop {
                match events.try_recv() {
                    Ok(Event::AccountsChanged) => accounts_changed = true,
                    Ok(Event::FoldersChanged { .. }) => folders_changed = true,
                    Ok(Event::MessagesChanged { folder_id }) => {
                        let state = self.state();
                        if state.selection.search || state.selection.folder == Some(folder_id) {
                            messages_changed = true;
                        }
                    }
                    Ok(Event::NewMessages {
                        account_id,
                        count,
                        message_ids,
                        ..
                    }) => new_messages.push((account_id, count, message_ids)),
                    Ok(Event::BodyReady { message_id }) => body_ready = Some(message_id),
                    Ok(Event::OutboxChanged) => outbox_changed = true,
                    Ok(Event::AccountError { message, .. }) => account_error = Some(message),
                    Ok(Event::ScheduledActionRan { .. }) => {
                        // A scheduled action moved messages around: refresh the
                        // folders and the message list.
                        folders_changed = true;
                        messages_changed = true;
                    }
                    Ok(Event::ActionFailed { message, .. }) => {
                        self.show_error(&message);
                    }
                    Ok(Event::SyncStatusChanged { account_id, status }) => {
                        let state = self.state();
                        if state.selection.account == Some(account_id) {
                            sync_status_update = Some(status);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Lagged(_)) => {
                        // A slow UI is allowed to miss events: refresh everything
                        // that is visible instead.
                        accounts_changed = true;
                        folders_changed = true;
                        messages_changed = true;
                    }
                    Err(TryRecvError::Closed) => break,
                }
            }
        }

        if accounts_changed {
            self.refresh_accounts();
        } else if folders_changed {
            self.refresh_folders();
        }
        if messages_changed {
            self.refresh_messages();
        }
        if let Some(message_id) = body_ready {
            self.resolve_pending_attachment(message_id);
            if self.state().selection.message == Some(message_id) {
                self.render_body(message_id);
                self.render_attachments(message_id);
                if let Some(message) = self.selected_message() {
                    self.render_headers(&message);
                    self.update_actions(&message);
                }
            }
        }
        for (account_id, count, message_ids) in new_messages {
            let account = self
                .state()
                .accounts
                .iter()
                .find(|account| account.id == account_id)
                .map(|account| account.email.clone())
                .unwrap_or_else(|| "Mailin".to_owned());
            let text = format!(
                "{} in {account}",
                mailin_core::plural(count, "new message", "new messages")
            );
            self.show_banner(&text);
            self.show_toast_notification(account_id, &text, &message_ids);
        }
        if outbox_changed {
            // Read the flag and drop the guard first: refresh_messages() and
            // refresh_folders() lock `state` again, which would deadlock here.
            let show_outbox = self.state().show_outbox;
            if show_outbox {
                self.refresh_messages();
            } else {
                self.refresh_folders();
            }
        }
        if let Some(message) = account_error {
            self.show_error(&message);
        }
        if let Some(status) = sync_status_update {
            let (text, brush) = match status {
                mailin_core::model::SyncStatus::Idle => {
                    ("Idle", design_tokens::tokens().text_muted_brush().ok())
                }
                mailin_core::model::SyncStatus::Syncing => {
                    ("Syncing…", design_tokens::tokens().accent_brush().ok())
                }
                mailin_core::model::SyncStatus::Connected => {
                    ("Connected", design_tokens::tokens().success_brush().ok())
                }
                mailin_core::model::SyncStatus::Offline => {
                    ("Offline", design_tokens::tokens().warning_brush().ok())
                }
                mailin_core::model::SyncStatus::Error => {
                    ("Error", design_tokens::tokens().error_brush().ok())
                }
            };
            let _ = self.widgets.sync_status.SetText(text);
            if let Some(brush) = brush {
                let _ = self.widgets.sync_status.SetForeground(&brush);
            }
        }
    }

    // -------------------------------------------------------------- actions

    fn sync_now(&self) {
        let Some(account) = self.state().selection.account else {
            return;
        };
        self.show_banner("");
        self.app.engine.sync_account(account);
        self.show_banner("Synchronizing…");
    }
}

// The mailto: draft is captured from the protocol activation at launch and
// consumed when the main window is (or becomes) ready.
thread_local! {
    static MAILTO_DRAFT: std::cell::RefCell<Option<OutgoingMessage>> =
        const { std::cell::RefCell::new(None) };
}

/// Queue a `mailto:` draft captured at process start; the next main window
/// creation opens the composer with it.
pub fn set_mailto_draft(draft: OutgoingMessage) {
    MAILTO_DRAFT.with(|slot| *slot.borrow_mut() = Some(draft));
}

fn take_mailto_draft() -> Option<OutgoingMessage> {
    MAILTO_DRAFT.with(|slot| slot.borrow_mut().take())
}

/// Create and activate the main window.
pub fn create(state: &Arc<AppState>) -> Result<Window> {
    install_control_resources()?;

    let root = Grid::new().context("could not create the root grid")?;
    ui::auto_row(&root)?;
    ui::auto_row(&root)?;
    ui::star_row(&root)?;

    // ----------------------------------------------------------- toolbar
    let toolbar = ui::stack_sm(Orientation::Horizontal)?;
    ui::margin_horizontal(&toolbar, 12.0)?;
    ui::margin_vertical(&toolbar, 8.0)?;
    let title = ui::heading("Mailin", Some(20.0), Some(700), Some(1))?;
    let account_combo = ui::combo(&[], 0, Some("Switch account"))?;
    ui::min_width(&account_combo, 220.0)?;

    // Hamburger menu button (visible on mobile/tablet)
    let hamburger = ui::button("☰", Some("Menu"), Some("Open main menu"))?;
    ui::min_width(&hamburger, 40.0)?;
    ui::set_visible(&hamburger, false)?; // Hidden by default (desktop)

    let add_account = ui::button(
        "Add account",
        Some("Add account"),
        Some("Add a new mail account (Ctrl+Shift+A)"),
    )?;
    let edit_account = ui::button(
        "Edit account",
        Some("Edit account"),
        Some("Edit the selected account"),
    )?;
    let compose = ui::button(
        "Compose",
        Some("Compose"),
        Some("Create a new message (Ctrl+N)"),
    )?;
    let sync = ui::button("Sync", Some("Sync"), Some("Synchronize mail (F5)"))?;
    let sync_status = ui::text("Idle")?;
    ui::margin_left(&sync_status, 8.0)?;
    ui::vertical_alignment_center(&sync_status)?;
    // Accessibility: live region so screen readers announce status changes
    AutomationProperties::SetLiveSetting(
        &sync_status,
        mailin_winui_sys::AutomationLiveSetting::Polite,
    )?;
    let spacer = ui::text("")?;
    ui::stretch_horizontal(&spacer)?;
    let search = ui::text_box("Search mail", Some("Search mail"))?;
    ui::min_width(&search, 220.0)?;
    let search_button = ui::button("Search", Some("Search"), Some("Search messages (Ctrl+F)"))?;

    ui::append(&toolbar, &hamburger)?;
    ui::append(&toolbar, &title)?;
    ui::append(&toolbar, &account_combo)?;
    ui::append(&toolbar, &add_account)?;
    ui::append(&toolbar, &edit_account)?;
    ui::append(&toolbar, &compose)?;
    ui::append(&toolbar, &sync)?;
    ui::append(&toolbar, &sync_status)?;
    ui::append(&toolbar, &spacer)?;
    ui::append(&toolbar, &search)?;
    ui::append(&toolbar, &search_button)?;
    ui::set_row(&toolbar, 0)?;
    ui::append(&root, &toolbar)?;

    let banner = ui::text("")?;
    ui::margin_horizontal(&banner, 12.0)?;
    ui::margin_vertical(&banner, 2.0)?;
    // Live region so screen readers announce status changes (Archiving…, errors, etc.)
    AutomationProperties::SetLiveSetting(&banner, mailin_winui_sys::AutomationLiveSetting::Polite)?;
    ui::set_visible(&banner, false)?;
    ui::set_row(&banner, 1)?;
    ui::append(&root, &banner)?;

    // Undo banner for move operations (initially hidden)
    let undo_banner = ui::stack_sm(Orientation::Horizontal)?;
    ui::margin_horizontal(&undo_banner, 12.0)?;
    ui::margin_vertical(&undo_banner, 8.0)?;
    ui::set_visible(&undo_banner, false)?;
    let undo_text = ui::text("Message moved")?;
    ui::stretch_horizontal(&undo_text)?;
    ui::append(&undo_banner, &undo_text)?;
    let undo_button = ui::button(
        "Undo",
        Some("Undo move"),
        Some("Undo the last move operation"),
    )?;
    ui::append(&undo_banner, &undo_button)?;
    ui::set_row(&undo_banner, 2)?; // Will be shown in row 2, pushing content down
    ui::append(&root, &undo_banner)?;

    // ------------------------------------------------------------- panes
    let host = Grid::new().context("could not create the content host")?;
    ui::star_row(&host)?;
    ui::set_row(&host, 3)?;
    ui::append(&root, &host)?;

    let welcome = ui::stack_md(Orientation::Vertical)?;
    ui::margin_xl(&welcome)?;
    ui::margin_vertical(&welcome, 40.0)?;
    let welcome_title = ui::heading("Welcome to Mailin", Some(28.0), Some(700), Some(1))?;
    let welcome_body = ui::text("Add a mail account to read and send mail.")?;
    let welcome_add = ui::button(
        "Add Account…",
        Some("Add Account"),
        Some("Add a new mail account"),
    )?;
    ui::append(&welcome, &welcome_title)?;
    ui::append(&welcome, &welcome_body)?;
    ui::append(&welcome, &welcome_add)?;
    ui::append(&host, &welcome)?;

    let mail = Grid::new().context("could not create the mail layout")?;
    ui::star_row(&mail)?;
    // Responsive columns: sidebar (min 200, max 280), list (min 300), reader (star)
    ui::star_column_min(&mail, MIN_SIDEBAR_WIDTH, 280.0)?;
    ui::star_column_min(&mail, MIN_LIST_WIDTH, 0.0)?; // 0 = no max
    ui::star_column(&mail)?;
    ui::set_visible(&mail, false)?;
    ui::append(&host, &mail)?;

    // ----------------------------------------------------------- folders
    let sidebar = ui::stack(Orientation::Vertical, 6.0)?;
    ui::margin(&sidebar, 10.0, 8.0, 6.0, 8.0)?;
    let folder_heading = ui::subheading("Folders")?;
    ui::append(&sidebar, &folder_heading)?;
    let folder_empty = ui::text("Folders appear after the first synchronization.")?;
    ui::set_visible(&folder_empty, true)?;
    ui::append(&sidebar, &folder_empty)?;
    let folder_list = ui::stack_xs(Orientation::Vertical)?;
    let folder_scroller = ui::scroll(&folder_list)?;
    ui::append(&sidebar, &folder_scroller)?;
    ui::set_row(&sidebar, 0)?;
    ui::set_column(&sidebar, 0)?;
    ui::append(&mail, &sidebar)?;

    // ------------------------------------------------------ message list
    let list_pane = Grid::new().context("could not create the message pane")?;
    ui::auto_row(&list_pane)?;
    ui::auto_row(&list_pane)?; // bulk bar; collapses while nothing is ticked
    ui::star_row(&list_pane)?;
    ui::auto_row(&list_pane)?;
    ui::margin_md(&list_pane)?;
    let list_title = ui::subheading("Messages")?;
    ui::margin(&list_title, 6.0, 0.0, 6.0, 4.0)?;
    ui::set_row(&list_title, 0)?;
    ui::append(&list_pane, &list_title)?;
    // The bulk bar sits above the list and stays hidden until a row is
    // ticked. One dropdown keeps it a single row tall even in a narrow pane.
    let bulk_bar = ui::stack_sm(Orientation::Horizontal)?;
    ui::margin(&bulk_bar, 6.0, 2.0, 6.0, 4.0)?;
    let bulk_count = ui::text("0 selected")?;
    ui::append(&bulk_bar, &bulk_count)?;
    let bulk_spacer = ui::text("")?;
    ui::stretch_horizontal(&bulk_spacer)?;
    ui::append(&bulk_bar, &bulk_spacer)?;
    let bulk_actions = ui::combo(
        &[
            "Choose an action…",
            "Mark as read",
            "Mark as unread",
            "Star",
            "Unstar",
            "Move to folder…",
            "Archive",
            "Move to Trash",
            "Clear selection",
        ],
        0,
        Some("Bulk actions"),
    )?;
    ui::append(&bulk_bar, &bulk_actions)?;
    ui::set_visible(&bulk_bar, false)?;
    ui::set_row(&bulk_bar, 1)?;
    ui::append(&list_pane, &bulk_bar)?;
    let message_list = ui::stack_xs(Orientation::Vertical)?;
    let message_scroll = ui::scroll(&message_list)?;
    ui::set_row(&message_scroll, 2)?;
    ui::append(&list_pane, &message_scroll)?;
    let message_empty = ui::text("This folder is empty, or has not been synchronized yet.")?;
    ui::margin(&message_empty, 6.0, 12.0, 6.0, 6.0)?;
    ui::set_row(&message_empty, 2)?;
    ui::append(&list_pane, &message_empty)?;
    let load_more = ui::button("Load more", Some("Load more"), Some("Load more messages"))?;
    ui::set_visible(&load_more, false)?;
    ui::set_row(&load_more, 3)?;
    ui::append(&list_pane, &load_more)?;
    ui::set_column(&list_pane, 1)?;
    ui::append(&mail, &list_pane)?;

    // ------------------------------------------------------------ reader
    let reader_host = Grid::new().context("could not create the reader host")?;
    ui::star_row(&reader_host)?;
    ui::set_column(&reader_host, 2)?;
    ui::append(&mail, &reader_host)?;

    let reader_empty = ui::stack_sm(Orientation::Vertical)?;
    ui::margin_lg(&reader_empty)?;
    ui::margin_vertical(&reader_empty, 16.0)?;
    ui::append(
        &reader_empty,
        &ui::heading("Select a message", Some(22.0), Some(600), Some(2))?,
    )?;
    ui::append(
        &reader_empty,
        &ui::text("Pick a message from the list to read it here.")?,
    )?;
    ui::append(&reader_host, &reader_empty)?;

    let reader_content = Grid::new().context("could not create the reader")?;
    ui::auto_row(&reader_content)?;
    ui::star_row(&reader_content)?;
    ui::margin(&reader_content, 16.0, 8.0, 16.0, 8.0)?;
    ui::set_visible(&reader_content, false)?;
    ui::append(&reader_host, &reader_content)?;

    let header = ui::stack(Orientation::Vertical, 6.0)?;
    let actions = ui::stack(Orientation::Horizontal, 6.0)?;
    let html_button = ui::button(
        "Show HTML",
        Some("Show HTML"),
        Some("Show the original HTML body"),
    )?;
    let reply_button = ui::button("Reply", Some("Reply"), Some("Reply to sender (Ctrl+R)"))?;
    let reply_all_button = ui::button(
        "Reply all",
        Some("Reply all"),
        Some("Reply to all recipients (Ctrl+Shift+R)"),
    )?;
    let forward_button = ui::button("Forward", Some("Forward"), Some("Forward message"))?;
    let actions_spacer = ui::text("")?;
    ui::stretch_horizontal(&actions_spacer)?;
    let flag_button = ui::button("Star", Some("Star"), Some("Toggle star/flag"))?;
    let read_button = ui::button("Mark read", Some("Mark read"), Some("Toggle read status"))?;
    let archive_button = ui::button(
        "Archive",
        Some("Archive"),
        Some("Archive message (Ctrl+Shift+E)"),
    )?;
    let trash_button = ui::button("Trash", Some("Trash"), Some("Move to trash (Delete)"))?;
    let junk_button = ui::button("Junk", Some("Junk"), Some("Mark as junk (Ctrl+Shift+J)"))?;
    let outbox_retry_button = ui::button("Retry", Some("Retry"), Some("Retry sending"))?;
    let outbox_discard_button = ui::button("Discard", Some("Discard"), Some("Discard message"))?;
    let outbox_edit_button = ui::button("Edit", Some("Edit"), Some("Edit message"))?;
    ui::set_visible(&outbox_retry_button, false)?;
    ui::set_visible(&outbox_discard_button, false)?;
    ui::set_visible(&outbox_edit_button, false)?;
    ui::append(&actions, &html_button)?;
    ui::append(&actions, &reply_button)?;
    ui::append(&actions, &reply_all_button)?;
    ui::append(&actions, &forward_button)?;
    ui::append(&actions, &archive_button)?;
    ui::append(&actions, &junk_button)?;
    ui::append(&actions, &trash_button)?;
    ui::append(&actions, &outbox_retry_button)?;
    ui::append(&actions, &outbox_discard_button)?;
    ui::append(&actions, &outbox_edit_button)?;
    ui::append(&actions, &actions_spacer)?;
    ui::append(&actions, &flag_button)?;
    ui::append(&actions, &read_button)?;
    ui::append(&header, &actions)?;

    let reader_subject = ui::heading("", Some(22.0), Some(700), Some(2))?;
    let reader_from = ui::heading("", Some(14.0), Some(600), Some(3))?;
    let reader_to = ui::text("")?;
    let reader_cc = ui::text("")?;
    let reader_date = ui::text("")?;
    ui::append(&header, &reader_subject)?;
    ui::append(&header, &reader_from)?;
    ui::append(&header, &reader_to)?;
    ui::append(&header, &reader_cc)?;
    ui::append(&header, &reader_date)?;

    let remote_panel = ui::stack(Orientation::Horizontal, 8.0)?;
    let remote_text = ui::text("Remote content is blocked to protect your privacy.")?;
    let remote_once = ui::button(
        "Load once",
        Some("Load once"),
        Some("Load remote content for this message only"),
    )?;
    let remote_trust = ui::button(
        "Always allow sender",
        Some("Always allow sender"),
        Some("Always load remote content from this sender"),
    )?;
    let remote_dismiss = ui::button("×", Some("Dismiss"), Some("Not now"))?;
    ui::padding(&remote_dismiss, 8.0, 2.0, 8.0, 2.0)?;
    ui::append(&remote_panel, &remote_text)?;
    ui::append(&remote_panel, &remote_once)?;
    ui::append(&remote_panel, &remote_trust)?;
    ui::append(&remote_panel, &remote_dismiss)?;
    ui::set_visible(&remote_panel, false)?;
    ui::append(&header, &remote_panel)?;

    // PGP: the body is replaced with a Decrypt/Verify action until the user
    // asks for the content to be unwrapped.
    let pgp_panel = ui::stack(Orientation::Vertical, 8.0)?;
    let pgp_status = ui::text("")?;
    let pgp_action = ui::button("Decrypt", Some("Decrypt"), Some("Decrypt this message"))?;
    let pgp_controls = ui::stack(Orientation::Horizontal, 8.0)?;
    ui::append(&pgp_controls, &pgp_action)?;
    ui::append(&pgp_panel, &pgp_status)?;
    ui::append(&pgp_panel, &pgp_controls)?;
    ui::set_visible(&pgp_panel, false)?;
    ui::append(&header, &pgp_panel)?;

    let attachments_panel = ui::stack(Orientation::Vertical, 4.0)?;
    ui::set_visible(&attachments_panel, false)?;
    ui::append(&header, &attachments_panel)?;
    ui::set_row(&header, 0)?;
    ui::append(&reader_content, &header)?;

    let body = Grid::new().context("could not create the reader body")?;
    // Row 0 holds the "inline images could not be loaded" notice, row 1 the
    // body itself.
    ui::auto_row(&body)?;
    ui::star_row(&body)?;
    let body_notice = ui::text("")?;
    ui::margin(&body_notice, 8.0, 4.0, 8.0, 4.0)?;
    ui::set_visible(&body_notice, false)?;
    ui::set_row(&body_notice, 0)?;
    ui::append(&body, &body_notice)?;
    let body_text = ui::text("")?;
    body_text.SetIsTextSelectionEnabled(true)?;
    ui::margin(&body_text, 4.0, 8.0, 4.0, 8.0)?;
    let body_plain = ui::scroll(&body_text)?;
    let body_html = WebView2::new().context("could not create the WebView2 control")?;
    ui::set_visible(&body_plain, false)?;
    ui::set_visible(&body_html, false)?;
    ui::set_row(&body_plain, 1)?;
    ui::set_row(&body_html, 1)?;
    ui::append(&body, &body_plain)?;
    ui::append(&body, &body_html)?;
    ui::set_row(&body, 1)?;
    ui::append(&reader_content, &body)?;

    // ------------------------------------------------------------- window
    let window = Window::new().context("could not create the WinUI window")?;
    window.SetTitle("Mailin")?;
    window.SetContent(&root)?;

    // Responsive layout handler.
    //
    // On narrow windows the folder list collapses into the hamburger menu.
    // The size handler and the hamburger button share the state, so they
    // always agree on whether the list is open.
    let sidebar_for_layout = sidebar.clone();
    let hamburger_btn = hamburger.clone();
    let sidebar_open = Rc::new(Cell::new(true));
    let was_mobile = Rc::new(Cell::new(false));
    let sidebar_open_for_size = sidebar_open.clone();
    let was_mobile_for_size = was_mobile.clone();

    let size_changed_token = window.SizeChanged(move |_sender, args| {
        // `NewSize` lives on `ISizeChangedEventArgs`; the window's own event
        // args type only carries the (empty) window-specific interface.
        let width = args
            .as_ref()
            .and_then(|args| {
                args.cast::<mailin_winui_sys::ISizeChangedEventArgs>()
                    .ok()
                    .and_then(|args| args.NewSize().ok())
            })
            .unwrap_or_default()
            .width as f64;
        let is_mobile = width < BREAKPOINT_MOBILE;
        let is_tablet = (BREAKPOINT_MOBILE..BREAKPOINT_TABLET).contains(&width);

        // Show/hide hamburger menu
        let _ = ui::set_visible(&hamburger_btn, is_mobile || is_tablet);

        // On mobile the folder list is hidden until the hamburger opens it.
        // On tablet and desktop the grid's star columns size themselves, so
        // the list is always shown.
        if is_mobile {
            if !was_mobile_for_size.replace(true) {
                // The window just became narrow: collapse the folder list.
                sidebar_open_for_size.set(false);
                let _ = ui::set_visible(&sidebar_for_layout, false);
            }
        } else {
            was_mobile_for_size.set(false);
            sidebar_open_for_size.set(true);
            let _ = ui::set_visible(&sidebar_for_layout, true);
        }
    });

    // Store the token to keep the handler alive
    let _ = size_changed_token;

    // Hamburger click handler for mobile sidebar toggle
    let hamburger_btn_click = hamburger.clone();
    let sidebar_for_hamburger = sidebar.clone();
    let sidebar_open_for_hamburger = sidebar_open.clone();
    let _hamburger_token = ui::on_click(&hamburger_btn_click, move || {
        let open = !sidebar_open_for_hamburger.get();
        sidebar_open_for_hamburger.set(open);
        let _ = ui::set_visible(&sidebar_for_hamburger, open);
    })?;

    // Open links in the system browser/mail client instead of navigating
    // the reader WebView. Anything that is not About/HTTP(S)/mailto is
    // cancelled outright.
    let link_token = body_html.NavigationStarting(move |_, args| {
        let Some(args) = args.as_ref() else {
            return;
        };
        if let Ok(uri) = args.Uri() {
            let lower = uri.to_ascii_lowercase();
            if lower.starts_with("about:") {
                // Initial document from NavigateToString — leave alone.
            } else if lower.starts_with("http://")
                || lower.starts_with("https://")
                || lower.starts_with("mailto:")
            {
                let _ = args.SetCancel(true);
                crate::platform::open_url(&uri);
            } else {
                let _ = args.SetCancel(true);
            }
        }
    })?;

    let this = Rc::new(MainWindow {
        app: Arc::clone(state),
        state: Mutex::new(State {
            selection: Selection {
                limit: state.config().ui.page_size.clamp(1, MAX_PAGE),
                ..Selection::default()
            },
            ..State::default()
        }),
        programmatic: AtomicBool::new(false),
        html_mode: AtomicBool::new(state.config().ui.html_enabled),
        pgp_shown: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        last_move: Mutex::new(None),
        undo_timer: std::rc::Rc::new(std::cell::RefCell::new(None)),
        events: ui::Revokers::default(),
        folder_events: ui::Revokers::default(),
        message_events: ui::Revokers::default(),
        attachment_events: ui::Revokers::default(),
        dialogs: std::cell::RefCell::new(Vec::new()),
        dialog_events: ui::Revokers::default(),
        checkboxes: std::cell::RefCell::new(Vec::new()),
        checkboxes_by_id: std::cell::RefCell::new(HashMap::new()),
        widgets: Widgets {
            window: window.clone(),
            title,
            banner,
            account_combo,
            add_account,
            edit_account,
            compose,
            sync,
            search,
            search_button,
            welcome,
            welcome_add,
            mail,
            folder_list,
            folder_empty,
            list_title,
            bulk_bar,
            bulk_count,
            bulk_actions,
            message_scroll,
            message_list,
            message_empty,
            load_more,
            reader_empty,
            reader_content,
            reader_subject,
            reader_from,
            reader_to,
            reader_cc,
            reader_date,
            remote_panel,
            remote_once,
            remote_trust,
            remote_dismiss,
            pgp_panel,
            pgp_status,
            pgp_action,
            attachments_panel,
            body_plain,
            body_text,
            body_html,
            body_notice,
            html_button,
            reply_button,
            reply_all_button,
            forward_button,
            flag_button,
            read_button,
            archive_button,
            trash_button,
            junk_button,
            outbox_retry_button,
            outbox_discard_button,
            outbox_edit_button,
            sync_status,
            undo_banner,
            undo_button,
            shortcuts_window: std::cell::RefCell::new(None),
        },
    });

    this.events.keep(link_token);

    // Undo banner button: restore whatever the last undoable action moved.
    this.events.keep(ui::on_click(&this.widgets.undo_button, {
        let this = Rc::downgrade(&this);
        move || {
            if let Some(this) = this.upgrade() {
                this.undo_last_move();
            }
        }
    })?);

    // Keyboard accelerators, attached to the content root so they fire
    // wherever the focus is inside the window.
    this.events.keep(add_accelerator(
        &root,
        VirtualKey::F,
        Some(VirtualKeyModifiers::Control),
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    // Focus-first, like Windows: Enter in the search box runs
                    // the search, the button triggers it too.
                    if let Ok(search) = this.widgets.search.cast::<IUIElement>() {
                        let _ = search.Focus(mailin_winui_sys::FocusState::Programmatic);
                    }
                }
            }
        },
    )?);
    this.events
        .keep(add_accelerator(&root, VirtualKey::F5, None, {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.sync_now();
                }
            }
        })?);
    // M3b-3: register Ctrl+N (new message) and Ctrl+R (reply) so they
    // match the advertised tooltips and the help table.
    this.events.keep(add_accelerator(
        &root,
        VirtualKey::N,
        Some(VirtualKeyModifiers::Control),
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.open_compose(this.new_draft());
                }
            }
        },
    )?);
    this.events.keep(add_accelerator(
        &root,
        VirtualKey::R,
        Some(VirtualKeyModifiers::Control),
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.open_reply();
                }
            }
        },
    )?);
    this.events.keep(add_accelerator(
        &root,
        VirtualKey::A,
        Some(VirtualKeyModifiers::Control | VirtualKeyModifiers::Shift),
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.add_account();
                }
            }
        },
    )?);
    // M3b-3: register Ctrl+Shift+R (reply-all) and Ctrl+Shift+F (forward).
    this.events.keep(add_accelerator(
        &root,
        VirtualKey::R,
        Some(VirtualKeyModifiers::Control | VirtualKeyModifiers::Shift),
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.compose_reply(true);
                }
            }
        },
    )?);
    this.events.keep(add_accelerator(
        &root,
        VirtualKey::F,
        Some(VirtualKeyModifiers::Control | VirtualKeyModifiers::Shift),
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.compose_forward();
                }
            }
        },
    )?);
    this.events
        .keep(add_accelerator(&root, VirtualKey::F1, None, {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.show_shortcuts_help();
                }
            }
        })?);
    // Bulk-selection shortcuts live on the message list itself rather than
    // the window: scoping them to the pane keeps Ctrl+A working as
    // "select all text" inside the search box and keeps Escape available to
    // dialogs and other panes.
    this.events.keep(add_accelerator(
        &this.widgets.message_scroll,
        VirtualKey::A,
        Some(VirtualKeyModifiers::Control),
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.select_all();
                }
            }
        },
    )?);
    this.events.keep(add_accelerator(
        &this.widgets.message_scroll,
        VirtualKey::Escape,
        None,
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.clear_bulk_selection();
                }
            }
        },
    )?);
    // This projection exposes no OEM punctuation keys (no `OemComma`/
    // `OemQuestion`), so preferences uses a letter chord instead of the
    // Windows-native `Ctrl+,`.
    this.events.keep(add_accelerator(
        &root,
        VirtualKey::P,
        Some(VirtualKeyModifiers::Control | VirtualKeyModifiers::Shift),
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.show_preferences();
                }
            }
        },
    )?);

    connect(&this)?;
    this.refresh_accounts();

    // A mailto: draft captured at startup opens the composer once the main
    // window exists. (Protocol activation between launches is not wired yet:
    // that needs AppInstance rather than command-line arguments.)
    if let Some(draft) = take_mailto_draft() {
        match state
            .mailin
            .list_accounts()
            .ok()
            .and_then(|accounts| accounts.into_iter().next().map(|account| account.id))
        {
            Some(account_id) => {
                let _ = crate::compose::present(Arc::clone(state), account_id, draft, move || {});
            }
            None => tracing::warn!("mailto: draft dropped: no account is configured yet"),
        }
    }

    let _window = ui::keep_alive(&window);
    window.Activate().context("could not show the window")?;
    tracing::info!("WinUI window activated");
    Ok(window)
}

/// Register a keyboard accelerator on an element of the window.
///
/// Attach it to the window's content root for app-wide shortcuts, or to a
/// pane to keep a shortcut scoped to that pane's focus. The returned revoker
/// must be kept alive for as long as the shortcut should work; callers store
/// it alongside their other window event revokers.
fn add_accelerator(
    element: &impl Interface,
    key: VirtualKey,
    modifiers: Option<VirtualKeyModifiers>,
    handler: impl Fn() + 'static,
) -> Result<EventRevoker> {
    let accelerator =
        KeyboardAccelerator::new().context("could not create a keyboard accelerator")?;
    accelerator.SetKey(key)?;
    if let Some(modifiers) = modifiers {
        accelerator.SetModifiers(modifiers)?;
    }
    let revoker = accelerator.Invoked(move |_, _| handler())?;
    ui::add_keyboard_accelerator(element, &accelerator)?;
    Ok(revoker)
}

/// Wire events, actions and the core event pump.
fn connect(this: &Rc<MainWindow>) -> Result<()> {
    let closed_revoker = this.widgets.window.Closed({
        let closed = Rc::clone(this);
        move |_, _| closed.closed.store(true, Ordering::Relaxed)
    })?;
    this.events.keep(closed_revoker);

    this.events
        .keep(ui::on_selection_changed(&this.widgets.account_combo, {
            let this = Rc::clone(this);
            move || {
                if this.programmatic.load(Ordering::Relaxed) {
                    return;
                }
                let index = ui::selected_index(&this.widgets.account_combo);
                let account = this.state().accounts.get(index).map(|account| account.id);
                if let Some(account) = account {
                    this.select_account(account);
                }
            }
        })?);

    this.events.keep(ui::on_click(&this.widgets.add_account, {
        let this = Rc::clone(this);
        move || this.add_account()
    })?);
    this.events.keep(ui::on_click(&this.widgets.edit_account, {
        let this = Rc::clone(this);
        move || this.edit_account()
    })?);
    this.events.keep(ui::on_click(&this.widgets.welcome_add, {
        let this = Rc::clone(this);
        move || this.add_account()
    })?);
    this.events.keep(ui::on_click(&this.widgets.compose, {
        let this = Rc::clone(this);
        move || this.open_compose(this.new_draft())
    })?);
    this.events.keep(ui::on_click(&this.widgets.sync, {
        let this = Rc::clone(this);
        move || this.sync_now()
    })?);
    this.events.keep(ui::on_click(&this.widgets.search_button, {
        let this = Rc::clone(this);
        move || this.run_search()
    })?);
    this.events
        .keep(ui::on_selection_changed(&this.widgets.bulk_actions, {
            let this = Rc::clone(this);
            move || this.run_bulk_action()
        })?);
    this.events.keep(ui::on_text_changed(&this.widgets.search, {
        let this = Rc::clone(this);
        move || {
            if ui::text_value(&this.widgets.search).trim().is_empty() {
                this.clear_search();
            }
        }
    })?);
    // Enter in the search box runs the search.
    this.events
        .keep(this.widgets.search.cast::<IUIElement>()?.KeyDown({
            let this = Rc::downgrade(this);
            move |_, args| {
                let Some(args) = args.as_ref() else {
                    return;
                };
                if let Ok(key) = args.Key()
                    && key == mailin_winui_sys::VirtualKey::Enter
                {
                    if let Some(this) = this.upgrade() {
                        this.run_search();
                    }
                    let _ = args.SetHandled(true);
                }
            }
        })?);
    this.events.keep(ui::on_click(&this.widgets.load_more, {
        let this = Rc::clone(this);
        move || this.request_more()
    })?);
    this.events.keep(ui::on_click(&this.widgets.read_button, {
        let this = Rc::clone(this);
        move || this.toggle_seen()
    })?);
    this.events.keep(ui::on_click(&this.widgets.flag_button, {
        let this = Rc::clone(this);
        move || this.toggle_flagged()
    })?);
    this.events
        .keep(ui::on_click(&this.widgets.archive_button, {
            let this = Rc::clone(this);
            move || this.archive_selected()
        })?);
    this.events.keep(ui::on_click(&this.widgets.trash_button, {
        let this = Rc::clone(this);
        move || this.delete_selected()
    })?);
    this.events.keep(ui::on_click(&this.widgets.junk_button, {
        let this = Rc::clone(this);
        move || this.mark_selected_junk(true)
    })?);
    this.events
        .keep(ui::on_click(&this.widgets.outbox_retry_button, {
            let this = Rc::clone(this);
            move || this.outbox_retry_selected()
        })?);
    this.events
        .keep(ui::on_click(&this.widgets.outbox_discard_button, {
            let this = Rc::clone(this);
            move || this.outbox_discard_selected()
        })?);
    this.events
        .keep(ui::on_click(&this.widgets.outbox_edit_button, {
            let this = Rc::clone(this);
            move || this.outbox_edit_selected()
        })?);
    this.events.keep(ui::on_click(&this.widgets.html_button, {
        let this = Rc::clone(this);
        move || {
            let mode = !this.html_mode.load(Ordering::Relaxed);
            this.html_mode.store(mode, Ordering::Relaxed);
            // Copy the id out of the state: the guard must not be held
            // while `render_body` locks the state again.
            let message_id = this.state().selection.message;
            if let Some(message_id) = message_id {
                this.render_body(message_id);
            }
        }
    })?);
    this.events.keep(ui::on_click(&this.widgets.pgp_action, {
        let this = Rc::clone(this);
        move || {
            // Copy the id out of the state: the guard must not be held
            // while the body is decrypted.
            let message_id = this.state().selection.message;
            if let Some(message_id) = message_id {
                this.decrypt_pgp_body(message_id);
            }
        }
    })?);
    this.events.keep(ui::on_click(&this.widgets.remote_once, {
        let this = Rc::clone(this);
        move || {
            let Some(message_id) = this.state().selection.message else {
                return;
            };
            this.state().remote_allowed = Some(message_id);
            let _ = ui::set_visible(&this.widgets.remote_panel, false);
            this.render_body(message_id);
        }
    })?);
    this.events.keep(ui::on_click(&this.widgets.remote_trust, {
        let this = Rc::clone(this);
        move || this.trust_current_sender()
    })?);
    this.events
        .keep(ui::on_click(&this.widgets.remote_dismiss, {
            let this = Rc::clone(this);
            move || {
                let _ = ui::set_visible(&this.widgets.remote_panel, false);
            }
        })?);
    this.events.keep(ui::on_click(&this.widgets.reply_button, {
        let this = Rc::clone(this);
        move || this.compose_reply(false)
    })?);
    this.events
        .keep(ui::on_click(&this.widgets.reply_all_button, {
            let this = Rc::clone(this);
            move || this.compose_reply(true)
        })?);
    this.events
        .keep(ui::on_click(&this.widgets.forward_button, {
            let this = Rc::clone(this);
            move || this.compose_forward()
        })?);

    // Event pump. Polling the broadcast channel on the UI thread keeps every
    // store read where the widgets are.
    let events = Arc::new(Mutex::new(this.app.mailin.subscribe()));
    let queue =
        DispatcherQueue::GetForCurrentThread().context("the UI thread has no dispatcher queue")?;
    let timer = queue
        .CreateTimer()
        .context("could not create the event timer")?;
    timer.SetInterval(windows_time::TimeSpan {
        duration: EVENT_POLL_MILLIS * 10_000,
    })?;
    timer.SetIsRepeating(true)?;
    let timer_for_tick = timer.clone();
    let this_for_tick = Rc::clone(this);
    let tick_revoker = timer.Tick(move |_, _| {
        if this_for_tick.closed.load(Ordering::Relaxed) {
            let _ = timer_for_tick.Stop();
            return;
        }
        this_for_tick.drain_events(&events);
    })?;
    this.events.keep(tick_revoker);
    timer.Start().context("could not start the event timer")?;

    // Keyboard navigation for message list: Up/Down to navigate, Space to toggle
    // selection, Enter to open message.
    let message_scroll = this.widgets.message_scroll.clone();
    let this_weak = Rc::downgrade(this);
    let key_down_revoker = message_scroll
        .cast::<IUIElement>()?
        .KeyDown(move |_, args| {
            let Some(args) = args.as_ref() else {
                return;
            };
            if let Some(this) = this_weak.upgrade()
                && let Ok(key) = args.Key()
            {
                let handled = match key {
                    VirtualKey::Up => {
                        this.select_previous_message();
                        true
                    }
                    VirtualKey::Down => {
                        this.select_next_message();
                        true
                    }
                    VirtualKey::Space => {
                        this.toggle_current_selection();
                        true
                    }
                    VirtualKey::Enter => {
                        this.open_selected_message();
                        true
                    }
                    _ => false,
                };
                if handled {
                    let _ = args.SetHandled(true);
                }
            }
        })?;
    this.events.keep(key_down_revoker);

    Ok(())
}

/// Add the WinUI default styles. Without `XamlControlsResources` the templated
/// controls (including WebView2) have no style and render as empty rectangles.
fn install_control_resources() -> Result<()> {
    let application = Application::Current().context("no current XAML application")?;
    let resources =
        ResourceDictionary::new().context("could not create the resource dictionary")?;
    let controls: ResourceDictionary = ui::load(
        "<XamlControlsResources xmlns=\"http://schemas.microsoft.com/winfx/2006/xaml/presentation\" />",
    )
    .context("could not create XamlControlsResources; is the Windows App SDK runtime installed?")?;
    resources
        .MergedDictionaries()
        .context("could not read the merged dictionaries")?
        .Append(&controls)
        .context("could not add XamlControlsResources")?;
    application
        .SetResources(&resources)
        .context("could not install the application resources")
}

// ---------------------------------------------------------------- widgets

fn signature_label(status: mailin_core::SignatureStatus) -> &'static str {
    use mailin_core::SignatureStatus;
    match status {
        SignatureStatus::Valid => "valid",
        SignatureStatus::Invalid => "invalid",
        SignatureStatus::UnknownKey => "unknown key",
        SignatureStatus::Error => "error",
    }
}

fn role_marker(role: FolderRole) -> &'static str {
    match role {
        FolderRole::Inbox => "●",
        FolderRole::Sent => "→",
        FolderRole::Drafts => "✎",
        FolderRole::Trash => "🗑",
        FolderRole::Junk => "!",
        FolderRole::Archive => "▣",
        FolderRole::Snoozed => "⏰",
        FolderRole::Other => "•",
    }
}

pub(crate) fn folder_title(folder: &Folder) -> String {
    let leaf = if folder.delimiter.is_empty() {
        folder.name.as_str()
    } else {
        folder
            .name
            .rsplit(folder.delimiter.as_str())
            .next()
            .unwrap_or(folder.name.as_str())
    };
    if leaf.is_empty() {
        folder.name.clone()
    } else {
        leaf.to_owned()
    }
}

fn folder_row(folder: &Folder, this: &Rc<MainWindow>) -> Result<Button> {
    let row = ui::stack(Orientation::Horizontal, 8.0)?;
    ui::padding(&row, 6.0, 6.0, 6.0, 6.0)?;
    let marker = role_marker(folder.role);
    ui::append(&row, &ui::text(marker)?)?;
    let name = ui::text(&folder_title(folder))?;
    ui::stretch_horizontal(&name)?;
    ui::append(&row, &name)?;
    if folder.unread_count > 0 {
        ui::append(&row, &ui::text(&folder.unread_count.to_string())?)?;
    }
    let button = ui::content_button(
        &row,
        Some(&format!(
            "{}, {}",
            folder_title(folder),
            mailin_core::plural(folder.unread_count, "unread message", "unread messages")
        )),
        None,
    )?;
    ui::padding(&button, 0.0, 0.0, 0.0, 0.0)?;

    // Right-click context menu for folder operations
    let flyout = MenuFlyout::new()?;
    let folder_id = folder.id.0.to_string();

    // Every action row needs both a `MenuFlyoutItemBase` for `Items()` and the
    // concrete item to wire `Click`, so keep the pair around.
    let item_base = |item: MenuFlyoutItem| -> Result<mailin_winui_sys::MenuFlyoutItemBase> {
        item.cast().context("could not cast a menu item")
    };

    // New Folder
    let new_folder_item = MenuFlyoutItem::new()?;
    new_folder_item.SetText("New Folder")?;
    let this_clone = this.clone();
    let new_folder_revoker = new_folder_item.Click(move |_, _| {
        this_clone.new_folder_action();
    })?;
    this.folder_events.keep(new_folder_revoker);
    flyout.Items()?.Append(&item_base(new_folder_item)?)?;

    // Rename Folder
    let rename_item = MenuFlyoutItem::new()?;
    rename_item.SetText("Rename Folder")?;
    let this_clone = this.clone();
    let folder_id_clone = folder_id.clone();
    let rename_revoker = rename_item.Click(move |_, _| {
        this_clone.rename_folder_action(&folder_id_clone);
    })?;
    this.folder_events.keep(rename_revoker);
    flyout.Items()?.Append(&item_base(rename_item)?)?;

    // Delete Folder
    let delete_item = MenuFlyoutItem::new()?;
    delete_item.SetText("Delete Folder")?;
    let this_clone = this.clone();
    let folder_id_clone = folder_id.clone();
    let delete_revoker = delete_item.Click(move |_, _| {
        this_clone.delete_folder_action(&folder_id_clone);
    })?;
    this.folder_events.keep(delete_revoker);
    flyout.Items()?.Append(&item_base(delete_item)?)?;

    // Subscribe/Unsubscribe
    let sub_text = if folder.subscribed {
        "Unsubscribe"
    } else {
        "Subscribe"
    };
    let sub_item = MenuFlyoutItem::new()?;
    sub_item.SetText(sub_text)?;
    let this_clone = this.clone();
    let folder_id_clone = folder_id.clone();
    let sub_scribed = folder.subscribed;
    let sub_revoker = sub_item.Click(move |_, _| {
        this_clone.subscribe_folder_action(&folder_id_clone, !sub_scribed);
    })?;
    this.folder_events.keep(sub_revoker);
    flyout.Items()?.Append(&item_base(sub_item)?)?;

    // Properties
    let props_item = MenuFlyoutItem::new()?;
    props_item.SetText("Properties")?;
    let this_clone = this.clone();
    let folder_id_clone = folder_id.clone();
    let props_revoker = props_item.Click(move |_, _| {
        this_clone.folder_properties_action(&folder_id_clone);
    })?;
    this.folder_events.keep(props_revoker);
    flyout.Items()?.Append(&item_base(props_item)?)?;

    // ContextFlyout gives pointer right-click and Shift+F10/Menu key for free.
    let button_element: mailin_winui_sys::UIElement = button.cast()?;
    let flyout_base: mailin_winui_sys::FlyoutBase = flyout.cast()?;
    let _ = button_element.SetContextFlyout(&flyout_base);

    Ok(button)
}

/// Build one message row: a selection checkbox beside the content button.
///
/// Returns the row grid, the content button (single click opens the message)
/// and the checkbox, so the caller can wire the selection handlers.
fn message_row(
    message: &MessageHeader,
    now: i64,
    density: MessageListDensity,
    selected: bool,
) -> Result<(Grid, Button, CheckBox, Option<StackPanel>)> {
    use MessageListDensity::{Compact, Cozy};
    let seen = message.is_seen();
    let is_thread_root = message.thread_reply_count > 0
        || (message
            .thread_root_key
            .as_ref()
            .is_some_and(|k| k == message.message_id.as_deref().unwrap_or("")));
    let depth = message.thread_depth;

    let (vertical_padding, horizontal_padding, spacing, show_snippet) = match density {
        MessageListDensity::Comfortable => (8.0, 12.0, 3.0, true),
        Cozy => (5.0, 10.0, 2.0, true),
        Compact => (2.0, 8.0, 1.0, false),
    };

    let column = ui::stack(Orientation::Vertical, spacing)?;
    // Indent by depth for threaded view
    let indent = (depth as f64) * 20.0;
    ui::padding(
        &column,
        vertical_padding,
        horizontal_padding + indent,
        vertical_padding,
        horizontal_padding,
    )?;

    let line = ui::stack(Orientation::Horizontal, 6.0)?;

    // Thread expand/collapse chevron for thread roots
    if is_thread_root {
        let chevron = ui::text("▼")?;
        // Note: WinUI doesn't have easy rotation, so we use text
        ui::append(&line, &chevron)?;
    } else if depth > 0 {
        // Placeholder for alignment with thread roots
        let spacer = ui::text("  ")?;
        ui::append(&line, &spacer)?;
    } else {
        let spacer = ui::text("  ")?;
        ui::append(&line, &spacer)?;
    }

    let sender = ui::text(&format::sender_label(message))?;
    if !seen {
        sender.SetFontWeight(mailin_winui_sys::FontWeight { weight: 600 })?;
    }
    ui::stretch_horizontal(&sender)?;
    ui::append(&line, &sender)?;

    // Reply count badge for thread roots
    if is_thread_root && message.thread_reply_count > 0 {
        let badge = ui::text(&format!(" ({})", message.thread_reply_count))?;
        ui::append(&line, &badge)?;
    }

    if message.is_flagged() {
        ui::append(&line, &ui::text("★")?)?;
    }
    if message.has_attachments {
        ui::append(&line, &ui::text("📎")?)?;
    }
    let timestamp = message.sent_at.unwrap_or(message.received_at);
    ui::append(&line, &ui::text(&format::list_time(now, timestamp))?)?;
    ui::append(&column, &line)?;

    let subject = ui::text(&format::subject_label(message))?;
    if !seen {
        subject.SetFontWeight(mailin_winui_sys::FontWeight { weight: 600 })?;
    }
    ui::append(&column, &subject)?;

    if show_snippet && let Some(snippet) = message.snippet.as_deref() {
        let text = format::one_line(snippet, 140);
        if !text.is_empty() {
            ui::append(&column, &ui::text(&text)?)?;
        }
    }

    let button = ui::content_button(
        &column,
        Some(&format!(
            "{}{}{}{} — {}",
            if seen { "" } else { "Unread. " },
            if message.is_flagged() {
                "Starred. "
            } else {
                ""
            },
            if message.has_attachments {
                "Has attachments. "
            } else {
                ""
            },
            message.from_display(),
            format::subject_label(message)
        )),
        None,
    )?;
    ui::padding(&button, 0.0, 0.0, 0.0, 0.0)?;

    // The checkbox is a sibling of the content button rather than its child:
    // ticking a box must not also open the message.
    let checkbox = CheckBox::new().context("could not create a selection checkbox")?;
    let toggle: IToggleButton = checkbox.cast()?;
    toggle.SetIsChecked(Some(selected))?;
    // Accessible name for the checkbox (multi-select toggle)
    AutomationProperties::SetName(&checkbox, "Select message")?;
    let checkbox_element: FrameworkElement = checkbox.cast()?;
    checkbox_element.SetVerticalAlignment(VerticalAlignment::Center)?;

    // Quick-actions bar: hidden by default, revealed on hover.
    let actions_bar = ui::stack(Orientation::Horizontal, 4.0)?;
    ui::padding(&actions_bar, 4.0, 4.0, 4.0, 4.0)?;
    ui::set_visible(&actions_bar, false)?;

    let row = Grid::new().context("could not create a message row")?;
    ui::add_column(&row, GridUnitType::Auto, 0.0)?;
    ui::star_column(&row)?;
    ui::add_column(&row, GridUnitType::Auto, 0.0)?;
    ui::set_column(&checkbox, 0)?;
    ui::append(&row, &checkbox)?;
    ui::set_column(&button, 1)?;
    ui::append(&row, &button)?;
    ui::set_column(&actions_bar, 2)?;
    ui::append(&row, &actions_bar)?;
    Ok((row, button, checkbox, Some(actions_bar)))
}

fn outbox_entry_row(entry: &OutboxEntry, _now: i64) -> Result<Button> {
    let column = ui::stack(Orientation::Vertical, 2.0)?;
    ui::padding(&column, 6.0, 6.0, 6.0, 6.0)?;

    let line = ui::stack(Orientation::Horizontal, 6.0)?;

    let state_text = entry.state.as_str().to_uppercase();
    let state_label = ui::text(&state_text)?;
    // Add color based on state
    match entry.state {
        OutboxState::Queued => {
            state_label.SetForeground(&ui::solid_brush(design_tokens::Colors::default().info)?)?
        }
        OutboxState::Sending => state_label
            .SetForeground(&ui::solid_brush(design_tokens::Colors::default().warning)?)?,
        OutboxState::Failed => {
            state_label.SetForeground(&ui::solid_brush(design_tokens::Colors::default().error)?)?
        }
        OutboxState::Sent => state_label
            .SetForeground(&ui::solid_brush(design_tokens::Colors::default().success)?)?,
    }
    ui::append(&line, &state_label)?;

    if let Some(error) = &entry.last_error {
        let error_label = ui::text(&format!("— {}", error))?;
        ui::stretch_horizontal(&error_label)?;
        ui::append(&line, &error_label)?;
    } else {
        let attempts_label = ui::text(&format!("attempt {}/5", entry.attempts.saturating_add(1)))?;
        ui::append(&line, &attempts_label)?;
    }
    ui::append(&column, &line)?;

    // Parse envelope to show recipient
    let subject = if let Ok(env) = serde_json::from_str::<OutgoingEnvelope>(&entry.envelope_json) {
        let to = env.to.join(", ");
        let cc = if env.cc.is_empty() {
            String::new()
        } else {
            format!(" (Cc: {})", env.cc.join(", "))
        };
        format!("To: {}{}", to, cc)
    } else {
        "Unknown recipient".to_owned()
    };
    let subject_label = ui::text(&subject)?;
    ui::append(&column, &subject_label)?;

    let button = ui::content_button(&column, Some(&format!("Outbox entry: {}", subject)), None)?;
    ui::padding(&button, 0.0, 0.0, 0.0, 0.0)?;
    Ok(button)
}

/// The recipient of a queued message, used in the discard confirmation so
/// the user can see which message is about to go.
fn outbox_entry_recipient(entry: &OutboxEntry) -> String {
    let Ok(env) = serde_json::from_str::<OutgoingEnvelope>(&entry.envelope_json) else {
        return "an unknown recipient".to_owned();
    };
    let recipients = env
        .recipients()
        .into_iter()
        .filter(|address| !address.trim().is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    if recipients.is_empty() {
        "an unknown recipient".to_owned()
    } else {
        recipients
    }
}

/// Build the keyboard shortcuts help dialog using XAML (ContentDialog for proper modality).
fn build_shortcuts_window(xaml_root: &XamlRoot) -> Result<ContentDialog> {
    let dialog = ContentDialog::new().context("could not create shortcuts dialog")?;
    dialog.SetTitle(&ui::text("Keyboard Shortcuts")?)?;
    dialog.SetPrimaryButtonText("Close")?;
    dialog.SetDefaultButton(ContentDialogButton::Primary)?;
    ui::set_xaml_root(&dialog, xaml_root)?;

    // Build content from XAML
    let content = ui::load::<StackPanel>(
        r#"
        <StackPanel xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation" 
                    Orientation="Vertical" Spacing="8" Margin="24,16,24,24">
            <TextBlock Text="Mailin Keyboard Shortcuts" FontSize="18" FontWeight="SemiBold" Margin="0,0,0,8"/>
            <StackPanel Orientation="Vertical" Spacing="4">
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Ctrl+N" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="New message"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Ctrl+R" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Reply to selected message"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Ctrl+Shift+R" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Reply all"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Ctrl+Shift+F" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Forward"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Ctrl+F" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Search mail"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="F5" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Synchronize"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Ctrl+Shift+A" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Add account"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Ctrl+Shift+P" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Preferences"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Ctrl+Enter" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Send message (in composer)"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="↑ / ↓" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Navigate message list"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Space" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Tick or untick the focused message"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Ctrl+A" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Select all messages"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Esc" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Clear the selection"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="Enter" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Open selected message"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="F1" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Show this help"/>
                </Grid>
            </StackPanel>
        </StackPanel>
        "#,
    )?;

    // Give the content a floor so the shortcut list does not collapse.
    ui::min_width(&content, 420.0)?;
    ui::min_height(&content, 320.0)?;
    ui::set_content(&dialog, &content)?;

    Ok(dialog)
}
