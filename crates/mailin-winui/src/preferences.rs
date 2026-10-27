//! Preferences dialog for the WinUI front-end.

use std::sync::Arc;

use mailin_core::{Config, Paths, Theme};
use mailin_winui_sys::{
    ComboBox, ComboBoxItem, ContentDialog, StackPanel, TextBlock, ToggleSwitch, XamlRoot,
};
use windows_core::Result;

use crate::{AppState, ui};

/// Show the preferences dialog.
pub fn show_preferences(state: &Arc<AppState>, xaml_root: &XamlRoot) -> Result<()> {
    let config = state.config.clone();
    let paths = Paths::discover().expect("paths");

    // Create the dialog
    let dialog = ContentDialog::new()?;
    dialog.SetTitle("Preferences")?;

    // Create main content panel
    let main_panel = StackPanel::new()?;
    main_panel.SetSpacing(16)?;
    main_panel.SetMargin(ui::thickness(24, 24, 24, 24))?;

    // General section
    let general_panel = create_general_section(&config)?;
    main_panel.Children()?.Append(&general_panel)?;

    // Sync section
    let sync_panel = create_sync_section(&config)?;
    main_panel.Children()?.Append(&sync_panel)?;

    // Accounts section
    let accounts_panel = create_accounts_section()?;
    main_panel.Children()?.Append(&accounts_panel)?;

    dialog.SetContent(&main_panel)?;
    dialog.SetPrimaryButtonText("Close")?;
    dialog.SetDefaultButton(mailin_winui_sys::ContentDialogButton::Primary)?;
    dialog.SetXamlRoot(xaml_root)?;

    // Show dialog and handle close
    let config_clone = config.clone();
    let paths_clone = paths.clone();
    dialog.PrimaryButtonClick({
        let dialog = dialog.clone();
        move |_, _| {
            save_config(&config_clone, &paths_clone);
            let _ = dialog.Hide();
        }
    })?;

    dialog.ShowAsync()?;
    Ok(())
}

fn create_general_section(config: &Config) -> Result<StackPanel> {
    let panel = StackPanel::new()?;
    panel.SetSpacing(12)?;

    // Section header
    let header = TextBlock::new()?;
    header.SetText("General")?;
    header.SetStyle(ui::style("TitleTextBlockStyle"))?;
    panel.Children()?.Append(&header)?;

    // Theme
    let theme_panel = StackPanel::new()?;
    theme_panel.SetOrientation(mailin_winui_sys::Orientation::Horizontal)?;
    theme_panel.SetSpacing(12)?;

    let theme_label = TextBlock::new()?;
    theme_label.SetText("Theme")?;
    theme_label.SetVerticalAlignment(mailin_winui_sys::VerticalAlignment::Center)?;
    theme_label.SetWidth(200.0)?;
    theme_panel.Children()?.Append(&theme_label)?;

    let theme_combo = ComboBox::new()?;
    theme_combo
        .Items()?
        .Append(&ComboBoxItem::new()?.SetContent("System")?)?;
    theme_combo
        .Items()?
        .Append(&ComboBoxItem::new()?.SetContent("Light")?)?;
    theme_combo
        .Items()?
        .Append(&ComboBoxItem::new()?.SetContent("Dark")?)?;
    match config.ui.theme {
        Theme::System => theme_combo.SetSelectedIndex(0)?,
        Theme::Light => theme_combo.SetSelectedIndex(1)?,
        Theme::Dark => theme_combo.SetSelectedIndex(2)?,
    }
    theme_combo.SetWidth(200.0)?;
    theme_panel.Children()?.Append(&theme_combo)?;

    panel.Children()?.Append(&theme_panel)?;

    // HTML rendering
    let html_panel = create_switch_row(
        "Render HTML messages",
        "Display HTML formatting when available",
        config.ui.html_enabled,
    )?;
    panel.Children()?.Append(&html_panel)?;

    // Remote content
    let remote_panel = create_switch_row(
        "Load remote content",
        "Allow images and resources from the network (can be overridden per-account)",
        config.ui.remote_content_enabled,
    )?;
    panel.Children()?.Append(&remote_panel)?;

    // Page size
    let page_size_panel = StackPanel::new()?;
    page_size_panel.SetOrientation(mailin_winui_sys::Orientation::Horizontal)?;
    page_size_panel.SetSpacing(12)?;

    let page_size_label = TextBlock::new()?;
    page_size_label.SetText("Messages per page")?;
    page_size_label.SetVerticalAlignment(mailin_winui_sys::VerticalAlignment::Center)?;
    page_size_label.SetWidth(200.0)?;
    page_size_panel.Children()?.Append(&page_size_label)?;

    // For simplicity, use a ComboBox for page size
    let page_size_combo = ComboBox::new()?;
    for size in [50, 100, 200, 500, 1000] {
        page_size_combo
            .Items()?
            .Append(&ComboBoxItem::new()?.SetContent(size.to_string())?)?;
    }
    let current_page_size = config.ui.page_size;
    let selected_index = match current_page_size {
        50 => 0,
        100 => 1,
        200 => 2,
        500 => 3,
        1000 => 4,
        _ => 1,
    };
    page_size_combo.SetSelectedIndex(selected_index)?;
    page_size_combo.SetWidth(200.0)?;
    page_size_panel.Children()?.Append(&page_size_combo)?;

    panel.Children()?.Append(&page_size_panel)?;

    // Mark as read
    let mark_read_panel = create_switch_row(
        "Mark as read on open",
        "Automatically mark messages read when opened",
        config.ui.mark_read_on_open,
    )?;
    panel.Children()?.Append(&mark_read_panel)?;

    // Notifications
    let notifications_panel = create_switch_row(
        "Show new mail notifications",
        "Display desktop notifications for new messages",
        config.ui.notifications,
    )?;
    panel.Children()?.Append(&notifications_panel)?;

    Ok(panel)
}

