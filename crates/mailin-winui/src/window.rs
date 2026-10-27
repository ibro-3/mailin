//! The three-pane mail window: folders, message list and reader.
//!
//! Like the GTK client, the window owns no mail state of its own: everything is
//! read from the core store. A `DispatcherQueueTimer` drains the core event
//! channel on the UI thread and refreshes the panes that changed. Widgets are
//! composed from Rust; there is no XAML compiler in a plain Cargo build.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use mailin_core::{
    Account, AccountId, AttachmentMeta, Event, Folder, FolderId, FolderRole, MessageHeader,
    MessageId, OutboxEntry, OutboxState, OutgoingEnvelope, OutgoingMessage, html,
};
use mailin_winui_sys::{
    Application, Button, ComboBox, DispatcherQueue, Grid, KeyboardAccelerator, Orientation,
    ResourceDictionary, ScrollViewer, StackPanel, TextBlock, TextBox, ToastNotification,
    ToastNotificationManager, ToastNotifier, ToastTemplateType, VirtualKey, VirtualKeyModifiers,
    WebView2, Window, XmlDocument,
};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;

use crate::account_dialog;
use crate::app::AppState;
use crate::compose;
use crate::format;
use crate::platform;
use crate::ui;

const SEARCH_LIMIT: u32 = 200;
const MAX_PAGE: u32 = 1_000;
const EVENT_POLL_MILLIS: i64 = 200;

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
    outbox_entries: Vec<OutboxEntry>,
    show_outbox: bool,
    remote_allowed: Option<MessageId>,
    pending_attachment: Option<PendingAttachment>,
}

struct Widgets {
    window: Window,
    title: TextBlock,
    banner: TextBlock,
    account_combo: ComboBox,
    add_account: Button,
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
    remote_button: Button,
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

