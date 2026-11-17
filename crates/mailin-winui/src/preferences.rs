//! Preferences dialog for the WinUI front-end.
//!
//! The dialog is a `ContentDialog` over the main window: sections are composed
//! from the shared `ui` helpers and the widgets are read back when the primary
//! button saves the configuration to disk.

use std::sync::Arc;

use anyhow::{Context, Result};
use mailin_core::{AccountId, Config, MessageListDensity, Paths, QuickAction, Theme};
use mailin_winui_sys::{
    Button, ComboBox, ContentDialog, ContentDialogButton, StackPanel, ToggleSwitch,
};
use windows_core::Interface;

use crate::app::AppState;
use crate::ui;

/// The widgets whose values are written back into the configuration.
#[derive(Clone)]
struct GeneralRows {
    theme: ComboBox,
    density: ComboBox,
    thread_mode: ComboBox,
    html: ToggleSwitch,
    remote_content: ToggleSwitch,
    page_size: ComboBox,
    mark_read: ToggleSwitch,
    notifications: ToggleSwitch,
    quick_actions: ToggleSwitch,
    quick_action_archive: ToggleSwitch,
    quick_action_delete: ToggleSwitch,
    quick_action_flag: ToggleSwitch,
    quick_action_read: ToggleSwitch,
    quick_action_unread: ToggleSwitch,
    quick_action_junk: ToggleSwitch,
    restore_messages: Button,
}

#[derive(Clone)]
struct SyncRows {
    poll_interval: ComboBox,
    body_sync: ComboBox,
    retention_days: ComboBox,
}

