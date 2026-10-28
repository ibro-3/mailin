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
use gdk::{Key, ModifierType};
use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::{gio, glib, pango};
use mailin_core::{
    Account, AccountId, AttachmentMeta, Event, Folder, FolderId, FolderRole, MessageHeader,
    MessageId, OutboxEntry, OutboxState, OutgoingEnvelope, OutgoingMessage,
};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;
use webkit6::prelude::*;

use crate::app::AppState;
use crate::compose;
use crate::format;
use crate::html;

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
    /// When `true`, the message list shows outbox entries instead of the selected folder.
    show_outbox: bool,
    message: Option<MessageId>,
    /// Outbox entries currently shown (when show_outbox is true)
    outbox_entries: Vec<OutboxEntry>,
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
    account_menu_button: gtk::MenuButton,
    compose_button: gtk::Button,
    empty_add: gtk::Button,
    folder_list: gtk::ListBox,
    folder_stack: gtk::Stack,
    list_title: gtk::Label,
    archive_button: gtk::Button,
    junk_button: gtk::Button,
    trash_button: gtk::Button,
    message_menu_button: gtk::MenuButton,
    bulk_bar: gtk::Revealer,
    bulk_count: gtk::Label,
    bulk_read: gtk::Button,
    bulk_unread: gtk::Button,
    bulk_flag: gtk::Button,
    bulk_unflag: gtk::Button,
    bulk_move: gtk::Button,
    bulk_clear: gtk::Button,
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
    remote_banner: adw::Banner,
    reader_attachments: gtk::Box,
    reader_body: gtk::Box,
    read_button: gtk::Button,
    flag_button: gtk::ToggleButton,
    html_button: gtk::ToggleButton,
    reply_button: gtk::Button,
    reply_all_button: gtk::Button,
    forward_button: gtk::Button,
}

/// An attachment whose bytes are needed before the save dialog can open.
#[derive(Debug, Clone)]
struct PendingAttachment {
    message_id: MessageId,
    attachment: AttachmentMeta,
}

/// Holder for a mailto: draft that should be opened on the next window creation.
/// Used when the mailto: link activates before the main window exists.
static MAILTO_DRAFT: std::sync::Mutex<Option<OutgoingMessage>> = std::sync::Mutex::new(None);

/// Public API to set a mailto draft for the next window.
pub fn set_mailto_draft(draft: OutgoingMessage) {
    if let Ok(mut guard) = MAILTO_DRAFT.lock() {
        *guard = Some(draft);
    }
}

