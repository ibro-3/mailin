//! The three-pane mail window: folders, message list and reader.
//!
//! The window owns no mail state of its own: everything is read from the core
//! store. A small timer drains the core event channel and refreshes the widgets
//! that changed, which keeps the UI correct regardless of whether an update
//! came from the network worker or from a local edit.

use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gio::prelude::*;
use gtk::{gio, glib, pango};
use mailin_core::{
    Account, AccountId, Event, Folder, FolderId, FolderRole, MessageHeader, MessageId,
};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;

use crate::account_dialog;
use crate::app::AppState;
use crate::format;

/// How often the core event channel is drained on the GTK main loop.
const EVENT_POLL: Duration = Duration::from_millis(200);
/// Debounce for the search entry.
const SEARCH_DELAY: Duration = Duration::from_millis(250);
const SEARCH_LIMIT: u32 = 200;
const MAX_PAGE: u32 = 1_000;

#[derive(Debug, Default)]
struct Selection {
    account: Option<AccountId>,
    folder: Option<FolderId>,
    message: Option<MessageId>,
    query: String,
    search: bool,
    /// Number of headers requested for the current folder.
    limit: u32,
}

struct Widgets {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    stack: gtk::Stack,
    toast: adw::ToastOverlay,
    banner: adw::Banner,
    account_dropdown: gtk::DropDown,
    add_button: gtk::Button,
    empty_add: gtk::Button,
    folder_list: gtk::ListBox,
    folder_stack: gtk::Stack,
    list_title: gtk::Label,
    message_list: gtk::ListBox,
    message_empty: adw::StatusPage,
    list_stack: gtk::Stack,
    load_more: gtk::Button,
    search: gtk::SearchEntry,
    reader_stack: gtk::Stack,
    reader_subject: gtk::Label,
    reader_from: gtk::Label,
    reader_to: gtk::Label,
    reader_cc: gtk::Label,
    reader_date: gtk::Label,
    reader_attachments: gtk::Label,
    reader_body: gtk::Box,
    read_button: gtk::Button,
    flag_button: gtk::ToggleButton,
}

pub struct MainWindow {
    app: Rc<AppState>,
    selection: RefCell<Selection>,
    accounts: RefCell<Vec<Account>>,
    folders: RefCell<Vec<Folder>>,
    messages: RefCell<Vec<MessageHeader>>,
    /// Set while widgets are being updated from code; signal handlers ignore
    /// those changes so a refresh never re-enters itself.
    programmatic: Cell<bool>,
    search_generation: Cell<u64>,
    closed: Rc<Cell<bool>>,
    widgets: Widgets,
}

impl MainWindow {
    pub fn new(application: &adw::Application, app: Rc<AppState>) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("Mailin")
            .default_width(1180)
            .default_height(760)
            .build();