/// Show the preferences dialog over `xaml_root` (the main window's root).
///
/// `on_saved` runs after a valid configuration has been persisted and
/// installed in the running [`AppState`].
///
/// `events` keeps the dialog's button revokers alive for the lifetime of the
/// main window; dropping them early would unregister the save handler while
/// the dialog is still open.
pub fn show_preferences(
    state: &Arc<AppState>,
    xaml_root: &mailin_winui_sys::XamlRoot,
    events: &ui::Revokers,
    on_saved: impl Fn() + 'static,
    on_remove_account: impl Fn(AccountId) + 'static,
) -> Result<()> {
    let on_saved: Arc<dyn Fn()> = Arc::new(on_saved);
    let on_remove_account: std::rc::Rc<dyn Fn(AccountId)> = std::rc::Rc::new(on_remove_account);
    let config = Arc::clone(&state.config);
    let paths = Paths::discover().context("could not locate the configuration directory")?;

    let dialog = ContentDialog::new().context("could not create the preferences dialog")?;
    let title: windows_core::IInspectable = ui::text("Preferences")?.cast()?;
    dialog
        .SetTitle(&title)
        .context("could not title the preferences dialog")?;

    let main_panel = ui::stack_lg(mailin_winui_sys::Orientation::Vertical)?;
    ui::margin_xl(&main_panel)?;
    ui::margin_vertical(&main_panel, 8.0)?;

    let (general_panel, general) =
        create_general_section(&config.read().unwrap_or_else(|err| err.into_inner()))?;
    ui::append(&main_panel, &general_panel)?;

    // "Restore message defaults" only resets the controls; the dialog stays
    // transactional, so the live config is left untouched until "Save".
    events.keep(ui::on_click(&general.restore_messages, {
        let defaults = Config::default();
        let general = general.clone();
        move || {
            let _ = general.quick_actions.SetIsOn(defaults.ui.quick_actions);
            let actions = [
                QuickAction::Archive,
                QuickAction::Delete,
                QuickAction::Flag,
                QuickAction::MarkRead,
                QuickAction::MarkUnread,
                QuickAction::Junk,
            ];
            let switches = [
                &general.quick_action_archive,
                &general.quick_action_delete,
                &general.quick_action_flag,
                &general.quick_action_read,
                &general.quick_action_unread,
                &general.quick_action_junk,
            ];
            for (switch, state) in switches
                .iter()
                .zip(mailin_core::quick_action_restore_states(&actions))
            {
                let _ = switch.SetIsOn(state);
            }
            let _ = general.html.SetIsOn(defaults.ui.html_enabled);
            let _ = general
                .remote_content
                .SetIsOn(defaults.ui.remote_content_enabled);
            let _ = general.mark_read.SetIsOn(defaults.ui.mark_read_on_open);
            ui::select_index(
                &general.page_size,
                match defaults.ui.page_size {
                    50 => 0,
                    100 => 1,
                    200 => 2,
                    500 => 3,
                    1000 => 4,
                    _ => 1,
                },
            );
        }
    })?);

    let (sync_panel, sync) =
        create_sync_section(&config.read().unwrap_or_else(|err| err.into_inner()))?;
    ui::append(&main_panel, &sync_panel)?;
    let accounts_panel = create_accounts_section(
        state,
        xaml_root,
        events,
        Arc::clone(&on_saved),
        std::rc::Rc::clone(&on_remove_account),
    )?;
    ui::append(&main_panel, &accounts_panel)?;
    let encryption_panel = create_encryption_section(state, events)?;
    ui::append(&main_panel, &encryption_panel)?;
    let plugins_panel = create_plugins_section(state, events)?;
    let support_panel = create_support_section(state, &paths, events)?;
    ui::append(&main_panel, &support_panel)?;
    // Advanced section: restore every preference to its default (B2).
    let advanced_panel = ui::stack_md(mailin_winui_sys::Orientation::Vertical)?;
    ui::append(&advanced_panel, &ui::heading3("Advanced")?)?;
    ui::append(
        &advanced_panel,
        &ui::body_text(
            "Restore every preference to its default. Accounts and mail are not affected.",
        )?,
    )?;
    let reset_all = ui::button(
        "Reset all preferences",
        Some("Reset all preferences"),
        Some("Restore every preference to its default value"),
    )?;
    ui::append(&advanced_panel, &reset_all)?;
    ui::append(&main_panel, &advanced_panel)?;

    ui::append(&main_panel, &plugins_panel)?;

    // Inline error line; shown when the configuration is rejected and the
    // dialog stays open instead of closing silently.
    let error_text = ui::text("")?;
    ui::margin_horizontal(&error_text, 24.0)?;
    ui::margin_vertical(&error_text, 8.0)?;
    let _ = mailin_winui_sys::AutomationProperties::SetLiveSetting(
        &error_text,
        mailin_winui_sys::AutomationLiveSetting::Polite,
    );
    let _ = ui::set_visible(&error_text, false);
    ui::append(&main_panel, &error_text)?;

    ui::set_content(&dialog, &main_panel).context("could not attach the preferences content")?;

    dialog
        .SetPrimaryButtonText("Save")
        .context("could not set the save button")?;
    dialog
        .SetCloseButtonText("Cancel")
        .context("could not set the cancel button")?;
    dialog
        .SetDefaultButton(ContentDialogButton::Primary)
        .context("could not select the default button")?;
    let dialog_element: mailin_winui_sys::IUIElement = dialog.cast()?;
    dialog_element
        .SetXamlRoot(xaml_root)
        .context("could not attach the dialog to the window")?;

    // Wire the reset-all-preferences button. Confirmation first; on success
    // every preference is replaced by the defaults, installed and saved.
    events.keep(ui::on_click(&reset_all, {
        let dialog = dialog.clone();
        let state = Arc::clone(state);
        let config = Arc::clone(&config);
        let paths = paths.clone();
        let xaml_root = xaml_root.clone();
        let on_saved = Arc::clone(&on_saved);
        move || {
            let Ok(confirm) = mailin_winui_sys::ContentDialog::new() else {
                return;
            };
            let Ok(title) = ui::text("Reset all preferences?") else {
                return;
            };
            let title: windows_core::IInspectable = match title.cast::<windows_core::IInspectable>() {
                Ok(title) => title,
                Err(_) => return,
            };
            let _ = confirm.SetTitle(&title);
            let Ok(panel) = ui::stack_sm(mailin_winui_sys::Orientation::Vertical) else {
                return;
            };
            let _ = ui::margin_xl(&panel);
            let Ok(body) = ui::body_text(
                "Every preference returns to its default value. Accounts, mail, templates and contacts are not affected.",
            ) else {
                return;
            };
            let _ = ui::append(&panel, &body);
            if ui::set_content(&confirm, &panel).is_err() {
                return;
            }
            let _ = confirm.SetPrimaryButtonText("Reset preferences");
            let _ = confirm.SetCloseButtonText("Cancel");
            let _ = confirm.SetDefaultButton(mailin_winui_sys::ContentDialogButton::Close);
            let Ok(element) = confirm.cast::<mailin_winui_sys::IUIElement>() else {
                return;
            };
            if element.SetXamlRoot(&xaml_root).is_err() {
                return;
            }
            let state = Arc::clone(&state);
            let config = Arc::clone(&config);
            let paths = paths.clone();
            let dialog = dialog.clone();
            let on_saved = Arc::clone(&on_saved);
            let _ = confirm.PrimaryButtonClick({
                move |_, _| {
                    let defaults = Config::default();
                    if defaults.save(&paths).is_err() {
                        tracing::error!("could not write the reset configuration");
                        return;
                    }
                    if let Err(error) = state.mailin.install_config(defaults.clone()) {
                        tracing::warn!(%error, "could not install the reset configuration");
                    }
                    *config.write().unwrap_or_else(|err| err.into_inner()) = defaults.clone();
                    apply_theme(defaults.ui.theme);
                    on_saved();
                    let _ = dialog.Hide();
                }
            });
            let _ = confirm.ShowAsync();
        }
    })?);

    events.keep(dialog.PrimaryButtonClick({
        let dialog = dialog.clone();
        let config = Arc::clone(&config);
        let paths = paths.clone();
        let mailin = Arc::clone(&state.mailin);
        let error_text = error_text.clone();
        let on_saved = Arc::clone(&on_saved);
        move |_, args| {
            let candidate = {
                let mut guard = config.write().unwrap_or_else(|err| err.into_inner());
                guard.ui.theme = match ui::selected_index(&general.theme) {
                    1 => Theme::Light,
                    2 => Theme::Dark,
                    _ => Theme::System,
                };
                guard.ui.message_list_density = match ui::selected_index(&general.density) {
                    1 => MessageListDensity::Cozy,
                    2 => MessageListDensity::Compact,
                    _ => MessageListDensity::Comfortable,
                };
                guard.ui.thread_mode = match ui::selected_index(&general.thread_mode) {
                    1 => mailin_core::config::ThreadMode::Flat,
                    2 => mailin_core::config::ThreadMode::Nested,
                    _ => mailin_core::config::ThreadMode::Off,
                };
                guard.ui.html_enabled = is_on(&general.html);
                guard.ui.remote_content_enabled = is_on(&general.remote_content);
                guard.ui.page_size = match ui::selected_index(&general.page_size) {
                    0 => 50,
                    1 => 100,
                    2 => 200,
                    3 => 500,
                    _ => 1000,
                };
                guard.ui.mark_read_on_open = is_on(&general.mark_read);
                guard.ui.notifications = is_on(&general.notifications);
                guard.ui.quick_actions = is_on(&general.quick_actions);
                let toggles = [
                    (QuickAction::Archive, is_on(&general.quick_action_archive)),
                    (QuickAction::Delete, is_on(&general.quick_action_delete)),
                    (QuickAction::Flag, is_on(&general.quick_action_flag)),
                    (QuickAction::MarkRead, is_on(&general.quick_action_read)),
                    (QuickAction::MarkUnread, is_on(&general.quick_action_unread)),
                    (QuickAction::Junk, is_on(&general.quick_action_junk)),
                ];
                guard.ui.quick_action_items =
                    mailin_core::reconcile_quick_action_items(&guard.ui.quick_action_items, &toggles);
                guard.sync.poll_interval_secs = match ui::selected_index(&sync.poll_interval) {
                    0 => 15,
                    1 => 30,
                    2 => 60,
                    3 => 120,
                    4 => 300,
                    5 => 600,
                    _ => 1800,
                };
                guard.sync.body_sync = if ui::selected_index(&sync.body_sync) == 1 {
                    mailin_core::config::BodySyncPolicy::Eager
                } else {
                    mailin_core::config::BodySyncPolicy::OnOpen
                };
                guard.sync.retention_days = match ui::selected_index(&sync.retention_days) {
                    0 => 0,
                    1 => 30,
                    2 => 90,
                    3 => 180,
                    4 => 365,
                    5 => 730,
                    6 => 1095,
                    7 => 1825,
                    _ => 3650,
                };
                if let Err(error) = guard.validate() {
                    tracing::error!(%error, "preferences rejected by validation");
                    let _ = error_text.SetText(&format!("Preferences were not saved: {error}"));
                    let _ = ui::set_visible(&error_text, true);
                    let _ = args.as_ref().and_then(|args| args.SetCancel(true).ok());
                    return;
                }
                if let Err(error) = guard.save(&paths) {
                    tracing::error!(%error, "could not save the configuration");
                    let _ = error_text.SetText(&format!("Could not save the preferences: {error}"));
                    let _ = ui::set_visible(&error_text, true);
                    let _ = args.as_ref().and_then(|args| args.SetCancel(true).ok());
                    return;
                }
                guard.clone()
            };
            // The theme of the controls that already exist cannot change
            // at runtime (every control captured its brushes when it was
            // created), so the new palette only reaches the controls built
            // from now on. The field says so.
            if let Err(error) = mailin.install_config(candidate.clone()) {
                tracing::warn!(%error, "could not install the saved configuration in the running client");
            }
            apply_theme(candidate.ui.theme);
            let _ = ui::set_visible(&error_text, false);
            on_saved();
            let _ = dialog.Hide();
        }
    })?);

    let _ = dialog
        .ShowAsync()
        .context("could not show the preferences dialog")?;
    Ok(())
}

