//! Startup recovery window for the WinUI client (F1a).
//!
//! The GTK client never dead-ends on an unopenable local database: it offers
//! Retry / Open data folder / Reset local cache / Quit. This module brings the
//! same four options to Windows. It is only used when `AppState::open()` fails;
//! a successful start goes straight to the normal main window.

use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use mailin_winui_sys::{ContentDialog, ContentDialogButton, Orientation, TextBlock, Window};
use windows_core::Interface as _;

use crate::{app::AppState, platform, ui};

/// Show the recovery window. The state slot and the shared error text let a
/// successful retry hand control back to the normal application.
pub fn present(
    error: &str,
    state: Arc<RwLock<Option<Arc<AppState>>>>,
    startup_error: Arc<Mutex<Option<String>>>,
) -> Result<()> {
    let root = ui::stack_sm(Orientation::Vertical)?;
    ui::margin_lg(&root)?;

    let heading = ui::heading(
        "Mailin could not open its local data",
        Some(20.0),
        Some(700),
        Some(1),
    )?;
    ui::append(&root, &heading)?;

    let message = ui::text(error)?;
    ui::margin_vertical(&message, 12.0)?;
    ui::append(&root, &message)?;

    let hint = ui::text(
        "Your mail on the server is not affected. You can retry, open the data \
         folder to inspect the files, reset the local cache (which salvages what \
         it can), or quit.",
    )?;
    ui::margin(&hint, 0.0, 0.0, 0.0, 12.0)?;
    ui::append(&root, &hint)?;

    let buttons = ui::stack_sm(Orientation::Horizontal)?;
    let retry = ui::button("Retry", Some("Retry"), None)?;
    let folder = ui::button("Open data folder", Some("Open data folder"), None)?;
    let reset = ui::destructive_button(
        "Reset local cache…",
        Some("Reset local cache"),
        Some("Move the unreadable database aside and download mail again"),
    )?;
    let quit = ui::button("Quit", Some("Quit"), None)?;
    for button in [&retry, &folder, &reset, &quit] {
        ui::append(&buttons, button)?;
    }
    ui::append(&root, &buttons)?;

    let window = Window::new().context("could not create the recovery window")?;
    window.SetTitle("Mailin — Recovery")?;
    window.SetContent(&root)?;

    let revokers = ui::Revokers::default();

    {
        let state = Arc::clone(&state);
        let startup_error = Arc::clone(&startup_error);
        let recovery_window = window.clone();
        let message_for_retry = message.clone();
        revokers.keep(ui::on_click(&retry, move || match AppState::open() {
            Ok(opened) => start_with(
                Arc::new(opened),
                &state,
                &startup_error,
                &recovery_window,
                &message_for_retry,
            ),
            Err(error) => {
                let text = format!("Still could not open the local data:\n\n{error:#}");
                let _ = message_for_retry.SetText(&text);
                if let Ok(mut slot) = startup_error.lock() {
                    *slot = Some(text);
                }
            }
        })?);
    }

    {
        let recovery_window = window.clone();
        revokers.keep(ui::on_click(&folder, move || {
            if let Ok(paths) = mailin_core::Paths::discover() {
                platform::open_folder(&paths.data_dir);
            }
            // Keep the window alive for the lifetime of this closure.
            let _ = &recovery_window;
        })?);
    }

    {
        let state = Arc::clone(&state);
        let startup_error = Arc::clone(&startup_error);
        let recovery_window = window.clone();
        let message_for_reset = message.clone();
        revokers.keep(ui::on_click(&reset, move || {
            let dialog = match build_confirmation(&recovery_window) {
                Ok(dialog) => dialog,
                Err(error) => {
                    tracing::warn!(%error, "could not build the reset confirmation");
                    return;
                }
            };
            if let Ok(token) = dialog.PrimaryButtonClick({
                let state = Arc::clone(&state);
                let startup_error = Arc::clone(&startup_error);
                let recovery_window = recovery_window.clone();
                let message = message_for_reset.clone();
                move |_, _| match mailin_core::Paths::discover()
                    .and_then(|paths| mailin_core::reset_local_cache(&paths))
                {
                    Ok(report) => {
                        tracing::info!(?report, "local cache reset from the recovery window");
                        match AppState::open() {
                            Ok(opened) => start_with(
                                Arc::new(opened),
                                &state,
                                &startup_error,
                                &recovery_window,
                                &message,
                            ),
                            Err(error) => {
                                let text =
                                    format!("Still could not open the local data:\n\n{error:#}");
                                let _ = message.SetText(&text);
                                if let Ok(mut slot) = startup_error.lock() {
                                    *slot = Some(text);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        let text = format!("Could not reset the local cache:\n\n{error:#}");
                        let _ = message.SetText(&text);
                        if let Ok(mut slot) = startup_error.lock() {
                            *slot = Some(text);
                        }
                    }
                }
            }) {
                // The dialog's own lifetime is kept alive by ShowAsync; keep
                // the handler subscription alongside it.
                std::mem::forget(token);
            }
            let _ = dialog.ShowAsync();
        })?);
    }

    {
        revokers.keep(ui::on_click(&quit, move || {
            if let Ok(application) = mailin_winui_sys::Application::Current() {
                let _ = application.Exit();
            }
        })?);
    }

    window.Activate()?;
    // This window exists for the rest of the process if recovery fails; its
    // handlers must not be unsubscribed when `present` returns.
    std::mem::forget(revokers);
    Ok(())
}

fn build_confirmation(parent: &Window) -> Result<ContentDialog> {
    let dialog = ContentDialog::new().context("could not create the reset confirmation")?;
    let title: windows_core::IInspectable = ui::text("Reset local cache?")?.cast()?;
    dialog.SetTitle(&title)?;
    let body = ui::text(
        "Mailin will move its local database aside and download your mail again. \
         Accounts and queued mail are recovered from the old database when \
         possible; passwords stay in the keyring. Data that cannot be read stays \
         in the renamed files (mailin.db.corrupt-…).",
    )?;
    ui::set_content(&dialog, &body)?;
    dialog.SetPrimaryButtonText("Reset local cache")?;
    dialog.SetCloseButtonText("Cancel")?;
    dialog.SetDefaultButton(ContentDialogButton::Close)?;
    let root = ui::window_xaml_root(parent)?;
    ui::set_xaml_root(&dialog, &root)?;
    Ok(dialog)
}

/// Install an opened core as the application state and close the recovery
/// window. The main window is created first so the last-window shutdown mode
/// never exits the app in between.
fn start_with(
    opened: Arc<AppState>,
    state: &Arc<RwLock<Option<Arc<AppState>>>>,
    startup_error: &Arc<Mutex<Option<String>>>,
    recovery_window: &Window,
    message: &TextBlock,
) {
    if let Err(error) = crate::window::create(&opened) {
        let text = format!("Could not open the main window:\n\n{error:#}");
        let _ = message.SetText(&text);
        if let Ok(mut slot) = startup_error.lock() {
            *slot = Some(text);
        }
        tracing::error!(%error, "could not create the main window after recovery");
        return;
    }
    if let Ok(mut slot) = state.write() {
        *slot = Some(opened);
    }
    if let Ok(mut slot) = startup_error.lock() {
        *slot = None;
    }
    let _ = recovery_window.Close();
}
