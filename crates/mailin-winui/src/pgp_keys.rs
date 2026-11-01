//! PGP key manager: list, import, generate and delete keys for an account.

use std::sync::Arc;

use anyhow::{Context, Result};
use mailin_core::AccountId;
use mailin_winui_sys::{
    ContentDialog, ContentDialogButton, Grid, Orientation, StackPanel, TextBlock, TextWrapping,
    Window,
};
use windows_core::Interface;

use crate::app::AppState;
use crate::ui;

/// Show the key manager for `account`.
pub fn present(app: Arc<AppState>, account: AccountId, account_email: String) -> Result<Window> {
    let root = Grid::new().context("could not create the key manager layout")?;
    ui::auto_row(&root)?;
    ui::star_row(&root)?;

    let header = ui::stack_sm(Orientation::Horizontal)?;
    ui::margin_horizontal(&header, 16.0)?;
    ui::margin_vertical(&header, 8.0)?;
    let title = ui::heading2(&format!("PGP Keys — {account_email}"))?;
    ui::stretch_horizontal(&title)?;
    ui::append(&header, &title)?;
    let import = ui::button(
        "Import",
        Some("Import key"),
        Some("Import an armored public or secret key"),
    )?;
    let generate = ui::button(
        "Generate",
        Some("Generate key"),
        Some("Generate a new key pair for this account"),
    )?;
    ui::append(&header, &import)?;
    ui::append(&header, &generate)?;
    ui::set_row(&header, 0)?;
    ui::append(&root, &header)?;

    let error: TextBlock = ui::text("")?;
    ui::margin_horizontal(&error, 16.0)?;
    ui::set_visible(&error, false)?;
    ui::append(&root, &error)?;

    let list = ui::stack_xs(Orientation::Vertical)?;
    ui::margin_lg(&list)?;
    let scroller = ui::scroll(&list)?;
    ui::set_row(&scroller, 1)?;
    ui::append(&root, &scroller)?;

    let window = Window::new().context("could not create the key manager window")?;
    window.SetTitle("PGP Keys")?;
    window.SetContent(&root)?;

    let events = ui::Revokers::default();

    populate(&list, &app, account, &window, &error, &events)?;

    {
        let app = Arc::clone(&app);
        let list = list.clone();
        let window = window.clone();
        let error = error.clone();
        let events = events.clone();
        let email = account_email.clone();
        events.keep(ui::on_click(&import, move || {
            if let Err(failure) =
                import_dialog(&app, account, &email, &list, &window, &error, &events)
            {
                tracing::error!(%failure, "could not open the PGP import dialog");
            }
        })?);
    }
    {
        let app = Arc::clone(&app);
        let list = list.clone();
        let window = window.clone();
        let error = error.clone();
        let events = events.clone();
        let email = account_email.clone();
        events.keep(ui::on_click(&generate, move || {
            if let Err(failure) =
                generate_dialog(&app, account, &email, &list, &window, &error, &events)
            {
                tracing::error!(%failure, "could not open the PGP generate dialog");
            }
        })?);
    }

    // Keep the revokers alive for as long as the window is open.
    let token = ui::keep_alive(&window);
    let cleanup = events.clone();
    let closed = window.Closed(move |_, _| {
        cleanup.clear();
        ui::release(token);
    })?;
    events.keep(closed);

    window.Activate()?;
    Ok(window)
}

fn populate(
    list: &StackPanel,
    app: &Arc<AppState>,
    account: AccountId,
    window: &Window,
    error: &TextBlock,
    events: &ui::Revokers,
) -> Result<()> {
    ui::clear(list)?;

    let keys = match app.mailin.list_pgp_keys(&account) {
        Ok(keys) => keys,
        Err(failure) => {
            let _ = error.SetText(&format!("Could not load keys: {failure}"));
            let _ = ui::set_visible(error, true);
            return Ok(());
        }
    };

    if keys.is_empty() {
        ui::append(
            list,
            &ui::body_text("No keys yet. Import a key or generate a new pair to use PGP.")?,
        )?;
        return Ok(());
    }

    for key in keys {
        let has_secret = app.mailin.has_pgp_secret(&account, &key.fingerprint);
        let row = ui::stack_sm(Orientation::Horizontal)?;
        let labels = ui::stack_xs(Orientation::Vertical)?;
        let title = if key.user_ids.is_empty() {
            key.fingerprint.clone()
        } else {
            key.user_ids.join(", ")
        };
        ui::append(&labels, &ui::body_text(&title)?)?;
        let subtitle = if has_secret {
            format!("{} · secret key available", key.fingerprint)
        } else {
            key.fingerprint.clone()
        };
        let subtitle_block = ui::caption(&subtitle)?;
        ui::append(&labels, &subtitle_block)?;
        ui::stretch_horizontal(&labels)?;
        ui::append(&row, &labels)?;

        let delete = ui::destructive_button(
            "Delete",
            Some(&format!("Delete key {}", key.fingerprint)),
            Some("Remove this key from the account"),
        )?;
        ui::append(&row, &delete)?;
        ui::append(list, &row)?;

        let app = Arc::clone(app);
        let list = list.clone();
        let window = window.clone();
        let error = error.clone();
        let events = events.clone();
        let fingerprint = key.fingerprint.clone();
        events.keep(ui::on_click(&delete, move || {
            match app.mailin.delete_pgp_key(&account, &fingerprint) {
                Ok(_) => {
                    let _ = ui::set_visible(&error, false);
                    let _ = populate(&list, &app, account, &window, &error, &events);
                }
                Err(failure) => {
                    let _ = error.SetText(&format!("Could not delete the key: {failure}"));
                    let _ = ui::set_visible(&error, true);
                }
            }
        })?);
    }
    Ok(())
}