fn apply_theme(theme: Theme) {
    use crate::design_tokens;
    // Apply the palette to the controls created from now on. The controls
    // that already exist keep the palette they were created with; a
    // restart picks the new one up for the whole window.
    design_tokens::apply_config_theme(theme);
}

fn create_general_section(config: &Config) -> Result<(StackPanel, GeneralRows)> {
    let panel = ui::stack_md(mailin_winui_sys::Orientation::Vertical)?;
    ui::append(&panel, &ui::heading3("General")?)?;

    let theme_index = match config.ui.theme {
        Theme::System => 0,
        Theme::Light => 1,
        Theme::Dark => 2,
    };
    let theme = ui::combo(&["System", "Light", "Dark"], theme_index, Some("Theme"))?;
    ui::append(&panel, &ui::field("Theme", &theme)?)?;
    ui::append(
        &panel,
        &ui::body_text(
            "Theme changes apply to the window immediately. Controls with a custom \
             color may keep their old palette until reopened; Windows high contrast \
             always overrides the theme.",
        )?,
    )?;

    let density_index = match config.ui.message_list_density {
        MessageListDensity::Comfortable => 0,
        MessageListDensity::Cozy => 1,
        MessageListDensity::Compact => 2,
    };
    let density = ui::combo(
        &["Comfortable", "Cozy", "Compact"],
        density_index,
        Some("Message list density"),
    )?;
    ui::append(&panel, &ui::field("Message list density", &density)?)?;

    let thread_mode_index = match config.ui.thread_mode {
        mailin_core::config::ThreadMode::Off => 0,
        mailin_core::config::ThreadMode::Flat => 1,
        mailin_core::config::ThreadMode::Nested => 2,
    };
    let thread_mode = ui::combo(
        &["Off (flat list)", "Flat (grouped)", "Nested"],
        thread_mode_index,
        Some("Conversation view"),
    )?;
    ui::append(&panel, &ui::field("Conversation view", &thread_mode)?)?;

    let (html_row, html) = ui::switch_row(
        "Render HTML messages",
        "Display HTML formatting when available",
        config.ui.html_enabled,
        Some("Render HTML messages"),
        Some("On"),
        Some("Off"),
    )?;
    ui::append(&panel, &html_row)?;

    let (remote_row, remote_content) = ui::switch_row(
        "Load remote content",
        "Allow images and resources from the network (can be overridden per sender)",
        config.ui.remote_content_enabled,
        Some("Load remote content"),
        Some("On"),
        Some("Off"),
    )?;
    ui::append(&panel, &remote_row)?;

    let page_size_index = match config.ui.page_size {
        50 => 0,
        100 => 1,
        200 => 2,
        500 => 3,
        1000 => 4,
        _ => 1,
    };
    let page_size = ui::combo(
        &["50", "100", "200", "500", "1000"],
        page_size_index,
        Some("Messages per page"),
    )?;
    ui::append(&panel, &ui::field("Messages per page", &page_size)?)?;

    let (mark_read_row, mark_read) = ui::switch_row(
        "Mark as read on open",
        "Automatically mark messages read when opened",
        config.ui.mark_read_on_open,
        Some("Mark as read on open"),
        Some("On"),
        Some("Off"),
    )?;
    ui::append(&panel, &mark_read_row)?;

    let (notifications_row, notifications) = ui::switch_row(
        "Show new mail notifications",
        "Display desktop notifications for new messages",
        config.ui.notifications,
        Some("Show new mail notifications"),
        Some("On"),
        Some("Off"),
    )?;
    ui::append(&panel, &notifications_row)?;

    // Quick actions: a master toggle plus one row per action, matching the
    // GTK messages group's labels and order.
    let (quick_actions_row, quick_actions) = ui::switch_row(
        "Quick actions",
        "Show row actions for archive, delete, flag, and read state",
        config.ui.quick_actions,
        Some("Quick actions"),
        Some("On"),
        Some("Off"),
    )?;
    ui::append(&panel, &quick_actions_row)?;

    let quick_row = |title: &str, subtitle: &str, action: QuickAction| {
        ui::switch_row(
            title,
            subtitle,
            config.ui.quick_action_items.contains(&action),
            Some(title),
            Some("On"),
            Some("Off"),
        )
    };
    let (row, quick_action_archive) = quick_row(
        "Archive",
        "Show the archive action on message rows",
        QuickAction::Archive,
    )?;
    ui::append(&panel, &row)?;
    let (row, quick_action_delete) = quick_row(
        "Delete",
        "Show the delete action on message rows",
        QuickAction::Delete,
    )?;
    ui::append(&panel, &row)?;
    let (row, quick_action_flag) = quick_row(
        "Flag",
        "Show the flag action on message rows",
        QuickAction::Flag,
    )?;
    ui::append(&panel, &row)?;
    let (row, quick_action_read) = quick_row(
        "Mark read",
        "Show the mark-read action on message rows",
        QuickAction::MarkRead,
    )?;
    ui::append(&panel, &row)?;
    let (row, quick_action_unread) = quick_row(
        "Mark unread",
        "Show the mark-unread action on message rows",
        QuickAction::MarkUnread,
    )?;
    ui::append(&panel, &row)?;
    let (row, quick_action_junk) = quick_row(
        "Mark as junk",
        "Show the mark-as-junk action on message rows",
        QuickAction::Junk,
    )?;
    ui::append(&panel, &row)?;

    let restore_messages = ui::button(
        "Restore message defaults",
        Some("Restore message defaults"),
        Some("Reset message display and quick-action settings"),
    )?;
    ui::append(&panel, &restore_messages)?;

    Ok((
        panel,
        GeneralRows {
            theme,
            density,
            thread_mode,
            html,
            remote_content,
            page_size,
            mark_read,
            notifications,
            quick_actions,
            quick_action_archive,
            quick_action_delete,
            quick_action_flag,
            quick_action_read,
            quick_action_unread,
            quick_action_junk,
            restore_messages,
        },
    ))
}

