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
use mailin_core::{Config, MessageListDensity, Paths, QuickAction, Theme, ThreadMode};

use crate::app::{AppState, apply_theme};

/// Fill the Accounts page with actionable account rows. Listing accounts
/// touches the keyring, so it runs on a worker; the sync status is a cheap
/// in-memory read from the engine on the UI thread.
fn populate_accounts_group(group: adw::PreferencesGroup, app: Rc<AppState>, window: gtk::Window) {
    let mailin = std::sync::Arc::clone(&app.mailin);
    crate::background::spawn_result(
        move || mailin.list_accounts().map_err(|error| error.to_string()),
        move |result| {
            // Keep a handle to the page: the dialog may have closed while the
            // worker was running, in which case there is nothing to update.
            let accounts = match result {
                Ok(Ok(accounts)) => accounts,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "could not list accounts in Preferences");
                    return;
                }
                Err(failed) => {
                    tracing::warn!(message = %failed.message, "account list worker failed");
                    return;
                }
            };
            while let Some(row) = group.first_child() {
                if let Some(row) = row.downcast_ref::<adw::ActionRow>() {
                    group.remove(row);
                } else {
                    break;
                }
            }
            if accounts.is_empty() {
                group.add(
                    &adw::ActionRow::builder()
                        .title("No accounts yet")
                        .subtitle("Add an account to read and send mail.")
                        .build(),
                );
                return;
            }
            for account in accounts {
                let status = app.engine.sync_status(&account.id);
                let status_text = match status {
                    mailin_core::model::SyncStatus::Idle => "Idle",
                    mailin_core::model::SyncStatus::Syncing => "Syncing…",
                    mailin_core::model::SyncStatus::Connected => "Connected",
                    mailin_core::model::SyncStatus::Offline => "Offline",
                    mailin_core::model::SyncStatus::Error => "Error",
                };
                let row = adw::ActionRow::builder()
                    .title(account.email.clone())
                    .subtitle(format!("{} · {}", account.display_name, status_text))
                    .build();
                let edit = gtk::Button::with_label("Edit");
                edit.add_css_class("flat");
                edit.set_valign(gtk::Align::Center);
                {
                    use gtk::accessible::Property;
                    edit.update_property(&[Property::Label(&format!(
                        "Edit account {}",
                        account.email
                    ))]);
                }
                let remove = gtk::Button::with_label("Remove");
                remove.add_css_class("flat");
                remove.add_css_class("destructive-action");
                remove.set_valign(gtk::Align::Center);
                {
                    use gtk::accessible::Property;
                    remove.update_property(&[Property::Label(&format!(
                        "Remove account {}",
                        account.email
                    ))]);
                }
                row.add_suffix(&edit);
                row.add_suffix(&remove);

                let edit_app = Rc::clone(&app);
                let edit_account = account.clone();
                let edit_window = window.clone();
                let edit_group = group.clone();
                edit.connect_clicked(move |_| {
                    crate::account_dialog::present(
                        &edit_window,
                        Rc::clone(&edit_app),
                        crate::account_dialog::AccountDialogMode::Edit,
                        Some(edit_account.clone()),
                        {
                            let group = edit_group.clone();
                            let app = Rc::clone(&edit_app);
                            let window = edit_window.clone();
                            move |_| {
                                populate_accounts_group(
                                    group.clone(),
                                    Rc::clone(&app),
                                    window.clone(),
                                );
                            }
                        },
                    );
                });

                let remove_app = Rc::clone(&app);
                let remove_account = account.clone();
                let remove_window = window.clone();
                let remove_group = group.clone();
                remove.connect_clicked(move |_| {
                    crate::dialogs::confirm_remove_account(
                        Some(&remove_window),
                        std::sync::Arc::clone(&remove_app.mailin),
                        &remove_account,
                        {
                            let group = remove_group.clone();
                            let app = Rc::clone(&remove_app);
                            let window = remove_window.clone();
                            move || populate_accounts_group(group, app, window)
                        },
                        |error| tracing::warn!(%error, "could not remove account from Preferences"),
                    );
                });
                group.add(&row);
            }
        },
    );
}

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
    let on_saved: Rc<dyn Fn()> = Rc::new(on_saved);
    let parent_window = parent
        .upcast_ref::<gtk::Widget>()
        .downcast_ref::<gtk::Window>()
        .cloned();
    let paths = match Paths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            tracing::error!(%error, "could not discover configuration paths");
            return;
        }
    };

    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");

    // General page
    let general_page = adw::PreferencesPage::new();
    general_page.set_title("General");
    general_page.set_icon_name(Some("preferences-system-symbolic"));

    // Appearance group
    let appearance_group = adw::PreferencesGroup::builder().title("Appearance").build();

    let theme_row = adw::ComboRow::new();
    theme_row.set_title("Theme");
    theme_row.set_model(Some(&gtk::StringList::new(&["System", "Light", "Dark"])));
    theme_row.set_subtitle("Choose the application color scheme");
    theme_row.set_selected(match config.borrow().ui.theme {
        Theme::System => 0,
        Theme::Light => 1,
        Theme::Dark => 2,
    });
    theme_row.connect_selected_notify({
        let config = Rc::clone(&config);
        move |row| {
            let theme = match row.selected() {
                1 => Theme::Light,
                2 => Theme::Dark,
                _ => Theme::System,
            };
            config.borrow_mut().ui.theme = theme;
        }
    });
    appearance_group.add(&theme_row);

    let density_row = adw::ComboRow::new();
    density_row.set_title("Message list density");
    density_row.set_model(Some(&gtk::StringList::new(&[
        "Comfortable",
        "Cozy",
        "Compact",
    ])));
    density_row.set_subtitle("Row spacing and preview lines in the message list");
    density_row.set_selected(match config.borrow().ui.message_list_density {
        MessageListDensity::Comfortable => 0,
        MessageListDensity::Cozy => 1,
        MessageListDensity::Compact => 2,
    });
    density_row.connect_selected_notify({
        let config = Rc::clone(&config);
        move |row| {
            let density = match row.selected() {
                1 => MessageListDensity::Cozy,
                2 => MessageListDensity::Compact,
                _ => MessageListDensity::Comfortable,
            };
            config.borrow_mut().ui.message_list_density = density;
        }
    });
    appearance_group.add(&density_row);

    let thread_row = adw::ComboRow::new();
    thread_row.set_title("Conversation view");
    thread_row.set_model(Some(&gtk::StringList::new(&[
        "Off (flat list)",
        "Flat (grouped)",
        "Nested",
    ])));
    thread_row.set_subtitle("Group messages by thread");
    thread_row.set_selected(match config.borrow().ui.thread_mode {
        ThreadMode::Off => 0,
        ThreadMode::Flat => 1,
        ThreadMode::Nested => 2,
    });
    thread_row.connect_selected_notify({
        let config = Rc::clone(&config);
        move |row| {
            let mode = match row.selected() {
                1 => ThreadMode::Flat,
                2 => ThreadMode::Nested,
                _ => ThreadMode::Off,
            };
            config.borrow_mut().ui.thread_mode = mode;
        }
    });
    appearance_group.add(&thread_row);

    let restore_appearance = gtk::Button::builder()
        .label("Restore defaults")
        .valign(gtk::Align::Center)
        .build();
    restore_appearance.add_css_class("flat");
    restore_appearance.connect_clicked({
        let config = Rc::clone(&config);
        let theme_row = theme_row.clone();
        let density_row = density_row.clone();
        let thread_row = thread_row.clone();
        move |_| {
            let defaults = Config::default();
            {
                let mut cfg = config.borrow_mut();
                cfg.ui.theme = defaults.ui.theme;
                cfg.ui.message_list_density = defaults.ui.message_list_density;
                cfg.ui.thread_mode = defaults.ui.thread_mode;
            }
            theme_row.set_selected(0);
            density_row.set_selected(0);
            thread_row.set_selected(0);
        }
    });
    appearance_group.set_header_suffix(Some(&restore_appearance));

    general_page.add(&appearance_group);

    // Messages group
    let messages_group = adw::PreferencesGroup::builder().title("Messages").build();

    let html_row = adw::SwitchRow::new();
    html_row.set_title("Render HTML messages");
    html_row.set_subtitle("Display HTML formatting when available");
    html_row.set_active(config.borrow().ui.html_enabled);
    html_row.connect_active_notify({
        let config = Rc::clone(&config);
        move |row| config.borrow_mut().ui.html_enabled = row.is_active()
    });
    messages_group.add(&html_row);

    let remote_row = adw::SwitchRow::new();
    remote_row.set_title("Load remote content");
    remote_row
        .set_subtitle("Allow images and resources from the network (can be overridden per sender)");
    remote_row.set_active(config.borrow().ui.remote_content_enabled);
    remote_row.connect_active_notify({
        let config = Rc::clone(&config);
        move |row| config.borrow_mut().ui.remote_content_enabled = row.is_active()
    });
    messages_group.add(&remote_row);

    let page_size_row = adw::SpinRow::with_range(1.0, 1000.0, 1.0);
    page_size_row.set_title("Messages per page");
    page_size_row.set_subtitle("Number of messages loaded at once");
    page_size_row.set_value(config.borrow().ui.page_size as f64);
    page_size_row.connect_notify_local(Some("value"), {
        let config = Rc::clone(&config);
        move |row, _| config.borrow_mut().ui.page_size = row.value() as u32
    });
    messages_group.add(&page_size_row);

    let mark_read_row = adw::SwitchRow::new();
    mark_read_row.set_title("Mark as read on open");
    mark_read_row.set_subtitle("Automatically mark messages read when opened");
    mark_read_row.set_active(config.borrow().ui.mark_read_on_open);
    mark_read_row.connect_active_notify({
        let config = Rc::clone(&config);
        move |row| config.borrow_mut().ui.mark_read_on_open = row.is_active()
    });
    messages_group.add(&mark_read_row);

    let quick_actions_row = adw::SwitchRow::new();
    quick_actions_row.set_title("Quick actions");
    quick_actions_row.set_subtitle("Show row actions for archive, delete, flag, and read state");
    quick_actions_row.set_active(config.borrow().ui.quick_actions);
    quick_actions_row.connect_active_notify({
        let config = Rc::clone(&config);
        move |row| config.borrow_mut().ui.quick_actions = row.is_active()
    });
    messages_group.add(&quick_actions_row);

    let quick_item_rows: Rc<RefCell<Vec<(QuickAction, adw::SwitchRow)>>> =
        Rc::new(RefCell::new(Vec::new()));

    let restore_messages = gtk::Button::builder()
        .label("Restore defaults")
        .valign(gtk::Align::Center)
        .build();
    restore_messages.add_css_class("flat");
    restore_messages.connect_clicked({
        let config = Rc::clone(&config);
        let html_row = html_row.clone();
        let remote_row = remote_row.clone();
        let page_size_row = page_size_row.clone();
        let mark_read_row = mark_read_row.clone();
        let quick_actions_row = quick_actions_row.clone();
        let quick_item_rows = Rc::clone(&quick_item_rows);
        move |_| {
            let defaults = Config::default();
            {
                let mut cfg = config.borrow_mut();
                cfg.ui.html_enabled = defaults.ui.html_enabled;
                cfg.ui.remote_content_enabled = defaults.ui.remote_content_enabled;
                cfg.ui.page_size = defaults.ui.page_size;
                cfg.ui.mark_read_on_open = defaults.ui.mark_read_on_open;
                cfg.ui.quick_actions = defaults.ui.quick_actions;
                cfg.ui.quick_action_items = defaults.ui.quick_action_items.clone();
            }
            html_row.set_active(defaults.ui.html_enabled);
            remote_row.set_active(defaults.ui.remote_content_enabled);
            page_size_row.set_value(defaults.ui.page_size as f64);
            mark_read_row.set_active(defaults.ui.mark_read_on_open);
            quick_actions_row.set_active(defaults.ui.quick_actions);
            let rows = quick_item_rows.borrow();
            let actions: Vec<QuickAction> = rows.iter().map(|(action, _)| *action).collect();
            for ((_, row), active) in rows
                .iter()
                .zip(mailin_core::quick_action_restore_states(&actions))
            {
                row.set_active(active);
            }
        }
    });
    messages_group.set_header_suffix(Some(&restore_messages));

    // Per-item quick-action toggles (M3a-6).
    let quick_item_defs = [
        (QuickAction::Archive, "Quick action: Archive"),
        (QuickAction::Delete, "Quick action: Delete"),
        (QuickAction::Flag, "Quick action: Flag"),
        (QuickAction::MarkRead, "Quick action: Mark read"),
        (QuickAction::MarkUnread, "Quick action: Mark unread"),
        (QuickAction::Junk, "Quick action: Mark as junk"),
    ];
    for (action, title) in quick_item_defs {
        let row = adw::SwitchRow::new();
        row.set_title(title);
        row.set_active(config.borrow().ui.quick_action_items.contains(&action));
        row.connect_active_notify({
            let config = Rc::clone(&config);
            move |row| {
                let mut cfg = config.borrow_mut();
                if row.is_active() {
                    if !cfg.ui.quick_action_items.contains(&action) {
                        cfg.ui.quick_action_items.push(action);
                    }
                } else {
                    cfg.ui.quick_action_items.retain(|item| item != &action);
                }
            }
        });
        messages_group.add(&row);
        quick_item_rows.borrow_mut().push((action, row));
    }

    general_page.add(&messages_group);

    // Notifications group
    let notifications_group = adw::PreferencesGroup::builder()
        .title("Notifications")
        .build();

    let notifications_row = adw::SwitchRow::new();
    notifications_row.set_title("Show new mail notifications");
    notifications_row.set_subtitle("Display desktop notifications for new messages");
    notifications_row.set_active(config.borrow().ui.notifications);
    notifications_row.connect_active_notify({
        let config = Rc::clone(&config);
        move |row| config.borrow_mut().ui.notifications = row.is_active()
    });
    // B1: a single row does not need a restore-defaults button.
    notifications_group.add(&notifications_row);

    general_page.add(&notifications_group);

    dialog.add(&general_page);

    // Sync page
    let sync_page = adw::PreferencesPage::new();
    sync_page.set_title("Synchronization");
    sync_page.set_icon_name(Some("view-refresh-symbolic"));

    // Polling group
    let polling_group = adw::PreferencesGroup::builder().title("Polling").build();

    let poll_row = adw::SpinRow::with_range(15.0, 1800.0, 1.0);
    poll_row.set_title("Poll interval");
    poll_row.set_subtitle(
        "Default seconds between automatic folder checks; accounts can override it in Edit Account",
    );
    poll_row.set_value(config.borrow().sync.poll_interval_secs as f64);
    poll_row.connect_notify_local(Some("value"), {
        let config = Rc::clone(&config);
        move |row, _| config.borrow_mut().sync.poll_interval_secs = row.value() as u64
    });
    polling_group.add(&poll_row);

    // B1: a single row does not need a restore-defaults button.
    sync_page.add(&polling_group);

    // Message Bodies group
    let body_group = adw::PreferencesGroup::builder()
        .title("Message Bodies")
        .build();

    let body_row = adw::ComboRow::new();
    body_row.set_title("Body fetching");
    body_row.set_model(Some(&gtk::StringList::new(&[
        "On demand (when opened)",
        "Eager (background)",
    ])));
    body_row.set_subtitle("When to download message bodies");
    body_row.set_selected(match config.borrow().sync.body_sync {
        mailin_core::config::BodySyncPolicy::OnOpen => 0,
        mailin_core::config::BodySyncPolicy::Eager => 1,
    });
    body_row.connect_selected_notify({
        let config = Rc::clone(&config);
        move |row| {
            config.borrow_mut().sync.body_sync = if row.selected() == 1 {
                mailin_core::config::BodySyncPolicy::Eager
            } else {
                mailin_core::config::BodySyncPolicy::OnOpen
            };
        }
    });
    body_group.add(&body_row);

    sync_page.add(&body_group);

    // Retention group
    let retention_group = adw::PreferencesGroup::builder().title("Retention").build();

    let retention_row = adw::SpinRow::with_range(0.0, 3650.0, 1.0);
    retention_row.set_title("Keep messages for");
    retention_row.set_subtitle(
        "Drop cached messages older than this many days (0 = forever). Server mail is not touched",
    );
    retention_row.set_value(config.borrow().sync.retention_days as f64);
    retention_row.connect_notify_local(Some("value"), {
        let config = Rc::clone(&config);
        move |row, _| config.borrow_mut().sync.retention_days = row.value() as u32
    });
    retention_group.add(&retention_row);

    sync_page.add(&retention_group);

    dialog.add(&sync_page);

    // Accounts page (M3a-6: removed dead-end row; point to the main window).
    let accounts_page = adw::PreferencesPage::new();
    accounts_page.set_title("Accounts");
    accounts_page.set_icon_name(Some("mail-account-symbolic"));

    let account_group = adw::PreferencesGroup::builder()
        .title("Your accounts")
        .description("Add, edit or remove accounts. Signatures, trusted senders, templates and contacts are edited in the account dialog; OpenPGP keys are managed on the Encryption page.")
        .build();
    let accounts_window = parent_window.clone().or_else(|| {
        parent
            .as_ref()
            .root()
            .and_then(|root| root.downcast::<gtk::Window>().ok())
    });
    if let Some(window) = accounts_window {
        populate_accounts_group(account_group.clone(), Rc::clone(app), window.clone());
        let add_group = adw::PreferencesGroup::new();
        let add = gtk::Button::with_label("Add Account…");
        add.add_css_class("suggested-action");
        add.add_css_class("pill");
        add.set_halign(gtk::Align::Center);
        let add_app = Rc::clone(app);
        let add_window = window.clone();
        let add_group_for_callback = account_group.clone();
        add.connect_clicked(move |_| {
            crate::account_dialog::present(
                &add_window,
                Rc::clone(&add_app),
                crate::account_dialog::AccountDialogMode::Add,
                None,
                {
                    let group = add_group_for_callback.clone();
                    let app = Rc::clone(&add_app);
                    let window = add_window.clone();
                    move |_| populate_accounts_group(group.clone(), Rc::clone(&app), window.clone())
                },
            );
        });
        add_group.add(
            &adw::ActionRow::builder()
                .title("Need another mailbox?")
                .subtitle("Add an IMAP account with password or OAuth2 sign-in.")
                .build(),
        );
        // Place the button on the row itself using a custom suffix widget.
        if let Some(row) = add_group
            .first_child()
            .and_then(|w| w.downcast::<adw::ActionRow>().ok())
        {
            row.add_suffix(&add);
        }
        accounts_page.add(&add_group);
    }
    accounts_page.add(&account_group);
    dialog.add(&accounts_page);

    // Encryption page: PGP key management per account.
    let encryption_page = adw::PreferencesPage::new();
    encryption_page.set_title("Encryption");
    encryption_page.set_icon_name(Some("channel-secure-symbolic"));

    let pgp_group = adw::PreferencesGroup::builder()
        .title("OpenPGP keys")
        .description("Manage the keys used to encrypt, sign, decrypt and verify mail.")
        .build();

    // The account list reads the store and asks the keyring for every
    // account: do it on a worker and fill the page when it arrives.
    {
        let mailin = std::sync::Arc::clone(&app.mailin);
        let app_for_accounts = Rc::clone(app);
        let pgp_group = pgp_group.clone();
        let parent_window = parent
            .as_ref()
            .root()
            .and_then(|root| root.downcast::<gtk::Window>().ok());
        crate::background::spawn_result(
            move || {
                mailin
                    .list_accounts()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|account| {
                        let key_count = mailin
                            .list_pgp_keys(&account.id)
                            .map(|keys| keys.len())
                            .unwrap_or(0);
                        (account, key_count)
                    })
                    .collect::<Vec<_>>()
            },
            move |result| {
                let Ok(rows) = result else {
                    return;
                };
                if rows.is_empty() {
                    let row = adw::ActionRow::builder()
                        .title("No accounts yet")
                        .subtitle("Add an account to manage its PGP keys.")
                        .build();
                    pgp_group.add(&row);
                    return;
                }
                for (account, key_count) in rows {
                    let row = adw::ActionRow::builder()
                        .title(&account.email)
                        .subtitle(match key_count {
                            1 => "1 key".to_owned(),
                            count => format!("{count} keys"),
                        })
                        .build();
                    let manage = gtk::Button::with_label("Manage Keys");
                    manage.add_css_class("flat");
                    manage.set_valign(gtk::Align::Center);
                    {
                        use gtk::accessible::Property;
                        let label = format!("Manage PGP keys for {}", account.email);
                        manage.update_property(&[Property::Label(&label)]);
                    }
                    row.add_suffix(&manage);

                    if let Some(window) = parent_window.clone() {
                        let app = Rc::clone(&app_for_accounts);
                        manage.connect_clicked(move |_| {
                            crate::pgp_keys::present(&window, Rc::clone(&app), account.clone());
                        });
                    }
                    pgp_group.add(&row);
                }
            },
        );
    }
    encryption_page.add(&pgp_group);
    dialog.add(&encryption_page);

    // Plugins page: manage Lua plugins.
    let plugins_page = adw::PreferencesPage::new();
    plugins_page.set_title("Plugins");
    plugins_page.set_icon_name(Some("application-x-addon-symbolic"));

    let plugins_group = adw::PreferencesGroup::builder()
        .title("Installed Plugins")
        .description(
            "Lua plugins extend Mailin with custom functionality. Enable or disable plugins here.",
        )
        .build();

    let group_for_plugins = plugins_group.clone();
    let mailin_for_plugins = std::sync::Arc::clone(&app.mailin);
    // The plugin list reads the store: run it on a worker.
    crate::background::spawn_async(
        {
            let mailin = std::sync::Arc::clone(&mailin_for_plugins);
            move |runtime| runtime.block_on(mailin.list_plugins())
        },
        move |result| {
            let Ok(plugins) = result else {
                return;
            };
            if plugins.is_empty() {
                let row = adw::ActionRow::builder()
                    .title("No plugins installed")
                    .subtitle("Place plugin directories in the plugins folder to install them.")
                    .build();
                group_for_plugins.add(&row);
            } else {
                for plugin in plugins {
                    let row = adw::ActionRow::builder()
                        .title(&plugin.name)
                        .subtitle(format!("v{} · {}", plugin.version, plugin.source_path))
                        .build();
                    let toggle = gtk::Switch::new();
                    toggle.set_active(plugin.enabled);
                    toggle.set_valign(gtk::Align::Center);
                    row.add_suffix(&toggle);

                    let mailin = std::sync::Arc::clone(&mailin_for_plugins);
                    let name = plugin.name.clone();
                    toggle.connect_state_set(move |_, enabled| {
                        // Enabling a plugin writes to the store: keep it off
                        // the UI thread.
                        let mailin = std::sync::Arc::clone(&mailin);
                        let name = name.clone();
                        crate::background::spawn_async(
                            move |runtime| {
                                runtime.block_on(mailin.set_plugin_enabled(&name, enabled))
                            },
                            |_| {},
                        );
                        gtk::glib::Propagation::Proceed
                    });

                    group_for_plugins.add(&row);
                }
            }
        },
    );

    plugins_page.add(&plugins_group);
    dialog.add(&plugins_page);

    // About page: application information, support actions and a
    // preferences reset (B1/B2).
    let about_page = adw::PreferencesPage::new();
    about_page.set_title("About");
    about_page.set_icon_name(Some("help-about-symbolic"));

    let mailin_group = adw::PreferencesGroup::builder().title("Mailin").build();
    let version_row = adw::ActionRow::builder()
        .title("Mailin")
        .subtitle(format!("Version {}", env!("CARGO_PKG_VERSION")))
        .build();
    let version_icon = gtk::Image::from_icon_name("dev.mailin.Mailin");
    version_icon.set_pixel_size(32);
    version_row.add_prefix(&version_icon);
    mailin_group.add(&version_row);
    let source_row = adw::ActionRow::builder()
        .title("Source code")
        .subtitle("github.com/ibro-3/mailin")
        .activatable(true)
        .build();
    source_row.add_suffix(&gtk::Image::from_icon_name("adw-external-link-symbolic"));
    source_row.connect_activated(|_| {
        let uri = "https://github.com/ibro-3/mailin";
        let _ = gtk::gio::AppInfo::launch_default_for_uri(uri, None::<&gtk::gio::AppLaunchContext>);
    });
    mailin_group.add(&source_row);
    let bug_row = adw::ActionRow::builder()
        .title("Report a bug")
        .subtitle("File an issue on the issue tracker")
        .activatable(true)
        .build();
    bug_row.add_suffix(&gtk::Image::from_icon_name("adw-external-link-symbolic"));
    bug_row.connect_activated(|_| {
        let uri = "https://github.com/ibro-3/mailin/issues";
        let _ = gtk::gio::AppInfo::launch_default_for_uri(uri, None::<&gtk::gio::AppLaunchContext>);
    });
    mailin_group.add(&bug_row);
    about_page.add(&mailin_group);

    let support_group = adw::PreferencesGroup::builder()
        .title("Support")
        .description("Share diagnostics or keep a copy of your local mail cache.")
        .build();

    let diagnostics_row = adw::ActionRow::builder()
        .title("Copy diagnostics")
        .subtitle("Version, settings and the last 100 log lines — no passwords or message bodies.")
        .activatable(true)
        .build();
    diagnostics_row.add_suffix(&gtk::Image::from_icon_name("edit-copy-symbolic"));
    {
        let app = Rc::clone(app);
        let paths = paths.clone();
        let row = diagnostics_row.clone();
        diagnostics_row.connect_activated(move |_| {
            let report =
                mailin_core::support::build_diagnostics(&app.mailin, &app.config.borrow(), &paths);
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&report);
            }
            row.set_subtitle("Copied to clipboard");
        });
    }
    support_group.add(&diagnostics_row);

    let backup_row = adw::ActionRow::builder()
        .title("Back up local data…")
        .subtitle("Save a zip of the local mail cache and preferences (no passwords).")
        .activatable(true)
        .build();
    backup_row.add_suffix(&gtk::Image::from_icon_name("document-save-symbolic"));
    {
        let paths = paths.clone();
        let parent = parent_window.clone();
        backup_row.connect_activated(move |_| {
            let file_dialog = gtk::FileDialog::builder()
                .title("Back up local data")
                .initial_name(
                    mailin_core::support::backup_filename()
                        .to_string_lossy()
                        .into_owned(),
                )
                .build();
            let paths = paths.clone();
            let parent = parent.clone();
            file_dialog.save(
                parent.as_ref(),
                None::<&gtk::gio::Cancellable>,
                move |result| {
                    let Ok(file) = result else {
                        return;
                    };
                    let Some(destination) = file.path() else {
                        return;
                    };
                    let paths = paths.clone();
                    crate::background::spawn_result(
                        move || {
                            mailin_core::support::backup_local_data(&paths, &destination)
                                .map_err(|error| error.to_string())
                        },
                        |result| {
                            let message = match result {
                                Ok(Ok(())) => "Backup saved.".to_owned(),
                                Ok(Err(error)) => {
                                    format!("Could not save the backup: {error}")
                                }
                                Err(failed) => {
                                    format!("Could not save the backup: {}", failed.message)
                                }
                            };
                            let alert = gtk::AlertDialog::builder().message(message).build();
                            alert.set_buttons(&["OK"]);
                            gtk::glib::spawn_future_local(async move {
                                let _ = alert.choose_future(None::<&gtk::Window>).await;
                            });
                        },
                    );
                },
            );
        });
    }
    support_group.add(&backup_row);
    about_page.add(&support_group);

    let preferences_group = adw::PreferencesGroup::builder()
        .title("Preferences")
        .build();
    let reset_all_row = adw::ActionRow::builder()
        .title("Reset all preferences")
        .subtitle("Restore every preference to its default. Accounts and mail are not affected.")
        .activatable(true)
        .build();
    reset_all_row.add_suffix(&gtk::Image::from_icon_name("edit-undo-symbolic"));
    {
        let config = Rc::clone(&config);
        let app = Rc::clone(app);
        let paths = paths.clone();
        let dialog = dialog.clone();
        let parent = parent_window.clone();
        let on_saved = on_saved.clone();
        reset_all_row.connect_activated(move |_| {
            let confirm = adw::AlertDialog::builder()
                .heading("Reset all preferences?")
                .body(
                    "Every preference returns to its default value. Accounts, mail, templates \\
                     and contacts are not affected.",
                )
                .build();
            confirm.add_responses(&[("cancel", "Cancel"), ("reset", "Reset preferences")]);
            confirm.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
            confirm.set_default_response(Some("cancel"));
            confirm.set_close_response("cancel");
            let config = Rc::clone(&config);
            let app = Rc::clone(&app);
            let paths = paths.clone();
            let dialog = dialog.clone();
            let on_saved = on_saved.clone();
            let parent = parent.clone();
            gtk::glib::spawn_future_local(async move {
                let response = confirm.choose_future(parent.as_ref()).await;
                if response.as_str() != "reset" {
                    return;
                }
                let defaults = Config::default();
                if let Err(error) = defaults.save(&paths) {
                    show_config_error(&format!("Could not write the configuration file: {error}"));
                    return;
                }
                apply_theme(defaults.ui.theme);
                if let Err(error) = app.mailin.install_config(defaults.clone()) {
                    tracing::warn!(%error, "could not install the reset configuration in the running client");
                }
                *app.config.borrow_mut() = defaults.clone();
                // The dialog's rows are bound to the old working copy; load
                // the defaults there too so the closed handler re-saves a
                // consistent state.
                *config.borrow_mut() = defaults;
                on_saved();
                dialog.close();
            });
        });
    }
    preferences_group.add(&reset_all_row);
    about_page.add(&preferences_group);
    dialog.add(&about_page);

    // Validate, persist and hand the new configuration back to the window.
    // Writing the configuration file touches the disk, so it happens on a
    // worker; on failure the user can retry the write or discard the
    // changes — the error text is shown in the alert.
    dialog.connect_closed({
        let config = config.clone();
        let paths = paths.clone();
        let app = Rc::clone(app);
        let on_saved = on_saved.clone();
        move |_| {
            let candidate = config.borrow().clone();
            if let Err(error) = candidate.validate() {
                show_config_error(&error.to_string());
                return;
            }
            attempt_save(
                candidate,
                paths.clone(),
                Rc::clone(&app),
                Rc::clone(&on_saved),
            );
        }
    });

    dialog.present(Some(parent));
}