/// Take the mailto draft if one was set.
pub fn take_mailto_draft() -> Option<OutgoingMessage> {
    if let Ok(mut guard) = MAILTO_DRAFT.lock() {
        guard.take()
    } else {
        None
    }
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
    html_view: RefCell<Option<webkit6::WebView>>,
    /// Whether the reader prefers HTML; starts from the user preference.
    html_mode: Cell<bool>,
    /// Message for which the user explicitly allowed remote content.
    remote_allowed: Cell<Option<MessageId>>,
    pending_attachment: Rc<RefCell<Option<PendingAttachment>>>,
    /// Draft edit window per message, so selecting the same draft row again
    /// focuses the open composer instead of spawning a second one.
    last_draft: RefCell<Option<(MessageId, glib::WeakRef<gtk::Window>)>>,
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

        let compose_button = icon_button("mail-message-new-symbolic", "New message (Ctrl+N)");
        header.pack_start(&compose_button);

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

        // Account menu button with edit/remove options
        let account_menu = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        let account_menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Account options")
            .popover(&account_menu)
            .build();

        let account_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        account_bar.set_margin_top(8);
        account_bar.set_margin_bottom(8);
        account_bar.set_margin_start(12);
        account_bar.set_margin_end(12);
        account_bar.append(&account_dropdown);
        account_bar.append(&add_button);
        account_bar.append(&account_menu_button);

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

        let archive_button = icon_button(
            "package-x-generic-symbolic",
            "Archive selected (Ctrl+Shift+E)",
        );
        let junk_button = icon_button("dialog-warning-symbolic", "Mark as Junk (Ctrl+Shift+J)");
        let trash_button = icon_button("user-trash-symbolic", "Move to Trash (Delete)");
        let message_menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Message actions")
            .build();
        list_header.append(&archive_button);
        list_header.append(&junk_button);
        list_header.append(&trash_button);
        list_header.append(&message_menu_button);

        // Bulk action bar: revealed while rows are selected in the list.
        let bulk_count = gtk::Label::new(Some("0 selected"));
        bulk_count.add_css_class("dim-label");
        let bulk_read = icon_button("mail-mark-read-symbolic", "Mark selected as read");
        let bulk_unread = icon_button("mail-mark-unread-symbolic", "Mark selected as unread");
        let bulk_flag = icon_button("starred-symbolic", "Star selected");
        let bulk_unflag = icon_button("non-starred-symbolic", "Unstar selected");
        let bulk_move = gtk::Button::with_label("Move…");
        bulk_move.add_css_class("flat");
        bulk_move.set_tooltip_text(Some("Move selected messages to a folder (Ctrl+M)"));
        let bulk_clear = icon_button("edit-clear-symbolic", "Clear selection (Escape)");
        let bulk_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bulk_row.set_margin_top(4);
        bulk_row.set_margin_bottom(4);
        bulk_row.set_margin_start(14);
        bulk_row.set_margin_end(8);
        bulk_row.append(&bulk_count);
        let bulk_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bulk_spacer.set_hexpand(true);
        bulk_row.append(&bulk_spacer);
        bulk_row.append(&bulk_read);
        bulk_row.append(&bulk_unread);
        bulk_row.append(&bulk_flag);
        bulk_row.append(&bulk_unflag);
        bulk_row.append(&bulk_move);
        bulk_row.append(&bulk_clear);
        let bulk_bar = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .child(&bulk_row)
            .build();

        let message_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Multiple)
            // Clicks must select first and activate on the second press so
            // that GTK's Ctrl/Shift+click toggle and extend handling applies.
            .activate_on_single_click(false)
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
        list_pane.append(&bulk_bar);
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

        let reader_attachments = gtk::Box::new(gtk::Orientation::Vertical, 6);
        reader_attachments.set_visible(false);

        let remote_banner = adw::Banner::new("Remote content is blocked to protect your privacy.");
        remote_banner.set_button_label(Some("Load remote content"));
        remote_banner.set_revealed(false);

        let reader_body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        reader_body.set_margin_top(6);
        reader_body.set_margin_bottom(24);

        let flag_button = gtk::ToggleButton::builder()
            .icon_name("non-starred-symbolic")
            .tooltip_text("Star message")
            .build();
        flag_button.add_css_class("flat");
        let read_button = icon_button("mail-mark-read-symbolic", "Mark as read");
        let html_button = gtk::ToggleButton::builder()
            .label("HTML")
            .tooltip_text("Show the original HTML body")
            .build();
        html_button.add_css_class("flat");

        let reply_button = icon_button("mail-reply-sender-symbolic", "Reply (Ctrl+R)");
        let reply_all_button =
            icon_button("mail-reply-all-symbolic", "Reply to all (Ctrl+Shift+R)");
        let forward_button = icon_button("mail-forward-symbolic", "Forward");

        let reader_actions = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        reader_actions.append(&html_button);
        reader_actions.append(&reply_button);
        reader_actions.append(&reply_all_button);
        reader_actions.append(&forward_button);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        reader_actions.append(&spacer);
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
        reader_content.append(&remote_banner);
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

        let html_mode = app.config.ui.html_enabled;
        let this = Rc::new(Self {
            app,
            selection: RefCell::new(Selection::default()),
            accounts: RefCell::new(Vec::new()),
            folders: RefCell::new(Vec::new()),
            messages: RefCell::new(Vec::new()),
            programmatic: Cell::new(false),
            search_generation: Cell::new(0),
            closed: Rc::new(Cell::new(false)),
            html_view: RefCell::new(None),
            html_mode: Cell::new(html_mode),
            remote_allowed: Cell::new(None),
            pending_attachment: Rc::new(RefCell::new(None)),
            last_draft: RefCell::new(None),
            widgets: Widgets {
                window,
                title,
                stack,
                toast,
                banner,
                account_dropdown,
                add_button,
                account_menu_button,
                compose_button,
                empty_add,
                folder_list,
                folder_stack,
                list_title,
                archive_button,
                junk_button,
                trash_button,
                message_menu_button,
                bulk_bar,
                bulk_count,
                bulk_read,
                bulk_unread,
                bulk_flag,
                bulk_unflag,
                bulk_move,
                bulk_clear,
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
                remote_banner,
                reader_attachments,
                reader_body,
                read_button,
                flag_button,
                html_button,
                reply_button,
                reply_all_button,
                forward_button,
            },
        });

        Self::connect(&this);
        this.refresh_accounts();

        // Check for a mailto: draft that was set before window creation
        if let Some(draft) = take_mailto_draft() {
            this.open_compose_with_draft(draft);
        }

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

        // Shortcuts window (F1 / Ctrl+?)
        let shortcuts = build_shortcuts_window(&this.widgets.window);
        let key_controller = gtk::EventControllerKey::new();
        key_controller.connect_key_pressed({
            let shortcuts = shortcuts.clone();
            move |_, keyval, _, state| {
                if keyval == Key::F1
                    || (keyval == Key::question && state.contains(ModifierType::CONTROL_MASK))
                {
                    shortcuts.set_visible(true);
                    shortcuts.present();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        });
        this.widgets.window.add_controller(key_controller);

        // Account menu (edit/remove)
        let menu = gio::Menu::new();
        let edit_action = gio::SimpleAction::new("account-edit", None);
        edit_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| MainWindow::edit_account(&this)
        });
        this.widgets.window.add_action(&edit_action);
        menu.append(Some("Edit"), Some("win.account-edit"));

        let remove_action = gio::SimpleAction::new("account-remove", None);
        remove_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| MainWindow::remove_account(&this)
        });
        this.widgets.window.add_action(&remove_action);
        menu.append(Some("Remove"), Some("win.account-remove"));

        let account_menu_model = gio::Menu::new();
        account_menu_model.append_section(None, &menu);
        this.widgets
            .account_menu_button
            .set_menu_model(Some(&account_menu_model));

        // Message actions: toolbar buttons and the overflow menu share the
        // same window actions so accelerators and menus never drift apart.
        let archive_action = gio::SimpleAction::new("archive", None);
        archive_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.archive_selected()
        });
        this.widgets.window.add_action(&archive_action);

        let delete_action = gio::SimpleAction::new("delete", None);
        delete_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.delete_selected(false)
        });
        this.widgets.window.add_action(&delete_action);

        let delete_permanent_action = gio::SimpleAction::new("delete-permanent", None);
        delete_permanent_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.delete_selected(true)
        });
        this.widgets.window.add_action(&delete_permanent_action);

        let junk_action = gio::SimpleAction::new("junk", None);
        junk_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.mark_selected_junk(true)
        });
        this.widgets.window.add_action(&junk_action);

        let not_junk_action = gio::SimpleAction::new("not-junk", None);
        not_junk_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.mark_selected_junk(false)
        });
        this.widgets.window.add_action(&not_junk_action);

        let empty_trash_action = gio::SimpleAction::new("empty-trash", None);
        empty_trash_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.empty_trash()
        });
        this.widgets.window.add_action(&empty_trash_action);

        let outbox_retry_action = gio::SimpleAction::new("outbox-retry", None);
        outbox_retry_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.outbox_retry_selected()
        });
        this.widgets.window.add_action(&outbox_retry_action);

        let outbox_discard_action = gio::SimpleAction::new("outbox-discard", None);
        outbox_discard_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.outbox_discard_selected()
        });
        this.widgets.window.add_action(&outbox_discard_action);

        let outbox_edit_action = gio::SimpleAction::new("outbox-edit", None);
        outbox_edit_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| MainWindow::outbox_edit_selected(&this)
        });
        this.widgets.window.add_action(&outbox_edit_action);

        this.widgets.archive_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.archive_selected()
        });
        this.widgets.junk_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.mark_selected_junk(true)
        });
        this.widgets.trash_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.delete_selected(false)
        });

        this.widgets.bulk_read.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.mark_selected_seen(true)
        });
        this.widgets.bulk_unread.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.mark_selected_seen(false)
        });
        this.widgets.bulk_flag.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.mark_selected_flagged(true)
        });
        this.widgets.bulk_unflag.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.mark_selected_flagged(false)
        });
        this.widgets.bulk_move.connect_clicked({
            let this = Rc::clone(this);
            move |_| Self::move_selected(&this)
        });
        this.widgets.bulk_clear.connect_clicked({
            let list_box = this.widgets.message_list.clone();
            move |_| list_box.unselect_all()
        });

        let message_menu = gio::Menu::new();
        message_menu.append(Some("Archive"), Some("win.archive"));
        message_menu.append(Some("Move to Trash"), Some("win.delete"));
        message_menu.append(Some("Move to Folder…"), Some("win.move"));
        message_menu.append(Some("Mark as Junk"), Some("win.junk"));
        message_menu.append(Some("Not Junk"), Some("win.not-junk"));
        let outbox_section = gio::Menu::new();
        outbox_section.append(Some("Retry"), Some("win.outbox-retry"));
        outbox_section.append(Some("Discard"), Some("win.outbox-discard"));
        outbox_section.append(Some("Edit"), Some("win.outbox-edit"));
        message_menu.append_section(None, &outbox_section);
        let destructive = gio::Menu::new();
        destructive.append(Some("Delete Permanently"), Some("win.delete-permanent"));
        destructive.append(Some("Empty Trash"), Some("win.empty-trash"));
        message_menu.append_section(None, &destructive);
        this.widgets
            .message_menu_button
            .set_menu_model(Some(&message_menu));

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

        // Selection changes drive the bulk bar and the reader: once the
        // selection settles on exactly one row, that message is shown.
        this.widgets.message_list.connect_selected_rows_changed({
            let this = Rc::clone(this);
            move |list_box| {
                if this.programmatic.get() {
                    return;
                }
                this.update_bulk_bar();
                if list_box.selected_rows().len() != 1 {
                    return;
                }
                let message = list_box.selected_rows().first().and_then(|row| {
                    usize::try_from(row.index())
                        .ok()
                        .and_then(|index| this.messages.borrow().get(index).cloned())
                });
                if let Some(message) = message {
                    this.open_message(&message);
                }
            }
        });

        // Extra keys for the message list. Native GTK already provides
        // Up/Down, Shift+Up/Down (extend), Ctrl+A (select all) and
        // Ctrl+Space (toggle row); Escape, plain Space and starting the
        // navigation from an empty selection are handled here.
        let list_key_controller = gtk::EventControllerKey::new();
        list_key_controller.connect_key_pressed({
            let list_box = this.widgets.message_list.clone();
            move |_, keyval, _, state| {
                if keyval == Key::Escape {
                    list_box.unselect_all();
                    glib::Propagation::Stop
                } else if keyval == Key::space && !state.contains(ModifierType::CONTROL_MASK) {
                    if let Some(row) = list_box.selected_row() {
                        if row.is_selected() {
                            list_box.unselect_row(&row);
                        } else {
                            list_box.select_row(Some(&row));
                        }
                    }
                    glib::Propagation::Stop
                } else if list_box.selected_row().is_none() {
                    // Nothing selected yet: start browsing instead of letting
                    // focus escape the list.
                    let row = match keyval {
                        Key::Down => list_box.row_at_index(0),
                        Key::Up => list_box
                            .observe_children()
                            .n_items()
                            .checked_sub(1)
                            .and_then(|last| list_box.row_at_index(i32::try_from(last).ok()?)),
                        _ => None,
                    };
                    if let Some(row) = row {
                        list_box.select_row(Some(&row));
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                } else {
                    glib::Propagation::Proceed
                }
            }
        });
        this.widgets
            .message_list
            .add_controller(list_key_controller);

        this.widgets.load_more.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.request_more()
        });

        this.widgets.read_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.toggle_seen()
        });

        this.widgets.html_button.connect_toggled({
            let this = Rc::clone(this);
            move |button| {
                if this.programmatic.get() {
                    return;
                }
                this.html_mode.set(button.is_active());
                if let Some(message_id) = this.selection.borrow().message {
                    this.render_body(message_id);
                }
            }
        });

        this.widgets.remote_banner.connect_button_clicked({
            let this = Rc::clone(this);
            move |banner| {
                let Some(message_id) = this.selection.borrow().message else {
                    return;
                };
                this.remote_allowed.set(Some(message_id));
                banner.set_revealed(false);
                this.render_body(message_id);
            }
        });

        this.widgets.compose_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| {
                let _ = Self::open_compose(&this, OutgoingMessage::default());
            }
        });

        this.widgets.reply_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| Self::compose_reply(&this, false)
        });
        this.widgets.reply_all_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| Self::compose_reply(&this, true)
        });
        this.widgets.forward_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| Self::compose_forward(&this)
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

        let move_action = gio::SimpleAction::new("move", None);
        move_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| Self::move_selected(&this)
        });
        this.widgets.window.add_action(&move_action);

        if let Some(application) = this.widgets.window.application() {
            application.set_accels_for_action("win.search", &["<Control>f"]);
            application.set_accels_for_action("win.sync", &["F5"]);
            application.set_accels_for_action("win.add-account", &["<Control><Shift>a"]);
            application.set_accels_for_action("win.compose", &["<Control>n"]);
            application.set_accels_for_action("win.reply", &["<Control>r"]);
            application.set_accels_for_action("win.reply-all", &["<Control><Shift>r"]);
            application.set_accels_for_action("win.forward", &["<Control><Shift>f"]);
            application.set_accels_for_action("win.preferences", &["<Control>comma"]);
            application.set_accels_for_action("win.archive", &["<Control><Shift>e"]);
            application.set_accels_for_action("win.delete", &["Delete"]);
            application.set_accels_for_action("win.delete-permanent", &["<Control>Delete"]);
            application.set_accels_for_action("win.junk", &["<Control><Shift>j"]);
            application.set_accels_for_action("win.not-junk", &["<Control><Shift>k"]);
            application.set_accels_for_action("win.move", &["<Control>m"]);
        }

        let compose_action = gio::SimpleAction::new("compose", None);
        compose_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| {
                let _ = Self::open_compose(&this, OutgoingMessage::default());
            }
        });
        this.widgets.window.add_action(&compose_action);

        let reply_action = gio::SimpleAction::new("reply", None);
        reply_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| Self::compose_reply(&this, false)
        });
        this.widgets.window.add_action(&reply_action);

        let reply_all_action = gio::SimpleAction::new("reply-all", None);
        reply_all_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| Self::compose_reply(&this, true)
        });
        this.widgets.window.add_action(&reply_all_action);

        let forward_action = gio::SimpleAction::new("forward", None);
        forward_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| Self::compose_forward(&this)
        });
        this.widgets.window.add_action(&forward_action);

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

        let preferences_action = gio::SimpleAction::new("preferences", None);
        preferences_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| crate::preferences::show_preferences(&this.widgets.window, &this.app)
        });
        this.widgets.window.add_action(&preferences_action);

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
        let is_outbox = folder.id.0 == -1;
        {
            let mut selection = self.selection.borrow_mut();
            if selection.folder == Some(folder.id)
                && !selection.search
                && selection.show_outbox == is_outbox
            {
                return;
            }
            selection.folder = Some(folder.id);
            selection.message = None;
            selection.search = false;
            selection.query.clear();
            selection.show_outbox = is_outbox;
            selection.limit = self.page_size();
        }
        self.programmatic.set(true);
        self.widgets.search.set_text("");
        self.programmatic.set(false);
        self.widgets.list_title.set_text(&folder_title(folder));
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

    fn refresh_messages(&self) {
        let (search, query, folder_id, limit, show_outbox, account_id) = {
            let selection = self.selection.borrow();
            (
                selection.search,
                selection.query.clone(),
                selection.folder,
                selection.limit,
                selection.show_outbox,
                selection.account,
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

    fn refresh_outbox(&self, account_id: AccountId) {
        match self.app.mailin.list_outbox(&account_id) {
            Ok(entries) => {
                let title = format!("Outbox ({})", entries.len());
                self.show_outbox_entries(entries, title);
            }
            Err(error) => self.show_error(&error.to_string()),
        }
    }

    fn show_outbox_entries(&self, entries: Vec<OutboxEntry>, title: String) {
        let now = format::now_unix();
        let empty = entries.is_empty();

        self.programmatic.set(true);
        clear_list(&self.widgets.message_list);
        for entry in &entries {
            self.widgets
                .message_list
                .append(&outbox_entry_row(entry, now));
        }
        self.programmatic.set(false);

        self.widgets.list_title.set_text(&title);
        self.widgets
            .list_stack
            .set_visible_child_name(if empty { "empty" } else { "list" });
        self.widgets.load_more.set_visible(false);
        self.show_reader_empty();

        // Store entries for selection tracking.
        self.selection.borrow_mut().outbox_entries = entries;
        // Outbox rows are not messages: drop the stale mapping so the
        // selection helpers can never address the wrong rows.
        self.messages.borrow_mut().clear();
        self.update_bulk_bar();
    }

    fn show_messages(&self, messages: Vec<MessageHeader>, title: String, more: bool) {
        let selected = self.selection.borrow().message;
        // Keep the whole selection alive across refreshes so a background
        // sync does not collapse a multi-selection back to one row.
        let previous = self.selected_message_ids();
        let now = format::now_unix();
        let empty = messages.is_empty();

        self.programmatic.set(true);
        clear_list(&self.widgets.message_list);
        for message in &messages {
            self.widgets.message_list.append(&message_row(message, now));
        }
        for (index, message) in messages.iter().enumerate() {
            if Some(message.id) != selected && !previous.contains(&message.id) {
                continue;
            }
            if let Some(row) = self.widgets.message_list.row_at_index(index as i32) {
                self.widgets.message_list.select_row(Some(&row));
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
        self.update_bulk_bar();
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
        self.update_bulk_bar();
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

    fn open_message(self: &Rc<Self>, message: &MessageHeader) {
        // A draft opens in the composer rather than the reader, ready to be
        // resumed and sent.
        let is_draft = message.is_draft()
            || self
                .folders
                .borrow()
                .iter()
                .find(|folder| folder.id == message.folder_id)
                .is_some_and(|folder| folder.role == FolderRole::Drafts);
        if is_draft {
            // The selection signal and row activation both reach this
            // method: focus an already-open composer instead of spawning a
            // second window for the same draft.
            if let Some((draft_id, window)) = self.last_draft.borrow().as_ref()
                && *draft_id == message.id
                && let Some(window) = window.upgrade()
            {
                window.present();
                return;
            }
            let body = self
                .app
                .mailin
                .message_body(message.id)
                .ok()
                .flatten()
                .and_then(|(text, _)| text);
            let draft = mailin_core::compose::draft_from_header(message, body.as_deref());
            if let Some(window) = Self::open_compose(self, draft) {
                *self.last_draft.borrow_mut() = Some((message.id, window));
            }
            return;
        }

        // The row that is already open must not re-run the open side
        // effects: both row activation and selection changes call this.
        if self.selection.borrow().message == Some(message.id) {
            return;
        }

        self.selection.borrow_mut().message = Some(message.id);
        if self.remote_allowed.get() != Some(message.id) {
            self.remote_allowed.set(None);
        }
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
        self.widgets.remote_banner.set_revealed(false);
        let Ok(body) = self.app.mailin.message_body(message_id) else {
            self.widgets
                .reader_body
                .append(&notice_body("Could not read this message."));
            return;
        };
        match body {
            Some((text, html)) => {
                let has_text = text.as_deref().is_some_and(|text| !text.trim().is_empty());
                let has_html = html.as_deref().is_some_and(|html| !html.trim().is_empty());
                let want_html = has_html && self.html_mode.get();

                self.programmatic.set(true);
                self.widgets.html_button.set_sensitive(has_html);
                self.widgets.html_button.set_active(want_html);
                self.programmatic.set(false);

                if want_html {
                    let html_body = html.as_deref().unwrap_or_default();
                    let allow_remote = self.app.config.ui.remote_content_enabled
                        || self.remote_allowed.get() == Some(message_id);
                    let images = self.inline_images(message_id);
                    let dark = adw::StyleManager::default().is_dark();
                    let view = self.html_view();
                    view.load_html(
                        &html::document(html_body, allow_remote, dark, &images),
                        None,
                    );
                    self.widgets.reader_body.append(&view);
                    if !allow_remote && html::has_remote_content(html_body) {
                        self.widgets.remote_banner.set_revealed(true);
                    }
                } else if has_text {
                    self.widgets
                        .reader_body
                        .append(&text_body(text.as_deref().unwrap_or_default()));
                } else if has_html {
                    self.widgets.reader_body.append(&notice_body(
                        "This message only provides an HTML body. Use the HTML button to show it.",
                    ));
                } else {
                    self.widgets
                        .reader_body
                        .append(&notice_body("This message has no readable body."));
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
        }
    }

    fn html_view(&self) -> webkit6::WebView {
        let mut slot = self.html_view.borrow_mut();
        if let Some(view) = slot.as_ref() {
            return view.clone();
        }
        let view = html::new_view();
        *slot = Some(view.clone());
        view
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

    fn render_attachments(&self, message_id: MessageId) {
        clear_box(&self.widgets.reader_attachments);
        let attachments = self
            .app
            .mailin
            .list_attachments(message_id)
            .unwrap_or_default();
        let visible = attachments
            .into_iter()
            .filter(|attachment| !attachment.is_inline)
            .collect::<Vec<_>>();
        if visible.is_empty() {
            self.widgets.reader_attachments.set_visible(false);
            return;
        }
        for attachment in visible {
            self.widgets
                .reader_attachments
                .append(&self.attachment_row(message_id, attachment));
        }
        self.widgets.reader_attachments.set_visible(true);
    }

    fn attachment_row(&self, message_id: MessageId, attachment: AttachmentMeta) -> gtk::Widget {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("card");
        row.set_margin_top(2);
        row.set_margin_bottom(2);

        let icon = gtk::Image::from_icon_name("mail-attachment-symbolic");
        icon.set_pixel_size(16);
        icon.set_margin_start(10);
        row.append(&icon);

        let name = gtk::Label::new(Some(&attachment.display_name()));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(pango::EllipsizeMode::Middle);
        row.append(&name);

        if let Some(size) = attachment.size {
            let size = gtk::Label::new(Some(&format::file_size(size)));
            size.add_css_class("caption");
            size.add_css_class("dim-label");
            row.append(&size);
        }

        let save = gtk::Button::with_label("Save");
        save.add_css_class("flat");
        save.set_margin_end(4);
        let app = Rc::clone(&self.app);
        let window = self.widgets.window.clone().upcast::<gtk::Window>();
        let toast = self.widgets.toast.clone();
        let pending = Rc::clone(&self.pending_attachment);
        save.connect_clicked(move |_| {
            request_save_attachment(
                Rc::clone(&app),
                window.clone(),
                toast.clone(),
                Rc::clone(&pending),
                message_id,
                attachment.clone(),
                false,
            );
        });
        row.append(&save);
        row.upcast()
    }

    /// Finish a save that was waiting for attachment bytes to be downloaded.
    fn resolve_pending_attachment(&self, message_id: MessageId) {
        let pending = self
            .pending_attachment
            .borrow()
            .as_ref()
            .filter(|pending| pending.message_id == message_id)
            .cloned();
        let Some(pending) = pending else {
            return;
        };
        self.pending_attachment.borrow_mut().take();
        request_save_attachment(
            Rc::clone(&self.app),
            self.widgets.window.clone().upcast::<gtk::Window>(),
            self.widgets.toast.clone(),
            Rc::clone(&self.pending_attachment),
            message_id,
            pending.attachment,
            true,
        );
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

    /// All messages currently selected in the list, in list order.
    fn selected_message_ids(&self) -> Vec<MessageId> {
        let messages = self.messages.borrow();
        self.widgets
            .message_list
            .selected_rows()
            .iter()
            .filter_map(|row| {
                messages
                    .get(usize::try_from(row.index()).unwrap_or(usize::MAX))
                    .map(|message| message.id)
            })
            .collect()
    }

    fn toast(&self, text: &str) {
        self.widgets.toast.add_toast(adw::Toast::new(text));
    }

    /// Show or hide the bulk action bar for the current selection.
    fn update_bulk_bar(&self) {
        if self.programmatic.get() {
            return;
        }
        let count = if self.selection.borrow().show_outbox {
            0
        } else {
            self.widgets.message_list.selected_rows().len()
        };
        self.widgets
            .bulk_count
            .set_text(&format!("{count} selected"));
        self.widgets.bulk_bar.set_reveal_child(count > 0);
    }

    fn archive_selected(&self) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        self.app.engine.archive_messages(&ids);
        self.toast("Archiving…");
    }

    fn delete_selected(&self, permanent: bool) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        if permanent {
            self.app.engine.permanently_delete_messages(&ids);
            self.toast("Deleting…");
            return;
        }
        let in_trash = self
            .selection
            .borrow()
            .folder
            .and_then(|folder_id| self.app.mailin.folder(folder_id).ok().flatten())
            .is_some_and(|folder| folder.role == FolderRole::Trash);
        self.app.engine.delete_messages(&ids);
        self.toast(if in_trash {
            "Deleting permanently…"
        } else {
            "Moving to Trash…"
        });
    }

    fn mark_selected_junk(&self, junk: bool) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        self.app.engine.mark_messages_junk(&ids, junk);
        self.toast(if junk {
            "Marking as Junk…"
        } else {
            "Moving to Inbox…"
        });
    }

    fn empty_trash(&self) {
        let Some(account_id) = self.selection.borrow().account else {
            return;
        };
        self.app.engine.empty_trash(account_id);
        self.toast("Emptying Trash…");
    }

    fn outbox_retry_selected(&self) {
        let Some(account_id) = self.selection.borrow().account else {
            return;
        };
        let ids = self.selected_outbox_ids();
        if ids.is_empty() {
            return;
        }
        for id in ids {
            self.app.engine.retry_outbox(account_id, id);
        }
        self.toast("Retrying…");
    }

    fn outbox_discard_selected(&self) {
        let Some(account_id) = self.selection.borrow().account else {
            return;
        };
        let ids = self.selected_outbox_ids();
        if ids.is_empty() {
            return;
        }
        for id in ids {
            self.app.engine.cancel_outbox(account_id, id);
        }
        self.toast("Discarding…");
    }

    fn outbox_edit_selected(this: &Rc<Self>) {
        let Some(account_id) = this.selection.borrow().account else {
            return;
        };
        let ids = this.selected_outbox_ids();
        if ids.len() != 1 {
            return;
        }
        let id = ids[0];
        if let Ok(Some(entry)) = this.app.mailin.outbox_entry(id) {
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
                } else {
                    // Fallback: extract subject from envelope
                    draft.subject = env.to.join(", ");
                }
                // Need to mark the original entry as cancelled so it doesn't get sent again
                this.app.engine.cancel_outbox(account_id, id);
                let _ = Self::open_compose(this, draft);
            }
        }
    }

    fn selected_outbox_ids(&self) -> Vec<i64> {
        let entries = self.selection.borrow().outbox_entries.clone();
        self.widgets
            .message_list
            .selected_rows()
            .iter()
            .filter_map(|row| {
                let index = row.index() as usize;
                entries.get(index).map(|entry| entry.id)
            })
            .collect()
    }

    fn mark_selected_seen(&self, seen: bool) {
        let ids = self.selected_message_ids();
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

    fn mark_selected_flagged(&self, flagged: bool) {
        let ids = self.selected_message_ids();
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

    fn toggle_seen(&self) {
        // The reader button reflects the open message; apply its direction
        // to every selected message.
        let Some(message) = self.selected_message() else {
            return;
        };
        self.mark_selected_seen(!message.is_seen());
    }

    fn toggle_flagged(&self, flagged: bool) {
        self.mark_selected_flagged(flagged);
    }

    /// Ask for a destination folder and move the selection there.
    fn move_selected(this: &Rc<Self>) {
        let ids = this.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let current = this.selection.borrow().folder;
        let folders = this.folders.borrow().clone();
        crate::move_dialog::show(&this.widgets.window, &folders, current, {
            let this = Rc::clone(this);
            move |folder_id| {
                this.app.engine.move_messages(&ids, folder_id);
                this.toast("Moving…");
            }
        });
    }

    // --------------------------------------------------------------- compose

    fn compose_reply(this: &Rc<Self>, all: bool) {
        let Some(message) = this.selected_message() else {
            return;
        };
        let Some(account_id) = this.selection.borrow().account else {
            return;
        };
        let Some(account) = this.app.mailin.account(&account_id).ok().flatten() else {
            return;
        };
        let body = this
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
        let _ = Self::open_compose(this, draft);
    }

    fn compose_forward(this: &Rc<Self>) {
        let Some(message) = this.selected_message() else {
            return;
        };
        let Some(account_id) = this.selection.borrow().account else {
            return;
        };
        let Some(account) = this.app.mailin.account(&account_id).ok().flatten() else {
            return;
        };
        let body = this
            .app
            .mailin
            .message_body(message.id)
            .ok()
            .flatten()
            .and_then(|(text, _)| text)
            .or_else(|| message.snippet.clone());
        let draft = compose::forward(&account, &message, body.as_deref());
        let _ = Self::open_compose(this, draft);
    }

    fn open_compose(this: &Rc<Self>, draft: OutgoingMessage) -> Option<glib::WeakRef<gtk::Window>> {
        let Some(account_id) = this.selection.borrow().account else {
            this.widgets
                .toast
                .add_toast(adw::Toast::new("Add an account before composing mail"));
            return None;
        };
        let parent = this.widgets.window.clone().upcast::<gtk::Window>();
        let toast = this.widgets.toast.clone();
        compose::present(
            &parent,
            Rc::clone(&this.app),
            account_id,
            draft,
            move || {
                toast.add_toast(adw::Toast::new("Message queued in the outbox"));
            },
        )
        .map(|window| window.upcast::<gtk::Window>().downgrade())
    }

    /// Public method to open compose window with a draft (for mailto: links).
    pub fn open_compose_with_draft(&self, draft: OutgoingMessage) {
        // We need an Rc<Self> for open_compose, so we create a weak reference
        // and upgrade it. Since this is called on an existing window, we can
        // use the fact that the window is already reference-counted.
        // We'll create a new Rc by using the existing window's reference.
        // For simplicity, we'll use a workaround: create a dummy Rc that
        // doesn't actually own the window but just calls the method.
        // The real solution would be to restructure open_compose to take &Self.

        // Instead, let's just call open_compose directly with a workaround
        // We can't easily get an Rc<Self> from &Self, so let's use a different approach.
        // Store the draft in the selection and trigger compose via the existing mechanism.
        // Actually, the simplest fix: make open_compose take &Self instead of &Rc<Self>

        // For now, let's use a different approach: just call the internal logic directly
        let Some(account_id) = self.selection.borrow().account else {
            self.widgets
                .toast
                .add_toast(adw::Toast::new("Add an account before composing mail"));
            return;
        };
        let parent = self.widgets.window.clone().upcast::<gtk::Window>();
        let toast = self.widgets.toast.clone();
        compose::present(
            &parent,
            Rc::clone(&self.app),
            account_id,
            draft,
            move || {
                toast.add_toast(adw::Toast::new("Message queued in the outbox"));
            },
        );
    }

    // --------------------------------------------------------------- events

    fn drain_events(&self, events: &mut broadcast::Receiver<Event>) {
        let mut accounts_changed = false;
        let mut folders_changed = false;
        let mut messages_changed = false;
        let mut body_ready = None;
        let mut account_error = None;
        let mut outbox_changed = false;
        let mut new_messages = Vec::new();

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
            if self.selection.borrow().message == Some(message_id) {
                self.render_body(message_id);
                self.render_attachments(message_id);
                if let Some(message) = self.selected_message() {
                    self.render_headers(&message);
                    self.update_actions(&message);
                }
            }
        }
        for (account_id, count) in new_messages {
            self.notify_new_mail(account_id, count);
        }
        if let Some(message) = account_error {
            self.show_error(&message);
        }
        if outbox_changed {
            // If we're viewing the outbox, refresh it. Otherwise refresh folders
            // to update the badge count.
            let selection = self.selection.borrow();
            if selection.show_outbox {
                self.refresh_messages();
            } else {
                self.refresh_folders();
            }
        }
    }

    /// Desktop notification for background mail. Clicking it activates the
    /// application through the default action.
    fn notify_new_mail(&self, account_id: AccountId, count: u32) {
        let application_active = self
            .widgets
            .window
            .application()
            .is_some_and(|application| {
                application
                    .windows()
                    .iter()
                    .any(|window| window.is_active())
            });
        if !self.app.config.ui.notifications || application_active {
            return;
        }
        let Some(application) = self.widgets.window.application() else {
            return;
        };
        let account = self
            .accounts
            .borrow()
            .iter()
            .find(|account| account.id == account_id)
            .map(|account| account.email.clone())
            .unwrap_or_else(|| "Mailin".to_owned());
        let notification = gio::Notification::new(if count == 1 {
            "New message"
        } else {
            "New messages"
        });
        notification.set_body(Some(&format!(
            "{count} new message{} in {account}",
            if count == 1 { "" } else { "s" }
        )));
        notification.set_default_action("app.present");
        application.send_notification(Some(&format!("mailin-{account_id}")), &notification);
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
        crate::account_dialog::present(
            &this.widgets.window,
            Rc::clone(&this.app),
            crate::account_dialog::AccountDialogMode::Add,
            None,
            {
                let this = Rc::clone(this);
                move |id| {
                    this.refresh_accounts();
                    this.select_account(id);
                }
            },
        );
    }

    fn edit_account(this: &Rc<Self>) {
        let account = this.selection.borrow().account;
        let Some(account_id) = account else { return };
        let Some(account) = this
            .accounts
            .borrow()
            .iter()
            .find(|a| a.id == account_id)
            .cloned()
        else {
            return;
        };
        crate::account_dialog::present(
            &this.widgets.window,
            Rc::clone(&this.app),
            crate::account_dialog::AccountDialogMode::Edit,
            Some(account),
            {
                let this = Rc::clone(this);
                move |id| {
                    this.refresh_accounts();
                    this.select_account(id);
                }
            },
        );
    }

    fn remove_account(this: &Rc<Self>) {
        let account = this.selection.borrow().account;
        let Some(account_id) = account else { return };
        let Some(account) = this
            .accounts
            .borrow()
            .iter()
            .find(|a| a.id == account_id)
            .cloned()
        else {
            return;
        };
        let confirm = gtk::AlertDialog::builder()
            .message("Remove account?")
            .detail(format!(
                "Are you sure you want to remove '{}' ({})? This will delete all local data for this account.",
                account.display_name, account.email
            ))
            .build();
        confirm.set_buttons(&["Cancel", "Remove"]);
        confirm.set_cancel_button(0);
        confirm.set_default_button(0);

        let mailin = this.app.mailin.clone();
        let this = Rc::clone(this);
        let account_id = account.id;

        glib::spawn_future_local(async move {
            let response = confirm.choose_future(Some(&this.widgets.window)).await;
            match response {
                Ok(1) => {
                    // "Remove" button (index 1)
                    if let Err(e) = mailin.remove_account(&account_id) {
                        this.show_error(&e.to_string());
                    } else {
                        this.refresh_accounts();
                    }
                }
                _ => {
                    // Cancel or error - do nothing
                }
            }
        });
    }
}

// -------------------------------------------------------------------- widgets

/// Start saving an attachment, downloading the body first when only metadata
/// was cached by an older version.
fn request_save_attachment(
    app: Rc<AppState>,
    window: gtk::Window,
    toast: adw::ToastOverlay,
    pending: Rc<RefCell<Option<PendingAttachment>>>,
    message_id: MessageId,
    attachment: AttachmentMeta,
    retry: bool,
) {
    let unavailable = |toast: &adw::ToastOverlay| {
        toast.add_toast(adw::Toast::new(
            "Could not load the attachment; it may no longer be available.",
        ));
    };
    match app.mailin.attachment(message_id, &attachment.part_id) {
        Ok(Some(data)) => match data.data {
            Some(bytes) => save_attachment_dialog(&window, &toast, &attachment, bytes),
            None if retry => unavailable(&toast),
            None => {
                *pending.borrow_mut() = Some(PendingAttachment {
                    message_id,
                    attachment: attachment.clone(),
                });
                app.engine.refresh_body(message_id);
                toast.add_toast(adw::Toast::new("Downloading attachment…"));
            }
        },
        Ok(None) if retry => unavailable(&toast),
        Ok(None) => {
            *pending.borrow_mut() = Some(PendingAttachment {
                message_id,
                attachment,
            });
            app.engine.refresh_body(message_id);
            toast.add_toast(adw::Toast::new("Downloading attachment…"));
        }
        Err(error) => {
            toast.add_toast(adw::Toast::new(&format!(
                "Could not read the attachment: {error}"
            )));
        }
    }
}

fn save_attachment_dialog(
    window: &gtk::Window,
    toast: &adw::ToastOverlay,
    attachment: &AttachmentMeta,
    data: Vec<u8>,
) {
    let dialog = gtk::FileDialog::builder()
        .title("Save attachment")
        .initial_name(attachment.display_name())
        .build();
    let toast = toast.clone();
    dialog.save(Some(window), None::<&gio::Cancellable>, move |result| {
        let Ok(file) = result else {
            return;
        };
        let result = file.replace_contents(
            &data,
            None::<&str>,
            false,
            gio::FileCreateFlags::REPLACE_DESTINATION,
            None::<&gio::Cancellable>,
        );
        match result {
            Ok(_) => toast.add_toast(adw::Toast::new("Attachment saved")),
            Err(error) => toast.add_toast(adw::Toast::new(&format!(
                "Could not save the attachment: {error}"
            ))),
        }
    });
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .build();
    button.add_css_class("flat");
    // Set accessible name via the Accessible interface
    use gtk::accessible::Property;
    button.update_property(&[Property::Description(tooltip)]);
    button
}

/// Build the keyboard shortcuts window for the help overlay.
fn build_shortcuts_window(parent: &adw::ApplicationWindow) -> gtk::ShortcutsWindow {
    let shortcuts = gtk::ShortcutsWindow::builder()
        .title("Keyboard Shortcuts")
        .modal(true)
        .transient_for(parent)
        .build();

    let section = gtk::ShortcutsSection::builder()
        .title("Mailin Keyboard Shortcuts")
        .max_height(300)
        .build();

    let shortcuts_data = [
        ("<Control>N", "New message"),
        ("<Control>F", "Search mail"),
        ("F5", "Synchronize"),
        ("<Control><Shift>A", "Add account"),
        ("<Control>comma", "Preferences"),
        ("<Control>Return", "Send message (in composer)"),
        ("Delete", "Move selected messages to Trash"),
        ("<Control>Delete", "Delete selected messages permanently"),
        ("<Control><Shift>E", "Archive selected messages"),
        ("<Control><Shift>J", "Mark selected messages as Junk"),
        ("<Control><Shift>K", "Mark selected messages as not Junk"),
        ("<Control>M", "Move selected messages to a folder"),
        ("<Control>A", "Select all messages"),
        ("Escape", "Clear message selection"),
        ("<Control>space", "Toggle selection of the current message"),
        ("Shift+Down", "Extend selection downwards"),
        ("Shift+Up", "Extend selection upwards"),
        ("Up", "Navigate message list (up)"),
        ("Down", "Navigate message list (down)"),
        ("Return", "Open selected message"),
        ("F1", "Show this help"),
        ("<Control>slash", "Show this help (alternate)"),
    ];

    for (accel, description) in shortcuts_data {
        let shortcut = gtk::ShortcutsShortcut::builder()
            .accelerator(accel)
            .title(description)
            .build();
        section.append(&shortcut);
    }

    shortcuts.set_child(Some(&section));
    shortcuts
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

pub(crate) fn role_icon(role: FolderRole) -> &'static str {
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

fn outbox_entry_row(entry: &OutboxEntry, _now: i64) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    outer.set_margin_top(8);
    outer.set_margin_bottom(8);
    outer.set_margin_start(12);
    outer.set_margin_end(12);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 3);
    column.set_hexpand(true);

    let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let state_text = entry.state.as_str().to_uppercase();
    let state_label = gtk::Label::new(Some(&state_text));
    state_label.add_css_class("caption");
    match entry.state {
        OutboxState::Queued => state_label.add_css_class("accent"),
        OutboxState::Sending => state_label.add_css_class("warning"),
        OutboxState::Failed => state_label.add_css_class("error"),
        OutboxState::Sent => state_label.add_css_class("success"),
    }
    line.append(&state_label);

    if let Some(error) = &entry.last_error {
        let error_label = gtk::Label::new(Some(&format!("— {}", error)));
        error_label.add_css_class("caption");
        error_label.add_css_class("dim-label");
        error_label.set_ellipsize(pango::EllipsizeMode::End);
        error_label.set_hexpand(true);
        line.append(&error_label);
    } else {
        let attempts_label = gtk::Label::new(Some(&format!("attempt {}/5", entry.attempts + 1)));
        attempts_label.add_css_class("caption");
        attempts_label.add_css_class("dim-label");
        line.append(&attempts_label);
    }
    column.append(&line);

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
    let subject_label = gtk::Label::new(Some(&subject));
    subject_label.set_xalign(0.0);
    subject_label.set_ellipsize(pango::EllipsizeMode::End);
    column.append(&subject_label);

    outer.append(&column);
    row.set_child(Some(&outer));
    row.set_tooltip_text(Some(&format!("ID: {}", entry.id)));
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