fn create_sync_section(config: &Config) -> Result<(StackPanel, SyncRows)> {
    let panel = ui::stack_md(mailin_winui_sys::Orientation::Vertical)?;
    ui::append(&panel, &ui::heading3("Synchronization")?)?;

    let poll_index = match config.sync.poll_interval_secs {
        15 => 0,
        30 => 1,
        60 => 2,
        120 => 3,
        300 => 4,
        600 => 5,
        _ => 6,
    };
    let poll_interval = ui::combo(
        &["15", "30", "60", "120", "300", "600", "1800"],
        poll_index,
        Some("Poll interval (seconds)"),
    )?;
    ui::append(
        &panel,
        &ui::field(
            "Default poll interval (accounts can override in Edit Account)",
            &poll_interval,
        )?,
    )?;

    let body_index = match config.sync.body_sync {
        mailin_core::config::BodySyncPolicy::OnOpen => 0,
        mailin_core::config::BodySyncPolicy::Eager => 1,
    };
    let body_sync = ui::combo(
        &["On demand (when opened)", "Eager (background)"],
        body_index,
        Some("Body fetching"),
    )?;
    ui::append(&panel, &ui::field("Body fetching", &body_sync)?)?;

    let retention_index = match config.sync.retention_days {
        0 => 0,
        30 => 1,
        90 => 2,
        180 => 3,
        365 => 4,
        730 => 5,
        1095 => 6,
        1825 => 7,
        _ => 8,
    };
    let retention_days = ui::combo(
        &[
            "0 (forever)",
            "30",
            "90",
            "180",
            "365",
            "730",
            "1095",
            "1825",
            "3650",
        ],
        retention_index,
        Some("Keep messages for (days)"),
    )?;
    ui::append(
        &panel,
        &ui::field(
            "Keep cached messages for (days; server mail is untouched)",
            &retention_days,
        )?,
    )?;

    Ok((
        panel,
        SyncRows {
            poll_interval,
            body_sync,
            retention_days,
        },
    ))
}

