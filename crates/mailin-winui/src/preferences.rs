//! Preferences dialog for the WinUI front-end.
//!
//! The dialog is a `ContentDialog` over the main window: sections are composed
//! from the shared `ui` helpers and the widgets are read back when the primary
//! button saves the configuration to disk.

use std::sync::Arc;

use anyhow::{Context, Result};
use mailin_core::{Config, Paths, Theme};
use mailin_winui_sys::{ComboBox, ContentDialog, ContentDialogButton, StackPanel, ToggleSwitch};
use windows_core::Interface;

use crate::app::AppState;
use crate::ui;

/// The widgets whose values are written back into the configuration.
#[derive(Clone)]
struct GeneralRows {
    theme: ComboBox,
    html: ToggleSwitch,
    remote_content: ToggleSwitch,
    page_size: ComboBox,
    mark_read: ToggleSwitch,
    notifications: ToggleSwitch,
}

#[derive(Clone)]
struct SyncRows {
    poll_interval: ComboBox,
    parallel_accounts: ComboBox,
    body_sync: ComboBox,
    retention_days: ComboBox,
}

/// Show the preferences dialog over `xaml_root` (the main window's root).
///
/// `events` keeps the dialog's button revokers alive for the lifetime of the
/// main window; dropping them early would unregister the save handler while
/// the dialog is still open.
pub fn show_preferences(
    state: &Arc<AppState>,
    xaml_root: &mailin_winui_sys::XamlRoot,
    events: &ui::Revokers,
) -> Result<()> {
    let config = state.config.clone();
    let paths = Paths::discover().context("could not locate the configuration directory")?;

    let dialog = ContentDialog::new().context("could not create the preferences dialog")?;
    let title: windows_core::IInspectable = ui::text("Preferences")?.cast()?;
    dialog
        .SetTitle(&title)
        .context("could not title the preferences dialog")?;

    let main_panel = ui::stack(mailin_winui_sys::Orientation::Vertical, 16.0)?;
    ui::margin(&main_panel, 24.0, 16.0, 24.0, 16.0)?;

    let (general_panel, general) = create_general_section(&config)?;
    ui::append(&main_panel, &general_panel)?;
    let (sync_panel, sync) = create_sync_section(&config)?;
    ui::append(&main_panel, &sync_panel)?;
    let accounts_panel = create_accounts_section()?;
    ui::append(&main_panel, &accounts_panel)?;

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

    events.keep(dialog.PrimaryButtonClick({
        let dialog = dialog.clone();
        let paths = paths.clone();
        move |_, _| {
            let mut config = config.clone();
            config.ui.theme = match ui::selected_index(&general.theme) {
                1 => Theme::Light,
                2 => Theme::Dark,
                _ => Theme::System,
            };
            config.ui.html_enabled = is_on(&general.html);
            config.ui.remote_content_enabled = is_on(&general.remote_content);
            config.ui.page_size = match ui::selected_index(&general.page_size) {
                0 => 50,
                1 => 100,
                2 => 200,
                3 => 500,
                _ => 1000,
            };
            config.ui.mark_read_on_open = is_on(&general.mark_read);
            config.ui.notifications = is_on(&general.notifications);
            config.sync.poll_interval_secs = match ui::selected_index(&sync.poll_interval) {
                0 => 15,
                1 => 30,
                2 => 60,
                3 => 120,
                4 => 300,
                5 => 600,
                _ => 1800,
            };
            config.sync.max_parallel_accounts =
                (ui::selected_index(&sync.parallel_accounts) + 1) as u8;
            config.sync.body_sync = if ui::selected_index(&sync.body_sync) == 1 {
                mailin_core::config::BodySyncPolicy::Eager
            } else {
                mailin_core::config::BodySyncPolicy::OnOpen
            };
            config.sync.retention_days = match ui::selected_index(&sync.retention_days) {
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
            save_config(&config, &paths);
            let _ = dialog.Hide();
        }
    })?);

    let _ = dialog
        .ShowAsync()
        .context("could not show the preferences dialog")?;
    Ok(())
}

fn create_general_section(config: &Config) -> Result<(StackPanel, GeneralRows)> {
    let panel = ui::stack(mailin_winui_sys::Orientation::Vertical, 12.0)?;
    ui::append(&panel, &ui::heading("General", Some(18.0), Some(600))?)?;

    let theme_index = match config.ui.theme {
        Theme::System => 0,
        Theme::Light => 1,
        Theme::Dark => 2,
    };
    let theme = ui::combo(&["System", "Light", "Dark"], theme_index)?;
    ui::append(&panel, &ui::field("Theme", &theme)?)?;

    let (html_row, html) = ui::switch_row(
        "Render HTML messages",
        "Display HTML formatting when available",
        config.ui.html_enabled,
    )?;
    ui::append(&panel, &html_row)?;

    let (remote_row, remote_content) = ui::switch_row(
        "Load remote content",
        "Allow images and resources from the network (can be overridden per account)",
        config.ui.remote_content_enabled,
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
    let page_size = ui::combo(&["50", "100", "200", "500", "1000"], page_size_index)?;
    ui::append(&panel, &ui::field("Messages per page", &page_size)?)?;

    let (mark_read_row, mark_read) = ui::switch_row(
        "Mark as read on open",
        "Automatically mark messages read when opened",
        config.ui.mark_read_on_open,
    )?;
    ui::append(&panel, &mark_read_row)?;

    let (notifications_row, notifications) = ui::switch_row(
        "Show new mail notifications",
        "Display desktop notifications for new messages",
        config.ui.notifications,
    )?;
    ui::append(&panel, &notifications_row)?;

    Ok((
        panel,
        GeneralRows {
            theme,
            html,
            remote_content,
            page_size,
            mark_read,
            notifications,
        },
    ))
}

fn create_sync_section(config: &Config) -> Result<(StackPanel, SyncRows)> {
    let panel = ui::stack(mailin_winui_sys::Orientation::Vertical, 12.0)?;
    ui::append(
        &panel,
        &ui::heading("Synchronization", Some(18.0), Some(600))?,
    )?;

    let poll_index = match config.sync.poll_interval_secs {
        15 => 0,
        30 => 1,
        60 => 2,
        120 => 3,
        300 => 4,
        600 => 5,
        _ => 6,
    };
    let poll_interval = ui::combo(&["15", "30", "60", "120", "300", "600", "1800"], poll_index)?;
    ui::append(
        &panel,
        &ui::field("Poll interval (seconds)", &poll_interval)?,
    )?;

    let parallel_labels = [
        "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
    ];
    let parallel_index = (config.sync.max_parallel_accounts.max(1) - 1).min(15) as usize;
    let parallel_accounts = ui::combo(&parallel_labels, parallel_index)?;
    ui::append(&panel, &ui::field("Parallel accounts", &parallel_accounts)?)?;

    let body_index = match config.sync.body_sync {
        mailin_core::config::BodySyncPolicy::OnOpen => 0,
        mailin_core::config::BodySyncPolicy::Eager => 1,
    };
    let body_sync = ui::combo(
        &["On demand (when opened)", "Eager (background)"],
        body_index,
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
    )?;
    ui::append(
        &panel,
        &ui::field("Keep messages for (days)", &retention_days)?,
    )?;

    Ok((
        panel,
        SyncRows {
            poll_interval,
            parallel_accounts,
            body_sync,
            retention_days,
        },
    ))
}

fn create_accounts_section() -> Result<StackPanel> {
    let panel = ui::stack(mailin_winui_sys::Orientation::Vertical, 12.0)?;
    ui::append(&panel, &ui::heading("Accounts", Some(18.0), Some(600))?)?;
    let desc = ui::text(
        "Account-specific settings like remote content loading are managed in the account wizard.",
    )?;
    ui::append(&panel, &desc)?;
    Ok(panel)
}

fn is_on(switch: &ToggleSwitch) -> bool {
    switch.IsOn().unwrap_or_default()
}

fn save_config(config: &Config, paths: &Paths) {
    if let Err(error) = config.validate() {
        tracing::warn!(%error, "refusing to save an invalid configuration");
        return;
    }
    if let Err(error) = config.save(paths) {
        tracing::warn!(%error, "could not save the configuration");
    }
}
