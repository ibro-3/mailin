//! The three-pane mail window: folders, message list and reader.
//!
//! The window owns no mail state of its own: everything is read from the core
//! store. A small timer drains the core event channel and refreshes the widgets
//! that changed, which keeps the UI correct regardless of whether an update
//! came from the network worker or from a local edit.

use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gdk::{Key, ModifierType};
use gdk_pixbuf::prelude::*;
use glib::variant::ToVariant;
use gtk::accessible::Property;
use gtk::prelude::*;
use gtk::{gio, glib, pango};
use mailin_core::{
    Account, AccountId, AttachmentMeta, ClientEvent, Folder, FolderId, FolderRole, MessageHeader,
    MessageId, MessageListDensity, OutboxEntry, OutboxState, OutgoingEnvelope, OutgoingMessage,
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

/// Responsive breakpoints (in CSS pixels, matching WinUI).
const BREAKPOINT_MOBILE: f64 = 720.0;
const BREAKPOINT_TABLET: f64 = 1024.0;

#[derive(Debug, Default, Clone)]
struct Selection {
    account: Option<AccountId>,
    folder: Option<FolderId>,
    /// When `true`, the message list shows outbox entries instead of the selected folder.
    show_outbox: bool,
    /// When `true`, the message list shows unified inbox across all accounts.
    unified_inbox: bool,
    message: Option<MessageId>,
    /// Outbox entries currently shown (when show_outbox is true)
    outbox_entries: Vec<OutboxEntry>,
    query: String,
    search: bool,
    /// Search scope: false = current folder, true = all mail
    search_all_mail: bool,
    /// Number of headers requested for the current folder.
    limit: u32,
    /// Current window width for responsive layout.
    window_width: f64,
    /// Whether we're in mobile layout (stack navigation).
    is_mobile: bool,
    /// Navigation stack for mobile message list -> reader navigation.
    nav_stack: Vec<NavTarget>,
}

/// Navigation target for mobile stack navigation.
#[derive(Debug, Clone)]
enum NavTarget {
    MessageReader,
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
    scope_button: gtk::ToggleButton,
    history_button: gtk::Button,
    overflow_button: gtk::MenuButton,
    reader_stack: gtk::Stack,
    reader_subject: gtk::Label,
    reader_avatar: adw::Avatar,
    reader_from: gtk::Label,
    reader_to: gtk::Label,
    reader_cc: gtk::Label,
    reader_date: gtk::Label,
    remote_panel: gtk::Revealer,
    remote_once: gtk::Button,
    remote_trust: gtk::Button,
    remote_dismiss: gtk::Button,
    reader_attachments: gtk::Box,
    reader_find_revealer: gtk::Revealer,
    reader_find_entry: gtk::SearchEntry,
    reader_find_prev: gtk::Button,
    reader_find_next: gtk::Button,
    reader_find_close: gtk::Button,
    reader_find_status: gtk::Label,
    reader_body: gtk::Box,
    read_button: gtk::Button,
    flag_button: gtk::ToggleButton,
    html_button: gtk::ToggleButton,
    reply_button: gtk::Button,
    reply_all_button: gtk::Button,
    forward_button: gtk::Button,
    sync_status: gtk::Image,
    /// Hamburger button for mobile responsive layout.
    hamburger_button: gtk::Button,
    /// Back button for mobile navigation (reader -> message list).
    back_button: gtk::Button,
    /// Message list pane; hidden while the mobile reader is on screen.
    list_pane: gtk::Box,
    /// Paned widget for message list / reader split.
    split: adw::OverlaySplitView,
}

/// An attachment whose bytes are needed before the save dialog can open.
/// `target_dir`, when set, means "save there" instead of prompting (e.g. Download all).
#[derive(Debug, Clone)]
struct PendingAttachment {
    message_id: MessageId,
    attachment: AttachmentMeta,
    target_dir: Option<PathBuf>,
}

/// Holder for a mailto: draft that should be opened on the next window creation.
/// Used when the mailto: link activates before the main window exists.
static MAILTO_DRAFT: std::sync::Mutex<Option<OutgoingMessage>> = std::sync::Mutex::new(None);

/// Request a new compose window (e.g. from `--compose` / desktop action).
static COMPOSE_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// Request a new compose window on the next activation / when the main window is ready.
pub fn set_compose_requested() {
    COMPOSE_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn take_compose_requested() -> bool {
    COMPOSE_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst)
}

// The live main window, weakly held on the GTK main thread so runtime
// `mailto:` activations can find it after startup.
thread_local! {
    static MAIN_WINDOW: RefCell<Option<std::rc::Weak<MainWindow>>> =
        const { RefCell::new(None) };
}

/// Open (or queue) the mailto draft captured at launch or runtime.
///
/// If the main window exists, present it and open the composer now; otherwise
/// keep the draft for the next window creation.
pub fn handle_mailto_draft() {
    let Some(draft) = take_mailto_draft() else {
        return;
    };
    let window = MAIN_WINDOW
        .with(|slot| slot.borrow().clone())
        .and_then(|weak| weak.upgrade());
    match window {
        Some(window) => {
            window.present();
            window.open_compose(draft);
        }
        None => {
            // No window yet; leave it for MainWindow::new to consume.
            set_mailto_draft(draft);
        }
    }
}

/// Open a new compose window if one was requested (e.g. `--compose`).
pub fn handle_compose_requested() {
    if !take_compose_requested() {
        return;
    }
    let window = MAIN_WINDOW
        .with(|slot| slot.borrow().clone())
        .and_then(|weak| weak.upgrade());
    if let Some(window) = window {
        window.present();
        window.open_compose_new();
    } else {
        // No window yet; MainWindow::new will consume the flag.
        set_compose_requested();
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
    /// HTML view currently targeted by find-in-message (main or decrypted).
    find_target_view: Rc<RefCell<Option<webkit6::WebView>>>,
    /// Whether the reader prefers HTML; starts from the user preference.
    html_mode: Cell<bool>,
    /// Message for which the user explicitly allowed remote content.
    remote_allowed: Cell<Option<MessageId>>,
    pending_attachment: Rc<RefCell<Vec<PendingAttachment>>>,
    /// Draft edit window per message, so selecting the same draft row again
    /// focuses the open composer instead of spawning a second one.
    last_draft: RefCell<Option<(MessageId, glib::WeakRef<gtk::Window>)>>,
    /// Expanded thread roots (thread_root_key) for threaded view.
    expanded_threads: RefCell<std::collections::HashSet<String>>,
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
        window.set_size_request(640, 480);

        let title = adw::WindowTitle::new("Mailin", "No account");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));

        let sidebar_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Toggle folders")
            .active(true)
            .build();
        sidebar_toggle.add_css_class("flat");
        // Icon-only toggle: give screen readers a name.
        {
            sidebar_toggle.update_property(&[Property::Label("Toggle folders")]);
        }
        header.pack_start(&sidebar_toggle);

        // Hamburger button for mobile responsive layout
        let hamburger_button = gtk::Button::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Menu")
            .build();
        hamburger_button.add_css_class("flat");
        hamburger_button.set_visible(false); // Hidden by default, shown on mobile
        header.pack_start(&hamburger_button);

        // Back button for mobile navigation (message reader -> message list)
        let back_button = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back")
            .build();
        back_button.add_css_class("flat");
        back_button.set_visible(false); // Hidden by default, shown on mobile when in reader view
        header.pack_start(&back_button);

        let compose_button = icon_button("mail-message-new-symbolic", "New message (Ctrl+N)");
        header.pack_start(&compose_button);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search mail")
            .width_chars(18)
            .build();
        let sync_button = icon_button("view-refresh-symbolic", "Synchronize now (F5)");

        // Sync status indicator (icon + colour per state; screen readers get
        // an accessible name, not just a tooltip)
        let sync_status = gtk::Image::from_icon_name("network-transmit-receive-symbolic");
        sync_status.set_pixel_size(16);
        sync_status.set_accessible_role(gtk::AccessibleRole::Status);
        sync_status.add_css_class("sync-status-idle");
        sync_status.set_tooltip_text(Some("Sync status: Idle"));
        {
            sync_status.update_property(&[Property::Label("Sync status: Idle")]);
        }

        // Scope toggle (Current folder / All mail)
        let scope_button = gtk::ToggleButton::builder()
            .label("Current folder")
            .tooltip_text("Search scope: Current folder / All mail")
            .build();
        scope_button.add_css_class("flat");

        // Search history button
        let history_button = icon_button("view-history-symbolic", "Search history");
        history_button.add_css_class("flat");

        // Overflow-menu actions. Always visible (GNOME-style app menu); the
        // search-specific toggles here stay in step with the toolbar via the
        // `search-all-mail` stateful action.
        let overflow_menu = gio::Menu::new();
        overflow_menu.append(Some("Search in all mail"), Some("win.search-all-mail"));
        overflow_menu.append(Some("Search history"), Some("win.search-history"));
        overflow_menu.append(Some("Synchronize now"), Some("win.sync"));
        overflow_menu.append(Some("New message"), Some("win.compose"));
        overflow_menu.append(Some("Preferences"), Some("win.preferences"));
        overflow_menu.append(Some("Keyboard shortcuts"), Some("win.show-help-overlay"));
        overflow_menu.append(Some("About Mailin"), Some("win.about"));
        let overflow_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("More actions")
            .menu_model(&overflow_menu)
            .build();
        overflow_button.add_css_class("flat");
        {
            overflow_button.update_property(&[Property::Label("More actions")]);
        }

        header.pack_end(&sync_button);
        header.pack_end(&sync_status);
        header.pack_end(&scope_button);
        header.pack_end(&history_button);
        header.pack_end(&search);
        header.pack_end(&overflow_button);

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
        {
            account_menu_button.update_property(&[Property::Label("Account options")]);
        }

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

        let folders_label = gtk::Label::builder()
            .label("Folders")
            .xalign(0.0)
            .hexpand(true)
            .build();
        folders_label.add_css_class("heading");
        let new_folder_button = icon_button("list-add-symbolic", "New folder (Ctrl+Shift+N)");
        new_folder_button.set_action_name(Some("win.new-folder"));
        let folders_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        folders_header.set_margin_top(8);
        folders_header.set_margin_bottom(4);
        folders_header.set_margin_start(12);
        folders_header.set_margin_end(12);
        folders_header.append(&folders_label);
        folders_header.append(&new_folder_button);

        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.set_width_request(250);
        sidebar.append(&account_bar);
        sidebar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        sidebar.append(&folders_header);
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
        {
            message_menu_button.update_property(&[Property::Label("Message actions")]);
        }
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

        let reader_avatar = adw::Avatar::builder()
            .size(40)
            .show_initials(true)
            .text("?")
            .build();
        reader_avatar.set_valign(gtk::Align::Start);

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

        // Remote-content banner: "Load once" keeps the session behaviour of the
        // old adw::Banner; the second action trusts the sender's domain on the
        // account, which is why the banner is a plain two-button box.
        let remote_text =
            gtk::Label::new(Some("Remote content is blocked to protect your privacy."));
        remote_text.set_xalign(0.0);
        remote_text.set_hexpand(true);
        remote_text.set_wrap(true);
        let remote_once = gtk::Button::with_label("Load once");
        remote_once.add_css_class("flat");
        let remote_trust = gtk::Button::with_label("Always allow sender");
        remote_trust.add_css_class("flat");
        let remote_dismiss = gtk::Button::from_icon_name("window-close-symbolic");
        remote_dismiss.add_css_class("flat");
        remote_dismiss.set_size_request(32, 32);
        remote_dismiss.set_tooltip_text(Some("Not now"));
        remote_dismiss.update_property(&[gtk::accessible::Property::Label(
            "Dismiss remote content prompt",
        )]);
        let remote_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        remote_row.add_css_class("remote-banner");
        remote_row.append(&remote_text);
        remote_row.append(&remote_once);
        remote_row.append(&remote_trust);
        remote_row.append(&remote_dismiss);
        let remote_panel = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .child(&remote_row)
            .build();

        // Find-in-message bar (for HTML reader)
        let reader_find_entry = gtk::SearchEntry::builder()
            .placeholder_text("Find in message")
            .hexpand(true)
            .build();
        let reader_find_prev = icon_button("go-up-symbolic", "Previous match");
        let reader_find_next = icon_button("go-down-symbolic", "Next match");
        let reader_find_close = icon_button("window-close-symbolic", "Close find");
        let reader_find_status = gtk::Label::new(None);
        reader_find_status.add_css_class("dim-label");
        reader_find_status.add_css_class("caption");
        let reader_find_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        reader_find_bar.append(&reader_find_entry);
        reader_find_bar.append(&reader_find_prev);
        reader_find_bar.append(&reader_find_next);
        reader_find_bar.append(&reader_find_status);
        reader_find_bar.append(&reader_find_close);
        let reader_find_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .child(&reader_find_bar)
            .build();

        let reader_body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        reader_body.set_margin_top(6);
        reader_body.set_margin_bottom(24);

        let flag_button = gtk::ToggleButton::builder()
            .icon_name("non-starred-symbolic")
            .tooltip_text("Star message")
            .build();
        flag_button.add_css_class("flat");
        {
            flag_button.update_property(&[Property::Label("Star message")]);
        }
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
        let reader_from_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        reader_from_row.set_valign(gtk::Align::Start);
        reader_from_row.append(&reader_avatar);
        reader_from_row.append(&reader_from);
        reader_from.set_hexpand(true);

        reader_content.append(&reader_actions);
        reader_content.append(&reader_subject);
        reader_content.append(&reader_from_row);
        reader_content.append(&reader_to);
        reader_content.append(&reader_cc);
        reader_content.append(&reader_date);
        reader_content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        reader_content.append(&remote_panel);
        reader_content.append(&reader_attachments);
        reader_content.append(&reader_find_revealer);
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

        let html_mode = app.config.borrow().ui.html_enabled;
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
            find_target_view: Rc::new(RefCell::new(None)),
            html_mode: Cell::new(html_mode),
            remote_allowed: Cell::new(None),
            pending_attachment: Rc::new(RefCell::new(Vec::new())),
            last_draft: RefCell::new(None),
            expanded_threads: RefCell::new(std::collections::HashSet::new()),
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
                scope_button,
                history_button,
                overflow_button,
                reader_stack,
                reader_subject,
                reader_avatar,
                reader_from,
                reader_to,
                reader_cc,
                reader_date,
                remote_panel,
                remote_once,
                remote_trust,
                remote_dismiss,
                reader_attachments,
                reader_find_revealer,
                reader_find_entry,
                reader_find_prev,
                reader_find_next,
                reader_find_close,
                reader_find_status,
                reader_body,
                read_button,
                flag_button,
                html_button,
                reply_button,
                reply_all_button,
                forward_button,
                sync_status,
                hamburger_button,
                back_button,
                list_pane,
                split,
            },
        });

        Self::connect(&this);
        this.refresh_accounts();

        // Remember the window so runtime "mailto:"/--compose activations can find it.
        MAIN_WINDOW.with(|slot| *slot.borrow_mut() = Some(Rc::downgrade(&this)));

        // Check for a mailto: draft that was set before window creation
        if let Some(draft) = take_mailto_draft() {
            this.open_compose(draft);
        }
        if take_compose_requested() {
            this.open_compose_new();
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
        // Register as the standard help overlay so GTK exposes it through the
        // `win.show-help-overlay` action and the window's accessibility tree.
        this.widgets.window.set_help_overlay(Some(&shortcuts));
        let key_controller = gtk::EventControllerKey::new();
        key_controller.connect_key_pressed({
            let shortcuts = shortcuts.clone();
            move |_, keyval, _, state| {
                let ctrl = state.contains(ModifierType::CONTROL_MASK);
                if keyval == Key::F1 || (ctrl && (keyval == Key::question || keyval == Key::slash))
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

        // Responsive layout update via AdwBreakpoint (M3a-2): event-driven at
        // boundary crossings, no polling. Logic is identical for all modes.
        let apply_breakpoints = {
            let this = Rc::clone(this);
            move |width: f64| {
                let is_mobile = width < BREAKPOINT_MOBILE;
                let is_tablet = (BREAKPOINT_MOBILE..BREAKPOINT_TABLET).contains(&width);
                let is_desktop = width >= BREAKPOINT_TABLET;

                let mut selection = this.selection.borrow_mut();
                selection.window_width = width;
                selection.is_mobile = is_mobile;
                drop(selection);

                this.widgets
                    .hamburger_button
                    .set_visible(is_mobile || is_tablet);

                this.widgets.scope_button.set_visible(is_desktop);
                this.widgets.history_button.set_visible(is_desktop);
                this.widgets.overflow_button.set_visible(true);

                if is_mobile {
                    this.widgets.split.set_collapsed(true);
                    this.widgets.split.set_show_sidebar(false);
                } else {
                    this.widgets.split.set_collapsed(false);
                    this.widgets.split.set_show_sidebar(true);
                    if is_tablet {
                        this.widgets.split.set_min_sidebar_width(200.0);
                        this.widgets.split.set_max_sidebar_width(280.0);
                    } else {
                        this.widgets.split.set_min_sidebar_width(220.0);
                        this.widgets.split.set_max_sidebar_width(320.0);
                    }
                    this.reset_mobile_nav();
                }
            }
        };

        let current_width = {
            let this = Rc::clone(this);
            move || {
                let allocated = this.widgets.window.width();
                if allocated > 0 {
                    allocated as f64
                } else {
                    this.widgets.window.default_width() as f64
                }
            }
        };

        apply_breakpoints(current_width());
        let on_breakpoint = {
            let apply_breakpoints = apply_breakpoints.clone();
            let current_width = current_width.clone();
            Rc::new(move || apply_breakpoints(current_width()))
        };
        if let Ok(cond) = adw::BreakpointCondition::parse(&format!(
            "max-width: {}px",
            BREAKPOINT_MOBILE as i32 - 1
        )) {
            let bp = adw::Breakpoint::new(cond);
            let f = Rc::clone(&on_breakpoint);
            bp.connect_apply(move |_| f());
            let f = Rc::clone(&on_breakpoint);
            bp.connect_unapply(move |_| f());
            this.widgets.window.add_breakpoint(bp);
        }
        if let Ok(cond) = adw::BreakpointCondition::parse(&format!(
            "max-width: {}px",
            BREAKPOINT_TABLET as i32 - 1
        )) {
            let bp = adw::Breakpoint::new(cond);
            let f = Rc::clone(&on_breakpoint);
            bp.connect_apply(move |_| f());
            let f = Rc::clone(&on_breakpoint);
            bp.connect_unapply(move |_| f());
            this.widgets.window.add_breakpoint(bp);
        }

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

        // Notification action: mark as read (accepts "account_id:message_id1,message_id2,..." parameter)
        let mark_read =
            gio::SimpleAction::new("notification-mark-read", Some(glib::VariantTy::STRING));
        mark_read.connect_activate({
            let this = Rc::clone(this);
            move |_, parameter| {
                let Some(param) = parameter.and_then(|v| v.str()) else {
                    return;
                };
                this.notification_mark_read(param);
            }
        });
        this.widgets.window.add_action(&mark_read);

        // Notification action: archive (accepts "account_id:message_id1,message_id2,..." parameter)
        let archive = gio::SimpleAction::new("notification-archive", Some(glib::VariantTy::STRING));
        archive.connect_activate({
            let this = Rc::clone(this);
            move |_, parameter| {
                let Some(param) = parameter.and_then(|v| v.str()) else {
                    return;
                };
                this.notification_archive(param);
            }
        });
        this.widgets.window.add_action(&archive);

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

        // Hamburger button click handler (mobile sidebar toggle)
        this.widgets.hamburger_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| {
                let split = &this.widgets.split;
                split.set_show_sidebar(!split.shows_sidebar());
            }
        });

        // Back button click handler (mobile navigation: reader -> message list)
        this.widgets.back_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| {
                let mut selection = this.selection.borrow_mut();
                if let Some(NavTarget::MessageReader) = selection.nav_stack.pop() {
                    // Back to the list: show it, hide the back button
                    this.widgets.list_pane.set_visible(true);
                    this.widgets.back_button.set_visible(false);
                }
            }
        });

        this.widgets.account_dropdown.connect_selected_notify({
            let this = Rc::clone(this);
            move |dropdown| {
                if this.programmatic.get() {
                    return;
                }
                let index = dropdown.selected();
                if index == 0 {
                    // "All Accounts" selected - enter unified inbox mode
                    this.enter_unified_inbox();
                } else {
                    // Regular account selected (index - 1 because of "All Accounts" at index 0)
                    let account = this
                        .accounts
                        .borrow()
                        .get((index - 1) as usize)
                        .map(|account| account.id);
                    if let Some(account) = account {
                        this.select_account(account);
                    }
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
                    let _ = this.select_folder(&folder);
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

        // Thread expand/collapse on chevron click. Restrict to the primary
        // button and to the left-hand chevron region so a normal row click still
        // opens the message, and claim the gesture so activation is suppressed.
        let click_gesture = gtk::GestureClick::new();
        click_gesture.set_button(gtk::gdk::BUTTON_PRIMARY);
        click_gesture.connect_pressed({
            let this = Rc::clone(this);
            move |gesture, n_press, x, y| {
                if this.programmatic.get() || n_press != 1 {
                    return;
                }
                // The chevron sits at the very start of the row (after a small
                // margin); only treat clicks there as expand/collapse.
                if x > 32.0 {
                    return;
                }
                let list_box = &this.widgets.message_list;
                if let Some(row) = list_box.row_at_y(y as i32) {
                    let index = row.index() as usize;
                    if let Some(message) = this.messages.borrow().get(index).cloned() {
                        let is_thread_root = message.thread_reply_count > 0
                            || (message
                                .thread_root_key
                                .as_ref()
                                .is_some_and(|k| k == message.message_id.as_deref().unwrap_or("")));
                        if is_thread_root {
                            let root_key = message
                                .thread_root_key
                                .as_ref()
                                .or(message.message_id.as_ref())
                                .cloned()
                                .unwrap_or_default();
                            let mut expanded = this.expanded_threads.borrow_mut();
                            if expanded.contains(&root_key) {
                                expanded.remove(&root_key);
                            } else {
                                expanded.insert(root_key);
                            }
                            drop(expanded);
                            this.refresh_messages();
                            // Consume the click so the row is not also activated.
                            gesture.set_state(gtk::EventSequenceState::Claimed);
                        }
                    }
                }
            }
        });
        this.widgets.message_list.add_controller(click_gesture);

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

        this.widgets.remote_once.connect_clicked({
            let this = Rc::clone(this);
            move |_| {
                let Some(message_id) = this.selection.borrow().message else {
                    return;
                };
                this.remote_allowed.set(Some(message_id));
                this.widgets.remote_panel.set_reveal_child(false);
                this.render_body(message_id);
            }
        });

        this.widgets.remote_trust.connect_clicked({
            let this = Rc::clone(this);
            move |_| this.trust_current_sender()
        });

        this.widgets.remote_dismiss.connect_clicked({
            let this = Rc::clone(this);
            move |_| {
                this.widgets.remote_panel.set_reveal_child(false);
            }
        });

        this.widgets.compose_button.connect_clicked({
            let this = Rc::clone(this);
            move |_| {
                let _ = this.open_compose(this.new_draft());
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

        // M4-D: find in message (HTML reader)
        {
            let entry = this.widgets.reader_find_entry.clone();
            let status = this.widgets.reader_find_status.clone();
            let find_target = Rc::clone(&this.find_target_view);
            let fallback_view = this.html_view.borrow().clone();
            let _ = fallback_view; // may be unused if target is set
            entry.connect_search_changed(move |entry| {
                let text = entry.text().to_string();
                let view_opt = find_target.borrow().clone();
                if let Some(view) = view_opt {
                    if let Some(fc) = view.find_controller() {
                        if text.is_empty() {
                            fc.search_finish();
                            status.set_text("");
                        } else {
                            let opts = webkit6::FindOptions::CASE_INSENSITIVE
                                | webkit6::FindOptions::WRAP_AROUND;
                            fc.search(&text, opts.bits(), u32::MAX);
                            fc.count_matches(&text, opts.bits(), u32::MAX);
                            status.set_text("");
                        }
                    }
                }
            });
            let entry = this.widgets.reader_find_entry.clone();
            let find_target = Rc::clone(&this.find_target_view);
            entry.connect_activate(move |_| {
                if let Some(view) = find_target.borrow().as_ref() {
                    if let Some(fc) = view.find_controller() {
                        fc.search_next();
                    }
                }
            });
            let entry = this.widgets.reader_find_entry.clone();
            let this_stop = Rc::clone(this);
            entry.connect_stop_search(move |e| {
                e.set_text("");
                this_stop.widgets.reader_find_status.set_text("");
                this_stop
                    .widgets
                    .reader_find_revealer
                    .set_reveal_child(false);
                if let Some(view) = this_stop.find_target_view.borrow().as_ref() {
                    if let Some(fc) = view.find_controller() {
                        fc.search_finish();
                    }
                }
            });
            let find_target = Rc::clone(&this.find_target_view);
            this.widgets.reader_find_next.connect_clicked(move |_| {
                if let Some(view) = find_target.borrow().as_ref() {
                    if let Some(fc) = view.find_controller() {
                        fc.search_next();
                    }
                }
            });
            let find_target = Rc::clone(&this.find_target_view);
            this.widgets.reader_find_prev.connect_clicked(move |_| {
                if let Some(view) = find_target.borrow().as_ref() {
                    if let Some(fc) = view.find_controller() {
                        fc.search_previous();
                    }
                }
            });
            let this_close = Rc::clone(this);
            this.widgets.reader_find_close.connect_clicked(move |_| {
                this_close
                    .widgets
                    .reader_find_revealer
                    .set_reveal_child(false);
                if let Some(view) = this_close.find_target_view.borrow().as_ref() {
                    if let Some(fc) = view.find_controller() {
                        fc.search_finish();
                    }
                }
                this_close.widgets.reader_find_entry.set_text("");
                this_close.widgets.reader_find_status.set_text("");
            });
        }

        // Overflow-menu counterparts of the secondary search controls. They
        // drive the same widgets, so state never diverges between the toolbar
        // and the narrow-window menu.
        let search_all_action =
            gio::SimpleAction::new_stateful("search-all-mail", None, &false.to_variant());
        search_all_action.connect_activate({
            let this = Rc::clone(this);
            move |action, _| {
                let active = !this.widgets.scope_button.is_active();
                this.widgets.scope_button.set_active(active);
                action.set_state(&active.to_variant());
                if this.selection.borrow().search {
                    this.run_search();
                }
            }
        });
        this.widgets.window.add_action(&search_all_action);

        let search_history_action = gio::SimpleAction::new("search-history", None);
        search_history_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.widgets.history_button.emit_clicked()
        });
        this.widgets.window.add_action(&search_history_action);

        // Keep the checkable overflow item in step with the toolbar toggle.
        this.widgets.scope_button.connect_toggled({
            let this = Rc::clone(this);
            move |button| {
                if let Some(action) = this.widgets.window.lookup_action("search-all-mail") {
                    if let Some(action) = action.downcast_ref::<gio::SimpleAction>() {
                        action.set_state(&button.is_active().to_variant());
                    }
                }
            }
        });

        // Search history popover (attached to history_button)
        let history_popover = gtk::Popover::builder().has_arrow(true).build();
        let history_list = gtk::ListBox::new();
        history_list.set_selection_mode(gtk::SelectionMode::None);
        history_popover.set_child(Some(&history_list));
        history_popover.set_parent(&this.widgets.history_button);

        this.widgets.history_button.connect_clicked({
            let this = Rc::clone(this);
            let history_popover = history_popover.clone();
            let history_list = history_list.clone();
            move |_| {
                // Always answer the click: an empty or unavailable history
                // is shown as a placeholder row, not as no response at all.
                while let Some(child) = history_list.first_child() {
                    history_list.remove(&child);
                }
                let Some(account) = this.selection.borrow().account else {
                    let row = adw::ActionRow::builder()
                        .title("Search history is stored per account")
                        .subtitle("Open an account to see its recent searches.")
                        .build();
                    history_list.append(&row);
                    history_popover.popup();
                    return;
                };
                let history = this.app.mailin.search_history(&account);
                if history.is_empty() {
                    let row = adw::ActionRow::builder()
                        .title("No recent searches")
                        .subtitle("Searches you run will appear here.")
                        .build();
                    history_list.append(&row);
                    history_popover.popup();
                    return;
                }
                let now = format::now_unix();
                for entry in history {
                    let row = adw::ActionRow::builder()
                        .title(&entry.query)
                        .subtitle(format!("{} ago", format::list_time(now, entry.timestamp)))
                        .build();
                    row.connect_activated({
                        let this = Rc::clone(&this);
                        let query = entry.query.clone();
                        let folder_id = entry.folder_id;
                        let history_popover = history_popover.clone();
                        move |_| {
                            history_popover.popdown();
                            this.widgets.search.set_text(&query);
                            if let Some(folder_id) = folder_id {
                                this.select_folder_by_id(folder_id);
                            }
                            this.run_search();
                        }
                    });
                    history_list.append(&row);
                }
                history_popover.popup();
            }
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

        // Folder management actions. Folder-specific actions take the folder id
        // as a string parameter so every menu button can target its own row.
        let new_folder_action = gio::SimpleAction::new("new-folder", None);
        new_folder_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| Self::create_folder(this.clone())
        });
        this.widgets.window.add_action(&new_folder_action);

        let rename_folder_action =
            gio::SimpleAction::new("rename-folder", Some(glib::VariantTy::STRING));
        rename_folder_action.connect_activate({
            let this = Rc::clone(this);
            move |_, param| {
                let Some(param) = param.and_then(|v| v.str()) else {
                    return;
                };
                Self::rename_folder(this.clone(), param.to_string())
            }
        });
        this.widgets.window.add_action(&rename_folder_action);

        let delete_folder_action =
            gio::SimpleAction::new("delete-folder", Some(glib::VariantTy::STRING));
        delete_folder_action.connect_activate({
            let this = Rc::clone(this);
            move |_, param| {
                let Some(param) = param.and_then(|v| v.str()) else {
                    return;
                };
                Self::delete_folder(this.clone(), param.to_string())
            }
        });
        this.widgets.window.add_action(&delete_folder_action);

        let subscribe_folder_action =
            gio::SimpleAction::new("subscribe-folder", Some(glib::VariantTy::STRING));
        subscribe_folder_action.connect_activate({
            let this = Rc::clone(this);
            move |_, param| {
                let Some(param) = param.and_then(|v| v.str()) else {
                    return;
                };
                Self::subscribe_folder(this.clone(), param.to_string(), true)
            }
        });
        this.widgets.window.add_action(&subscribe_folder_action);

        let unsubscribe_folder_action =
            gio::SimpleAction::new("unsubscribe-folder", Some(glib::VariantTy::STRING));
        unsubscribe_folder_action.connect_activate({
            let this = Rc::clone(this);
            move |_, param| {
                let Some(param) = param.and_then(|v| v.str()) else {
                    return;
                };
                Self::subscribe_folder(this.clone(), param.to_string(), false)
            }
        });
        this.widgets.window.add_action(&unsubscribe_folder_action);

        let folder_properties_action =
            gio::SimpleAction::new("folder-properties", Some(glib::VariantTy::STRING));
        folder_properties_action.connect_activate({
            let this = Rc::clone(this);
            move |_, param| {
                let Some(param) = param.and_then(|v| v.str()) else {
                    return;
                };
                Self::folder_properties(Rc::clone(&this), param.to_string())
            }
        });
        this.widgets.window.add_action(&folder_properties_action);

        let move_action = gio::SimpleAction::new("move", None);
        move_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| Self::move_selected(&this)
        });
        this.widgets.window.add_action(&move_action);

        if let Some(application) = this.widgets.window.application() {
            for (action, accels) in WINDOW_ACTION_ACCELS {
                application.set_accels_for_action(action, accels);
            }
        }

        let compose_action = gio::SimpleAction::new("compose", None);
        compose_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| {
                let _ = this.open_compose(this.new_draft());
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
            let this = Rc::clone(this);
            move |_, _| {
                // M4-D: find in HTML message when available, else focus mail search.
                if this.widgets.html_button.is_sensitive()
                    && this.widgets.reader_find_revealer.child().is_some()
                {
                    this.widgets.reader_find_revealer.set_reveal_child(true);
                    this.widgets.reader_find_entry.grab_focus();
                } else {
                    this.widgets.search.grab_focus();
                }
            }
        });
        this.widgets.window.add_action(&search_action);

        let find_message_action = gio::SimpleAction::new("find-message", None);
        find_message_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| {
                this.widgets.reader_find_revealer.set_reveal_child(true);
                this.widgets.reader_find_entry.grab_focus();
            }
        });
        this.widgets.window.add_action(&find_message_action);

        let sync_action = gio::SimpleAction::new("sync", None);
        sync_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| this.sync_now()
        });
        this.widgets.window.add_action(&sync_action);

        let move_to_folder_action =
            gio::SimpleAction::new("move-messages-to-folder", Some(glib::VariantTy::STRING));
        move_to_folder_action.connect_activate({
            let this = Rc::clone(this);
            move |_, parameter| {
                let Some(param) = parameter.and_then(|v| v.str()) else {
                    return;
                };
                let mut parts = param.splitn(2, ':');
                let (Some(folder_str), Some(msg_str)) = (parts.next(), parts.next()) else {
                    return;
                };
                let Ok(folder_id) = folder_str.parse::<i64>() else {
                    return;
                };
                if folder_id <= 0 {
                    return;
                }
                let Ok(msg_id) = msg_str.parse::<i64>() else {
                    return;
                };
                this.move_message_to_folder(MessageId(msg_id), FolderId(folder_id));
            }
        });
        this.widgets.window.add_action(&move_to_folder_action);

        let undo_move_action =
            gio::SimpleAction::new("undo-move-multi", Some(glib::VariantTy::STRING));
        undo_move_action.connect_activate({
            let this = Rc::clone(this);
            move |_, parameter| {
                let Some(param) = parameter.and_then(|v| v.str()) else {
                    return;
                };
                this.undo_move_multi(param);
            }
        });
        this.widgets.window.add_action(&undo_move_action);

        // Surface a Retry button on the account/sync error banner. Activating
        // it runs the same action as the F5 "Synchronize now" shortcut.
        this.widgets.banner.set_button_label(Some("Retry"));
        this.widgets.banner.connect_button_clicked({
            let this = Rc::clone(this);
            move |_| {
                this.sync_now();
            }
        });

        let preferences_action = gio::SimpleAction::new("preferences", None);
        preferences_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| {
                let window = Rc::clone(&this);
                crate::preferences::show_preferences(&this.widgets.window, &this.app, move || {
                    window.apply_config();
                });
            }
        });
        this.widgets.window.add_action(&preferences_action);

        let about_action = gio::SimpleAction::new("about", None);
        about_action.connect_activate({
            let this = Rc::clone(this);
            move |_, _| {
                let about = adw::AboutDialog::builder()
                    .application_name("Mailin")
                    .application_icon("dev.mailin.Mailin")
                    .version(env!("CARGO_PKG_VERSION"))
                    .developer_name("Mailin contributors")
                    .developers(["Mailin contributors"])
                    .license_type(gtk::License::MitX11)
                    .comments("Native desktop email client for IMAP and SMTP.")
                    .copyright("© 2026 Mailin contributors")
                    .website("https://github.com/mailin-app/mailin")
                    .issue_url("https://github.com/mailin-app/mailin/issues")
                    .build();
                about.present(Some(&this.widgets.window));
            }
        });
        this.widgets.window.add_action(&about_action);

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
        let mut names: Vec<String> = accounts
            .iter()
            .map(|account| format!("{} <{}>", account.display_name, account.email))
            .collect();
        // Add "All Accounts" option for unified inbox at the top
        names.insert(0, "All Accounts (Unified Inbox)".to_string());
        let model = gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
        self.widgets.account_dropdown.set_model(Some(&model));
        if let Some(index) =
            keep.and_then(|id| accounts.iter().position(|account| account.id == id))
        {
            // +1 because we inserted "All Accounts" at index 0
            self.widgets
                .account_dropdown
                .set_selected((index + 1) as u32);
        } else if accounts.is_empty() {
            // No accounts, select "All Accounts" (which will show empty)
            self.widgets.account_dropdown.set_selected(0);
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
            selection.unified_inbox = false;
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
            self.widgets
                .account_dropdown
                .set_selected((index + 1) as u32); // +1 for "All Accounts"
        }
        self.programmatic.set(false);
        self.update_subtitle();
        self.refresh_folders();
    }

    fn enter_unified_inbox(&self) {
        {
            let mut selection = self.selection.borrow_mut();
            selection.folder = None;
            selection.message = None;
            selection.search = false;
            selection.query.clear();
            selection.unified_inbox = true;
            selection.account = None;
            if selection.limit == 0 {
                selection.limit = self.page_size();
            }
        }
        self.programmatic.set(true);
        self.widgets.account_dropdown.set_selected(0); // "All Accounts"
        self.programmatic.set(false);
        self.update_subtitle();
        self.refresh_folders();
    }

    fn update_subtitle(&self) {
        let selection = self.selection.borrow();
        if selection.unified_inbox {
            let total_accounts = self.accounts.borrow().len();
            self.widgets
                .title
                .set_subtitle(&format!("Unified Inbox ({} accounts)", total_accounts));
        } else {
            let account = selection.account;
            let subtitle = account
                .and_then(|id| self.accounts.borrow().iter().find(|a| a.id == id).cloned())
                .map(|account| account.email)
                .unwrap_or_else(|| "No account".to_owned());
            self.widgets.title.set_subtitle(&subtitle);
        }
    }

    // -------------------------------------------------------------- folders

    fn refresh_folders(&self) {
        let selection = self.selection.borrow();
        if selection.unified_inbox {
            // In unified inbox mode, show just "Unified Inbox" as the only folder
            self.clear_folders();
            let unread_total = self.app.mailin.unread_total_all().unwrap_or(0);
            let unified_folder = Folder {
                id: FolderId::UNIFIED_INBOX, // UI virtual: Unified Inbox
                account_id: AccountId::default(),
                name: "Unified Inbox".to_owned(),
                delimiter: "/".to_owned(),
                role: FolderRole::Other,
                uid_validity: None,
                uid_next: None,
                highest_modseq: None,
                last_synced_at: None,
                subscribed: true,
                message_count: 0,
                unread_count: unread_total,
            };

            self.programmatic.set(true);
            clear_list(&self.widgets.folder_list);
            self.widgets
                .folder_list
                .append(&folder_row(&unified_folder));
            if let Some(row) = self.widgets.folder_list.row_at_index(0) {
                self.widgets.folder_list.select_row(Some(&row));
            } else {
                tracing::warn!("unified folder row missing after append");
            }
            self.widgets.folder_stack.set_visible_child_name("list");
            *self.folders.borrow_mut() = vec![unified_folder];
            self.programmatic.set(false);
            return;
        }

        let Some(account_id) = selection.account else {
            self.clear_folders();
            return;
        };
        drop(selection);

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
                    let _ = self.select_folder(&folder);
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

    fn select_folder(&self, folder: &Folder) -> mailin_core::Result<()> {
        let is_outbox = folder.id.is_outbox();
        {
            let mut selection = self.selection.borrow_mut();
            if selection.folder == Some(folder.id)
                && !selection.search
                && selection.show_outbox == is_outbox
            {
                return Ok(());
            }
            selection.folder = Some(folder.id);
            selection.message = None;
            selection.search = false;
            selection.query.clear();
            selection.show_outbox = is_outbox;
            selection.limit = self.page_size();
        }
        self.reset_mobile_nav();
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
                self.app.mailin.store().refresh_folder(folder.id)?;
            }
        }
        Ok(())
    }

    fn select_folder_by_id(&self, folder_id: FolderId) {
        if let Ok(Some(folder)) = self.app.mailin.folder(folder_id) {
            let _ = self.select_folder(&folder);
        }
    }

    // ------------------------------------------------------------- messages

    /// Apply a freshly saved configuration to the widgets that cache values
    /// from it: the reader (HTML preference) and the message list (density,
    /// page size).
    fn apply_config(&self) {
        let html_enabled = self.app.config.borrow().ui.html_enabled;
        self.html_mode.set(html_enabled);
        self.refresh_messages();
        let message_id = self.selection.borrow().message;
        if let Some(message_id) = message_id {
            self.render_body(message_id);
        }
    }

    fn page_size(&self) -> u32 {
        self.app.config.borrow().ui.page_size.clamp(1, MAX_PAGE)
    }

    fn refresh_messages(&self) {
        let (search, query, folder_id, limit, show_outbox, account_id, unified_inbox) = {
            let selection = self.selection.borrow();
            (
                selection.search,
                selection.query.clone(),
                selection.folder,
                selection.limit,
                selection.show_outbox,
                selection.account,
                selection.unified_inbox,
            )
        };

        if unified_inbox {
            let account_ids: Vec<AccountId> = self.accounts.borrow().iter().map(|a| a.id).collect();
            if search {
                // Search across every account (unified inbox has no
                // single folder to scope to, so use the all-mail scope).
                let mut results = Vec::new();
                let mut failure: Option<String> = None;
                for account_id in &account_ids {
                    match self
                        .app
                        .mailin
                        .search_messages(account_id, None, &query, SEARCH_LIMIT)
                    {
                        Ok(mut found) => results.append(&mut found),
                        Err(error) => failure = Some(error.to_string()),
                    }
                }
                results.sort_by_key(|a| std::cmp::Reverse(a.received_at));
                results.truncate(SEARCH_LIMIT as usize);
                self.show_messages(results, format!("Results for “{query}” (All mail)"), false);
                if let Some(error) = failure {
                    self.show_error(&format!("Some accounts could not be searched: {error}"));
                }
                return;
            }
            let limit = limit.max(self.page_size());
            match self.app.mailin.list_messages_virtual(
                mailin_core::model::VirtualFolderKind::UnifiedInbox,
                &account_ids,
                limit,
                0,
            ) {
                Ok(messages) => {
                    let title =
                        format!("Unified Inbox ({} accounts)", self.accounts.borrow().len());
                    let more = messages.len() as u32 >= limit && limit < MAX_PAGE;
                    let message_headers: Vec<MessageHeader> =
                        messages.into_iter().map(|(h, _, _)| h).collect();
                    self.show_messages(message_headers, title, more);
                }
                Err(error) => self.show_error(&error.to_string()),
            }
            return;
        }

        if search {
            let Some(account_id) = account_id else {
                return;
            };
            let search_all_mail = self.selection.borrow().search_all_mail;
            let folder_id = if search_all_mail {
                None
            } else {
                self.selection.borrow().folder
            };
            match self
                .app
                .mailin
                .search_messages(&account_id, folder_id, &query, SEARCH_LIMIT)
            {
                Ok(results) => {
                    let scope_label = if search_all_mail {
                        "All mail"
                    } else {
                        "Current folder"
                    };
                    self.show_messages(
                        results,
                        format!("Results for “{query}” ({scope_label})"),
                        false,
                    );
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
        let thread_mode = self.app.config.borrow().ui.thread_mode;
        if thread_mode != mailin_core::config::ThreadMode::Off {
            let title = self
                .app
                .mailin
                .folder(folder_id)
                .ok()
                .flatten()
                .map(|folder| folder_title(&folder))
                .unwrap_or_else(|| "Messages".to_owned());
            let more = false; // Threaded view loads roots, more is handled differently
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
        let density = self.app.config.borrow().ui.message_list_density;
        let quick_actions_enabled = self.app.config.borrow().ui.quick_actions;
        let quick_action_items = self.app.config.borrow().ui.quick_action_items.clone();
        let app = Rc::clone(&self.app);
        for message in &messages {
            let row = message_row(message, now, density);
            if quick_actions_enabled {
                attach_quick_actions(
                    &row,
                    Rc::clone(&app),
                    quick_action_items.clone(),
                    message.id,
                );
            }
            // Drag source carrying the message id so it can be dropped onto a
            // folder row to move it.
            let drag_source = gtk::DragSource::new();
            drag_source.set_actions(gdk::DragAction::MOVE);
            let payload = message.id.to_string();
            drag_source.connect_prepare(move |_, _, _| {
                Some(gdk::ContentProvider::for_value(&glib::Value::from(
                    payload.clone(),
                )))
            });
            row.add_controller(drag_source);
            self.widgets.message_list.append(&row);
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

    /// Show messages with threaded/conversation view.
    fn show_threaded_messages(&self, folder_id: FolderId, title: String, more: bool) {
        let thread_mode = self.app.config.borrow().ui.thread_mode;
        if thread_mode == mailin_core::config::ThreadMode::Off {
            // Fall back to flat view
            let limit = self.selection.borrow().limit.max(self.page_size());
            if let Ok(messages) = self.app.mailin.list_messages(folder_id, 0, limit) {
                self.show_messages(messages, title, more);
            }
            return;
        }

        let selected = self.selection.borrow().message;
        let previous = self.selected_message_ids();
        let now = format::now_unix();

        // Fetch thread roots
        let limit = self.selection.borrow().limit.max(self.page_size());
        let roots = match self.app.mailin.store().get_thread_roots(folder_id, limit) {
            Ok(r) => r,
            Err(e) => {
                self.show_error(&e.to_string());
                return;
            }
        };

        // Build flat list: roots + expanded replies
        let mut flat_messages = Vec::new();
        let expanded = self.expanded_threads.borrow();

        for root in &roots {
            flat_messages.push(root.clone());
            let root_key = root.thread_root_key.as_ref().or(root.message_id.as_ref());
            if let Some(key) = root_key {
                if expanded.contains(key) {
                    // Fetch replies for this thread
                    if let Ok(replies) = self.app.mailin.store().list_thread_replies(key) {
                        for mut reply in replies {
                            reply.thread_depth = 1; // All replies at depth 1 for flat view
                            flat_messages.push(reply);
                        }
                    }
                }
            }
        }

        let empty = flat_messages.is_empty();

        self.programmatic.set(true);
        clear_list(&self.widgets.message_list);
        let density = self.app.config.borrow().ui.message_list_density;
        let quick_actions_enabled = self.app.config.borrow().ui.quick_actions;
        let quick_action_items = self.app.config.borrow().ui.quick_action_items.clone();
        let app = Rc::clone(&self.app);

        for message in &flat_messages {
            let row = message_row(message, now, density);
            if quick_actions_enabled {
                attach_quick_actions(
                    &row,
                    Rc::clone(&app),
                    quick_action_items.clone(),
                    message.id,
                );
            }
            let drag_source = gtk::DragSource::new();
            drag_source.set_actions(gdk::DragAction::MOVE);
            let payload = message.id.to_string();
            drag_source.connect_prepare(move |_, _, _| {
                Some(gdk::ContentProvider::for_value(&glib::Value::from(
                    payload.clone(),
                )))
            });
            row.add_controller(drag_source);
            self.widgets.message_list.append(&row);
        }

        // Restore selection
        for (index, message) in flat_messages.iter().enumerate() {
            if Some(message.id) != selected && !previous.contains(&message.id) {
                continue;
            }
            if let Some(row) = self.widgets.message_list.row_at_index(index as i32) {
                self.widgets.message_list.select_row(Some(&row));
            }
        }
        self.programmatic.set(false);

        *self.messages.borrow_mut() = flat_messages;
        self.widgets.list_title.set_text(&title);
        if empty {
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
        let search_all_mail = self.widgets.scope_button.is_active();
        {
            let mut selection = self.selection.borrow_mut();
            selection.search = true;
            selection.query = query.clone();
            selection.search_all_mail = search_all_mail;
        }
        // Add to search history
        if let Some(account) = self.selection.borrow().account {
            let _ = self.app.mailin.add_search_history(
                &account,
                query.clone(),
                self.selection.borrow().folder,
            );
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
            if let Some(window) = self.open_compose(draft) {
                *self.last_draft.borrow_mut() = Some((message.id, window));
            }
            return;
        }

        // The row that is already open must not re-run the open side
        // effects: both row activation and selection changes call this.
        if self.selection.borrow().message == Some(message.id) {
            return;
        }

        let is_mobile = self.selection.borrow().is_mobile;

        self.selection.borrow_mut().message = Some(message.id);
        if self.remote_allowed.get() != Some(message.id) {
            self.remote_allowed.set(None);
        }

        // Mobile: push to navigation stack, show the reader full-screen
        if is_mobile {
            self.selection
                .borrow_mut()
                .nav_stack
                .push(NavTarget::MessageReader);
            self.widgets.list_pane.set_visible(false);
            let back_button = &self.widgets.back_button;
            back_button.set_visible(true);
        }

        self.widgets.reader_stack.set_visible_child_name("message");
        self.render_headers(message);
        self.render_attachments(message.id);
        self.render_body(message.id);
        self.update_actions(message);
        if self.app.config.borrow().ui.mark_read_on_open && !message.is_seen() {
            self.app
                .engine
                .set_flags(message.id, vec!["\\Seen".to_owned()], Vec::new());
        }
    }

    fn render_headers(&self, message: &MessageHeader) {
        self.widgets
            .reader_subject
            .set_text(&format::subject_label(message));
        let from_name = message.from_name.as_deref().unwrap_or("").trim();
        let from_addr = message.from_addr.as_deref().unwrap_or("").trim();
        let avatar_text = if from_name.is_empty() {
            from_addr
        } else {
            from_name
        };
        self.widgets
            .reader_avatar
            .set_text(Some(if avatar_text.is_empty() {
                "?"
            } else {
                avatar_text
            }));
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

    /// Header for `message_id`, preferring the rows already in the list over a
    /// fresh store lookup.
    fn header_for(&self, message_id: MessageId) -> Option<MessageHeader> {
        self.messages
            .borrow()
            .iter()
            .find(|message| message.id == message_id)
            .cloned()
            .or_else(|| self.app.mailin.message(message_id).ok().flatten())
    }

    /// Whether the account behind the current selection trusts `domain` for
    /// remote content. Read from the store so the check stays correct after
    /// the account dialog or the banner's trust button saved a change.
    fn sender_domain_allowed(&self, domain: &str) -> bool {
        let account = self.selection.borrow().account;
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

    /// Persist "always allow" for the sender's domain on the current account
    /// and re-render the message with remote content enabled.
    fn trust_current_sender(&self) {
        let (message_id, account_id) = {
            let selection = self.selection.borrow();
            (selection.message, selection.account)
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
            self.show_error(&error.to_string());
            return;
        }
        self.remote_allowed.set(Some(message_id));
        self.widgets.remote_panel.set_reveal_child(false);
        self.widgets.toast.add_toast(adw::Toast::new(&format!(
            "Always showing content from @{domain}"
        )));
        self.render_body(message_id);
    }

    fn render_body(&self, message_id: MessageId) {
        clear_box(&self.widgets.reader_body);
        self.widgets.remote_panel.set_reveal_child(false);
        if let Ok(Some(pgp)) = self.app.mailin.pgp_mime(message_id) {
            self.render_pgp_body(message_id, &pgp);
            return;
        }
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

                if !has_html || !want_html {
                    self.widgets.reader_find_revealer.set_reveal_child(false);
                    self.widgets.reader_find_entry.set_text("");
                    self.widgets.reader_find_status.set_text("");
                    *self.find_target_view.borrow_mut() = None;
                    if let Some(view) = self.html_view.borrow().as_ref() {
                        if let Some(fc) = view.find_controller() {
                            fc.search_finish();
                        }
                    }
                }

                if want_html {
                    let html_body = html.as_deref().unwrap_or_default();
                    let sender_domain = self
                        .header_for(message_id)
                        .and_then(|header| header.from_domain().map(str::to_owned));
                    let trusted = sender_domain
                        .as_deref()
                        .is_some_and(|domain| self.sender_domain_allowed(domain));
                    let allow_remote = self.app.config.borrow().ui.remote_content_enabled
                        || self.remote_allowed.get() == Some(message_id)
                        || trusted;
                    let images = self.inline_images(message_id);
                    let view = self.html_view();
                    let rendered = html::document(html_body, allow_remote, &images);
                    view.load_html(&rendered, None);
                    *self.find_target_view.borrow_mut() = Some(view.clone());
                    self.widgets.reader_body.append(&view);
                    // If cid: image sources remain unresolved after rewriting,
                    // show a small notice (M4-D). The check parses the markup
                    // instead of grepping for `cid:` so quoted text and
                    // `data-src` attributes do not raise a false alarm.
                    let unresolved_cids = mailin_core::cid::references(&rendered);
                    if !unresolved_cids.is_empty() {
                        let count = unresolved_cids.len();
                        self.widgets.reader_body.append(&notice_body(&format!(
                            "{} couldn't be loaded.",
                            mailin_core::plural(count, "inline image", "inline images")
                        )));
                    }
                    if !allow_remote && html::has_remote_content(html_body) {
                        match sender_domain {
                            Some(domain) => {
                                self.widgets
                                    .remote_trust
                                    .set_label(&format!("Always allow @{domain}"));
                                self.widgets.remote_trust.set_visible(true);
                            }
                            None => self.widgets.remote_trust.set_visible(false),
                        }
                        self.widgets.remote_panel.set_reveal_child(true);
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

    /// Render the placeholder for a PGP-protected message and decrypt/verify on demand.
    fn render_pgp_body(&self, message_id: MessageId, pgp: &mailin_core::PgpMime) {
        *self.find_target_view.borrow_mut() = None;
        self.widgets.reader_find_revealer.set_reveal_child(false);
        self.widgets.reader_find_entry.set_text("");
        self.widgets.reader_find_status.set_text("");
        let encrypted = pgp.kind == mailin_core::PgpMimeKind::Encrypted;
        let title = if encrypted {
            "This message is encrypted with OpenPGP."
        } else {
            "This message is signed with OpenPGP."
        };
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let action_label = gtk::Label::new(Some(title));
        action_label.set_xalign(0.0);
        action_label.set_hexpand(true);
        action_label.set_wrap(true);
        let action = gtk::Button::with_label(if encrypted { "Decrypt" } else { "Verify" });
        action.add_css_class("suggested-action");
        actions.append(&action_label);
        actions.append(&action);
        actions.set_margin_top(6);
        actions.set_margin_bottom(6);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_visible(false);
        status.set_accessible_role(gtk::AccessibleRole::Status);

        let holder = gtk::Box::new(gtk::Orientation::Vertical, 8);
        self.widgets.reader_body.append(&actions);
        self.widgets.reader_body.append(&status);
        self.widgets.reader_body.append(&holder);

        let mailin = std::sync::Arc::clone(&self.app.mailin);
        let config = Rc::clone(&self.app.config);
        let actions_for_click = actions.clone();
        let status_for_click = status.clone();
        let holder_for_click = holder.clone();
        let find_target_for_click = Rc::clone(&self.find_target_view);
        let find_status_for_click = self.widgets.reader_find_status.clone();
        action.connect_clicked(move |button| {
            button.set_sensitive(false);
            button.set_label("Decrypting…");
            status_for_click.set_text("");
            status_for_click.set_visible(false);
            let mailin = std::sync::Arc::clone(&mailin);
            let config = Rc::clone(&config);
            let actions_for_click = actions_for_click.clone();
            let status_for_click = status_for_click.clone();
            let holder_for_click = holder_for_click.clone();
            let find_target_for_click = Rc::clone(&find_target_for_click);
            let find_status_for_click = find_status_for_click.clone();
            let button = button.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result = mailin.decrypted_body(message_id);
                let _ = tx.send(result);
            });
            glib::timeout_add_local(Duration::from_millis(50), move || {
                match rx.try_recv() {
                    Ok(Ok((text, html_body, signature))) => {
                        actions_for_click.set_visible(false);
                        status_for_click.set_text(&format!(
                            "OpenPGP signature: {}",
                            signature_label(signature)
                        ));
                        status_for_click.set_visible(true);
                        clear_box(&holder_for_click);
                        *find_target_for_click.borrow_mut() = None;

                        let has_html =
                            html_body.as_deref().is_some_and(|html| !html.trim().is_empty());
                        let has_text =
                            text.as_deref().is_some_and(|text| !text.trim().is_empty());
                        let want_html = has_html && config.borrow().ui.html_enabled;
                        if want_html {
                            let view = html::new_view();
                            view.load_html(
                                &html::document(
                                    html_body.as_deref().unwrap_or_default(),
                                    false,
                                    &[],
                                ),
                                None,
                            );
                            if let Some(fc) = view.find_controller() {
                                let st = find_status_for_click.clone();
                                fc.connect_found_text(move |_, match_count| {
                                    if match_count == 0 {
                                        st.set_text("No matches");
                                    } else {
                                        st.set_text(&format!("{match_count} matches"));
                                    }
                                });
                                let st = find_status_for_click.clone();
                                fc.connect_counted_matches(move |_, match_count| {
                                    if match_count == 0 {
                                        st.set_text("No matches");
                                    } else {
                                        st.set_text(&format!("{match_count} matches"));
                                    }
                                });
                            }
                            *find_target_for_click.borrow_mut() = Some(view.clone());
                            holder_for_click.append(&view);
                        } else if has_text {
                            holder_for_click
                                .append(&text_body(text.as_deref().unwrap_or_default()));
                        } else if has_html {
                            holder_for_click.append(&notice_body(
                                "This message only provides an HTML body. Enable HTML in Preferences to show it.",
                            ));
                        } else {
                            holder_for_click.append(&notice_body(
                                "The decrypted message has no readable body.",
                            ));
                        }
                        glib::ControlFlow::Break
                    }
                    Ok(Err(error)) => {
                        button.set_sensitive(true);
                        button.set_label("Decrypt");
                        let msg = error.to_string();
                        let friendly = if msg.to_ascii_lowercase().contains("secret key")
                            || msg.to_ascii_lowercase().contains("no key")
                            || msg.to_ascii_lowercase().contains("private key")
                        {
                            "No matching secret key is available for this message. Import your key in Account Settings → Encryption.".to_owned()
                        } else {
                            format!("Couldn't decrypt this message: {msg}")
                        };
                        status_for_click.set_text(&friendly);
                        status_for_click.add_css_class("error");
                        status_for_click.set_visible(true);
                        glib::ControlFlow::Break
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        button.set_sensitive(true);
                        button.set_label("Decrypt");
                        status_for_click.set_text("Couldn't decrypt this message.");
                        status_for_click.set_visible(true);
                        glib::ControlFlow::Break
                    }
                }
            });
        });
    }

    fn html_view(&self) -> webkit6::WebView {
        let mut slot = self.html_view.borrow_mut();
        if let Some(view) = slot.as_ref() {
            return view.clone();
        }
        let view = html::new_view();
        if let Some(fc) = view.find_controller() {
            let status = self.widgets.reader_find_status.clone();
            fc.connect_found_text(move |_, match_count| {
                if match_count == 0 {
                    status.set_text("No matches");
                } else {
                    status.set_text(&format!("{match_count} matches"));
                }
            });
            let status = self.widgets.reader_find_status.clone();
            fc.connect_counted_matches(move |_, match_count| {
                if match_count == 0 {
                    status.set_text("No matches");
                } else {
                    status.set_text(&format!("{match_count} matches"));
                }
            });
        }
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
        let visible: Vec<_> = attachments
            .into_iter()
            .filter(|attachment| !attachment.is_inline)
            .collect();
        if visible.is_empty() {
            self.widgets.reader_attachments.set_visible(false);
            return;
        }

        // "Download all" button when multiple attachments
        if visible.len() > 1 {
            let download_all_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            download_all_row.add_css_class("card");
            download_all_row.set_margin_top(2);
            download_all_row.set_margin_bottom(2);
            let download_all = gtk::Button::with_label("Download all");
            download_all.add_css_class("flat");
            download_all.set_margin_start(10);
            let app = Rc::clone(&self.app);
            let window = self.widgets.window.clone().upcast::<gtk::Window>();
            let toast = self.widgets.toast.clone();
            let pending = Rc::clone(&self.pending_attachment);
            let visible_clone = visible.clone();
            download_all.connect_clicked(move |_| {
                let dialog = gtk::FileDialog::builder().title("Save attachments").build();
                let app = Rc::clone(&app);
                let window = window.clone();
                let toast = toast.clone();
                let pending = Rc::clone(&pending);
                let visible_clone = visible_clone.clone();
                let parent = window.clone();
                dialog.select_folder(Some(&parent), None::<&gio::Cancellable>, move |result| {
                    let Ok(folder) = result else {
                        return;
                    };
                    let Some(dir) = folder.path() else {
                        toast.add_toast(adw::Toast::new("Couldn't access the selected folder."));
                        return;
                    };
                    let total = visible_clone.len();
                    let mut saved = 0usize;
                    let mut queued = 0usize;
                    let mut failed = 0usize;
                    for (i, att) in visible_clone.iter().enumerate() {
                        // Update progress for cached saves.
                        if i > 0 && (saved + failed) > 0 && (saved + failed).is_multiple_of(3) {
                            toast.add_toast(adw::Toast::new(&format!(
                                "Saving {} of {}…",
                                i + 1,
                                total
                            )));
                        }
                        match app.mailin.attachment(message_id, &att.part_id) {
                            Ok(Some(data)) if data.data.is_some() => {
                                match save_attachment_to_dir(
                                    &dir,
                                    att,
                                    data.data.as_deref().unwrap_or_default(),
                                ) {
                                    Ok(_) => saved += 1,
                                    Err(e) => {
                                        failed += 1;
                                        toast.add_toast(adw::Toast::new(&format!(
                                            "Could not save {}: {e}",
                                            att.display_name()
                                        )));
                                    }
                                }
                            }
                            Ok(_) => {
                                queued += 1;
                                request_save_attachment(
                                    Rc::clone(&app),
                                    window.clone(),
                                    toast.clone(),
                                    Rc::clone(&pending),
                                    message_id,
                                    att.clone(),
                                    false,
                                    Some(dir.clone()),
                                );
                            }
                            Err(e) => {
                                failed += 1;
                                toast.add_toast(adw::Toast::new(&format!(
                                    "Could not read {}: {e}",
                                    att.display_name()
                                )));
                            }
                        }
                    }
                    let msg = if queued > 0 {
                        format!("Saved {saved} of {total}; downloading {queued} more…")
                    } else if failed == 0 {
                        mailin_core::plural(saved, "attachment", "attachments")
                    } else {
                        format!("Saved {saved}, couldn't save {failed}")
                    };
                    toast.add_toast(adw::Toast::new(&msg));
                });
            });
            download_all_row.append(&download_all);
            self.widgets.reader_attachments.append(&download_all_row);
        }

        for attachment in visible {
            self.widgets
                .reader_attachments
                .append(&self.attachment_row(message_id, attachment));
        }
        self.widgets.reader_attachments.set_visible(true);
    }

    fn attachment_row(&self, message_id: MessageId, attachment: AttachmentMeta) -> gtk::Widget {
        let is_image = attachment.mime_type.starts_with("image/");
        let is_pdf = attachment.mime_type == "application/pdf";
        let is_large = attachment.size.map(|s| s > 40_000_000).unwrap_or(false);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("card");
        row.set_margin_top(2);
        row.set_margin_bottom(2);

        // Preview for images and PDFs
        if is_image || is_pdf {
            if let Ok(preview) = self.attachment_preview(message_id, attachment.clone()) {
                row.append(&preview);
            }
        }

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
            let size_label = gtk::Label::new(Some(&format::file_size(size)));
            size_label.add_css_class("caption");
            size_label.add_css_class("dim-label");
            if is_large {
                size_label.add_css_class("warning");
            }
            row.append(&size_label);
        }

        if is_large {
            let warning = gtk::Label::new(Some("⚠ Large file"));
            warning.add_css_class("warning");
            warning.set_margin_end(8);
            // Convey the warning text to screen readers (the ⚠ glyph alone is
            // decorative).
            {
                warning.update_property(&[Property::Label("Large file")]);
            }
            row.append(&warning);
        }

        let save = gtk::Button::with_label("Save");
        save.add_css_class("flat");
        save.set_margin_end(4);
        // Distinguish this Save button from others by naming the file.
        {
            let save_name = format!("Save {}", attachment.display_name());
            save.update_property(&[Property::Label(&save_name)]);
        }
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
                None,
            );
        });
        row.append(&save);
        row.upcast()
    }

    fn attachment_preview(
        &self,
        message_id: MessageId,
        attachment: AttachmentMeta,
    ) -> Result<gtk::Widget, glib::Error> {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 2);
        container.set_size_request(60, 60);

        let is_image = attachment.mime_type.starts_with("image/");
        let is_pdf = attachment.mime_type == "application/pdf";

        if is_image {
            // Image preview - show thumbnail
            if let Ok(Some(data)) = self.app.mailin.attachment(message_id, &attachment.part_id) {
                if let Some(bytes) = data.data {
                    if let Ok(pixbuf) = Self::bytes_to_pixbuf(&bytes) {
                        let texture = gtk::gdk::Texture::for_pixbuf(&pixbuf);
                        let image = gtk::Image::from_paintable(Some(&texture));
                        image.set_halign(gtk::Align::Center);
                        image.set_valign(gtk::Align::Center);
                        container.append(&image);
                    }
                }
            }
        } else if is_pdf {
            // PDF preview - show icon
            let icon = gtk::Image::from_icon_name("application-pdf-symbolic");
            icon.set_pixel_size(32);
            icon.set_halign(gtk::Align::Center);
            container.append(&icon);
            let label = gtk::Label::new(Some("PDF"));
            label.add_css_class("caption");
            label.set_halign(gtk::Align::Center);
            container.append(&label);
        }

        Ok(container.upcast())
    }

    fn bytes_to_pixbuf(bytes: &[u8]) -> Result<gdk_pixbuf::Pixbuf, glib::Error> {
        let loader = gdk_pixbuf::PixbufLoader::new();
        loader.write(bytes)?;
        loader.close()?;
        loader.pixbuf().ok_or_else(|| {
            glib::Error::new(
                gdk_pixbuf::PixbufError::InsufficientMemory,
                "Failed to load pixbuf",
            )
        })
    }

    /// Finish saves that were waiting for attachment bytes to be downloaded.
    /// Several attachments of the same message may be queued at once.
    fn resolve_pending_attachment(&self, message_id: MessageId) {
        let ready: Vec<PendingAttachment> = {
            let mut pending = self.pending_attachment.borrow_mut();
            let (for_message, rest): (Vec<_>, Vec<_>) = pending
                .drain(..)
                .partition(|pending| pending.message_id == message_id);
            *pending = rest;
            for_message
        };
        let mut saved = 0usize;
        let mut failed = 0usize;
        for pending in &ready {
            if let Some(dir) = &pending.target_dir {
                match self
                    .app
                    .mailin
                    .attachment(message_id, &pending.attachment.part_id)
                {
                    Ok(Some(data)) if data.data.is_some() => {
                        match save_attachment_to_dir(
                            dir,
                            &pending.attachment,
                            data.data.as_deref().unwrap_or_default(),
                        ) {
                            Ok(_) => saved += 1,
                            Err(e) => {
                                failed += 1;
                                self.widgets.toast.add_toast(adw::Toast::new(&format!(
                                    "Could not save {}: {e}",
                                    pending.attachment.display_name()
                                )));
                            }
                        }
                    }
                    _ => {
                        failed += 1;
                        request_save_attachment(
                            Rc::clone(&self.app),
                            self.widgets.window.clone().upcast::<gtk::Window>(),
                            self.widgets.toast.clone(),
                            Rc::clone(&self.pending_attachment),
                            message_id,
                            pending.attachment.clone(),
                            true,
                            pending.target_dir.clone(),
                        );
                    }
                }
            } else {
                request_save_attachment(
                    Rc::clone(&self.app),
                    self.widgets.window.clone().upcast::<gtk::Window>(),
                    self.widgets.toast.clone(),
                    Rc::clone(&self.pending_attachment),
                    message_id,
                    pending.attachment.clone(),
                    true,
                    None,
                );
            }
        }
        if saved > 0 || failed > 0 {
            let msg = if failed == 0 {
                mailin_core::plural(saved, "attachment", "attachments")
            } else {
                format!("Saved {saved}, couldn't save {failed}")
            };
            self.widgets.toast.add_toast(adw::Toast::new(&msg));
        }
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
        {
            let name = if flagged {
                "Remove star"
            } else {
                "Star message"
            };
            self.widgets
                .flag_button
                .update_property(&[Property::Label(name)]);
        }
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
        // If the reader was dismissed while the mobile list was hidden,
        // bring the list back.
        self.reset_mobile_nav();
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

    /// Return the mobile UI to the message list (folder switch, width
    /// growth, or explicit back navigation). Harmless on desktop widths.
    fn reset_mobile_nav(&self) {
        self.selection.borrow_mut().nav_stack.clear();
        self.widgets.list_pane.set_visible(true);
        self.widgets.back_button.set_visible(false);
    }

    /// Ask for confirmation before a destructive action, then run it.
    /// `run` is only invoked when the user picks the destructive button;
    /// Cancel (or closing the dialog) is always the safe default.
    fn confirm_destructive(
        &self,
        title: &str,
        detail: &str,
        ok_label: &str,
        run: impl FnOnce() + 'static,
    ) {
        let confirm = gtk::AlertDialog::builder()
            .message(title)
            .detail(detail)
            .build();
        confirm.set_buttons(&["Cancel", ok_label]);
        confirm.set_cancel_button(0);
        confirm.set_default_button(0);
        let window = self.widgets.window.clone();
        let prior_focus = gtk::prelude::RootExt::focus(&window);
        glib::spawn_future_local(async move {
            let confirmed = matches!(confirm.choose_future(Some(&window)).await, Ok(1));
            if confirmed {
                run();
            }
            // M3a-5: restore focus to the invoking control when possible.
            if let Some(focus) = prior_focus {
                let _ = focus.grab_focus();
            }
        });
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
        let pairs = self.undo_pairs(&ids);
        let count = ids.len();
        self.app.engine.archive_messages(&ids);
        self.toast_undo_move(
            &format!(
                "Archived {}",
                mailin_core::plural(count, "message", "messages")
            ),
            &pairs,
        );
    }

    /// Handle "Mark as read" action from notification.
    /// Parameter format: "account_id:message_id1,message_id2,..." (message IDs optional)
    pub fn notification_mark_read(&self, param: &str) {
        let (account_id_str, ids_str) = param.split_once(':').unwrap_or((param, ""));
        let Ok(account_id) = account_id_str.parse::<AccountId>() else {
            return;
        };

        // If message IDs provided, use them; otherwise fetch unread messages from inbox
        let ids: Vec<MessageId> = if !ids_str.is_empty() {
            ids_str
                .split(',')
                .filter_map(|s| s.parse::<i64>().ok().map(MessageId))
                .collect()
        } else {
            self.get_unread_inbox_messages(account_id)
        };

        if ids.is_empty() {
            return;
        }
        // Mark as read by adding \Seen flag
        self.app
            .engine
            .bulk_set_flags(&ids, vec!["\\Seen".to_owned()], Vec::new());
        self.toast("Marked as read");
    }

    /// Handle "Archive" action from notification.
    /// Parameter format: "account_id:message_id1,message_id2,..." (message IDs optional)
    pub fn notification_archive(&self, param: &str) {
        let (account_id_str, ids_str) = param.split_once(':').unwrap_or((param, ""));
        let Ok(account_id) = account_id_str.parse::<AccountId>() else {
            return;
        };

        // If message IDs provided, use them; otherwise fetch unread messages from inbox
        let ids: Vec<MessageId> = if !ids_str.is_empty() {
            ids_str
                .split(',')
                .filter_map(|s| s.parse::<i64>().ok().map(MessageId))
                .collect()
        } else {
            self.get_unread_inbox_messages(account_id)
        };

        if ids.is_empty() {
            return;
        }
        self.app.engine.archive_messages(&ids);
        self.toast("Archiving…");
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

    fn delete_selected(&self, permanent: bool) {
        // Bare Delete is scoped away from text entry: avoid trashing mail
        // while the user is typing in search (M3a-3). Button/menu activation
        // remains intentional even if focus moved.
        if !permanent && self.widgets.search.has_focus() {
            return;
        }
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let in_trash = self
            .selection
            .borrow()
            .folder
            .and_then(|folder_id| self.app.mailin.folder(folder_id).ok().flatten())
            .is_some_and(|folder| folder.role == FolderRole::Trash);
        // Deleting from Trash and an explicit permanent delete are both
        // irreversible — ask first, showing how much will be destroyed.
        if permanent || in_trash {
            let count = ids.len();
            let noun = mailin_core::plural_noun(count, "message", "messages");
            let app = self.app.clone();
            let toast = self.widgets.toast.clone();
            self.confirm_destructive(
                &format!("Delete {count} {noun} permanently?"),
                "The selected messages will be deleted from the server and from local storage. This cannot be undone.",
                "Delete",
                move || {
                    app.engine.permanently_delete_messages(&ids);
                    toast.add_toast(adw::Toast::new(mailin_core::plural_noun(
                        count,
                        "Message deleted permanently",
                        "Messages deleted permanently",
                    )));
                },
            );
            return;
        }
        let pairs = self.undo_pairs(&ids);
        let count = ids.len();
        self.app.engine.delete_messages(&ids);
        self.toast_undo_move(
            &format!(
                "Moved {} to Trash",
                mailin_core::plural(count, "message", "messages")
            ),
            &pairs,
        );
    }

    fn mark_selected_junk(&self, junk: bool) {
        let ids = self.selected_message_ids();
        if ids.is_empty() {
            return;
        }
        let pairs = self.undo_pairs(&ids);
        let count = ids.len();
        self.app.engine.mark_messages_junk(&ids, junk);
        let text = if junk {
            format!(
                "Marked {} as Junk",
                mailin_core::plural(count, "message", "messages")
            )
        } else {
            "Moved to Inbox".to_owned()
        };
        self.toast_undo_move(&text, &pairs);
    }

    fn empty_trash(&self) {
        let Some(account_id) = self.selection.borrow().account else {
            return;
        };
        let app = self.app.clone();
        let toast = self.widgets.toast.clone();
        self.confirm_destructive(
            "Empty Trash?",
            "All messages in Trash will be permanently deleted from the server and from local storage. This cannot be undone.",
            "Empty Trash",
            move || {
                app.engine.empty_trash(account_id);
                toast.add_toast(adw::Toast::new("Trash emptied"));
            },
        );
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
                } else {
                    // Fallback: extract subject from envelope
                    draft.subject = env.to.join(", ");
                }
                // Need to mark the original entry as cancelled so it doesn't get sent again
                this.app.engine.cancel_outbox(account_id, id);
                let _ = this.open_compose(draft);
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
                let pairs = this.undo_pairs(&ids);
                let count = ids.len();
                this.app.engine.move_messages(&ids, folder_id);
                this.toast_undo_move(
                    &format!(
                        "Moved {}",
                        mailin_core::plural(count, "message", "messages")
                    ),
                    &pairs,
                );
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
        let _ = this.open_compose(draft);
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
        let _ = this.open_compose(draft);
    }

    /// Draft for a brand-new message: an empty body that already carries the
    /// selected account's signature, so the caret starts above it.
    fn new_draft(&self) -> OutgoingMessage {
        let signature = self
            .selection
            .borrow()
            .account
            .and_then(|account_id| self.app.mailin.account(&account_id).ok().flatten())
            .map(|account| mailin_core::compose::signature_block(&account.signature))
            .unwrap_or_default();
        OutgoingMessage {
            body: signature,
            ..OutgoingMessage::default()
        }
    }

    pub fn open_compose_new(&self) {
        let _ = self.open_compose(self.new_draft());
    }

    /// Open a compose window with `draft` (also used for mailto: links).
    pub fn open_compose(&self, draft: OutgoingMessage) -> Option<glib::WeakRef<gtk::Window>> {
        let Some(account_id) = self.selection.borrow().account else {
            self.widgets
                .toast
                .add_toast(adw::Toast::new("Add an account before composing mail"));
            return None;
        };
        let parent = self.widgets.window.clone().upcast::<gtk::Window>();
        let toast = self.widgets.toast.clone();
        let toast_for_close = self.widgets.toast.clone();
        compose::present(
            &parent,
            Rc::clone(&self.app),
            account_id,
            draft,
            move || {
                toast.add_toast(adw::Toast::new("Message queued in the outbox"));
            },
            move |draft_saved| {
                if draft_saved {
                    toast_for_close.add_toast(adw::Toast::new("Draft saved"));
                }
            },
        )
        .map(|window| window.upcast::<gtk::Window>().downgrade())
    }

    // --------------------------------------------------------------- events

    fn drain_events(&self, events: &mut broadcast::Receiver<ClientEvent>) {
        let mut accounts_changed = false;
        let mut folders_changed = false;
        let mut messages_changed = false;
        let mut body_ready = None;
        let mut account_error = None;
        let mut outbox_changed = false;
        let mut new_messages = Vec::new();

        loop {
            match events.try_recv() {
                Ok(ClientEvent::AccountsChanged) => accounts_changed = true,
                Ok(ClientEvent::FoldersChanged { .. }) => folders_changed = true,
                Ok(ClientEvent::MessagesChanged { folder_id }) => {
                    let selection = self.selection.borrow();
                    if selection.search || selection.folder == Some(folder_id) {
                        messages_changed = true;
                    }
                }
                Ok(ClientEvent::NewMessages {
                    account_id,
                    count,
                    message_ids,
                    ..
                }) => new_messages.push((account_id, count, message_ids)),
                Ok(ClientEvent::BodyReady { message_id }) => body_ready = Some(message_id),
                Ok(ClientEvent::OutboxChanged) => outbox_changed = true,
                Ok(ClientEvent::AccountError { message, .. }) => account_error = Some(message),
                Ok(ClientEvent::ActionFailed { message, .. }) => {
                    self.widgets.banner.set_title(&message);
                    self.widgets.banner.set_revealed(true);
                }
                Ok(ClientEvent::ScheduledActionRan { .. }) => {
                    // Scheduled action completed, refresh folders and messages
                    folders_changed = true;
                    messages_changed = true;
                }
                Ok(ClientEvent::SyncStatusChanged { account_id, status }) => {
                    // Update sync status indicator for the currently selected account
                    let selection = self.selection.borrow();
                    if selection.account == Some(account_id) {
                        let sync_status = &self.widgets.sync_status;
                        let (css_class, tooltip) = match status {
                            mailin_core::model::SyncStatus::Idle => {
                                ("sync-status-idle", "Sync status: Idle")
                            }
                            mailin_core::model::SyncStatus::Syncing => {
                                ("sync-status-syncing", "Sync status: Syncing...")
                            }
                            mailin_core::model::SyncStatus::Connected => {
                                ("sync-status-connected", "Sync status: Connected")
                            }
                            mailin_core::model::SyncStatus::Offline => {
                                ("sync-status-offline", "Sync status: Offline")
                            }
                            mailin_core::model::SyncStatus::Error => {
                                ("sync-status-error", "Sync status: Error")
                            }
                        };
                        sync_status.remove_css_class("sync-status-idle");
                        sync_status.remove_css_class("sync-status-syncing");
                        sync_status.remove_css_class("sync-status-connected");
                        sync_status.remove_css_class("sync-status-offline");
                        sync_status.remove_css_class("sync-status-error");
                        sync_status.add_css_class(css_class);
                        sync_status.set_tooltip_text(Some(tooltip));
                        {
                            sync_status.update_property(&[Property::Label(tooltip)]);
                        }
                        if matches!(status, mailin_core::model::SyncStatus::Error) {
                            self.widgets.banner.set_button_label(Some("Retry"));
                            self.widgets.banner.set_title("Sync failed");
                            self.widgets.banner.set_revealed(true);
                        }
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
        for (account_id, count, message_ids) in new_messages {
            self.notify_new_mail(account_id, count, &message_ids);
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
    /// application through the default action. Buttons act on exactly the
    /// messages that arrived in this sync — never on the whole unread inbox.
    fn notify_new_mail(&self, account_id: AccountId, count: u32, message_ids: &[MessageId]) {
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
        if !self.app.config.borrow().ui.notifications || application_active {
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

        // Parameter for notification actions: account_id plus the message IDs
        // this notification is about (comma-separated). The handlers fall back
        // to unread inbox messages only when the IDs are missing.
        let param = if message_ids.is_empty() {
            account_id.to_string()
        } else {
            format!(
                "{}:{}",
                account_id,
                message_ids
                    .iter()
                    .map(|id| id.0.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };

        let notification = gio::Notification::new(mailin_core::plural_noun(
            count,
            "New message",
            "New messages",
        ));
        notification.set_body(Some(&format!(
            "{} in {account}",
            mailin_core::plural(count, "new message", "new messages")
        )));
        notification.set_default_action("app.present");

        // Add "Mark as read" button
        notification.add_button(
            "Mark as read",
            &format!("app.notification-mark-read::{}", param),
        );
        // Add "Archive" button
        notification.add_button("Archive", &format!("app.notification-archive::{}", param));

        application.send_notification(Some(&format!("mailin-{account_id}")), &notification);
    }

    fn show_error(&self, message: &str) {
        self.widgets.banner.set_button_label(Some("Retry"));
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

    fn move_message_to_folder(&self, message_id: MessageId, target_folder: FolderId) {
        // Find the source folder for undo.
        let source_folder = self
            .messages
            .borrow()
            .iter()
            .find(|m| m.id == message_id)
            .map(|m| m.folder_id);

        self.app.engine.move_messages(&[message_id], target_folder);

        // Refresh the visible list/folders so the move is reflected.
        self.refresh_folders();
        self.refresh_messages();

        if let Some(source_folder) = source_folder {
            self.toast_undo_move("Moved 1 message", &[(message_id, source_folder)]);
        }
    }

    /// (id, source folder) pairs for the given messages. Mixed selections
    /// store a pair per message so undo restores each to its origin.
    fn undo_pairs(&self, ids: &[MessageId]) -> Vec<(MessageId, FolderId)> {
        let messages = self.messages.borrow();
        ids.iter()
            .filter_map(|id| {
                messages
                    .iter()
                    .find(|message| message.id == *id)
                    .map(|message| (message.id, message.folder_id))
            })
            .collect()
    }

    /// Show an undo toast for a folder-moving action. Per-message sources
    /// are encoded in the payload so mixed selections unwind correctly.
    fn toast_undo_move(&self, text: &str, pairs: &[(MessageId, FolderId)]) {
        if pairs.is_empty() {
            return;
        }
        let param = pairs
            .iter()
            .map(|(message, folder)| format!("{}:{}", message.0, folder.0))
            .collect::<Vec<_>>()
            .join(",");
        let toast = adw::Toast::new(text);
        toast.set_button_label(Some("Undo"));
        toast.set_timeout(5);
        let window = self.widgets.window.clone();
        toast.connect_button_clicked(move |_| {
            let _ = gtk::prelude::WidgetExt::activate_action(
                &window,
                "undo-move-multi",
                Some(&param.to_variant()),
            );
        });
        self.widgets.toast.add_toast(toast);
    }

    /// Undo a folder-moving action: move each message back to its source
    /// folder. Permanent deletes never reach this path.
    fn undo_move_multi(&self, param: &str) {
        let mut groups: std::collections::HashMap<i64, Vec<MessageId>> =
            std::collections::HashMap::new();
        for pair in param.split(',') {
            let mut parts = pair.split(':');
            let (Some(message), Some(folder)) = (parts.next(), parts.next()) else {
                continue;
            };
            if let (Ok(message), Ok(folder)) = (message.parse::<i64>(), folder.parse::<i64>()) {
                groups.entry(folder).or_default().push(MessageId(message));
            }
        }
        for (folder, ids) in groups {
            self.app.engine.move_messages(&ids, FolderId(folder));
        }
        self.refresh_folders();
        self.refresh_messages();
        self.toast("Move undone");
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
        let mailin = this.app.mailin.clone();
        let this_ok = Rc::clone(this);
        let this_err = Rc::clone(this);
        let parent = this.widgets.window.clone().upcast::<gtk::Window>();
        crate::dialogs::confirm_remove_account(
            Some(&parent),
            mailin,
            &account,
            move || this_ok.refresh_accounts(),
            move |e| this_err.show_error(&e),
        );
    }

    // Folder management actions
    fn create_folder(this: Rc<Self>) {
        let Some(account_id) = this.selection.borrow().account else {
            return;
        };
        let dialog = adw::MessageDialog::builder()
            .transient_for(&this.widgets.window)
            .modal(true)
            .heading("New folder")
            .body("Enter the name of the new folder. Use “/” to nest it, for example Archive/2025.")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("create", "Create");
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");

        let entry = adw::EntryRow::builder()
            .title("Folder name")
            .show_apply_button(true)
            .build();
        dialog.set_extra_child(Some(&entry));

        let app = this.app.clone();
        let this = Rc::clone(&this);
        let entry_for_response = entry.clone();
        let prior_focus = gtk::prelude::RootExt::focus(&this.widgets.window);
        let win = this.widgets.window.clone();
        dialog.connect_response(None, move |dialog, response| {
            dialog.close();
            if response == "create" {
                let name = entry_for_response.text().trim().to_owned();
                if !name.is_empty() {
                    app.engine
                        .create_folder(account_id, name, "/".to_owned(), FolderRole::Other);
                    this.refresh_folders();
                }
            }
            if let Some(focus) = prior_focus.clone() {
                let _ = focus.grab_focus();
            } else {
                let _ = win.grab_focus();
            }
        });
        dialog.present();
        entry.grab_focus();
    }

    fn rename_folder(this: Rc<Self>, folder_id: String) {
        let Ok(folder_id) = folder_id.parse::<i64>().map(FolderId) else {
            return;
        };
        let Some(folder) = this
            .folders
            .borrow()
            .iter()
            .find(|folder| folder.id == folder_id)
            .cloned()
        else {
            return;
        };
        if folder.id.is_virtual_ui() || folder.role != FolderRole::Other {
            this.toast("This folder cannot be renamed");
            return;
        }

        let dialog = adw::MessageDialog::builder()
            .transient_for(&this.widgets.window)
            .modal(true)
            .heading("Rename folder")
            .body(format!("Choose a new name for “{}”.", folder.name))
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("rename", "Rename");
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");

        let entry = adw::EntryRow::builder()
            .title("Folder name")
            .show_apply_button(true)
            .build();
        entry.set_text(&folder.name);
        dialog.set_extra_child(Some(&entry));

        let app = this.app.clone();
        let this = Rc::clone(&this);
        let entry_for_response = entry.clone();
        let prior_focus = gtk::prelude::RootExt::focus(&this.widgets.window);
        let win = this.widgets.window.clone();
        dialog.connect_response(None, move |dialog, response| {
            dialog.close();
            if response == "rename" {
                let name = entry_for_response.text().trim().to_owned();
                if !name.is_empty() && name != folder.name {
                    app.engine.rename_folder(folder_id, name);
                    this.refresh_folders();
                }
            }
            if let Some(focus) = prior_focus.clone() {
                let _ = focus.grab_focus();
            } else {
                let _ = win.grab_focus();
            }
        });
        dialog.present();
        entry.grab_focus();
    }

    fn delete_folder(this: Rc<Self>, folder_id: String) {
        let Ok(folder_id) = folder_id.parse::<i64>().map(FolderId) else {
            return;
        };
        if folder_id.is_virtual_ui() {
            this.toast("This folder cannot be deleted");
            return;
        }
        let Some(folder) = this
            .folders
            .borrow()
            .iter()
            .find(|folder| folder.id == folder_id)
            .cloned()
        else {
            return;
        };
        if folder.role != FolderRole::Other {
            this.toast("This folder cannot be deleted");
            return;
        }
        let confirm = gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("Delete “{}”?", folder.name))
            .detail("This removes the folder from the server. Messages inside it may be deleted as well.")
            .build();
        confirm.set_buttons(&["Cancel", "Delete"]);
        confirm.set_cancel_button(0);
        confirm.set_default_button(0);

        let app = this.app.clone();
        let this = Rc::clone(&this);
        let prior_focus = gtk::prelude::RootExt::focus(&this.widgets.window);
        glib::spawn_future_local(async move {
            let response = confirm.choose_future(Some(&this.widgets.window)).await;
            if response == Ok(1) {
                app.engine.delete_folder(folder_id);
                this.refresh_folders();
            }
            if let Some(focus) = prior_focus {
                let _ = focus.grab_focus();
            }
        });
    }

    fn subscribe_folder(this: Rc<Self>, folder_id: String, subscribe: bool) {
        let Ok(folder_id) = folder_id.parse::<i64>().map(FolderId) else {
            return;
        };
        if folder_id.is_virtual_ui() {
            return;
        }
        let is_system = this
            .folders
            .borrow()
            .iter()
            .find(|f| f.id == folder_id)
            .is_some_and(|f| f.role != FolderRole::Other);
        if is_system {
            return;
        }
        let app = this.app.clone();
        let this = Rc::clone(&this);
        glib::spawn_future_local(async move {
            app.engine.subscribe_folder(folder_id, subscribe);
            this.refresh_folders();
        });
    }

    fn folder_properties(this: Rc<Self>, folder_id: String) {
        let Ok(folder_id) = folder_id.parse::<i64>().map(FolderId) else {
            return;
        };
        if folder_id.is_virtual_ui() {
            this.toast("This folder has no server properties");
            return;
        }
        let Some(folder) = this
            .folders
            .borrow()
            .iter()
            .find(|folder| folder.id == folder_id)
            .cloned()
        else {
            return;
        };

        let dialog = adw::MessageDialog::builder()
            .transient_for(&this.widgets.window)
            .modal(true)
            .heading("Folder properties")
            .body(format!(
                "Details reported by the server for “{}”.",
                folder.name
            ))
            .build();
        dialog.add_response("close", "Close");
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        let prior_focus = gtk::prelude::RootExt::focus(&this.widgets.window);
        let win = this.widgets.window.clone();
        dialog.connect_response(None, move |d, _| {
            d.close();
            if let Some(focus) = prior_focus.clone() {
                let _ = focus.grab_focus();
            } else {
                let _ = win.grab_focus();
            }
        });

        let rows = gtk::ListBox::new();
        rows.set_selection_mode(gtk::SelectionMode::None);
        rows.add_css_class("boxed-list");
        let add_row = |title: &str, value: String| {
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(value)
                .build();
            row.set_activatable(false);
            rows.append(&row);
        };
        add_row("Name", folder.name.clone());
        add_row("Role", folder.role.as_str().to_owned());
        add_row("Messages", folder.message_count.to_string());
        add_row("Unread", folder.unread_count.to_string());
        add_row(
            "Delimiter",
            if folder.delimiter.is_empty() {
                "None".to_owned()
            } else {
                folder.delimiter.clone()
            },
        );
        add_row(
            "UID validity",
            folder
                .uid_validity
                .map_or_else(|| "Unknown".to_owned(), |value| value.to_string()),
        );
        add_row(
            "Next UID",
            folder
                .uid_next
                .map_or_else(|| "Unknown".to_owned(), |value| value.to_string()),
        );
        add_row(
            "Highest mod-sequence",
            folder
                .highest_modseq
                .map_or_else(|| "Unknown".to_owned(), |value| value.to_string()),
        );
        add_row(
            "Last synchronized",
            folder
                .last_synced_at
                .map_or_else(|| "Never".to_owned(), format::full_date),
        );

        let scroller = gtk::ScrolledWindow::builder()
            .min_content_height(260)
            .min_content_width(360)
            .child(&rows)
            .build();
        dialog.set_extra_child(Some(&scroller));
        dialog.present();
    }
}

/// Start saving an attachment, downloading the body first when only metadata
/// was cached by an older version. If `target_dir` is set, save there instead
/// of prompting (used by "Download all" and queued completions).
#[allow(clippy::too_many_arguments)]
fn request_save_attachment(
    app: Rc<AppState>,
    window: gtk::Window,
    toast: adw::ToastOverlay,
    pending: Rc<RefCell<Vec<PendingAttachment>>>,
    message_id: MessageId,
    attachment: AttachmentMeta,
    retry: bool,
    target_dir: Option<PathBuf>,
) {
    let unavailable = |toast: &adw::ToastOverlay, target_dir: &Option<PathBuf>| {
        let msg = if target_dir.is_some() {
            "Couldn't download an attachment; it may no longer be available."
        } else {
            "Could not load the attachment; it may no longer be available."
        };
        toast.add_toast(adw::Toast::new(msg));
    };
    let queue = |app: &Rc<AppState>,
                 toast: &adw::ToastOverlay,
                 attachment: &AttachmentMeta,
                 target_dir: &Option<PathBuf>| {
        let mut pending = pending.borrow_mut();
        // Avoid queueing the same attachment twice.
        if !pending
            .iter()
            .any(|p| p.message_id == message_id && p.attachment.part_id == attachment.part_id)
        {
            pending.push(PendingAttachment {
                message_id,
                attachment: attachment.clone(),
                target_dir: target_dir.clone(),
            });
        } else if let Some(dir) = target_dir {
            // If already queued (e.g. user requested Download all after a
            // single save started), prefer saving to the chosen folder.
            if let Some(p) = pending
                .iter_mut()
                .find(|p| p.message_id == message_id && p.attachment.part_id == attachment.part_id)
            {
                p.target_dir = Some(dir.clone());
            }
        }
        app.engine.refresh_body(message_id);
        if target_dir.is_none() {
            toast.add_toast(adw::Toast::new("Downloading attachment…"));
        }
    };
    match app.mailin.attachment(message_id, &attachment.part_id) {
        Ok(Some(data)) => match data.data {
            Some(bytes) => {
                if let Some(dir) = target_dir {
                    match save_attachment_to_dir(&dir, &attachment, &bytes) {
                        Ok(_) => { /* progress reported by caller for batch saves */ }
                        Err(e) => toast.add_toast(adw::Toast::new(&format!(
                            "Could not save {}: {e}",
                            attachment.display_name()
                        ))),
                    }
                } else {
                    save_attachment_dialog(&window, &toast, &attachment, bytes);
                }
            }
            None if retry => unavailable(&toast, &target_dir),
            None => queue(&app, &toast, &attachment, &target_dir),
        },
        Ok(None) if retry => unavailable(&toast, &target_dir),
        Ok(None) => queue(&app, &toast, &attachment, &target_dir),
        Err(error) => {
            toast.add_toast(adw::Toast::new(&format!(
                "Could not read the attachment: {error}"
            )));
        }
    }
}

fn unique_attachment_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let base = dir.join(name);
    if !base.exists() {
        return base;
    }
    let path = std::path::Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|s| s.to_str());
    for i in 1..10_000u32 {
        let candidate = if let Some(ext) = ext {
            dir.join(format!("{stem} ({i}).{ext}"))
        } else {
            dir.join(format!("{stem} ({i})"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    base
}

fn save_attachment_to_dir(
    dir: &std::path::Path,
    attachment: &AttachmentMeta,
    data: &[u8],
) -> Result<PathBuf, String> {
    let path = unique_attachment_path(dir, &attachment.display_name());
    std::fs::write(&path, data).map_err(|e| e.to_string())?;
    Ok(path)
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
    // M3a-8: maintain a comfortable touch/click target.
    button.set_size_request(32, 32);
    // Icon-only buttons have no text content, so screen readers have nothing
    // to announce. `Property::Label` is the accessible *name*; the tooltip
    // remains as the accessible *description* (GTK maps tooltip-text to it).
    // The name is the tooltip text without the trailing accelerator hint.
    let name = tooltip.split(" (").next().unwrap_or(tooltip);
    button.update_property(&[Property::Label(name), Property::Description(tooltip)]);
    button
}

/// Main-window action accelerators. `build_shortcuts_window` renders the
/// same source of truth so help never drifts from registered shortcuts.
const WINDOW_ACTION_ACCELS: &[(&str, &[&str])] = &[
    ("win.search", &["<Control>f"]),
    ("win.find-message", &["F3"]),
    ("win.sync", &["F5"]),
    ("win.add-account", &["<Control><Shift>a"]),
    ("win.new-folder", &["<Control><Shift>n"]),
    ("win.compose", &["<Control>n"]),
    ("win.reply", &["<Control>r"]),
    ("win.reply-all", &["<Control><Shift>r"]),
    ("win.forward", &["<Control><Shift>f"]),
    ("win.preferences", &["<Control>comma"]),
    ("win.archive", &["<Control><Shift>e"]),
    ("win.delete", &["Delete"]),
    ("win.delete-permanent", &["<Control>Delete"]),
    ("win.junk", &["<Control><Shift>j"]),
    ("win.not-junk", &["<Control><Shift>k"]),
    ("win.move", &["<Control>m"]),
];

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
        ("<Control>R", "Reply"),
        ("<Control><Shift>R", "Reply all"),
        ("<Control><Shift>F", "Forward"),
        ("<Control>F", "Search mail / find in message (HTML)"),
        ("F3", "Find next in message"),
        ("F5", "Synchronize"),
        ("<Control><Shift>A", "Add account"),
        ("<Control><Shift>N", "New folder"),
        ("<Control>comma", "Preferences"),
        ("<Control>Return", "Send message (in composer)"),
        (
            "Delete",
            "Move selected messages to Trash (when message list is active)",
        ),
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
        FolderRole::Archive => "mail-archive-symbolic",
        FolderRole::Snoozed => "alarm-symbolic",
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
    // Expose the unread count in words; the bare number badge alone reads as
    // just a digit with no context.
    if folder.unread_count > 0 {
        let name = format!(
            "{}, {}",
            folder_title(folder),
            mailin_core::plural(folder.unread_count, "unread message", "unread messages")
        );
        row.update_property(&[Property::Label(&name)]);
    }

    // M3a-4: gio::Menu + PopoverMenu. System/virtual folders must not offer
    // Rename/Delete/Subscribe. Keyboard: Menu key / Shift+F10.
    let menu = gio::Menu::new();
    menu.append(Some("New Folder"), Some("win.new-folder"));
    let is_virtual = folder.id.is_virtual_ui();
    let is_system_role = folder.role != FolderRole::Other;
    let target = folder.id.0.to_string().to_variant();
    if !is_virtual && !is_system_role {
        let item = gio::MenuItem::new(Some("Rename Folder"), None);
        item.set_action_and_target_value(Some("win.rename-folder"), Some(&target));
        menu.append_item(&item);
        let item = gio::MenuItem::new(Some("Delete Folder"), None);
        item.set_action_and_target_value(Some("win.delete-folder"), Some(&target));
        menu.append_item(&item);
    }
    if !is_virtual {
        let item = gio::MenuItem::new(Some("Properties"), None);
        item.set_action_and_target_value(Some("win.folder-properties"), Some(&target));
        menu.append_item(&item);
        if !is_system_role {
            // M3a-4: state-aware label (core `subscribed`).
            if folder.subscribed {
                let item = gio::MenuItem::new(Some("Unsubscribe"), None);
                item.set_action_and_target_value(Some("win.unsubscribe-folder"), Some(&target));
                menu.append_item(&item);
            } else {
                let item = gio::MenuItem::new(Some("Subscribe"), None);
                item.set_action_and_target_value(Some("win.subscribe-folder"), Some(&target));
                menu.append_item(&item);
            }
        }
    }
    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_has_arrow(true);
    popover.set_parent(&row);

    let gesture = gtk::GestureClick::new();
    gesture.set_button(0);
    let popover_for_gesture = popover.clone();
    gesture.connect_pressed(move |gesture, n_press, _x, _y| {
        if n_press == 1 && gesture.current_button() == gdk::BUTTON_SECONDARY {
            popover_for_gesture.set_pointing_to(None::<&gdk::Rectangle>);
            popover_for_gesture.present();
        }
    });
    row.add_controller(gesture);

    let key = gtk::EventControllerKey::new();
    let popover_for_key = popover.clone();
    key.connect_key_pressed(move |_, keyval, _, state| {
        if keyval == gtk::gdk::Key::Menu
            || (keyval == gtk::gdk::Key::F10 && state.contains(gtk::gdk::ModifierType::SHIFT_MASK))
        {
            popover_for_key.present();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    row.add_controller(key);

    // Accept messages dragged onto real folders only (not virtual/system views).
    if folder.id.0 > 0 {
        let drop_target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
        let folder_id = folder.id.0;
        drop_target.connect_drop(move |target, value, _, _| {
            let Ok(msg_id) = value.get::<String>() else {
                return false;
            };
            let Some(widget) = target.widget() else {
                return false;
            };
            let param = format!("{folder_id}:{msg_id}");
            let _ = widget.activate_action("move-messages-to-folder", Some(&param.to_variant()));
            true
        });
        row.add_controller(drop_target);
    }

    row
}

fn message_row(message: &MessageHeader, now: i64, density: MessageListDensity) -> gtk::ListBoxRow {
    use MessageListDensity::{Compact, Cozy};

    let seen = message.is_seen();
    let is_thread_root = message.thread_reply_count > 0
        || (message
            .thread_root_key
            .as_ref()
            .is_some_and(|k| k == message.message_id.as_deref().unwrap_or("")));
    let depth = message.thread_depth;

    let row = gtk::ListBoxRow::new();
    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let (margin_vertical, margin_horizontal, spacing, show_snippet) = match density {
        MessageListDensity::Comfortable => (8, 12, 3, true),
        Cozy => (5, 10, 2, true),
        Compact => (2, 8, 1, false),
    };
    outer.set_margin_top(margin_vertical);
    outer.set_margin_bottom(margin_vertical);
    outer.set_margin_start(margin_horizontal + (depth as i32 * 16)); // Indent by depth
    outer.set_margin_end(margin_horizontal);

    // Thread expand/collapse chevron for thread roots
    if is_thread_root {
        let chevron = gtk::Image::from_icon_name("pan-down-symbolic");
        chevron.set_pixel_size(12);
        chevron.add_css_class("dim-label");
        chevron.set_margin_start(4);
        outer.append(&chevron);
    } else if depth > 0 {
        // Placeholder for alignment with thread roots
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_size_request(20, 0);
        outer.append(&spacer);
    } else {
        // Regular message (not in thread view or flat mode)
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_size_request(20, 0);
        outer.append(&spacer);
    }

    let dot = gtk::Label::new(Some("●"));
    dot.add_css_class("accent");
    dot.set_valign(gtk::Align::Start);
    dot.set_margin_top(4);
    if seen {
        dot.set_opacity(0.0);
    }
    // The unread dot is a decorative indicator; its state is announced via the
    // row's accessible label instead, so hide it from the accessibility tree.
    {
        use gtk::accessible::State;
        dot.update_state(&[State::Hidden(true)]);
    }
    outer.append(&dot);

    // M4-A: sender avatar in list rows (hidden in compact density).
    if density != MessageListDensity::Compact {
        let avatar_text = message
            .from_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(message.from_addr.as_deref())
            .unwrap_or("?");
        let avatar = adw::Avatar::builder()
            .size(32)
            .text(avatar_text)
            .show_initials(true)
            .build();
        avatar.set_valign(gtk::Align::Start);
        avatar.set_margin_top(2);
        outer.append(&avatar);
    }

    let column = gtk::Box::new(gtk::Orientation::Vertical, spacing);
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

    // Reply count badge for thread roots
    if is_thread_root && message.thread_reply_count > 0 {
        let badge = gtk::Label::new(Some(&message.thread_reply_count.to_string()));
        badge.add_css_class("caption");
        badge.add_css_class("dim-label");
        line.append(&badge);
    }

    if message.is_flagged() {
        let star = gtk::Image::from_icon_name("starred-symbolic");
        star.set_pixel_size(12);
        // Decorative: flagged state is announced via the row's accessible label.
        {
            use gtk::accessible::State;
            star.update_state(&[State::Hidden(true)]);
        }
        line.append(&star);
    }
    if message.has_attachments {
        let clip = gtk::Image::from_icon_name("mail-attachment-symbolic");
        clip.set_pixel_size(12);
        clip.add_css_class("dim-label");
        {
            use gtk::accessible::State;
            clip.update_state(&[State::Hidden(true)]);
        }
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

    if show_snippet && let Some(snippet) = message.snippet.as_deref() {
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
    let summary = format!(
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
    );
    row.set_tooltip_text(Some(&summary));
    // Expose unread/flagged/attachment state to screen readers; the visual
    // indicators (dot, star, clip) are decorative and hidden.
    {
        row.update_property(&[Property::Label(&summary)]);
    }
    row
}

/// Attach swipe-to-reveal quick actions to a message row.
///
/// A `GestureSwipe` on the row slides an overlay of action buttons into view:
/// swipe left reveals the "destructive" side (archive / delete), swipe right
/// reveals the "positive" side (flag / mark read). A second swipe in the same
/// direction, or tapping elsewhere, hides the overlay again.
fn attach_quick_actions(
    row: &gtk::ListBoxRow,
    app: Rc<AppState>,
    actions: Vec<mailin_core::config::QuickAction>,
    message_id: MessageId,
) {
    use mailin_core::config::QuickAction;

    // The overlay lives inside the row's child box, aligned to the end.
    // It starts hidden; the swipe gesture toggles it.
    let row_child = match row.child() {
        Some(child) => child,
        None => return,
    };
    // The row child is the horizontal `outer` box built by `message_row`.
    // Wrap it in an overlay so actions can float on top without disturbing
    // the existing layout.
    let outer = match row_child.downcast::<gtk::Box>() {
        Ok(b) => b,
        Err(_) => return,
    };

    let overlay = gtk::Overlay::new();
    // Move every child of `outer` into the overlay's main child.
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    while let Some(child) = outer.first_child() {
        outer.remove(&child);
        content.append(&child);
    }
    content.set_hexpand(true);
    overlay.set_child(Some(&content));

    // Build the actions bar.
    let actions_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    actions_bar.set_halign(gtk::Align::End);
    actions_bar.set_valign(gtk::Align::Fill);
    actions_bar.set_visible(false);

    for action in &actions {
        let button = gtk::Button::from_icon_name(action.icon());
        button.set_tooltip_text(Some(action.label()));
        button.add_css_class("flat");
        {
            let name = action.label();
            button.update_property(&[Property::Label(name)]);
        }
        button.set_margin_top(4);
        button.set_margin_bottom(4);
        button.set_margin_start(4);
        button.set_margin_end(4);
        match action {
            QuickAction::Archive => button.add_css_class("suggested-action"),
            QuickAction::Delete => button.add_css_class("destructive-action"),
            _ => {}
        }
        let action = *action;
        let app = Rc::clone(&app);
        let id = message_id;
        button.connect_clicked(move |_| {
            let ids = vec![id];
            match action {
                QuickAction::Archive => app.engine.archive_messages(&ids),
                QuickAction::Delete => app.engine.delete_messages(&ids),
                QuickAction::Junk => app.engine.mark_messages_junk(&ids, true),
                QuickAction::Flag => {
                    app.engine
                        .bulk_set_flags(&ids, vec!["\\Flagged".to_owned()], Vec::new());
                }
                QuickAction::Unflag => {
                    app.engine
                        .bulk_set_flags(&ids, Vec::new(), vec!["\\Flagged".to_owned()]);
                }
                QuickAction::MarkRead => {
                    app.engine
                        .bulk_set_flags(&ids, vec!["\\Seen".to_owned()], Vec::new());
                }
                QuickAction::MarkUnread => {
                    app.engine
                        .bulk_set_flags(&ids, Vec::new(), vec!["\\Seen".to_owned()]);
                }
            }
        });
        actions_bar.append(&button);
    }

    overlay.add_overlay(&actions_bar);

    // Replace the row's child with the overlay.
    row.set_child(Some(&overlay));

    // Swipe gesture: horizontal velocity above a threshold reveals actions.
    let swipe = gtk::GestureSwipe::new();
    let actions_bar_weak = actions_bar.downgrade();
    let revealed = std::cell::Cell::new(false);
    swipe.connect_swipe({
        let actions_bar_weak = actions_bar_weak.clone();
        let revealed = revealed.clone();
        move |_, velocity_x, _velocity_y| {
            let bar = match actions_bar_weak.upgrade() {
                Some(bar) => bar,
                None => return,
            };
            let show = velocity_x.abs() > 200.0;
            if show {
                // Direction picks which end the bar sits on.
                if velocity_x < 0.0 {
                    bar.set_halign(gtk::Align::End);
                } else {
                    bar.set_halign(gtk::Align::Start);
                }
                bar.set_visible(true);
                revealed.set(true);
            } else if revealed.get() {
                bar.set_visible(false);
                revealed.set(false);
            }
        }
    });
    row.add_controller(swipe);

    // A click anywhere on the row hides the overlay again.
    let click = gtk::GestureClick::new();
    let actions_bar_weak = actions_bar.downgrade();
    let revealed = revealed.clone();
    click.connect_pressed(move |_, _, _, _| {
        if let Some(bar) = actions_bar_weak.upgrade() {
            if revealed.get() {
                bar.set_visible(false);
                revealed.set(false);
            }
        }
    });
    row.add_controller(click);
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
    } else if entry.attempts > 0 {
        let attempts_label = gtk::Label::new(Some(&format!("Attempt {}", entry.attempts + 1)));
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
    let accessible = format!("Queued message, {}, {}", entry.state.as_str(), subject);
    row.update_property(&[Property::Label(&accessible)]);
    row.set_tooltip_text(Some(&subject));
    row
}

fn signature_label(status: mailin_core::SignatureStatus) -> &'static str {
    use mailin_core::SignatureStatus;
    match status {
        SignatureStatus::Valid => "Valid",
        SignatureStatus::Invalid => "Invalid — do not trust this message",
        SignatureStatus::UnknownKey => "Unknown key — cannot verify",
        SignatureStatus::Error => "Could not verify signature",
    }
}

/// Plain-text body with `>`-quoted history collapsed behind a stub. A run of
/// consecutive quote lines renders as a single line — "N quoted lines" — with
/// a toggle that expands the quote inline.
fn text_body(text: &str) -> gtk::Widget {
    fn plain_label(text: &str) -> gtk::Label {
        let label = gtk::Label::new(Some(text.trim_end()));
        label.set_wrap(true);
        label.set_wrap_mode(pango::WrapMode::WordChar);
        label.set_selectable(true);
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label
    }

    fn quote_stub(quote: &str, count: usize) -> gtk::Widget {
        let expand_label = format!(
            "Show {}",
            mailin_core::plural(count, "quoted line", "quoted lines")
        );
        let collapse_label = format!(
            "Hide {}",
            mailin_core::plural(count, "quoted line", "quoted lines")
        );
        let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let button = gtk::Button::with_label(&expand_label);
        button.add_css_class("flat");
        button.set_halign(gtk::Align::Start);
        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();
        let label = gtk::Label::new(Some(quote.trim_end()));
        label.set_wrap(true);
        label.set_wrap_mode(pango::WrapMode::WordChar);
        label.set_selectable(true);
        label.set_xalign(0.0);
        label.add_css_class("dim-label");
        revealer.set_child(Some(&label));
        let revealer_for_click = revealer.clone();
        button.connect_clicked(move |button| {
            let shown = revealer_for_click.reveals_child();
            revealer_for_click.set_reveal_child(!shown);
            button.set_label(if shown {
                &expand_label
            } else {
                &collapse_label
            });
        });
        row.append(&button);
        row.append(&revealer);
        row.upcast()
    }

    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.set_hexpand(true);
    root.set_margin_top(4);
    root.set_margin_bottom(4);

    let mut plain = String::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with('>') {
            if !plain.trim().is_empty() {
                root.append(&plain_label(&plain));
            }
            plain.clear();
            let start = i;
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                i += 1;
            }
            let quote = lines[start..i].join("\n");
            root.append(&quote_stub(&quote, i - start));
        } else {
            plain.push_str(line);
            plain.push('\n');
            i += 1;
        }
    }
    if !plain.trim().is_empty() {
        root.append(&plain_label(&plain));
    }
    root.upcast()
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
