//! Preferences dialog for the GTK front-end.
//!
//! Every widget writes into a working copy of the configuration as it is
//! changed. When the dialog closes the copy is validated, written to disk and
//! handed back to the running application through the `on_saved` callback, so
//! preference changes take effect without restarting Mailin.

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::prelude::*;
use mailin_core::{Config, MessageListDensity, Paths, Theme};

use crate::app::{AppState, apply_theme};

/// Show the preferences dialog.
///
/// `on_saved` runs after a valid configuration has been persisted and
/// installed in the running [`AppState`].
pub fn show_preferences(
    parent: &impl IsA<gtk::Widget>,
    app: &Rc<AppState>,
    on_saved: impl Fn() + 'static,
) {
    let config = Rc::new(RefCell::new(app.config.borrow().clone()));
    let paths = Paths::discover().expect("paths");

    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");

    // General page
    let general_page = adw::PreferencesPage::new();
    general_page.set_title("General");
    general_page.set_icon_name(Some("preferences-system-symbolic"));

    // Appearance group
    let appearance_group = adw::PreferencesGroup::builder().title("Appearance").build();

    let theme_combo = gtk::DropDown::from_strings(&["System", "Light", "Dark"]);
    match config.borrow().ui.theme {
        Theme::System => theme_combo.set_selected(0),
        Theme::Light => theme_combo.set_selected(1),
        Theme::Dark => theme_combo.set_selected(2),
    }
    theme_combo.connect_selected_notify({
        let config = Rc::clone(&config);
        move |combo| {
            let theme = match combo.selected() {
                1 => Theme::Light,
                2 => Theme::Dark,
                _ => Theme::System,
            };
            config.borrow_mut().ui.theme = theme;
        }
    });

    let theme_row = adw::ActionRow::builder()
        .title("Theme")
        .subtitle("Choose the application color scheme")
        .build();
    theme_row.add_suffix(&theme_combo);
    appearance_group.add(&theme_row);

    let density_combo = gtk::DropDown::from_strings(&["Comfortable", "Cozy", "Compact"]);
    density_combo.set_selected(match config.borrow().ui.message_list_density {
        MessageListDensity::Comfortable => 0,
        MessageListDensity::Cozy => 1,
        MessageListDensity::Compact => 2,
    });
    density_combo.connect_selected_notify({
        let config = Rc::clone(&config);
        move |combo| {
            let density = match combo.selected() {
                1 => MessageListDensity::Cozy,
                2 => MessageListDensity::Compact,
                _ => MessageListDensity::Comfortable,
            };
            config.borrow_mut().ui.message_list_density = density;
        }
    });

    let density_row = adw::ActionRow::builder()
        .title("Message list density")
        .subtitle("Row spacing and preview lines in the message list")
        .build();
    density_row.add_suffix(&density_combo);
    appearance_group.add(&density_row);

    general_page.add(&appearance_group);

    // Messages group
    let messages_group = adw::PreferencesGroup::builder().title("Messages").build();

    let html_switch = gtk::Switch::builder()
        .active(config.borrow().ui.html_enabled)
        .build();
    html_switch.connect_state_set({
        let config = Rc::clone(&config);
        move |_, state| {
            config.borrow_mut().ui.html_enabled = state;
            gtk::glib::Propagation::Stop
        }
    });
    let html_row = adw::ActionRow::builder()
        .title("Render HTML messages")
        .subtitle("Display HTML formatting when available")
        .build();
    html_row.add_suffix(&html_switch);
    messages_group.add(&html_row);

    let remote_content_switch = gtk::Switch::builder()
        .active(config.borrow().ui.remote_content_enabled)
        .build();
    remote_content_switch.connect_state_set({
        let config = Rc::clone(&config);
        move |_, state| {
            config.borrow_mut().ui.remote_content_enabled = state;
            gtk::glib::Propagation::Stop
        }
    });
    let remote_content_row = adw::ActionRow::builder()
        .title("Load remote content")
        .subtitle("Allow images and resources from the network (can be overridden per-sender)")
        .build();
    remote_content_row.add_suffix(&remote_content_switch);
    messages_group.add(&remote_content_row);

    let page_size_spin = gtk::SpinButton::builder()
        .adjustment(&gtk::Adjustment::new(
            config.borrow().ui.page_size as f64,
            1.0,
            1000.0,
            1.0,
            10.0,
            0.0,
        ))
        .numeric(true)
        .build();
    page_size_spin.connect_value_changed({
        let config = Rc::clone(&config);
        move |spin| {
            config.borrow_mut().ui.page_size = spin.value() as u32;
        }
    });
    let page_size_row = adw::ActionRow::builder()
        .title("Messages per page")
        .subtitle("Number of messages loaded at once")
        .build();
    page_size_row.add_suffix(&page_size_spin);
    messages_group.add(&page_size_row);

    let mark_read_switch = gtk::Switch::builder()
        .active(config.borrow().ui.mark_read_on_open)
        .build();
    mark_read_switch.connect_state_set({
        let config = Rc::clone(&config);
        move |_, state| {
            config.borrow_mut().ui.mark_read_on_open = state;
            gtk::glib::Propagation::Stop
        }
    });
    let mark_read_row = adw::ActionRow::builder()
        .title("Mark as read on open")
        .subtitle("Automatically mark messages read when opened")
        .build();
    mark_read_row.add_suffix(&mark_read_switch);
    messages_group.add(&mark_read_row);

    general_page.add(&messages_group);

    // Notifications group
    let notifications_group = adw::PreferencesGroup::builder()
        .title("Notifications")
        .build();

    let notifications_switch = gtk::Switch::builder()
        .active(config.borrow().ui.notifications)
        .build();
    notifications_switch.connect_state_set({
        let config = Rc::clone(&config);
        move |_, state| {
            config.borrow_mut().ui.notifications = state;
            gtk::glib::Propagation::Stop
        }
    });
    let notifications_row = adw::ActionRow::builder()
        .title("Show new mail notifications")
        .subtitle("Display desktop notifications for new messages")
        .build();
    notifications_row.add_suffix(&notifications_switch);
    notifications_group.add(&notifications_row);

    general_page.add(&notifications_group);

    dialog.add(&general_page);

    // Sync page
    let sync_page = adw::PreferencesPage::new();
    sync_page.set_title("Synchronization");
    sync_page.set_icon_name(Some("view-refresh-symbolic"));

    // Polling group
    let polling_group = adw::PreferencesGroup::builder().title("Polling").build();

    let poll_interval_spin = gtk::SpinButton::builder()
        .adjustment(&gtk::Adjustment::new(
            config.borrow().sync.poll_interval_secs as f64,
            15.0,
            1800.0,
            1.0,
            60.0,
            0.0,
        ))
        .numeric(true)
        .build();
    poll_interval_spin.connect_value_changed({
        let config = Rc::clone(&config);
        move |spin| {
            config.borrow_mut().sync.poll_interval_secs = spin.value() as u64;
        }
    });
    let poll_interval_row = adw::ActionRow::builder()
        .title("Poll interval")
        .subtitle("Seconds between automatic folder checks (when IDLE is unavailable)")
        .build();
    poll_interval_row.add_suffix(&poll_interval_spin);
    polling_group.add(&poll_interval_row);

    let max_parallel_spin = gtk::SpinButton::builder()
        .adjustment(&gtk::Adjustment::new(
            config.borrow().sync.max_parallel_accounts as f64,
            1.0,
            16.0,
            1.0,
            1.0,
            0.0,
        ))
        .numeric(true)
        .build();
    max_parallel_spin.connect_value_changed({
        let config = Rc::clone(&config);
        move |spin| {
            config.borrow_mut().sync.max_parallel_accounts = spin.value() as u8;
        }
    });
    let max_parallel_row = adw::ActionRow::builder()
        .title("Parallel accounts")
        .subtitle("Maximum number of accounts to sync simultaneously")
        .build();
    max_parallel_row.add_suffix(&max_parallel_spin);
    polling_group.add(&max_parallel_row);

    sync_page.add(&polling_group);

    // Message Bodies group
    let body_group = adw::PreferencesGroup::builder()
        .title("Message Bodies")
        .build();

    let body_sync_combo =
        gtk::DropDown::from_strings(&["On demand (when opened)", "Eager (background)"]);
    match config.borrow().sync.body_sync {
        mailin_core::config::BodySyncPolicy::OnOpen => body_sync_combo.set_selected(0),
        mailin_core::config::BodySyncPolicy::Eager => body_sync_combo.set_selected(1),
    }
    body_sync_combo.connect_selected_notify({
        let config = Rc::clone(&config);
        move |combo| {
            let policy = if combo.selected() == 1 {
                mailin_core::config::BodySyncPolicy::Eager
            } else {
                mailin_core::config::BodySyncPolicy::OnOpen
            };
            config.borrow_mut().sync.body_sync = policy;
        }
    });

    let body_sync_row = adw::ActionRow::builder()
        .title("Body fetching")
        .subtitle("When to download message bodies")
        .build();
    body_sync_row.add_suffix(&body_sync_combo);
    body_group.add(&body_sync_row);

    sync_page.add(&body_group);

    // Retention group
    let retention_group = adw::PreferencesGroup::builder().title("Retention").build();

    let retention_spin = gtk::SpinButton::builder()
        .adjustment(&gtk::Adjustment::new(
            config.borrow().sync.retention_days as f64,
            0.0,
            3650.0,
            1.0,
            30.0,
            0.0,
        ))
        .numeric(true)
        .build();
    retention_spin.connect_value_changed({
        let config = Rc::clone(&config);
        move |spin| {
            config.borrow_mut().sync.retention_days = spin.value() as u32;
        }
    });
    let retention_row = adw::ActionRow::builder()
        .title("Keep messages for")
        .subtitle("Days to retain message headers (0 = forever)")
        .build();
    retention_row.add_suffix(&retention_spin);
    retention_group.add(&retention_row);

    sync_page.add(&retention_group);

    dialog.add(&sync_page);

    // Accounts page
    let accounts_page = adw::PreferencesPage::new();
    accounts_page.set_title("Accounts");
    accounts_page.set_icon_name(Some("mail-account-symbolic"));

    let account_group = adw::PreferencesGroup::builder()
        .title("Per-Account Settings")
        .description("Account-specific settings like signatures and remote content trust are managed in the account settings dialog.")
        .build();

    let account_row = adw::ActionRow::builder()
        .title("Account settings")
        .subtitle("Open account settings to configure per-account options")
        .build();
    account_group.add(&account_row);

    accounts_page.add(&account_group);
    dialog.add(&accounts_page);

    // Validate, persist and hand the new configuration back to the window.
    dialog.connect_closed({
        let config = config.clone();
        let paths = paths.clone();
        let app = Rc::clone(app);
        move |_| {
            if apply_config(&config, &paths, &app) {
                on_saved();
            }
        }
    });

    dialog.present(Some(parent));
}

/// Validate the edited configuration, write it to disk and install it in the
/// running application. Returns `false` when validation failed, in which case
/// nothing is written anywhere.
fn apply_config(edited: &Rc<RefCell<Config>>, paths: &Paths, app: &Rc<AppState>) -> bool {
    let candidate = edited.borrow().clone();

    if let Err(e) = candidate.validate() {
        eprintln!("Config validation failed: {e}");
        return false;
    }

    if let Err(e) = candidate.save(paths) {
        eprintln!("Failed to save config: {e}");
    }

    // Apply theme immediately
    apply_theme(candidate.ui.theme);
    *app.config.borrow_mut() = candidate;
    true
}