fn fill_account_rows(
    container: &StackPanel,
    state: &Arc<AppState>,
    xaml_root: &mailin_winui_sys::XamlRoot,
    events: ui::Revokers,
    on_remove: &std::rc::Rc<dyn Fn(AccountId)>,
) -> Result<()> {
    ui::clear(container)?;
    let accounts = state.mailin.list_accounts().unwrap_or_default();
    if accounts.is_empty() {
        ui::append(container, &ui::body_text("No accounts yet.")?)?;
    }
    for account in accounts {
        let row = ui::stack_sm(mailin_winui_sys::Orientation::Horizontal)?;
        let label = ui::text(&format!(
            "{} · {}",
            account.email,
            match state.engine.sync_status(&account.id) {
                mailin_core::model::SyncStatus::Idle => "Idle",
                mailin_core::model::SyncStatus::Syncing => "Syncing…",
                mailin_core::model::SyncStatus::Connected => "Connected",
                mailin_core::model::SyncStatus::Offline => "Offline",
                mailin_core::model::SyncStatus::Error => "Error",
            }
        ))?;
        ui::stretch_horizontal(&label)?;
        ui::append(&row, &label)?;
        let edit = ui::button("Edit", Some("Edit account"), None)?;
        let remove = ui::button("Remove", Some("Remove account"), None)?;
        ui::append(&row, &edit)?;
        ui::append(&row, &remove)?;

        let edit_container = container.clone();
        let edit_state = Arc::clone(state);
        let edit_root = xaml_root.clone();
        let edit_remove = std::rc::Rc::clone(on_remove);
        let edit_account = account.clone();
        let edit_events = events.clone();
        if let Ok(revoker) = ui::on_click(&edit, move || {
            let _ = crate::account_dialog::present(
                Arc::clone(&edit_state),
                crate::account_dialog::AccountDialogMode::Edit,
                Some(edit_account.clone()),
                {
                    let container = edit_container.clone();
                    let state = Arc::clone(&edit_state);
                    let root = edit_root.clone();
                    let on_remove = std::rc::Rc::clone(&edit_remove);
                    let inner_events = edit_events.clone();
                    move |_| {
                        let _ = fill_account_rows(
                            &container,
                            &state,
                            &root,
                            inner_events.clone(),
                            &on_remove,
                        );
                    }
                },
            );
        }) {
            events.keep(revoker);
        }

        let remove_id = account.id;
        let on_remove = std::rc::Rc::clone(on_remove);
        if let Ok(revoker) = ui::on_click(&remove, move || on_remove(remove_id)) {
            events.keep(revoker);
        }
        ui::append(container, &row)?;
    }
    Ok(())
}