        let title = adw::WindowTitle::new("Mailin", "No account");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));

        let sidebar_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Toggle folders")
            .active(true)
            .build();
        sidebar_toggle.add_css_class("flat");
        header.pack_start(&sidebar_toggle);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search mail")
            .width_chars(18)
            .build();
        let sync_button = icon_button("view-refresh-symbolic", "Synchronize now (F5)");
        header.pack_end(&sync_button);
        header.pack_end(&search);

        // ---------------------------------------------------------- sidebar
        let account_dropdown = gtk::DropDown::new(
            Some(gtk::StringList::new(&[] as &[&str])),
            None::<gtk::Expression>,
        );
        account_dropdown.set_hexpand(true);
        account_dropdown.set_tooltip_text(Some("Switch account"));
        let add_button = icon_button("list-add-symbolic", "Add account (Ctrl+Shift+A)");
        let account_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        account_bar.set_margin_top(8);
        account_bar.set_margin_bottom(8);
        account_bar.set_margin_start(12);
        account_bar.set_margin_end(12);
        account_bar.append(&account_dropdown);
        account_bar.append(&add_button);

        let folder_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        folder_list.add_css_class("navigation-sidebar");
        let folder_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&folder_list)
            .build();

        let folder_empty = adw::StatusPage::builder()
            .icon_name("folder-symbolic")
            .title("No folders")
            .description("Folders appear after the first synchronization.")
            .build();
        let folder_stack = gtk::Stack::new();
        folder_stack.set_vexpand(true);
        folder_stack.add_named(&folder_scroller, Some("list"));
        folder_stack.add_named(&folder_empty, Some("empty"));
        folder_stack.set_visible_child_name("empty");

        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.set_width_request(250);
        sidebar.append(&account_bar);
        sidebar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        sidebar.append(&folder_stack);

        // ------------------------------------------------------ message list
        let list_title = gtk::Label::new(Some("Inbox"));
        list_title.set_xalign(0.0);
        list_title.set_hexpand(true);
        list_title.set_ellipsize(pango::EllipsizeMode::End);
        list_title.add_css_class("heading");
        let list_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        list_header.set_margin_top(10);
        list_header.set_margin_bottom(10);
        list_header.set_margin_start(14);
        list_header.set_margin_end(14);
        list_header.append(&list_title);

        let message_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        message_list.add_css_class("navigation-sidebar");
        let message_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&message_list)
            .build();
        let message_empty = adw::StatusPage::builder()
            .icon_name("mail-unread-symbolic")
            .title("No messages")
            .description("This folder is empty, or has not been synchronized yet.")
            .build();
        let list_stack = gtk::Stack::new();
        list_stack.set_vexpand(true);
        list_stack.add_named(&message_scroller, Some("list"));
        list_stack.add_named(&message_empty, Some("empty"));
        list_stack.set_visible_child_name("empty");

        let load_more = gtk::Button::with_label("Load more");
        load_more.add_css_class("flat");
        load_more.set_margin_top(6);
        load_more.set_margin_bottom(6);
        load_more.set_visible(false);

        let list_pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list_pane.set_size_request(300, -1);
        list_pane.append(&list_header);
        list_pane.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        list_pane.append(&list_stack);
        list_pane.append(&load_more);

        // ------------------------------------------------------------ reader
        let reader_subject = gtk::Label::new(None);
        reader_subject.set_wrap(true);
        reader_subject.set_xalign(0.0);
        reader_subject.add_css_class("title-2");

        let reader_from = gtk::Label::new(None);
        reader_from.set_wrap(true);
        reader_from.set_xalign(0.0);
        reader_from.add_css_class("heading");

        let reader_to = gtk::Label::new(None);
        reader_to.set_wrap(true);
        reader_to.set_xalign(0.0);
        reader_to.add_css_class("dim-label");
        reader_to.add_css_class("caption");

        let reader_cc = gtk::Label::new(None);
        reader_cc.set_wrap(true);
        reader_cc.set_xalign(0.0);
        reader_cc.add_css_class("dim-label");
        reader_cc.add_css_class("caption");

        let reader_date = gtk::Label::new(None);
        reader_date.set_xalign(0.0);
        reader_date.add_css_class("dim-label");
        reader_date.add_css_class("caption");

        let reader_attachments = gtk::Label::new(None);
        reader_attachments.set_wrap(true);
        reader_attachments.set_xalign(0.0);
        reader_attachments.add_css_class("dim-label");
        reader_attachments.add_css_class("caption");
        reader_attachments.set_visible(false);

        let reader_body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        reader_body.set_margin_top(6);
        reader_body.set_margin_bottom(24);

        let flag_button = gtk::ToggleButton::builder()
            .icon_name("non-starred-symbolic")
            .tooltip_text("Star message")
            .build();
        flag_button.add_css_class("flat");
        let read_button = icon_button("mail-mark-read-symbolic", "Mark as read");

        let reader_actions = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        reader_actions.set_halign(gtk::Align::End);
        reader_actions.append(&flag_button);
        reader_actions.append(&read_button);

        let reader_content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        reader_content.set_margin_top(18);
        reader_content.set_margin_bottom(18);
        reader_content.set_margin_start(18);
        reader_content.set_margin_end(18);
        reader_content.append(&reader_actions);
        reader_content.append(&reader_subject);
        reader_content.append(&reader_from);
        reader_content.append(&reader_to);
        reader_content.append(&reader_cc);
        reader_content.append(&reader_date);
        reader_content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        reader_content.append(&reader_attachments);
        reader_content.append(&reader_body);

        let clamp = adw::Clamp::builder()
            .maximum_size(900)
            .tightening_threshold(700)
            .child(&reader_content)
            .build();
        let reader_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        let reader_empty = adw::StatusPage::builder()
            .icon_name("mail-unread-symbolic")
            .title("Select a message")
            .description("Pick a message from the list to read it here.")
            .build();
        let reader_stack = gtk::Stack::new();
        reader_stack.set_vexpand(true);
        reader_stack.set_hexpand(true);
        reader_stack.set_size_request(360, -1);
        reader_stack.add_named(&reader_empty, Some("empty"));
        reader_stack.add_named(&reader_scroller, Some("message"));
        reader_stack.set_visible_child_name("empty");

        // ------------------------------------------------------------- panes
        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&list_pane)
            .end_child(&reader_stack)
            .resize_start_child(true)
            .shrink_start_child(false)
            .resize_end_child(true)
            .shrink_end_child(false)
            .build();
        paned.set_position(380);

        let split = adw::OverlaySplitView::builder()
            .sidebar(&sidebar)
            .content(&paned)
            .show_sidebar(true)
            .min_sidebar_width(220.0)
            .max_sidebar_width(320.0)
            .sidebar_position(gtk::PackType::Start)
            .build();
        split
            .bind_property("show-sidebar", &sidebar_toggle, "active")
            .flags(glib::BindingFlags::SYNC_CREATE | glib::BindingFlags::BIDIRECTIONAL)
            .build();

        // ------------------------------------------------------- empty state
        let empty_add = gtk::Button::with_label("Add Account…");
        empty_add.add_css_class("suggested-action");
        empty_add.add_css_class("pill");
        empty_add.set_halign(gtk::Align::Center);
        let empty_page = adw::StatusPage::builder()
            .icon_name("mail-unread-symbolic")
            .title("Welcome to Mailin")
            .description("Add a mail account to read and send mail.")
            .child(&empty_add)
            .build();

        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.add_named(&empty_page, Some("empty"));
        stack.add_named(&split, Some("mail"));
        stack.set_visible_child_name("empty");

        let banner = adw::Banner::new("");
        banner.set_revealed(false);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&banner);
        content.append(&stack);

        let toast = adw::ToastOverlay::new();
        toast.set_child(Some(&content));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&toast));
        window.set_content(Some(&toolbar));

        let this = Rc::new(Self {
            app,
            selection: RefCell::new(Selection::default()),
            accounts: RefCell::new(Vec::new()),
            folders: RefCell::new(Vec::new()),
            messages: RefCell::new(Vec::new()),
            programmatic: Cell::new(false),
            search_generation: Cell::new(0),
            closed: Rc::new(Cell::new(false)),
            widgets: Widgets {
                window,
                title,
                stack,
                toast,
                banner,
                account_dropdown,
                add_button,
                empty_add,
                folder_list,
                folder_stack,
                list_title,
                message_list,
                message_empty,
                list_stack,
                load_more,
                search,
                reader_stack,
                reader_subject,
                reader_from,
                reader_to,
                reader_cc,
                reader_date,
                reader_attachments,
                reader_body,
                read_button,
                flag_button,
            },
        });

        Self::connect(&this);
        this.refresh_accounts();
        this
    }

    pub fn present(&self) {
        self.widgets.window.present();
    }

    /// Wire signals and the event pump. The handlers hold an `Rc<Self>`, which
    /// keeps the window alive for the (single, process length) window
    /// lifetime; the event pump stops as soon as the window is closed.
    fn connect(this: &Rc<Self>) {
        this.widgets.window.connect_close_request({
            let closed = Rc::clone(&this.closed);
            move |_| {
                closed.set(true);
                glib::Propagation::Proceed
            }
        });

        this.widgets.account_dropdown.connect_selected_notify({
            let this = Rc::clone(this);
            move |dropdown| {
                if this.programmatic.get() {
                    return;
                }
                let index = dropdown.selected();
                let account = this
                    .accounts
                    .borrow()
                    .get(index as usize)
                    .map(|account| account.id);
                if let Some(account) = account {
                    this.select_account(account);
                }
            }
        });

        this.widgets.folder_list.connect_row_selected({
            let this = Rc::clone(this);
            move |_, row| {
                if this.programmatic.get() {
                    return;
                }
                let Some(row) = row else {
                    return;
                };
                let folder = this.folders.borrow().get(row.index() as usize).cloned();
                if let Some(folder) = folder {
                    this.select_folder(&folder);
                }
            }
        });

        this.widgets.message_list.connect_row_activated({
            let this = Rc::clone(this);
            move |_, row| {
                if this.programmatic.get() {
                    return;
                }
                let message = this.messages.borrow().get(row.index() as usize).cloned();
                if let Some(message) = message {
                    this.open_message(&message);
                }
            }
        });

        this.widgets.load_more.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.request_more()
        });

        this.widgets.read_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.toggle_seen()
        });

        this.widgets.flag_button.connect_toggled({
            let this = Rc::clone(this);
            move |button| {
                if this.programmatic.get() {
                    return;
                }
                this.toggle_flagged(button.is_active());
            }
        });

        this.widgets.search.connect_search_changed({
            let this = Rc::clone(this);
            move |entry| {
                if this.programmatic.get() {
                    return;
                }
                let query = entry.text().trim().to_owned();
                if query.is_empty() {
                    this.search_generation
                        .set(this.search_generation.get().wrapping_add(1));
                    this.clear_search();
                } else {
                    Self::schedule_search(&this);
                }
            }
        });

        this.widgets.search.connect_activate({
            let this = Rc::clone(this);
            move |_| this.run_search()
        });

        this.widgets.add_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| Self::add_account(&this)
        });
        this.widgets.empty_add.connect_clicked({
            let this = Rc::clone(this);
            move |_| Self::add_account(&this)
        });

        let add_action = gio::SimpleAction::new("add-account", None);
        add_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| Self::add_account(&this)
        });
        this.widgets.window.add_action(&add_action);

        if let Some(application) = this.widgets.window.application() {
            application.set_accels_for_action("win.search", &["<Control>f"]);
            application.set_accels_for_action("win.sync", &["F5"]);
            application.set_accels_for_action("win.add-account", &["<Control><Shift>a"]);
        }

        let search_action = gio::SimpleAction::new("search", None);
        search_action.connect_activate({
            let search = this.widgets.search.clone();
            move |_, _| {
                search.grab_focus();
            }
        });
        this.widgets.window.add_action(&search_action);

        let sync_action = gio::SimpleAction::new("sync", None);
        sync_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.sync_now()
        });
        this.widgets.window.add_action(&sync_action);

        // Event pump. Polling the tokio broadcast channel keeps every store
        // read on the GTK main thread without an extra bridge thread.
        let mut events = this.app.mailin.subscribe();
        let pump = Rc::clone(this);
        let closed = Rc::clone(&this.closed);
        glib::timeout_add_local(EVENT_POLL, move || {
            if closed.get() {
                return glib::ControlFlow::Break;
            }
            pump.drain_events(&mut events);
            glib::ControlFlow::Continue
        });
    }

    // ------------------------------------------------------------- accounts

    fn refresh_accounts(&self) {
        let accounts = match self.app.mailin.list_accounts() {
            Ok(accounts) => accounts,
            Err(error) => {
                self.show_error(&error.to_string());
                return;
            }
        };
        let current = self.selection.borrow().account;
        let keep = current.filter(|id| accounts.iter().any(|account| account.id == *id));

        self.programmatic.set(true);
        let names: Vec<String> = accounts
            .iter()
            .map(|account| format!("{} <{}>", account.display_name, account.email))
            .collect();
        let model = gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
        self.widgets.account_dropdown.set_model(Some(&model));
        if let Some(index) =
            keep.and_then(|id| accounts.iter().position(|account| account.id == id))
        {
            self.widgets.account_dropdown.set_selected(index as u32);
        }
        self.programmatic.set(false);
        *self.accounts.borrow_mut() = accounts;

        if self.accounts.borrow().is_empty() {
            {
                let mut selection = self.selection.borrow_mut();
                selection.account = None;
                selection.folder = None;
                selection.message = None;
                selection.search = false;
                selection.query.clear();
            }
            self.widgets.stack.set_visible_child_name("empty");
            self.widgets.title.set_subtitle("No account");
            self.clear_messages();
            self.widgets.list_title.set_text("No messages");
            return;
        }

        self.widgets.stack.set_visible_child_name("mail");
        match keep {
            Some(_) => {
                self.update_subtitle();
                self.refresh_folders();
            }
            None => {
                let first = self.accounts.borrow()[0].id;
                self.select_account(first);
            }
        }
    }

    fn select_account(&self, id: AccountId) {
        {
            let mut selection = self.selection.borrow_mut();
            if selection.account != Some(id) {
                selection.folder = None;
                selection.message = None;
                selection.search = false;
                selection.query.clear();
            }
            selection.account = Some(id);
            if selection.limit == 0 {
                selection.limit = self.page_size();
            }
        }
        self.programmatic.set(true);
        if let Some(index) = self
            .accounts
            .borrow()
            .iter()
            .position(|account| account.id == id)
        {
            self.widgets.account_dropdown.set_selected(index as u32);
        }
        self.programmatic.set(false);
        self.update_subtitle();
        self.refresh_folders();
    }

    fn update_subtitle(&self) {
        let account = self.selection.borrow().account;
        let subtitle = account
            .and_then(|id| self.accounts.borrow().iter().find(|a| a.id == id).cloned())
            .map(|account| account.email)
            .unwrap_or_else(|| "No account".to_owned());
        self.widgets.title.set_subtitle(&subtitle);
    }

    // -------------------------------------------------------------- folders

    fn refresh_folders(&self) {
        let Some(account_id) = self.selection.borrow().account else {
            self.clear_folders();
            return;
        };
        let folders = match self.app.mailin.list_folders(&account_id) {
            Ok(folders) => folders,
            Err(error) => {
                self.show_error(&error.to_string());
                return;
            }
        };

        let selected_id = self.selection.borrow().folder;
        let index = selected_id
            .and_then(|id| folders.iter().position(|folder| folder.id == id))
            .or_else(|| {
                folders
                    .iter()
                    .position(|folder| folder.role == FolderRole::Inbox)
            })
            .or_else(|| (!folders.is_empty()).then_some(0));

        self.programmatic.set(true);
        clear_list(&self.widgets.folder_list);
        for folder in &folders {
            self.widgets.folder_list.append(&folder_row(folder));
        }
        if let Some(index) = index {
            if let Some(row) = self.widgets.folder_list.row_at_index(index as i32) {
                self.widgets.folder_list.select_row(Some(&row));
            }
        }
        self.widgets
            .folder_stack
            .set_visible_child_name(if folders.is_empty() { "empty" } else { "list" });
        self.programmatic.set(false);
        *self.folders.borrow_mut() = folders;

        match index {
            Some(index) => {
                let folder = self.folders.borrow()[index].clone();
                if self.selection.borrow().folder != Some(folder.id) {
                    self.select_folder(&folder);
                }
            }
            None => {
                self.widgets.list_title.set_text("No folders");
                self.clear_messages();
            }
        }
    }

    fn clear_folders(&self) {
        self.programmatic.set(true);
        clear_list(&self.widgets.folder_list);
        self.widgets.folder_stack.set_visible_child_name("empty");
        self.programmatic.set(false);
        self.folders.borrow_mut().clear();
        self.clear_messages();
    }

    fn select_folder(&self, folder: &Folder) {
        {
            let mut selection = self.selection.borrow_mut();
            if selection.folder == Some(folder.id) && !selection.search {
                return;
            }
            selection.folder = Some(folder.id);
            selection.message = None;
            selection.search = false;
            selection.query.clear();
            selection.limit = self.page_size();
        }
        self.programmatic.set(true);
        self.widgets.search.set_text("");
        self.programmatic.set(false);
        self.widgets.list_title.set_text(&folder_title(folder));
        self.show_reader_empty();
        self.refresh_messages();
        let stale = folder
            .last_synced_at
            .is_none_or(|synced| format::now_unix().saturating_sub(synced) >= 60);
        if stale {
            self.app.engine.sync_folder(folder.id);
        }
    }

    // ------------------------------------------------------------- messages

    fn page_size(&self) -> u32 {
        self.app.config.ui.page_size.clamp(1, MAX_PAGE)
    }

    fn refresh_messages(&self) {
        let (search, query, folder_id, limit) = {
            let selection = self.selection.borrow();
            (
                selection.search,
                selection.query.clone(),
                selection.folder,
                selection.limit,
            )
        };

        if search {
            let Some(account_id) = self.selection.borrow().account else {
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

    fn show_messages(&self, messages: Vec<MessageHeader>, title: String, more: bool) {
        let selected = self.selection.borrow().message;
        let now = format::now_unix();
        let empty = messages.is_empty();

        self.programmatic.set(true);
        clear_list(&self.widgets.message_list);
        for message in &messages {
            self.widgets.message_list.append(&message_row(message, now));
        }
        if let Some(id) = selected {
            if let Some(index) = messages.iter().position(|message| message.id == id) {
                if let Some(row) = self.widgets.message_list.row_at_index(index as i32) {
                    self.widgets.message_list.select_row(Some(&row));
                }
            }
        }
        self.programmatic.set(false);

        *self.messages.borrow_mut() = messages;
        self.widgets.list_title.set_text(&title);
        let searching = self.selection.borrow().search;
        if empty && searching {
            self.widgets.message_empty.set_title("No results");
            self.widgets
                .message_empty
                .set_description(Some("Try a different search term."));
        } else if empty {
            self.widgets.message_empty.set_title("No messages");
            self.widgets.message_empty.set_description(Some(
                "This folder is empty, or has not been synchronized yet.",
            ));
        }
        self.widgets
            .list_stack
            .set_visible_child_name(if empty { "empty" } else { "list" });
        self.widgets.load_more.set_visible(more);
    }

    fn clear_messages(&self) {
        self.programmatic.set(true);
        clear_list(&self.widgets.message_list);
        self.programmatic.set(false);
        self.messages.borrow_mut().clear();
        self.selection.borrow_mut().message = None;
        self.widgets.list_stack.set_visible_child_name("empty");
        self.widgets.load_more.set_visible(false);
        self.show_reader_empty();
    }

    fn request_more(&self) {
        {
            let mut selection = self.selection.borrow_mut();
            if selection.search {
                return;
            }
            selection.limit = (selection.limit + self.page_size()).min(MAX_PAGE);
        }
        self.refresh_messages();
    }

    fn schedule_search(this: &Rc<Self>) {
        let generation = this.search_generation.get().wrapping_add(1);
        this.search_generation.set(generation);
        let this = Rc::clone(this);
        glib::timeout_add_local_once(SEARCH_DELAY, move || {
            if this.search_generation.get() == generation {
                this.run_search();
            }
        });
    }

    fn run_search(&self) {
        self.search_generation
            .set(self.search_generation.get().wrapping_add(1));
        let query = self.widgets.search.text().trim().to_owned();
        if query.is_empty() {
            self.clear_search();
            return;
        }
        {
            let mut selection = self.selection.borrow_mut();
            selection.search = true;
            selection.query = query;
        }
        self.refresh_messages();
    }

    fn clear_search(&self) {
        let was_searching = {
            let mut selection = self.selection.borrow_mut();
            let was_searching = selection.search;
            selection.search = false;
            selection.query.clear();
            was_searching
        };
        if was_searching {
            self.refresh_messages();
        }
    }

    // --------------------------------------------------------------- reader

    fn open_message(&self, message: &MessageHeader) {
        self.selection.borrow_mut().message = Some(message.id);
        self.widgets.reader_stack.set_visible_child_name("message");
        self.render_headers(message);
        self.render_attachments(message.id);
        self.render_body(message.id);
        self.update_actions(message);
        if self.app.config.ui.mark_read_on_open && !message.is_seen() {
            self.app
                .engine
                .set_flags(message.id, vec!["\\Seen".to_owned()], Vec::new());
        }
    }

    fn render_headers(&self, message: &MessageHeader) {
        self.widgets
            .reader_subject
            .set_text(&format::subject_label(message));
        self.widgets.reader_from.set_text(&message.from_display());

        if message.to_addrs.is_empty() {
            self.widgets.reader_to.set_visible(false);
        } else {
            self.widgets
                .reader_to
                .set_text(&format!("To: {}", message.to_addrs));
            self.widgets.reader_to.set_visible(true);
        }
        if message.cc_addrs.is_empty() {
            self.widgets.reader_cc.set_visible(false);
        } else {
            self.widgets
                .reader_cc
                .set_text(&format!("Cc: {}", message.cc_addrs));
            self.widgets.reader_cc.set_visible(true);
        }

        let timestamp = message.sent_at.unwrap_or(message.received_at);
        self.widgets
            .reader_date
            .set_text(&format::full_date(timestamp));
    }

    fn render_body(&self, message_id: MessageId) {
        clear_box(&self.widgets.reader_body);
        match self.app.mailin.message_body(message_id) {
            Ok(Some((text, html))) => {
                let widget = match (text, html) {
                    (Some(text), _) if !text.trim().is_empty() => text_body(&text),
                    (Some(_), _) => notice_body("This message has no text body."),
                    (None, Some(_)) => notice_body(
                        "This message only provides an HTML body. It will be rendered in the \
                         HTML viewer.",
                    ),
                    (None, None) => notice_body("This message has no readable body."),
                };
                self.widgets.reader_body.append(&widget);
            }
            Ok(None) => {
                let fetched = self
                    .app
                    .mailin
                    .message(message_id)
                    .ok()
                    .flatten()
                    .is_some_and(|message| message.body_fetched);
                if fetched {
                    self.widgets
                        .reader_body
                        .append(&notice_body("This message has no readable body."));
                } else {
                    self.widgets
                        .reader_body
                        .append(&loading_body("Loading message…"));
                    self.app.engine.fetch_body(message_id);
                }
            }
            Err(error) => {
                self.widgets
                    .reader_body
                    .append(&notice_body(&error.to_string()));
            }
        }
    }

    fn render_attachments(&self, message_id: MessageId) {
        let attachments = self
            .app
            .mailin
            .store()
            .list_attachments(message_id)
            .unwrap_or_default();
        let mut visible = attachments
            .into_iter()
            .filter(|attachment| !attachment.is_inline)
            .collect::<Vec<_>>();
        if visible.is_empty() {
            self.widgets.reader_attachments.set_visible(false);
            return;
        }
        visible.truncate(4);
        let count = visible.len();
        let names = visible
            .iter()
            .map(|attachment| {
                let name = attachment
                    .filename
                    .clone()
                    .unwrap_or_else(|| attachment.mime_type.clone());
                match attachment.size {
                    Some(size) => format!("{name} ({})", format::file_size(size)),
                    None => name,
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        self.widgets.reader_attachments.set_text(&format!(
            "{count} attachment{}: {names}",
            if count == 1 { "" } else { "s" }
        ));
        self.widgets
            .reader_attachments
            .set_tooltip_text(Some("Downloading attachments arrives in milestone 4"));
        self.widgets.reader_attachments.set_visible(true);
    }

    fn update_actions(&self, message: &MessageHeader) {
        let seen = message.is_seen();
        let flagged = message.is_flagged();
        self.programmatic.set(true);
        self.widgets.flag_button.set_active(flagged);
        self.widgets.flag_button.set_icon_name(if flagged {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
        self.widgets.flag_button.set_tooltip_text(Some(if flagged {
            "Remove star"
        } else {
            "Star message"
        }));
        self.widgets.read_button.set_icon_name(if seen {
            "mail-mark-unread-symbolic"
        } else {
            "mail-mark-read-symbolic"
        });
        self.widgets.read_button.set_tooltip_text(Some(if seen {
            "Mark as unread"
        } else {
            "Mark as read"
        }));
        self.programmatic.set(false);
    }

    fn show_reader_empty(&self) {
        self.widgets.reader_stack.set_visible_child_name("empty");
    }

    fn selected_message(&self) -> Option<MessageHeader> {
        let id = self.selection.borrow().message?;
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

    fn toggle_flagged(&self, flagged: bool) {
        let Some(message) = self.selected_message() else {
            return;
        };
        if flagged {
            self.app
                .engine
                .set_flags(message.id, vec!["\\Flagged".to_owned()], Vec::new());
        } else {
            self.app
                .engine
                .set_flags(message.id, Vec::new(), vec!["\\Flagged".to_owned()]);
        }
    }

    // --------------------------------------------------------------- events

    fn drain_events(&self, events: &mut broadcast::Receiver<Event>) {
        let mut accounts_changed = false;
        let mut folders_changed = false;
        let mut messages_changed = false;
        let mut body_ready = None;
        let mut account_error = None;

        loop {
            match events.try_recv() {
                Ok(Event::AccountsChanged) => accounts_changed = true,
                Ok(Event::FoldersChanged { .. }) => folders_changed = true,
                Ok(Event::MessagesChanged { folder_id }) => {
                    let selection = self.selection.borrow();
                    if selection.search || selection.folder == Some(folder_id) {
                        messages_changed = true;
                    }
                }
                Ok(Event::BodyReady { message_id }) => body_ready = Some(message_id),
                Ok(Event::OutboxChanged) => {}
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

        if accounts_changed {
            self.refresh_accounts();
        } else if folders_changed {
            self.refresh_folders();
        }
        if messages_changed {
            self.refresh_messages();
        }
        if let Some(message_id) = body_ready {
            if self.selection.borrow().message == Some(message_id) {
                self.render_body(message_id);
                self.render_attachments(message_id);
                if let Some(message) = self.selected_message() {
                    self.render_headers(&message);
                    self.update_actions(&message);
                }
            }
        }
        if let Some(message) = account_error {
            self.show_error(&message);
        }
    }

    fn show_error(&self, message: &str) {
        let mut text = message.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().count() > 180 {
            text = text.chars().take(179).collect();
            text.push('…');
        }
        self.widgets.banner.set_title(&format!("Problem: {text}"));
        self.widgets.banner.set_revealed(true);
        tracing::debug!(%message, "account error surfaced in the UI");
    }

    // -------------------------------------------------------------- actions

    fn sync_now(&self) {
        let Some(account) = self.selection.borrow().account else {
            return;
        };
        self.widgets.banner.set_revealed(false);
        self.app.engine.sync_account(account);
        self.widgets
            .toast
            .add_toast(adw::Toast::new("Synchronizing…"));
    }

    fn add_account(this: &Rc<Self>) {
        account_dialog::present(&this.widgets.window, Rc::clone(&this.app), {
            let this = Rc::clone(this);
            move |id| {
                this.refresh_accounts();
                this.select_account(id);
            }
        });
    }
}

// -------------------------------------------------------------------- widgets

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .build();
    button.add_css_class("flat");
    button
}

fn clear_list(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn role_icon(role: FolderRole) -> &'static str {
    match role {
        FolderRole::Inbox => "mail-unread-symbolic",
        FolderRole::Sent => "mail-sent-symbolic",
        FolderRole::Drafts => "document-edit-symbolic",
        FolderRole::Trash => "user-trash-symbolic",
        FolderRole::Junk => "dialog-warning-symbolic",
        FolderRole::Archive => "package-x-generic-symbolic",
        FolderRole::Other => "folder-symbolic",
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

fn folder_row(folder: &Folder) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let icon = gtk::Image::from_icon_name(role_icon(folder.role));
    icon.set_pixel_size(16);
    content.append(&icon);

    let label = gtk::Label::new(Some(&folder_title(folder)));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(pango::EllipsizeMode::End);
    if folder.unread_count > 0 {
        label.add_css_class("heading");
    }
    content.append(&label);

    if folder.unread_count > 0 {
        let badge = gtk::Label::new(Some(&folder.unread_count.to_string()));
        badge.add_css_class("caption");
        badge.add_css_class("dim-label");
        content.append(&badge);
    }

    row.set_child(Some(&content));
    if folder_title(folder) != folder.name {
        row.set_tooltip_text(Some(&folder.name));
    }
    row
}

fn message_row(message: &MessageHeader, now: i64) -> gtk::ListBoxRow {
    let seen = message.is_seen();
    let row = gtk::ListBoxRow::new();
    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    outer.set_margin_top(8);
    outer.set_margin_bottom(8);
    outer.set_margin_start(12);
    outer.set_margin_end(12);

    let dot = gtk::Label::new(Some("●"));
    dot.add_css_class("accent");
    dot.set_valign(gtk::Align::Start);
    dot.set_margin_top(4);
    if seen {
        dot.set_opacity(0.0);
    }
    outer.append(&dot);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 3);
    column.set_hexpand(true);

    let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let sender = gtk::Label::new(Some(&format::sender_label(message)));
    sender.set_xalign(0.0);
    sender.set_hexpand(true);
    sender.set_ellipsize(pango::EllipsizeMode::End);
    if !seen {
        sender.add_css_class("heading");
    }
    line.append(&sender);

    if message.is_flagged() {
        let star = gtk::Image::from_icon_name("starred-symbolic");
        star.set_pixel_size(12);
        line.append(&star);
    }
    if message.has_attachments {
        let clip = gtk::Image::from_icon_name("mail-attachment-symbolic");
        clip.set_pixel_size(12);
        clip.add_css_class("dim-label");
        line.append(&clip);
    }

    let timestamp = message.sent_at.unwrap_or(message.received_at);
    let time = gtk::Label::new(Some(&format::list_time(now, timestamp)));
    time.add_css_class("caption");
    time.add_css_class("dim-label");
    line.append(&time);
    column.append(&line);

    let subject = gtk::Label::new(Some(&format::subject_label(message)));
    subject.set_xalign(0.0);
    subject.set_ellipsize(pango::EllipsizeMode::End);
    if !seen {
        subject.add_css_class("heading");
    }
    column.append(&subject);

    if let Some(snippet) = message.snippet.as_deref() {
        let text = format::one_line(snippet, 160);
        if !text.is_empty() {
            let label = gtk::Label::new(Some(&text));
            label.set_xalign(0.0);
            label.set_ellipsize(pango::EllipsizeMode::End);
            label.add_css_class("caption");
            label.add_css_class("dim-label");
            column.append(&label);
        }
    }

    outer.append(&column);
    row.set_child(Some(&outer));
    row.set_tooltip_text(Some(&format!(
        "{} — {}",
        message.from_display(),
        format::subject_label(message)
    )));
    row
}

fn text_body(text: &str) -> gtk::Widget {
    let label = gtk::Label::new(Some(text));
    label.set_wrap(true);
    label.set_wrap_mode(pango::WrapMode::WordChar);
    label.set_selectable(true);
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.upcast()
}

fn notice_body(text: &str) -> gtk::Widget {
    let label = gtk::Label::new(Some(text));
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.add_css_class("dim-label");
    label.upcast()
}

fn loading_body(text: &str) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.set_halign(gtk::Align::Center);
    row.set_margin_top(24);
    let spinner = gtk::Spinner::new();
    spinner.start();
    row.append(&spinner);
    let label = gtk::Label::new(Some(text));
    label.add_css_class("dim-label");
    row.append(&label);
    row.upcast()
}