fn create_sync_section(config: &Config) -> Result<StackPanel> {
    let panel = StackPanel::new()?;
    panel.SetSpacing(12)?;

    // Section header
    let header = TextBlock::new()?;
    header.SetText("Synchronization")?;
    header.SetStyle(ui::style("TitleTextBlockStyle"))?;
    panel.Children()?.Append(&header)?;

    // Poll interval
    let poll_panel = StackPanel::new()?;
    poll_panel.SetOrientation(mailin_winui_sys::Orientation::Horizontal)?;
    poll_panel.SetSpacing(12)?;

    let poll_label = TextBlock::new()?;
    poll_label.SetText("Poll interval (seconds)")?;
    poll_label.SetVerticalAlignment(mailin_winui_sys::VerticalAlignment::Center)?;
    poll_label.SetWidth(200.0)?;
    poll_panel.Children()?.Append(&poll_label)?;

    let poll_combo = ComboBox::new()?;
    for interval in [15, 30, 60, 120, 300, 600, 1800] {
        poll_combo
            .Items()?
            .Append(&ComboBoxItem::new()?.SetContent(interval.to_string())?)?;
    }
    let current_poll = config.sync.poll_interval_secs;
    let selected_index = match current_poll {
        15 => 0,
        30 => 1,
        60 => 2,
        120 => 3,
        300 => 4,
        600 => 5,
        1800 => 6,
        _ => 2,
    };
    poll_combo.SetSelectedIndex(selected_index)?;
    poll_combo.SetWidth(200.0)?;
    poll_panel.Children()?.Append(&poll_combo)?;

    panel.Children()?.Append(&poll_panel)?;

    // Max parallel accounts
    let parallel_panel = StackPanel::new()?;
    parallel_panel.SetOrientation(mailin_winui_sys::Orientation::Horizontal)?;
    parallel_panel.SetSpacing(12)?;

    let parallel_label = TextBlock::new()?;
    parallel_label.SetText("Parallel accounts")?;
    parallel_label.SetVerticalAlignment(mailin_winui_sys::VerticalAlignment::Center)?;
    parallel_label.SetWidth(200.0)?;
    parallel_panel.Children()?.Append(&parallel_label)?;

    let parallel_combo = ComboBox::new()?;
    for i in 1..=16 {
        parallel_combo
            .Items()?
            .Append(&ComboBoxItem::new()?.SetContent(i.to_string())?)?;
    }
    parallel_combo.SetSelectedIndex((config.sync.max_parallel_accounts as i32 - 1) as u32)?;
    parallel_combo.SetWidth(200.0)?;
    parallel_panel.Children()?.Append(&parallel_combo)?;

    panel.Children()?.Append(&parallel_panel)?;

    // Body sync policy
    let body_panel = StackPanel::new()?;
    body_panel.SetOrientation(mailin_winui_sys::Orientation::Horizontal)?;
    body_panel.SetSpacing(12)?;

    let body_label = TextBlock::new()?;
    body_label.SetText("Body fetching")?;
    body_label.SetVerticalAlignment(mailin_winui_sys::VerticalAlignment::Center)?;
    body_label.SetWidth(200.0)?;
    body_panel.Children()?.Append(&body_label)?;

    let body_combo = ComboBox::new()?;
    body_combo
        .Items()?
        .Append(&ComboBoxItem::new()?.SetContent("On demand (when opened)")?)?;
    body_combo
        .Items()?
        .Append(&ComboBoxItem::new()?.SetContent("Eager (background)")?)?;
    match config.sync.body_sync {
        mailin_core::config::BodySyncPolicy::OnOpen => body_combo.SetSelectedIndex(0)?,
        mailin_core::config::BodySyncPolicy::Eager => body_combo.SetSelectedIndex(1)?,
    }
    body_combo.SetWidth(300.0)?;
    body_panel.Children()?.Append(&body_combo)?;

    panel.Children()?.Append(&body_panel)?;

    // Retention
    let retention_panel = StackPanel::new()?;
    retention_panel.SetOrientation(mailin_winui_sys::Orientation::Horizontal)?;
    retention_panel.SetSpacing(12)?;

    let retention_label = TextBlock::new()?;
    retention_label.SetText("Keep messages for (days, 0 = forever)")?;
    retention_label.SetVerticalAlignment(mailin_winui_sys::VerticalAlignment::Center)?;
    retention_label.SetWidth(200.0)?;
    retention_panel.Children()?.Append(&retention_label)?;

    let retention_combo = ComboBox::new()?;
    for days in [0, 30, 90, 180, 365, 730, 1095, 1825, 3650] {
        retention_combo
            .Items()?
            .Append(&ComboBoxItem::new()?.SetContent(days.to_string())?)?;
    }
    let current_retention = config.sync.retention_days;
    let selected_index = match current_retention {
        0 => 0,
        30 => 1,
        90 => 2,
        180 => 3,
        365 => 4,
        730 => 5,
        1095 => 6,
        1825 => 7,
        3650 => 8,
        _ => 0,
    };
    retention_combo.SetSelectedIndex(selected_index)?;
    retention_combo.SetWidth(200.0)?;
    retention_panel.Children()?.Append(&retention_combo)?;

    panel.Children()?.Append(&retention_panel)?;

    Ok(panel)
}