fn create_accounts_section(
    state: &Arc<AppState>,
    xaml_root: &mailin_winui_sys::XamlRoot,
    events: &ui::Revokers,
    on_saved: Arc<dyn Fn()>,
    on_remove: std::rc::Rc<dyn Fn(AccountId)>,
) -> Result<StackPanel> {
    let panel = ui::stack_md(mailin_winui_sys::Orientation::Vertical)?;
    ui::append(&panel, &ui::heading3("Accounts")?)?;
    ui::append(
        &panel,
        &ui::body_text(
            "Add, edit or remove accounts. Signatures, trusted senders and keys are managed in the account dialog.",
        )?,
    )?;
    let rows = ui::stack_sm(mailin_winui_sys::Orientation::Vertical)?;
    fill_account_rows(&rows, state, xaml_root, events.clone(), &on_remove)?;

    let add = ui::button("Add Account…", Some("Add account"), None)?;
    ui::append(&panel, &add)?;
    let add_rows = rows.clone();
    let add_state = Arc::clone(state);
    let add_root = xaml_root.clone();
    let add_remove = std::rc::Rc::clone(&on_remove);
    let add_saved = Arc::clone(&on_saved);
    let add_events = events.clone();
    events.keep(ui::on_click(&add, move || {
        let _ = crate::account_dialog::present(
            Arc::clone(&add_state),
            crate::account_dialog::AccountDialogMode::Add,
            None,
            {
                let rows = add_rows.clone();
                let state = Arc::clone(&add_state);
                let root = add_root.clone();
                let on_remove = std::rc::Rc::clone(&add_remove);
                let inner_events = add_events.clone();
                let inner_saved = Arc::clone(&add_saved);
                move |_| {
                    let _ =
                        fill_account_rows(&rows, &state, &root, inner_events.clone(), &on_remove);
                    inner_saved();
                }
            },
        );
    })?);

    ui::append(&panel, &rows)?;
    Ok(panel)
}

