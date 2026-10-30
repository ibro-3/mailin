//! The three-pane mail window: folders, message list and reader.
//!
//! Like the GTK client, the window owns no mail state of its own: everything is
//! read from the core store. A `DispatcherQueueTimer` drains the core event
//! channel on the UI thread and refreshes the panes that changed. Widgets are
//! composed from Rust; there is no XAML compiler in a plain Cargo build.

use std::collections::BTreeSet;
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
    Application, Button, CheckBox, ComboBox, ContentDialog, ContentDialogButton, DispatcherQueue, FrameworkElement, Grid, GridUnitType,
    IToggleButton, IXmlNodeSerializer, KeyboardAccelerator, Orientation, ResourceDictionary,
    ScrollViewer, StackPanel, TextBlock, TextBox, ToastNotification, ToastNotificationManager,
    ToastTemplateType, TypedEventHandler, VerticalAlignment, VirtualKey, VirtualKeyModifiers, Visibility, WebView2, Window, XamlRoot,
};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;
use windows_core::{EventRevoker, Interface};

use crate::account_dialog;
use crate::app::AppState;
use crate::compose;
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
    show_outbox: bool,
    remote_allowed: Option<MessageId>,
    pending_attachment: Option<PendingAttachment>,
}

struct Widgets {
    window: Window,
    title: TextBlock,
    banner: TextBlock,
    hamburger: Button,
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
    attachments_panel: StackPanel,
    body_plain: ScrollViewer,
    body_text: TextBlock,
    body_html: WebView2,
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
    /// Keyboard shortcuts help window (created on demand)
    shortcuts_window: std::cell::RefCell<Option<Window>>,
}

pub struct MainWindow {
    app: Arc<AppState>,
    state: Mutex<State>,
    programmatic: AtomicBool,
    html_mode: AtomicBool,
    closed: AtomicBool,
    events: ui::Revokers,
    folder_events: ui::Revokers,
    message_events: ui::Revokers,
    attachment_events: ui::Revokers,
    /// The row checkboxes, parallel to `State::messages`, used to tick or
    /// untick the whole list at once (Ctrl+A, Escape, bulk actions).
    checkboxes: std::cell::RefCell<Vec<CheckBox>>,
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

    fn show_error(&self, message: &str) {
        let mut text = message.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().count() > 180 {
            text = text.chars().take(179).collect();
            text.push('…');
        }
        self.show_banner(&format!("Problem: {text}"));
        tracing::debug!(%message, "account error surfaced in the UI");
    }