fn create_accounts_section() -> Result<StackPanel> {
    let panel = StackPanel::new()?;
    panel.SetSpacing(12)?;

    let header = TextBlock::new()?;
    header.SetText("Accounts")?;
    header.SetStyle(ui::style("TitleTextBlockStyle"))?;
    panel.Children()?.Append(&header)?;

    let desc = TextBlock::new()?;
    desc.SetText("Account-specific settings like remote content loading are managed in the account settings dialog.")?;
    desc.SetTextWrapping(mailin_winui_sys::TextWrapping::WrapWholeWords)?;
    desc.SetOpacity(0.7)?;
    panel.Children()?.Append(&desc)?;

    Ok(panel)
}

fn create_switch_row(title: &str, subtitle: &str, active: bool) -> Result<StackPanel> {
    let panel = StackPanel::new()?;
    panel.SetOrientation(mailin_winui_sys::Orientation::Horizontal)?;
    panel.SetSpacing(12)?;

    let text_panel = StackPanel::new()?;
    text_panel.SetSpacing(2)?;

    let title_block = TextBlock::new()?;
    title_block.SetText(title)?;
    title_block.SetFontWeight(mailin_winui_sys::FontWeights::SemiBold?)?;
    text_panel.Children()?.Append(&title_block)?;

    let subtitle_block = TextBlock::new()?;
    subtitle_block.SetText(subtitle)?;
    subtitle_block.SetOpacity(0.7)?;
    subtitle_block.SetFontSize(12.0)?;
    text_panel.Children()?.Append(&subtitle_block)?;

    panel.Children()?.Append(&text_panel)?;

    let switch = ToggleSwitch::new()?;
    switch.SetIsOn(active)?;
    switch.SetVerticalAlignment(mailin_winui_sys::VerticalAlignment::Center)?;
    panel.Children()?.Append(&switch)?;

    Ok(panel)
}

fn save_config(config: &Config, paths: &Paths) {
    if let Err(e) = config.validate() {
        eprintln!("Config validation failed: {e}");
        return;
    }

    if let Err(e) = config.save(paths) {
        eprintln!("Failed to save config: {e}");
    }

    // Note: Theme change requires app restart in WinUI for now
    // crate::apply_theme(config.ui.theme); // Would need to implement
}