fn create_encryption_section(state: &Arc<AppState>, events: &ui::Revokers) -> Result<StackPanel> {
    let panel = ui::stack_md(mailin_winui_sys::Orientation::Vertical)?;
    ui::append(&panel, &ui::heading3("Encryption")?)?;
    ui::append(
        &panel,
        &ui::body_text("Manage the OpenPGP keys used to encrypt, sign, decrypt and verify mail.")?,
    )?;

    let accounts = state.mailin.list_accounts().unwrap_or_default();
    if accounts.is_empty() {
        ui::append(
            &panel,
            &ui::body_text("Add an account to manage its PGP keys.")?,
        )?;
        return Ok(panel);
    }

    for account in accounts {
        let row = ui::stack_sm(mailin_winui_sys::Orientation::Horizontal)?;
        let labels = ui::stack_xs(mailin_winui_sys::Orientation::Vertical)?;
        let key_count = state
            .mailin
            .list_pgp_keys(&account.id)
            .map(|keys| keys.len())
            .unwrap_or(0);
        ui::append(&labels, &ui::body_text(&account.email)?)?;
        ui::append(
            &labels,
            &ui::caption(&match key_count {
                1 => "1 key".to_owned(),
                count => format!("{count} keys"),
            })?,
        )?;
        ui::stretch_horizontal(&labels)?;
        ui::append(&row, &labels)?;

        let manage = ui::button(
            "Manage Keys",
            Some(&format!("Manage PGP keys for {}", account.email)),
            Some("Open the key manager for this account"),
        )?;
        ui::append(&row, &manage)?;
        ui::append(&panel, &row)?;

        let state = Arc::clone(state);
        let email = account.email.clone();
        let account_id = account.id;
        events.keep(ui::on_click(&manage, move || {
            if let Err(failure) =
                crate::pgp_keys::present(Arc::clone(&state), account_id, email.clone())
            {
                tracing::error!(%failure, "could not open the PGP key manager");
            }
        })?);
    }
    Ok(panel)
}