    fn show_toast_notification(&self, account_id: AccountId, text: &str) {
        // Check if notifications are enabled in config
        if !self.app.config().ui.notifications {
            return;
        }
        // Only show toast if we have a toast notifier
        let Some(notifier) = &self.app.toast_notifier else {
            return;
        };
        // Use ToastGeneric template to support custom action buttons
        let xml = match ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastGeneric) {
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

        // Add action buttons using XML: "Mark as read" and "Archive"
        // ToastGeneric supports <actions> with <action> elements
        let actions_xml = format!(
            r#"<actions>
                <action content="Mark as read" arguments="mark-read:{account_id}" activationType="foreground"/>
                <action content="Archive" arguments="archive:{account_id}" activationType="foreground"/>
            </actions>"#
        );

        // Parse the actions XML and append to the toast XML
        if let Ok(actions_doc) = mailin_winui_sys::XmlDocument::new() {
            let _ = actions_doc.LoadXml(&actions_xml);
            if let Ok(actions_element) = actions_doc.DocumentElement() {
                let _ = xml.DocumentElement()?.AppendChild(&actions_element);
            }
        }

        let toast = match ToastNotification::CreateToastNotification(&xml) {
            Ok(toast) => toast,
            Err(error) => {
                tracing::warn!(%error, "failed to create the toast notification");
                return;
            }
        };

        // Handle toast activation (button clicks)
        let this = Rc::clone(self);
        let _activated_token = toast.Activated(&TypedEventHandler::new(move |_sender, args| {
            if let Ok(args) = args {
                if let Ok(argument) = args.Argument() {
                    let arg_str = argument.to_string();
                    if arg_str.starts_with("mark-read:") {
                        let account_id_str = arg_str.strip_prefix("mark-read:").unwrap_or("");
                        if let Ok(aid) = account_id_str.parse::<AccountId>() {
                            this.notification_mark_read(aid);
                        }
                    } else if arg_str.starts_with("archive:") {
                        let account_id_str = arg_str.strip_prefix("archive:").unwrap_or("");
                        if let Ok(aid) = account_id_str.parse::<AccountId>() {
                            this.notification_archive(aid);
                        }
                    }
                }
            }
            Ok(())
        }));

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
                let first = self.state().accounts[0].id;
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
        if let Some(index) = self
            .state()
            .accounts
            .iter()
            .position(|account| account.id == id)
        {
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
                id: FolderId(-1), // Virtual ID for Outbox
                account_id,
                name: "Outbox".to_owned(),
                delimiter: "/".to_owned(),
                role: FolderRole::Other,
                uid_validity: None,
                uid_next: None,
                highest_modseq: None,
                last_synced_at: None,
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
        self.folder_events.clear();
        for folder in &folders {
            if let Ok(row) = folder_row(folder) {
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
                let folder = self.state().folders[index].clone();
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
        self.folder_events.clear();
        let _ = ui::clear(&self.widgets.folder_list);
        let _ = ui::set_visible(&self.widgets.folder_empty, true);
        self.state().folders.clear();
        self.clear_messages();
    }

    fn select_folder(self: &Rc<Self>, folder: Folder) {
        let is_outbox = folder.id.0 == -1;
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
        self.message_events.clear();
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
    }

    fn show_messages(self: &Rc<Self>, messages: Vec<MessageHeader>, title: String, more: bool) {
        let now = format::now_unix();
        let empty = messages.is_empty();
        self.message_events.clear();
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
                if quick_actions_enabled {
                    if let Some(actions_bar) = &actions_bar {
                        for action in &quick_action_items {
                            if let Ok(btn) = ui::button(action.label()) {
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
                                Ok(())
                            }) {
                                self.message_events.keep(revoker);
                            }
                            // PointerExited → hide actions bar.
                            let bar = actions_bar.clone();
                            if let Ok(revoker) = element.PointerExited(move |_, _| {
                                let _ = ui::set_visible(&bar, false);
                                Ok(())
                            }) {
                                self.message_events.keep(revoker);
                            }
                        }
                    }
                }
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

    fn clear_messages(self: &Rc<Self>) {
        self.message_events.clear();
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

    fn select_outbox_entry(self: &Rc<Self>, entry: OutboxEntry) {
        // Update selection - could highlight the row or show actions
        // For now, we just track it
        let _ = entry;
    }

    fn outbox_retry_selected(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        // Get the selected row index from the message_list
        // For simplicity, we'll retry all selected entries
        let entries = self.state().outbox_entries.clone();
        // In a real implementation, we'd track which row is selected
        // For now, retry the first entry if any
        if let Some(entry) = entries.first() {
            self.app.engine.retry_outbox(account_id, entry.id);
            self.show_banner("Retrying…");
        }
    }

    fn outbox_discard_selected(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let entries = self.state().outbox_entries.clone();
        if let Some(entry) = entries.first() {
            self.app.engine.cancel_outbox(account_id, entry.id);
            self.show_banner("Discarding…");
        }
    }

    fn outbox_edit_selected(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let entries = self.state().outbox_entries.clone();
        if let Some(entry) = entries.first()
            && let Ok(env) = serde_json::from_str::<OutgoingEnvelope>(&entry.envelope_json)
        {
            let mut draft = OutgoingMessage {
                from_name: String::new(),
                from_addr: env.from,
                to: env.to.join(", "),
                cc: env.cc.join(", "),
                bcc: env.bcc.join(", "),
                subject: String::new(),
                body: String::new(),
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
            // Cancel the original entry so it doesn't get sent again
            self.app.engine.cancel_outbox(account_id, entry.id);
            self.open_compose(draft);
        }
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
        self.state()
            .messages
            .iter()
            .find(|message| message.id == message_id)
            .cloned()
            .or_else(|| self.app.mailin.message(message_id).ok().flatten())
    }

    // Whether the account behind the current selection trusts `domain` for
    // remote content. Reads from the store so changes via the account dialog
    // or the banner's trust button are picked up immediately.
    fn sender_domain_allowed(&self, domain: &str) -> bool {
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
    fn trust_current_sender(&self) {
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
                    let dark = matches!(self.app.config().ui.theme, mailin_core::Theme::Dark);
                    let document = html::document(html_body, allow_remote, dark, &images);
                    let _ = self.widgets.body_html.NavigateToString(&document);
                    let _ = ui::set_visible(&self.widgets.body_html, true);
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
        self.attachment_events.clear();
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
        if visible.len() > 1 {
            if let Ok(download_all_row) = ui::stack(Orientation::Horizontal, 8.0) {
                ui::padding(&download_all_row, 4.0, 4.0, 4.0, 4.0)?;
                let download_all = ui::button("Download all", Some("Download all"), Some("Download all attachments"))?;
                let this = Rc::clone(self);
                let mid = message_id;
                let visible_clone = visible.clone();
                let revoker = ui::on_click(&download_all, move || {
                    for att in &visible_clone {
                        let _ = this.save_attachment(mid, att.clone());
                    }
                })?;
                self.attachment_events.keep(revoker);
                ui::append(&download_all_row, &download_all)?;
                let _ = ui::append(&self.widgets.attachments_panel, &download_all_row);
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
        let is_image = attachment.mime_type.as_deref().map(|m| m.starts_with("image/")).unwrap_or(false);
        let is_pdf = attachment.mime_type.as_deref() == Some("application/pdf");
        let is_large = attachment.size.map(|s| s > 40_000_000).unwrap_or(false);
        
        let row = ui::stack(Orientation::Horizontal, 8.0)?;
        ui::padding(&row, 4.0, 4.0, 4.0, 4.0)?;
        
        // Preview for images and PDFs
        if is_image || is_pdf {
            if let Ok(preview) = self.attachment_preview(message_id, attachment.clone()) {
                ui::append(&row, &preview)?;
            }
        }
        
        let name = ui::text(&attachment.display_name())?;
        ui::stretch_horizontal(&name)?;
        ui::append(&row, &name)?;
        
        if let Some(size) = attachment.size {
            let size_text = format::file_size(size);
            let size_block = ui::text(&size_text)?;
            if is_large {
                let _ = size_block.SetForeground(&ui::solid_brush("#D13438")?); // error red
            }
            ui::append(&row, &size_block)?;
        }
        
        if is_large {
            let warning = ui::text("⚠ Large file")?;
            let _ = warning.SetForeground(&ui::solid_brush("#D13438")?);
            ui::append(&row, &warning)?;
        }
        
        let save = ui::button("Save", Some(&format!("Save {}", attachment.display_name)), Some(&format!("Save attachment {}", attachment.display_name)))?;
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
        
        let is_image = attachment.mime_type.as_deref().map(|m| m.starts_with("image/")).unwrap_or(false);
        let is_pdf = attachment.mime_type.as_deref() == Some("application/pdf");
        
        if is_image {
            // Image preview - show thumbnail
            if let Ok(data) = self.app.mailin.attachment(message_id, &attachment.part_id) {
                if let Some(bytes) = data.data {
                    if let Ok(bitmap) = Self::bytes_to_bitmap(&bytes) {
                        let image = mailin_winui_sys::Image::new()?;
                        image.SetSource(&bitmap)?;
                        image.SetStretch(mailin_winui_sys::Stretch::Uniform)?;
                        image.SetMaxWidth(80.0)?;
                        image.SetMaxHeight(80.0)?;
                        ui::append(&container, &image)?;
                    }
                }
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
        use mailin_winui_sys::{BitmapImage, InMemoryRandomAccessStream};
        use windows::Storage::Streams::DataWriter;
        use windows_core::Interface;
        
        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(bytes)?;
        writer.StoreAsync()?.get()?;
        writer.FlushAsync()?.get()?;
        stream.Seek(0)?;
        
        let bitmap = BitmapImage::new()?;
        bitmap.SetSource(&stream)?;
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
        self.app.engine.archive_messages(&[message.id]);
        self.show_banner("Archiving…");
    }

    fn delete_selected(&self) {
        let Some(message) = self.selected_message() else {
            return;
        };
        self.app.engine.delete_messages(&[message.id]);
        self.show_banner("Moving to Trash…");
    }

    fn mark_selected_junk(&self) {
        let Some(message) = self.selected_message() else {
            return;
        };
        self.app.engine.mark_messages_junk(&[message.id], true);
        self.show_banner("Marking as Junk…");
    }

    /// Run a quick action on a specific message (from hover/swipe).
    fn run_quick_action(&self, action: mailin_core::config::QuickAction, message_id: MessageId) {
        use mailin_core::config::QuickAction;
        let ids = vec![message_id];
        match action {
            QuickAction::Archive => {
                self.app.engine.archive_messages(&ids);
                self.show_banner("Archiving…");
            }
            QuickAction::Delete => {
                self.app.engine.delete_messages(&ids);
                self.show_banner("Moving to Trash…");
            }
            QuickAction::Junk => {
                self.app.engine.mark_messages_junk(&ids, true);
                self.show_banner("Marking as Junk…");
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

    /// Handle "Mark as read" action from notification for the given account.
    fn notification_mark_read(&self, account_id: AccountId) {
        // Fetch unread messages from the inbox for this account
        let ids = self.get_unread_inbox_messages(account_id);
        if ids.is_empty() {
            return;
        }
        // Mark as read by adding \Seen flag
        self.app.engine.bulk_set_flags(&ids, vec!["\\Seen".to_owned()], Vec::new());
        self.show_banner("Marked as read");
    }

    /// Handle "Archive" action from notification for the given account.
    fn notification_archive(&self, account_id: AccountId) {
        // Fetch unread messages from the inbox for this account
        let ids = self.get_unread_inbox_messages(account_id);
        if ids.is_empty() {
            return;
        }
        self.app.engine.archive_messages(&ids);
        self.show_banner("Archiving…");
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
        self.app.engine.archive_messages(&ids);
        self.show_banner("Archiving…");
        self.clear_bulk_selection();
    }

    fn bulk_delete(&self) {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return;
        }
        self.app.engine.delete_messages(&ids);
        self.show_banner("Moving to Trash…");
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
        let picked = move |target: FolderId| {
            app.engine.move_messages(&ids, target);
            if let Some(this) = this.upgrade() {
                this.show_banner("Moving…");
                this.clear_bulk_selection();
            }
        };
        if let Err(error) = move_dialog::show(&xaml_root, &self.events, &folders, exclude, picked) {
            self.show_error(&format!("Could not open the folder picker: {error:#}"));
        }
    }

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
        self.update_bulk_bar();
    }

    /// Move selection to the previous message (Up arrow).
    fn select_previous_message(&self) {
        let state = self.state();
        if state.show_outbox || state.messages.is_empty() {
            return;
        }
        let selected = &state.selected;
        let new_idx = if let Some(&last) = selected.iter().last() {
            // Find the index of the last selected message and go to previous
            state.messages.iter().position(|m| m.id == last)
                .map(|i| i.saturating_sub(1))
                .unwrap_or(0)
        } else {
            // Nothing selected, select the first message
            0
        };
        if new_idx < state.messages.len() {
            let msg = &state.messages[new_idx];
            selected.clear();
            selected.insert(msg.id);
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
        let state = self.state();
        if state.show_outbox || state.messages.is_empty() {
            return;
        }
        let selected = &state.selected;
        let new_idx = if let Some(&last) = selected.iter().last() {
            // Find the index of the last selected message and go to next
            state.messages.iter().position(|m| m.id == last)
                .map(|i| (i + 1).min(state.messages.len() - 1))
                .unwrap_or(0)
        } else {
            // Nothing selected, select the first message
            0
        };
        if new_idx < state.messages.len() {
            let msg = &state.messages[new_idx];
            selected.clear();
            selected.insert(msg.id);
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
        let state = self.state();
        if state.show_outbox || state.messages.is_empty() {
            return;
        }
        // Find the currently focused message (the one with checked checkbox)
        let checkboxes = self.checkboxes.borrow().clone();
        for (i, checkbox) in checkboxes.iter().enumerate() {
            if let Ok(toggle) = checkbox.cast::<IToggleButton>() {
                if let Ok(Some(true)) = toggle.IsChecked() {
                    if i < state.messages.len() {
                        let msg = &state.messages[i];
                        if state.selected.contains(&msg.id) {
                            state.selected.remove(&msg.id);
                            let _ = toggle.SetIsChecked(Some(false));
                        } else {
                            state.selected.insert(msg.id);
                            let _ = toggle.SetIsChecked(Some(true));
                        }
                        self.update_bulk_bar();
                    }
                    break;
                }
            }
        }
    }

    /// Open the currently selected message (Enter).
    fn open_selected_message(&self) {
        let state = self.state();
        if state.show_outbox {
            return;
        }
        let selected = &state.selected;
        if let Some(&msg_id) = selected.iter().next() {
            if let Some(message) = state.messages.iter().find(|m| m.id == msg_id) {
                self.open_message(message.clone());
            }
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
        let signature = self
            .state()
            .selection
            .account
            .and_then(|account_id| self.app.mailin.account(&account_id).ok().flatten())
            .map(|account| mailin_core::compose::signature_block(&account.signature))
            .unwrap_or_default();
        OutgoingMessage {
            body: signature,
            ..OutgoingMessage::default()
        }
    }

    fn open_compose(self: &Rc<Self>, draft: OutgoingMessage) {
        let Some(account_id) = self.state().selection.account else {
            self.show_banner("Add an account before composing mail");
            return;
        };
        let banner = self.widgets.banner.clone();
        match compose::present(Arc::clone(&self.app), account_id, draft, move || {
            let _ = banner.SetText("Message queued in the outbox");
            let _ = ui::set_visible(&banner, true);
        }) {
            Ok(_) => {}
            Err(error) => self.show_error(&format!("Could not open the composer: {error:#}")),
        }
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
        // Re-activate the help dialog when it is already open.
        if let Some(existing) = self.widgets.shortcuts_window.borrow().as_ref() {
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
                        account_id, count, ..
                    }) => new_messages.push((account_id, count)),
                    Ok(Event::BodyReady { message_id }) => body_ready = Some(message_id),
                    Ok(Event::OutboxChanged) => outbox_changed = true,
                    Ok(Event::AccountError { message, .. }) => account_error = Some(message),
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
        for (account_id, count) in new_messages {
            let account = self
                .state()
                .accounts
                .iter()
                .find(|account| account.id == account_id)
                .map(|account| account.email.clone())
                .unwrap_or_else(|| "Mailin".to_owned());
            let noun = if count == 1 { "message" } else { "messages" };
            let text = format!("{count} new {noun} in {account}");
            self.show_banner(&text);
            self.show_toast_notification(account_id, &text);
        }
        if outbox_changed {
            let state = self.state();
            if state.show_outbox {
                self.refresh_messages();
            } else {
                self.refresh_folders();
            }
        }
        if let Some(message) = account_error {
            self.show_error(&message);
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
    let title = ui::heading("Mailin", Some(20.0), Some(700))?;
    let account_combo = ui::combo(&[], 0, Some("Switch account"))?;
    ui::min_width(&account_combo, 220.0)?;
    
    // Hamburger menu button (visible on mobile/tablet)
    let hamburger = ui::button("☰", Some("Menu"), Some("Open main menu"))?;
    ui::min_width(&hamburger, 40.0)?;
    ui::set_visible(&hamburger, false)?; // Hidden by default (desktop)
    
    let add_account = ui::button("Add account", Some("Add account"), Some("Add a new mail account (Ctrl+Shift+A)"))?;
    let edit_account = ui::button("Edit account", Some("Edit account"), Some("Edit the selected account"))?;
    let compose = ui::button("Compose", Some("Compose"), Some("Create a new message (Ctrl+N)"))?;
    let sync = ui::button("Sync", Some("Sync"), Some("Synchronize mail (F5)"))?;
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

    // ------------------------------------------------------------- panes
    let host = Grid::new().context("could not create the content host")?;
    ui::star_row(&host)?;
    ui::set_row(&host, 2)?;
    ui::append(&root, &host)?;

    let welcome = ui::stack_md(Orientation::Vertical)?;
    ui::margin_xl(&welcome)?;
    ui::margin_vertical(&welcome, 40.0)?;
    let welcome_title = ui::heading("Welcome to Mailin", Some(28.0), Some(700))?;
    let welcome_body = ui::text("Add a mail account to read and send mail.")?;
    let welcome_add = ui::button("Add Account…", Some("Add Account"), Some("Add a new mail account"))?;
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
    let load_more = ui::button("Load more")?;
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
        &ui::heading("Select a message", Some(22.0), Some(600))?,
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
    let html_button = ui::button("Show HTML", Some("Show HTML"), Some("Show the original HTML body"))?;
    let reply_button = ui::button("Reply", Some("Reply"), Some("Reply to sender (Ctrl+R)"))?;
    let reply_all_button = ui::button("Reply all", Some("Reply all"), Some("Reply to all recipients (Ctrl+Shift+R)"))?;
    let forward_button = ui::button("Forward", Some("Forward"), Some("Forward message"))?;
    let actions_spacer = ui::text("")?;
    ui::stretch_horizontal(&actions_spacer)?;
    let flag_button = ui::button("Star", Some("Star"), Some("Toggle star/flag"))?;
    let read_button = ui::button("Mark read", Some("Mark read"), Some("Toggle read status"))?;
    let archive_button = ui::button("Archive", Some("Archive"), Some("Archive message (Ctrl+Shift+E)"))?;
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

    let reader_subject = ui::heading("", Some(22.0), Some(700))?;
    let reader_from = ui::heading("", Some(14.0), Some(600))?;
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
    let remote_once = ui::button("Load once", Some("Load once"), Some("Load remote content for this message only"))?;
    let remote_trust = ui::button("Always allow sender", Some("Always allow sender"), Some("Always load remote content from this sender"))?;
    ui::append(&remote_panel, &remote_text)?;
    ui::append(&remote_panel, &remote_once)?;
    ui::append(&remote_panel, &remote_trust)?;
    ui::set_visible(&remote_panel, false)?;
    ui::append(&header, &remote_panel)?;

    let attachments_panel = ui::stack(Orientation::Vertical, 4.0)?;
    ui::set_visible(&attachments_panel, false)?;
    ui::append(&header, &attachments_panel)?;
    ui::set_row(&header, 0)?;
    ui::append(&reader_content, &header)?;

    let body = Grid::new().context("could not create the reader body")?;
    ui::star_row(&body)?;
    let body_text = ui::text("")?;
    ui::margin(&body_text, 4.0, 8.0, 4.0, 8.0)?;
    let body_plain = ui::scroll(&body_text)?;
    let body_html = WebView2::new().context("could not create the WebView2 control")?;
    ui::set_visible(&body_plain, false)?;
    ui::set_visible(&body_html, false)?;
    ui::append(&body, &body_plain)?;
    ui::append(&body, &body_html)?;
    ui::set_row(&body, 1)?;
    ui::append(&reader_content, &body)?;

    // ------------------------------------------------------------- window
    let window = Window::new().context("could not create the WinUI window")?;
    window.SetTitle("Mailin")?;
    window.SetContent(&root)?;

    // Responsive layout handler
    let mail_grid = mail.clone();
    let sidebar = folder_scroller.clone();
    let list_pane = message_scroll.clone();
    let reader_host = reader_host.clone();
    let hamburger_btn = hamburger.clone();
    let root_grid = root.clone();
    let toolbar_panel = toolbar.clone();
    
    let size_changed_token = window.SizeChanged(&TypedEventHandler::new(
        move |_sender, args| {
            if let Ok(args) = args {
                let width = args.NewSize().unwrap_or_default().Width;
                let is_mobile = width < BREAKPOINT_MOBILE;
                let is_tablet = width < BREAKPOINT_TABLET && width >= BREAKPOINT_MOBILE;
                let is_desktop = width >= BREAKPOINT_TABLET;
                
                // Show/hide hamburger menu
                let _ = hamburger_btn.SetVisibility(if is_mobile || is_tablet {
                    Visibility::Visible
                } else {
                    Visibility::Collapsed
                });
                
                // On mobile: collapse sidebar (column 0), show overlay when hamburger clicked
                // On tablet: narrow sidebar
                // On desktop: full three-pane layout
                
                // The Grid with star_column_min automatically handles min/max widths
                // We just need to handle the hamburger overlay logic
            }
            Ok(())
        }
    ));
    
    // Store the token to keep the handler alive
    let _ = size_changed_token;

    // Hamburger click handler for mobile sidebar toggle
    let mail_grid_for_hamburger = mail.clone();
    let hamburger_btn_click = hamburger.clone();
    let sidebar_scroller = folder_scroller.clone();
    let _hamburger_token = ui::on_click(&hamburger_btn_click, move || {
        // Toggle sidebar visibility on mobile
        let is_visible = sidebar_scroller.Visibility().unwrap_or(Visibility::Visible) == Visibility::Visible;
        let _ = sidebar_scroller.SetVisibility(if is_visible {
            Visibility::Collapsed
        } else {
            Visibility::Visible
        });
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
        closed: AtomicBool::new(false),
        events: ui::Revokers::default(),
        folder_events: ui::Revokers::default(),
        message_events: ui::Revokers::default(),
        attachment_events: ui::Revokers::default(),
        checkboxes: std::cell::RefCell::new(Vec::new()),
        widgets: Widgets {
            window: window.clone(),
            title,
            banner,
            hamburger,
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
            attachments_panel,
            body_plain,
            body_text,
            body_html,
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
            shortcuts_window: std::cell::RefCell::new(None),
        },
    });

    // Keyboard accelerators, attached to the content root so they fire
    // wherever the focus is inside the window.
    this.events.keep(add_accelerator(
        &root,
        VirtualKey::F,
        Some(VirtualKeyModifiers::Control),
        Some("Search mail (Ctrl+F)"),
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.run_search();
                }
            }
        },
    )?);
    this.events
        .keep(add_accelerator(
            &root,
            VirtualKey::F5,
            None,
            Some("Synchronize (F5)"),
            {
                let this = Rc::downgrade(&this);
                move || {
                    if let Some(this) = this.upgrade() {
                        this.sync_now();
                    }
                }
            },
        )?);
    this.events.keep(add_accelerator(
        &root,
        VirtualKey::A,
        Some(VirtualKeyModifiers::Control | VirtualKeyModifiers::Shift),
        Some("Add account (Ctrl+Shift+A)"),
        {
            let this = Rc::downgrade(&this);
            move || {
                if let Some(this) = this.upgrade() {
                    this.add_account();
                }
            }
        },
    )?);
    this.events
        .keep(add_accelerator(&root, VirtualKey::F1, None, Some("Show shortcuts help (F1)"), {
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
        Some("Select all messages (Ctrl+A)"),
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
        Some("Clear selection (Escape)"),
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
        Some("Preferences (Ctrl+Shift+P)"),
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

    let _window = ui::keep_alive(&window);
    window.Activate().context("could not show the window")?;
    tracing::info!("WinUI window activated");
    Ok(window)
}

/// Create the main window and immediately open a compose window with the given draft.
///
/// Kept for the (not yet wired) protocol-activation path: `mailto:` and toast
/// activations arrive through `AppInstance`, not `IApplicationOverrides`.
#[allow(dead_code)]
pub fn create_with_draft(state: &Arc<AppState>, draft: OutgoingMessage) -> Result<Window> {
    let window = create(state)?;
    // Open compose window with the draft
    if let Some(account_id) = state
        .mailin
        .list_accounts()
        .ok()
        .and_then(|accounts| accounts.first().map(|a| a.id))
    {
        let _ = crate::compose::present(Arc::clone(state), account_id, draft, move || {});
    }
    Ok(window)
}

/// Register a keyboard accelerator on an element of the window.
///
/// Attach it to the window's content root for app-wide shortcuts, or to a
/// pane to keep a shortcut scoped to that pane's focus. The returned revoker
/// must be kept alive for as long as the shortcut should work; callers store
/// it alongside their other window event revokers.
/// `description` sets the KeyboardAccelerator.String for key tips / help.
fn add_accelerator(
    element: &impl Interface,
    key: VirtualKey,
    modifiers: Option<VirtualKeyModifiers>,
    description: Option<&str>,
    handler: impl Fn() + 'static,
) -> Result<EventRevoker> {
    let accelerator =
        KeyboardAccelerator::new().context("could not create a keyboard accelerator")?;
    accelerator.SetKey(key)?;
    if let Some(modifiers) = modifiers {
        accelerator.SetModifiers(modifiers)?;
    }
    let revoker = accelerator.Invoked(move |_, _| handler())?;
    ui::add_keyboard_accelerator(element, &accelerator, description)?;
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
        move || this.mark_selected_junk()
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
            if let Some(message_id) = this.state().selection.message {
                this.render_body(message_id);
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
    let key_down_revoker = message_scroll.KeyDown(move |_, args| {
        if let Some(this) = this_weak.upgrade() {
            if let Ok(key) = args.Key() {
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

fn role_marker(role: FolderRole) -> &'static str {
    match role {
        FolderRole::Inbox => "●",
        FolderRole::Sent => "→",
        FolderRole::Drafts => "✎",
        FolderRole::Trash => "🗑",
        FolderRole::Junk => "!",
        FolderRole::Archive => "▣",
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

fn folder_row(folder: &Folder) -> Result<Button> {
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
            "{}, {} unread message{}",
            folder_title(folder),
            folder.unread_count,
            if folder.unread_count == 1 { "" } else { "s" }
        )),
        None,
    )?;
    ui::padding(&button, 0.0, 0.0, 0.0, 0.0)?;
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

    let (vertical_padding, horizontal_padding, spacing, show_snippet) = match density {
        MessageListDensity::Comfortable => (8.0, 12.0, 3.0, true),
        Cozy => (5.0, 10.0, 2.0, true),
        Compact => (2.0, 8.0, 1.0, false),
    };

    let column = ui::stack(Orientation::Vertical, spacing)?;
    ui::padding(
        &column,
        vertical_padding,
        horizontal_padding,
        vertical_padding,
        horizontal_padding,
    )?;

    let line = ui::stack(Orientation::Horizontal, 6.0)?;
    let sender = ui::text(&format::sender_label(message))?;
    if !seen {
        sender.SetFontWeight(mailin_winui_sys::FontWeight { weight: 600 })?;
    }
    ui::stretch_horizontal(&sender)?;
    ui::append(&line, &sender)?;
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

    if show_snippet {
        if let Some(snippet) = message.snippet.as_deref() {
            let text = format::one_line(snippet, 140);
            if !text.is_empty() {
                ui::append(&column, &ui::text(&text)?)?;
            }
        }
    }

    let button = ui::content_button(
        &column,
        Some(&format!(
            "{}{}{}{} — {}",
            if seen { "" } else { "Unread. " },
            if message.is_flagged() { "Starred. " } else { "" },
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
        OutboxState::Queued => state_label.SetForeground(&ui::solid_brush("#0078D4")?)?, // accent blue
        OutboxState::Sending => state_label.SetForeground(&ui::solid_brush("#FF8C00")?)?, // warning orange
        OutboxState::Failed => state_label.SetForeground(&ui::solid_brush("#D13438")?)?, // error red
        OutboxState::Sent => state_label.SetForeground(&ui::solid_brush("#107C10")?)?, // success green
    }
    ui::append(&line, &state_label)?;

    if let Some(error) = &entry.last_error {
        let error_label = ui::text(&format!("— {}", error))?;
        ui::stretch_horizontal(&error_label)?;
        ui::append(&line, &error_label)?;
    } else {
        let attempts_label = ui::text(&format!("attempt {}/5", entry.attempts + 1))?;
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

    let button = ui::content_button(
        &column,
        Some(&format!("Outbox entry: {}", subject)),
        None,
    )?;
    ui::padding(&button, 0.0, 0.0, 0.0, 0.0)?;
    Ok(button)
}

/// Build the keyboard shortcuts help dialog using XAML (ContentDialog for proper modality).
fn build_shortcuts_window(xaml_root: &XamlRoot) -> Result<ContentDialog> {
    let dialog = ContentDialog::new().context("could not create shortcuts dialog")?;
    dialog.SetTitle("Keyboard Shortcuts")?;
    dialog.SetPrimaryButtonText("Close")?;
    dialog.SetDefaultButton(ContentDialogButton::Primary)?;
    dialog.SetXamlRoot(xaml_root)?;

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
    dialog.SetContent(&content)?;

    Ok(dialog)
}