/// Show the "could not save preferences" alert.
fn show_config_error(message: &str) {
    let alert = gtk::AlertDialog::builder()
        .message("Couldn't save preferences")
        .detail(message)
        .build();
    alert.set_buttons(&["OK"]);
    alert.set_default_button(0);
    gtk::glib::spawn_future_local(async move {
        let _ = alert.choose_future(None::<&gtk::Window>).await;
    });
}

/// Validate-on-close succeeded: persist the candidate configuration on a
/// worker and hand it to the running application. On failure offer Retry
/// (which re-runs this same save) or Discard.
fn attempt_save(
    candidate: mailin_core::Config,
    paths: mailin_core::Paths,
    app: Rc<AppState>,
    on_saved: Rc<dyn Fn()>,
) {
    let saved = candidate.clone();
    let paths_c = paths.clone();
    let app_err = Rc::clone(&app);
    let on_saved_err = Rc::clone(&on_saved);
    let candidate_success = candidate.clone();
    crate::background::spawn_result(
        move || {
            saved
                .save(&paths_c)
                .map_err(|e| format!("Could not write the configuration file: {e}"))
        },
        move |result| match result {
            Ok(Ok(())) => {
                // Apply theme immediately
                apply_theme(candidate_success.ui.theme);
                // Workers (engine) read the same configuration the UI sees.
                if let Err(error) = app.mailin.install_config(candidate_success.clone()) {
                    tracing::warn!(%error, "could not install the saved configuration in the running client");
                }
                *app.config.borrow_mut() = candidate_success;
                on_saved();
            }
            Ok(Err(message)) => show_save_error(&message, candidate, paths, app_err, on_saved_err),
            Err(failed) => {
                show_save_error(&failed.message, candidate, paths, app_err, on_saved_err)
            }
        },
    );
}

/// The write failed: show the error with a Retry button so the user's
/// changes are not silently lost (B2).
fn show_save_error(
    message: &str,
    candidate: mailin_core::Config,
    paths: mailin_core::Paths,
    app: Rc<AppState>,
    on_saved: Rc<dyn Fn()>,
) {
    let alert = gtk::AlertDialog::builder()
        .message("Couldn't save preferences")
        .detail(format!(
            "{message}\n\nYour changes are kept in this window's memory; retry the save or discard them."
        ))
        .build();
    alert.set_buttons(&["Discard changes", "Retry save"]);
    alert.set_cancel_button(0);
    alert.set_default_button(1);
    gtk::glib::spawn_future_local(async move {
        let response = alert.choose_future(None::<&gtk::Window>).await;
        // 1 = Retry save, 0/discarded = Discard changes.
        if matches!(response, Ok(1)) {
            attempt_save(candidate, paths, app, on_saved);
        }
    });
}