    fn show_toast_notification(&self, text: &str) {
        // Check if notifications are enabled in config
        if !self.app.config.ui.notifications {
            return;
        }
        // Only show toast if we have a toast notifier
        let Some(notifier) = &self.app.toast_notifier else {
            return;
        };
        // Use ToastTemplateType::ToastText02 which has a title and body text
        let xml = match ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02) {
            Ok(xml) => xml,
            Err(e) => {
                tracing::warn!(%e, "failed to get toast template");
                return;
            }
        };
        // Find the text elements and set their content
        // ToastText02 has two text elements: [0] = title, [1] = body
        let text_nodes = match xml.GetElementsByTagName(&"text".into()) {
            Ok(nodes) => nodes,
            Err(e) => {
                tracing::warn!(%e, "failed to get toast text elements");
                return;
            }
        };
        if text_nodes.Length() >= 2 {
            if let Ok(title_node) = text_nodes.Item(0) {
                let _ = title_node.SetInnerText(&"New mail".into());
            }
            if let Ok(body_node) = text_nodes.Item(1) {
                let _ = body_node.SetInnerText(&text.into());
            }
        }
        // Create and show the toast notification
        let toast = match ToastNotification::CreateToastNotification(&xml) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(%e, "failed to create toast notification");
                return;
            }
        };
        // Set an argument so we can handle activation
        let _ = toast.SetArgument(&"new-mail".into());
        if let Err(e) = notifier.Show(&toast) {
            tracing::warn!(%e, "failed to show toast notification");
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
            if state.selection.folder == Some(folder.id) && !state.selection.search && state.show_outbox == is_outbox {
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

    fn page_size(&self) -> u32 {
        self.app.config.ui.page_size.clamp(1, MAX_PAGE)
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
        for entry in &entries {
            if let Ok(row) = outbox_entry_row(entry, now) {
                let this = Rc::clone(self);
                let entry = entry.clone();
                if let Ok(revoker) = ui::on_click(&row, move || this.select_outbox_entry(entry)) {
                    self.message_events.keep(revoker);
                }
                let _ = ui::append(&self.widgets.message_list, &row);
            }
        }
        self.state().outbox_entries = entries;
        let _ = self.widgets.list_title.SetText(&title);
        let empty_text = if empty { "Outbox is empty" } else { "Select an entry to see actions" };
        let _ = self.widgets.message_empty.SetText(empty_text);
        let _ = ui::set_visible(&self.widgets.message_empty, empty);
        let _ = self.widgets.load_more.SetIsVisible(false);
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
        for message in &messages {
            if let Ok(row) = message_row(message, now) {
                let this = Rc::clone(self);
                let message = message.clone();
                if let Ok(revoker) = ui::on_click(&row, move || this.open_message(message.clone()))
                {
                    self.message_events.keep(revoker);
                }
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
    }

    fn clear_messages(self: &Rc<Self>) {
        self.message_events.clear();
        let _ = ui::clear(&self.widgets.message_list);
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
        if self.app.config.ui.mark_read_on_open && !message.is_seen() {
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
            let _ = self.app.engine.retry_outbox(account_id, entry.id);
            self.show_banner("Retrying…");
        }
    }

    fn outbox_discard_selected(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let entries = self.state().outbox_entries.clone();
        if let Some(entry) = entries.first() {
            let _ = self.app.engine.cancel_outbox(account_id, entry.id);
            self.show_banner("Discarding…");
        }
    }

    fn outbox_edit_selected(self: &Rc<Self>) {
        let Some(account_id) = self.state().selection.account else {
            return;
        };
        let entries = self.state().outbox_entries.clone();
        if let Some(entry) = entries.first() {
            if let Ok(env) = serde_json::from_str::<OutgoingEnvelope>(&entry.envelope_json) {
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
                let _ = self.app.engine.cancel_outbox(account_id, entry.id);
                self.open_compose(draft);
            }
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
                    let allow_remote = self.app.config.ui.remote_content_enabled
                        || self.state().remote_allowed == Some(message_id);
                    let images = self.inline_images(message_id);
                    let dark = matches!(self.app.config.ui.theme, mailin_core::Theme::Dark);
                    let document = html::document(html_body, allow_remote, dark, &images);
                    let _ = self.widgets.body_html.NavigateToString(&document);
                    let _ = ui::set_visible(&self.widgets.body_html, true);
                    if !allow_remote && html::has_remote_content(html_body) {
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
        let visible = attachments
            .into_iter()
            .filter(|attachment| !attachment.is_inline)
            .collect::<Vec<_>>();
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
        let row = ui::stack(Orientation::Horizontal, 8.0)?;
        ui::padding(&row, 4.0, 4.0, 4.0, 4.0)?;
        let name = ui::text(&attachment.display_name())?;
        ui::stretch_horizontal(&name)?;
        ui::append(&row, &name)?;
        if let Some(size) = attachment.size {
            ui::append(&row, &ui::text(&format::file_size(size))?)?;
        }
        let save = ui::button("Save")?;
        let this = Rc::clone(self);
        let attachment = attachment.clone();
        let revoker = ui::on_click(&save, move || {
            this.save_attachment(message_id, attachment.clone())
        })?;
        self.attachment_events.keep(revoker);
        ui::append(&row, &save)?;
        Ok(row)
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
        let this = Rc::clone(this);
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

    fn remove_account(self: &Rc<Self>) {
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
        let mailin = self.app.mailin.clone();
        let this = Rc::clone(this);
        std::thread::spawn(move || {
            if let Err(e) = mailin.remove_account(&account.id) {
                eprintln!("Failed to remove account: {e}");
            } else {
                this.refresh_accounts();
            }
        });
    }

    /// Show the keyboard shortcuts help window.
    fn show_shortcuts_help(self: &Rc<Self>) {
        // Check if window already exists
        if let Some(existing) = self.widgets.shortcuts_window.borrow().as_ref() {
            let _ = existing.Activate();
            return;
        }

        let parent = self.widgets.window.clone();

        // Build the shortcuts window using XAML
        let shortcuts_window = match Self::build_shortcuts_window(&parent) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!("Failed to create shortcuts window: {e:#}");
                return;
            }
        };

        // Store reference
        *self.widgets.shortcuts_window.borrow_mut() = Some(shortcuts_window.clone());

        // Clean up reference when closed
        let this = Rc::clone(self);
        let _ = shortcuts_window.Closed(move |_, _| {
            *this.widgets.shortcuts_window.borrow_mut() = None;
        });

        let _ = shortcuts_window.Activate();
    }

    fn show_preferences(self: &Rc<Self>) {
        let xaml_root = match self.widgets.window.XamlRoot() {
            Ok(root) => root,
            Err(e) => {
                tracing::error!("Failed to get XamlRoot: {e:#}");
                return;
            }
        };
        if let Err(e) = crate::preferences::show_preferences(&self.app, &xaml_root) {
            tracing::error!("Failed to show preferences: {e:#}");
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
            self.show_toast_notification(&text);
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
    let toolbar = ui::stack(Orientation::Horizontal, 8.0)?;
    ui::margin(&toolbar, 12.0, 10.0, 12.0, 6.0)?;
    let title = ui::heading("Mailin", Some(20.0), Some(700))?;
    let account_combo = ui::combo(&[], 0)?;
    ui::min_width(&account_combo, 220.0)?;
    let add_account = ui::button("Add account")?;
    let compose = ui::button("Compose")?;
    let sync = ui::button("Sync")?;
    let spacer = ui::text("")?;
    ui::stretch_horizontal(&spacer)?;
    let search = ui::text_box("Search mail")?;
    ui::min_width(&search, 220.0)?;
    let search_button = ui::button("Search")?;
    ui::append(&toolbar, &title)?;
    ui::append(&toolbar, &account_combo)?;
    ui::append(&toolbar, &add_account)?;
    ui::append(&toolbar, &compose)?;
    ui::append(&toolbar, &sync)?;
    ui::append(&toolbar, &spacer)?;
    ui::append(&toolbar, &search)?;
    ui::append(&toolbar, &search_button)?;
    ui::set_row(&toolbar, 0)?;
    ui::append(&root, &toolbar)?;

    let banner = ui::text("")?;
    ui::margin(&banner, 12.0, 2.0, 12.0, 2.0)?;
    ui::set_visible(&banner, false)?;
    ui::set_row(&banner, 1)?;
    ui::append(&root, &banner)?;

    // ------------------------------------------------------------- panes
    let host = Grid::new().context("could not create the content host")?;
    ui::star_row(&host)?;
    ui::set_row(&host, 2)?;
    ui::append(&root, &host)?;

    let welcome = ui::stack(Orientation::Vertical, 10.0)?;
    ui::margin(&welcome, 40.0, 80.0, 40.0, 40.0)?;
    let welcome_title = ui::heading("Welcome to Mailin", Some(28.0), Some(700))?;
    let welcome_body = ui::text("Add a mail account to read and send mail.")?;
    let welcome_add = ui::button("Add Account…")?;
    ui::append(&welcome, &welcome_title)?;
    ui::append(&welcome, &welcome_body)?;
    ui::append(&welcome, &welcome_add)?;
    ui::append(&host, &welcome)?;

    let mail = Grid::new().context("could not create the mail layout")?;
    ui::star_row(&mail)?;
    ui::pixel_column(&mail, 230.0)?;
    ui::pixel_column(&mail, 330.0)?;
    ui::star_column(&mail)?;
    ui::set_visible(&mail, false)?;
    ui::append(&host, &mail)?;

    // ----------------------------------------------------------- folders
    let sidebar = ui::stack(Orientation::Vertical, 6.0)?;
    ui::margin(&sidebar, 10.0, 8.0, 6.0, 8.0)?;
    let folder_heading = ui::heading("Folders", Some(14.0), Some(600))?;
    ui::append(&sidebar, &folder_heading)?;
    let folder_empty = ui::text("Folders appear after the first synchronization.")?;
    ui::set_visible(&folder_empty, true)?;
    ui::append(&sidebar, &folder_empty)?;
    let folder_list = ui::stack(Orientation::Vertical, 2.0)?;
    let folder_scroller = ui::scroll(&folder_list)?;
    ui::append(&sidebar, &folder_scroller)?;
    ui::set_row(&sidebar, 0)?;
    ui::set_column(&sidebar, 0)?;
    ui::append(&mail, &sidebar)?;

    // ------------------------------------------------------ message list
    let list_pane = Grid::new().context("could not create the message pane")?;
    ui::auto_row(&list_pane)?;
    ui::star_row(&list_pane)?;
    ui::auto_row(&list_pane)?;
    ui::margin(&list_pane, 6.0, 8.0, 6.0, 8.0)?;
    let list_title = ui::heading("Messages", Some(16.0), Some(600))?;
    ui::margin(&list_title, 6.0, 0.0, 6.0, 4.0)?;
    ui::set_row(&list_title, 0)?;
    ui::append(&list_pane, &list_title)?;
    let message_list = ui::stack(Orientation::Vertical, 2.0)?;
    let message_scroll = ui::scroll(&message_list)?;
    ui::set_row(&message_scroll, 1)?;
    ui::append(&list_pane, &message_scroll)?;
    let message_empty = ui::text("This folder is empty, or has not been synchronized yet.")?;
    ui::margin(&message_empty, 6.0, 12.0, 6.0, 6.0)?;
    ui::set_row(&message_empty, 1)?;
    ui::append(&list_pane, &message_empty)?;
    let load_more = ui::button("Load more")?;
    ui::set_visible(&load_more, false)?;
    ui::set_row(&load_more, 2)?;
    ui::append(&list_pane, &load_more)?;
    ui::set_column(&list_pane, 1)?;
    ui::append(&mail, &list_pane)?;

    // ------------------------------------------------------------ reader
    let reader_host = Grid::new().context("could not create the reader host")?;
    ui::star_row(&reader_host)?;
    ui::set_column(&reader_host, 2)?;
    ui::append(&mail, &reader_host)?;

    let reader_empty = ui::stack(Orientation::Vertical, 8.0)?;
    ui::margin(&reader_empty, 24.0, 40.0, 24.0, 24.0)?;
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
    let html_button = ui::button("Show HTML")?;
    let reply_button = ui::button("Reply")?;
    let reply_all_button = ui::button("Reply all")?;
    let forward_button = ui::button("Forward")?;
    let actions_spacer = ui::text("")?;
    ui::stretch_horizontal(&actions_spacer)?;
    let flag_button = ui::button("Star")?;
    let read_button = ui::button("Mark read")?;
    let archive_button = ui::button("Archive")?;
    let trash_button = ui::button("Trash")?;
    let junk_button = ui::button("Junk")?;
    let outbox_retry_button = ui::button("Retry")?;
    let outbox_discard_button = ui::button("Discard")?;
    let outbox_edit_button = ui::button("Edit")?;
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
    let remote_button = ui::button("Load remote content")?;
    ui::append(&remote_panel, &remote_text)?;
    ui::append(&remote_panel, &remote_button)?;
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

    // Keyboard accelerators for accessibility
    let accelerators = window
        .KeyboardAccelerators()
        .context("could not get keyboard accelerators collection")?;
    // Ctrl+F: Search
    let search_accel = KeyboardAccelerator::new().context("could not create search accelerator")?;
    search_accel.SetKey(VirtualKey::F)?;
    search_accel.SetModifiers(VirtualKeyModifiers::Control)?;
    search_accel.Invoked({
        let this = Rc::downgrade(&this);
        move |_, _| {
            if let Some(this) = this.upgrade() {
                this.run_search();
            }
        }
    })?;
    accelerators.Append(&search_accel)?;

    // F5: Sync
    let sync_accel = KeyboardAccelerator::new().context("could not create sync accelerator")?;
    sync_accel.SetKey(VirtualKey::F5)?;
    sync_accel.Invoked({
        let this = Rc::downgrade(&this);
        move |_, _| {
            if let Some(this) = this.upgrade() {
                this.sync_now();
            }
        }
    })?;
    accelerators.Append(&sync_accel)?;

    // Ctrl+Shift+A: Add account
    let add_accel =
        KeyboardAccelerator::new().context("could not create add account accelerator")?;
    add_accel.SetKey(VirtualKey::A)?;
    add_accel.SetModifiers(VirtualKeyModifiers::Control | VirtualKeyModifiers::Shift)?;
    add_accel.Invoked({
        let this = Rc::downgrade(&this);
        move |_, _| {
            if let Some(this) = this.upgrade() {
                this.add_account();
            }
        }
    })?;
    accelerators.Append(&add_accel)?;

    // Ctrl+Enter: Send (handled in composer)
    // Note: This will be added in the compose window

    // F1 / Ctrl+/: Show keyboard shortcuts help
    let help_accel = KeyboardAccelerator::new().context("could not create help accelerator")?;
    help_accel.SetKey(VirtualKey::F1)?;
    help_accel.Invoked({
        let this = Rc::downgrade(&this);
        move |_, _| {
            if let Some(this) = this.upgrade() {
                this.show_shortcuts_help();
            }
        }
    })?;
    accelerators.Append(&help_accel)?;

    // Ctrl+/ (Ctrl+OemQuestion): Alternative shortcut for help
    let help_accel2 = KeyboardAccelerator::new().context("could not create help accelerator 2")?;
    help_accel2.SetKey(VirtualKey::OemQuestion)?;
    help_accel2.SetModifiers(VirtualKeyModifiers::Control)?;
    help_accel2.Invoked({
        let this = Rc::downgrade(&this);
        move |_, _| {
            if let Some(this) = this.upgrade() {
                this.show_shortcuts_help();
            }
        }
    })?;
    accelerators.Append(&help_accel2)?;

    // Ctrl+,: Show preferences
    let prefs_accel =
        KeyboardAccelerator::new().context("could not create preferences accelerator")?;
    prefs_accel.SetKey(VirtualKey::OemComma)?;
    prefs_accel.SetModifiers(VirtualKeyModifiers::Control)?;
    prefs_accel.Invoked({
        let this = Rc::downgrade(&this);
        move |_, _| {
            if let Some(this) = this.upgrade() {
                this.show_preferences();
            }
        }
    })?;
    accelerators.Append(&prefs_accel)?;

    let this = Rc::new(MainWindow {
        app: Arc::clone(state),
        state: Mutex::new(State {
            selection: Selection {
                limit: state.config.ui.page_size.clamp(1, MAX_PAGE),
                ..Selection::default()
            },
            ..State::default()
        }),
        programmatic: AtomicBool::new(false),
        html_mode: AtomicBool::new(state.config.ui.html_enabled),
        closed: AtomicBool::new(false),
        events: ui::Revokers::default(),
        folder_events: ui::Revokers::default(),
        message_events: ui::Revokers::default(),
        attachment_events: ui::Revokers::default(),
        widgets: Widgets {
            window: window.clone(),
            title,
            banner,
            account_combo,
            add_account,
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
            remote_button,
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

    connect(&this)?;
    this.refresh_accounts();

    let _window = ui::keep_alive(&window);
    window.Activate().context("could not show the window")?;
    tracing::info!("WinUI window activated");
    Ok(window)
}

/// Create the main window and immediately open a compose window with the given draft.
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
    this.events.keep(ui::on_click(&this.widgets.welcome_add, {
        let this = Rc::clone(this);
        move || this.add_account()
    })?);
    this.events.keep(ui::on_click(&this.widgets.compose, {
        let this = Rc::clone(this);
        move || this.open_compose(OutgoingMessage::default())
    })?);
    this.events.keep(ui::on_click(&this.widgets.sync, {
        let this = Rc::clone(this);
        move || this.sync_now()
    })?);
    this.events.keep(ui::on_click(&this.widgets.search_button, {
        let this = Rc::clone(this);
        move || this.run_search()
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
    this.events.keep(ui::on_click(&this.widgets.outbox_retry_button, {
        let this = Rc::clone(this);
        move || this.outbox_retry_selected()
    })?);
    this.events.keep(ui::on_click(&this.widgets.outbox_discard_button, {
        let this = Rc::clone(this);
        move || this.outbox_discard_selected()
    })?);
    this.events.keep(ui::on_click(&this.widgets.outbox_edit_button, {
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
    this.events.keep(ui::on_click(&this.widgets.remote_button, {
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

fn folder_title(folder: &Folder) -> String {
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
    ui::append(&row, &ui::text(role_marker(folder.role))?)?;
    let name = ui::text(&folder_title(folder))?;
    ui::stretch_horizontal(&name)?;
    ui::append(&row, &name)?;
    if folder.unread_count > 0 {
        ui::append(&row, &ui::text(&folder.unread_count.to_string())?)?;
    }
    let button = ui::content_button(&row)?;
    ui::padding(&button, 0.0, 0.0, 0.0, 0.0)?;
    Ok(button)
}

fn message_row(message: &MessageHeader, now: i64) -> Result<Button> {
    let seen = message.is_seen();
    let column = ui::stack(Orientation::Vertical, 2.0)?;
    ui::padding(&column, 6.0, 6.0, 6.0, 6.0)?;

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

    if let Some(snippet) = message.snippet.as_deref() {
        let text = format::one_line(snippet, 140);
        if !text.is_empty() {
            ui::append(&column, &ui::text(&text)?)?;
        }
    }

    let button = ui::content_button(&column)?;
    ui::padding(&button, 0.0, 0.0, 0.0, 0.0)?;
    Ok(button)
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

    let button = ui::content_button(&column)?;
    ui::padding(&button, 0.0, 0.0, 0.0, 0.0)?;
    Ok(button)
}

/// Build the keyboard shortcuts help window using XAML.
fn build_shortcuts_window(parent: &Window) -> Result<Window> {
    // Create a dialog-style window for shortcuts
    let shortcuts_window = Window::new().context("could not create shortcuts window")?;
    shortcuts_window.SetTitle("Keyboard Shortcuts")?;

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
                    <TextBlock Grid.Column="0" Text="Ctrl+," FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
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
                    <TextBlock Grid.Column="0" Text="Enter" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Open selected message"/>
                </Grid>
                <Grid>
                    <Grid.ColumnDefinitions>
                        <ColumnDefinition Width="Auto"/>
                        <ColumnDefinition Width="24"/>
                        <ColumnDefinition Width="*"/>
                    </Grid.ColumnDefinitions>
                    <TextBlock Grid.Column="0" Text="F1 / Ctrl+?" FontFamily="Consolas,Monospace" FontWeight="SemiBold"/>
                    <TextBlock Grid.Column="2" Text="Show this help"/>
                </Grid>
            </StackPanel>
        </StackPanel>
        "#,
    )?;

    shortcuts_window.SetContent(&content)?;
    shortcuts_window.SetWidth(400.0)?;
    shortcuts_window.SetHeight(400.0)?;

    Ok(shortcuts_window)
}