fn import_dialog(
    app: &Arc<AppState>,
    account: AccountId,
    email: &str,
    list: &StackPanel,
    window: &Window,
    error: &TextBlock,
    events: &ui::Revokers,
) -> Result<()> {
    let dialog = ContentDialog::new().context("could not create the import dialog")?;
    let title_block = ui::text("Import PGP key")?;
    title_block.SetText(&format!("Import PGP key — {email}"))?;
    let title: windows_core::IInspectable = title_block.cast()?;
    dialog.SetTitle(&title)?;
    let view = ui::text_box("Paste an armored public or secret key block", None)?;
    view.SetAcceptsReturn(true)?;
    view.SetTextWrapping(TextWrapping::Wrap)?;
    ui::min_height(&view, 200.0)?;
    let panel = ui::stack_md(Orientation::Vertical)?;
    ui::append(
        &panel,
        &ui::body_text("Paste an armored public or secret key block.")?,
    )?;
    ui::append(&panel, &view)?;
    ui::set_content(&dialog, &panel)?;
    dialog.SetPrimaryButtonText("Import")?;
    dialog.SetCloseButtonText("Cancel")?;
    dialog.SetDefaultButton(ContentDialogButton::Primary)?;
    let element: mailin_winui_sys::IUIElement = dialog.cast()?;
    element.SetXamlRoot(ui::window_xaml_root(window)?)?;

    let app = Arc::clone(app);
    let list = list.clone();
    let window = window.clone();
    let error = error.clone();
    let events = events.clone();
    events.keep(dialog.PrimaryButtonClick(move |dialog, _| {
        let armored = ui::text_value(&view);
        if armored.trim().is_empty() {
            let _ = error.SetText("No key data provided");
            let _ = ui::set_visible(&error, true);
            return Ok(());
        }
        let result = if armored.contains("PRIVATE KEY BLOCK") {
            app.mailin.import_pgp_secret_key(&account, &armored)
        } else {
            app.mailin.import_pgp_public_key(&account, &armored)
        };
        match result {
            Ok(key) => {
                let _ = error.SetText(&format!("Imported key {}", key.fingerprint));
                let _ = ui::set_visible(&error, true);
                let _ = populate(&list, &app, account, &window, &error, &events);
            }
            Err(failure) => {
                let _ = error.SetText(&format!("Could not import the key: {failure}"));
                let _ = ui::set_visible(&error, true);
            }
        }
        let _ = dialog.Hide();
        Ok(())
    })?);

    let _ = dialog.ShowAsync()?;
    Ok(())
}

fn generate_dialog(
    app: &Arc<AppState>,
    account: AccountId,
    email: &str,
    list: &StackPanel,
    window: &Window,
    error: &TextBlock,
    events: &ui::Revokers,
) -> Result<()> {
    let dialog = ContentDialog::new().context("could not create the generate dialog")?;
    let title: windows_core::IInspectable = ui::text("Generate key pair")?.cast()?;
    dialog.SetTitle(&title)?;
    let entry = ui::text_box("Mailin user <you@example.com>", Some("User ID"))?;
    entry.SetText(&format!("Mailin user <{email}>"))?;
    let panel = ui::stack_md(Orientation::Vertical)?;
    ui::append(
        &panel,
        &ui::body_text("The secret key is stored in the system keyring.")?,
    )?;
    ui::append(&panel, &ui::field("User ID", &entry)?)?;
    ui::set_content(&dialog, &panel)?;
    dialog.SetPrimaryButtonText("Generate")?;
    dialog.SetCloseButtonText("Cancel")?;
    dialog.SetDefaultButton(ContentDialogButton::Primary)?;
    let element: mailin_winui_sys::IUIElement = dialog.cast()?;
    element.SetXamlRoot(ui::window_xaml_root(window)?)?;

    let app = Arc::clone(app);
    let list = list.clone();
    let window = window.clone();
    let error = error.clone();
    let events = events.clone();
    events.keep(dialog.PrimaryButtonClick(move |dialog, _| {
        let user_id = ui::text_value(&entry);
        if user_id.trim().is_empty() {
            let _ = error.SetText("Enter a user ID (name and email)");
            let _ = ui::set_visible(&error, true);
            return Ok(());
        }
        match app.mailin.generate_pgp_key(&account, user_id.trim()) {
            Ok(key) => {
                let _ = error.SetText(&format!("Generated key {}", key.fingerprint));
                let _ = ui::set_visible(&error, true);
                let _ = populate(&list, &app, account, &window, &error, &events);
            }
            Err(failure) => {
                let _ = error.SetText(&format!("Could not generate a key: {failure}"));
                let _ = ui::set_visible(&error, true);
            }
        }
        let _ = dialog.Hide();
        Ok(())
    })?);

    let _ = dialog.ShowAsync()?;
    Ok(())
}