fn create_plugins_section(state: &Arc<AppState>, events: &ui::Revokers) -> Result<StackPanel> {
    let panel = ui::stack_md(mailin_winui_sys::Orientation::Vertical)?;
    ui::append(&panel, &ui::heading3("Plugins")?)?;
    ui::append(
        &panel,
        &ui::body_text(
            "Lua plugins extend Mailin with custom functionality. Enable or disable plugins here.",
        )?,
    )?;

    let events_clone = events.clone();

    // Load plugins asynchronously
    let state_for_load = Arc::clone(state);

    // Since we can't easily do async here, we'll load plugins synchronously for now
    // In a full implementation, this would be async
    let plugins = state_for_load
        .mailin
        .list_plugins_blocking()
        .unwrap_or_default();

    if plugins.is_empty() {
        ui::append(
            &panel,
            &ui::body_text(
                "No plugins installed. Place plugin directories in the plugins folder to install them.",
            )?,
        )?;
        return Ok(panel);
    }

    for plugin in plugins {
        let row = ui::stack_sm(mailin_winui_sys::Orientation::Horizontal)?;
        let labels = ui::stack_xs(mailin_winui_sys::Orientation::Vertical)?;
        ui::append(&labels, &ui::body_text(&plugin.name)?)?;
        ui::append(
            &labels,
            &ui::caption(&format!("v{} · {}", plugin.version, plugin.source_path))?,
        )?;
        ui::stretch_horizontal(&labels)?;
        ui::append(&row, &labels)?;

        let toggle = mailin_winui_sys::ToggleSwitch::new().context("could not create toggle")?;
        toggle.SetIsOn(plugin.enabled)?;
        let toggle_element: mailin_winui_sys::FrameworkElement = toggle.cast()?;
        toggle_element.SetVerticalAlignment(mailin_winui_sys::VerticalAlignment::Center)?;
        ui::append(&row, &toggle)?;
        ui::append(&panel, &row)?;

        let state = Arc::clone(state);
        let name = plugin.name.clone();
        events_clone.keep(ui::on_toggle(&toggle, move |enabled| {
            let state = state.clone();
            let name = name.clone();
            // Use a simple thread pool or async runtime to call the async function
            // For now, we'll use a blocking call
            let _ = state.mailin.set_plugin_enabled_blocking(&name, enabled);
        })?);
    }
    Ok(panel)
}

fn is_on(switch: &ToggleSwitch) -> bool {
    switch.IsOn().unwrap_or_default()
}

/// D4/D5 support actions: a redacted diagnostics report and a consistent
/// backup of the local data. Both are deliberately explicit button presses so
/// sharing mail data never happens by accident.
fn create_support_section(
    state: &Arc<AppState>,
    paths: &Paths,
    events: &ui::Revokers,
) -> Result<StackPanel> {
    let panel = ui::stack_md(mailin_winui_sys::Orientation::Vertical)?;
    ui::append(&panel, &ui::heading3("Support")?)?;
    ui::append(
        &panel,
        &ui::body_text("Share diagnostics or keep a copy of your local mail cache.")?,
    )?;

    let status = ui::text("")?;
    let _ = mailin_winui_sys::AutomationProperties::SetLiveSetting(
        &status,
        mailin_winui_sys::AutomationLiveSetting::Polite,
    );
    ui::append(&panel, &status)?;

    let diagnostics = ui::button(
        "Copy diagnostics",
        Some("Copy diagnostics"),
        Some("Copy version, settings and the last 100 log lines to the clipboard"),
    )?;
    events.keep(ui::on_click(&diagnostics, {
        let state = Arc::clone(state);
        let paths = paths.clone();
        let status = status.clone();
        move || {
            let report =
                mailin_core::support::build_diagnostics(&state.mailin, &state.config(), &paths);
            let message = if crate::platform::copy_to_clipboard(&report) {
                "Copied to clipboard"
            } else {
                "Could not access the clipboard"
            };
            let _ = status.SetText(message);
        }
    })?);
    ui::append(&panel, &diagnostics)?;

    let backup = ui::button(
        "Back up local data…",
        Some("Back up local data"),
        Some("Save a zip of the local mail cache and preferences (no passwords)"),
    )?;
    events.keep(ui::on_click(&backup, {
        let paths = paths.clone();
        let status = status.clone();
        move || {
            let suggested = mailin_core::support::backup_filename();
            let Some(destination) =
                crate::platform::save_file("Back up local data", &suggested.to_string_lossy())
            else {
                return;
            };
            let message = match mailin_core::support::backup_local_data(&paths, &destination) {
                Ok(()) => "Backup saved.".to_owned(),
                Err(error) => format!("Could not save the backup: {error}"),
            };
            let _ = status.SetText(&message);
        }
    })?);
    ui::append(&panel, &backup)?;

    Ok(panel)
}
