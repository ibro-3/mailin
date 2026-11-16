//! Application bootstrap: open the core, start the network engine and own the
//! top level window.

use gtk::glib;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use mailin_core::mailto::parse_mailto_uri;
use mailin_core::{Config, Engine, Mailin, Paths, Theme};

use crate::window::MainWindow;

const APP_ID: &str = "dev.mailin.Mailin";

/// One-shot notice left by a panic in the previous run (D3).
static STARTUP_NOTICE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn set_startup_notice(text: String) {
    if let Ok(mut slot) = STARTUP_NOTICE.lock() {
        *slot = Some(text);
    }
}

pub(crate) fn take_startup_notice() -> Option<String> {
    STARTUP_NOTICE.lock().ok()?.take()
}

/// Everything that lives for the whole process. The UI keeps an `Rc` of this.
///
/// The config sits behind an `Rc<RefCell<…>>` so the preferences dialog can
/// write changes back into the running application; the UI reads it on every
/// use instead of caching values that would go stale.
pub struct AppState {
    pub config: Rc<RefCell<Config>>,
    pub mailin: std::sync::Arc<Mailin>,
    pub engine: Engine,
    /// Set when startup had to reset a broken preferences file to defaults.
    /// Shown as a banner once the window is up.
    pub config_warning: Option<String>,
}

impl AppState {
    fn open() -> anyhow::Result<Self> {
        let paths = Paths::discover()?;
        paths.ensure()?;
        let (config, config_warning) = Config::load_recovering(&paths)?;
        if let Some(warning) = &config_warning {
            tracing::warn!(%warning, "preferences were reset to defaults");
        }
        let mailin = std::sync::Arc::new(Mailin::open(&paths)?);
        let engine = Engine::start(std::sync::Arc::clone(&mailin))?;
        Ok(Self {
            config: Rc::new(RefCell::new(config)),
            mailin,
            engine,
            config_warning,
        })
    }
}

pub fn run() -> glib::ExitCode {
    let application = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    application.add_main_option(
        "compose",
        b'c'.into(),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Open a new compose window",
        None,
    );

    // Notifications use `app.present` as their default action. Registering the
    // action explicitly makes a click bring the window to the foreground.
    let present = gtk::gio::SimpleAction::new("present", None);
    present.connect_activate({
        let application = application.clone();
        move |_, _| application.activate()
    });
    application.add_action(&present);

    // Startup runs once; activation runs for every new window request.
    let state: Rc<RefCell<Option<Rc<AppState>>>> = Rc::new(RefCell::new(None));
    let startup_error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    application.connect_startup({
        let state = Rc::clone(&state);
        let startup_error = Rc::clone(&startup_error);
        move |_| match attempt_open() {
            Ok(opened) => {
                *state.borrow_mut() = Some(Rc::new(opened));
            }
            Err(error) => {
                *startup_error.borrow_mut() = Some(error);
            }
        }
    });

    application.connect_activate({
        let state = Rc::clone(&state);
        let startup_error = Rc::clone(&startup_error);
        move |application| {
            if let Some(window) = application.active_window() {
                window.present();
                return;
            }
            if let Some(error) = startup_error.borrow().as_deref() {
                let error = error.to_owned();
                show_startup_recovery(application, &state, &startup_error, error);
                return;
            }
            let Some(state) = state.borrow().clone() else {
                return;
            };
            MainWindow::new(application, state).present();
        }
    });

    // Handle mailto: URI activation (e.g., from browser links)
    application.connect_open({
        let application = application.clone();
        move |_application, files, _hint| {
            application.activate();
            for file in files {
                let uri = file.uri();
                if let Some(draft) = parse_mailto_uri(&uri) {
                    crate::window::set_mailto_draft(draft);
                    crate::window::handle_mailto_draft();
                    return;
                }
            }
        }
    });

    application.connect_command_line({
        let application = application.clone();
        move |_app, cmdline| {
            let options = cmdline.options_dict();
            if options.contains("compose") {
                crate::window::set_compose_requested();
            }
            let args = cmdline.arguments();
            for arg in args.iter().skip(1) {
                let arg = arg.to_string_lossy();
                if arg == "--compose" || arg == "-c" {
                    crate::window::set_compose_requested();
                } else if arg.starts_with("mailto:")
                    && let Some(draft) = parse_mailto_uri(arg.as_ref())
                {
                    crate::window::set_mailto_draft(draft);
                }
            }
            application.activate();
            // After activate, the window may handle pending requests; also try directly.
            crate::window::handle_compose_requested();
            crate::window::handle_mailto_draft();
            glib::ExitCode::from(0)
        }
    });

    application.run()
}

/// Attempt to open the application state. Applies the theme and CSS on
/// success. `Err` carries a user-facing description otherwise.
fn attempt_open() -> Result<AppState, String> {
    match AppState::open() {
        Ok(opened) => {
            apply_theme(opened.config.borrow().ui.theme);
            install_css();
            Ok(opened)
        }
        Err(error) => {
            let friendly = error
                .downcast_ref::<mailin_core::Error>()
                .map(|error| error.friendly())
                .unwrap_or_else(|| format!("{error}"));
            Err(format!("{friendly}\n\nDetails: {error:#}"))
        }
    }
}

/// The startup failure path: one recovery window with everything the user
/// needs — retry, open the data folder, reset the local cache (salvages
/// what it can) or quit. Never a dead-end alert.
fn show_startup_recovery(
    application: &adw::Application,
    state: &Rc<RefCell<Option<Rc<AppState>>>>,
    startup_error: &Rc<RefCell<Option<String>>>,
    error: String,
) {
    // libadwaita dialogs need a parent window, so the recovery UI is the
    // window itself.
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Mailin")
        .default_width(520)
        .default_height(420)
        .resizable(false)
        .build();
    let page = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Mailin could not open its local data")
        .description(error)
        .build();

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::Center);
    let retry = gtk::Button::with_label("Retry");
    retry.add_css_class("suggested-action");
    let folder = gtk::Button::with_label("Open data folder");
    let reset = gtk::Button::with_label("Reset local cache…");
    reset.add_css_class("destructive-action");
    let quit = gtk::Button::with_label("Quit");
    for button in [&retry, &folder, &reset, &quit] {
        buttons.append(button);
    }
    page.set_child(Some(&buttons));
    window.set_content(Some(&page));
    window.present();

    // After a failed retry, say so on the page instead of stacking dialogs.
    fn note_failure(page: &adw::StatusPage, error: &str) {
        page.set_description(Some(&format!(
            "Still could not open the local data:\n\n{error}"
        )));
    }

    retry.connect_clicked({
        let app = application.clone();
        let window = window.clone();
        let page = page.clone();
        let state = Rc::clone(state);
        let startup_error = Rc::clone(startup_error);
        move |_| match attempt_open() {
            Ok(opened) => {
                *state.borrow_mut() = Some(Rc::new(opened));
                *startup_error.borrow_mut() = None;
                window.close();
                app.activate();
            }
            Err(error) => {
                *startup_error.borrow_mut() = Some(error.clone());
                note_failure(&page, &error);
            }
        }
    });

    folder.connect_clicked({
        let window = window.clone();
        move |_| {
            if let Ok(paths) = mailin_core::Paths::discover() {
                let uri = format!("file://{}", paths.data_dir.display());
                let opened = gtk::gio::AppInfo::launch_default_for_uri(
                    &uri,
                    None::<&gtk::gio::AppLaunchContext>,
                )
                .is_ok();
                if !opened {
                    let fallback = adw::AlertDialog::builder()
                        .heading("Data folder")
                        .body(paths.data_dir.display().to_string())
                        .build();
                    fallback.add_response("ok", "OK");
                    let parent = window.clone();
                    gtk::glib::spawn_future_local(async move {
                        let _ = fallback.choose_future(Some(&parent)).await;
                    });
                }
            }
        }
    });

    reset.connect_clicked({
        let app = application.clone();
        let window = window.clone();
        let page = page.clone();
        let state = Rc::clone(state);
        let startup_error = Rc::clone(startup_error);
        move |_| {
            let confirm = adw::AlertDialog::builder()
                .heading("Reset local cache?")
                .body(
                    "Mailin will move its local database aside and download your mail again. \
                     Your accounts and queued mail are recovered from the old database when \
                     possible; passwords stay in the keyring. Data that cannot be read stays \
                     in the renamed files (mailin.db.corrupt-…).",
                )
                .build();
            confirm.add_responses(&[("cancel", "Cancel"), ("reset", "Reset local cache")]);
            confirm.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
            confirm.set_default_response(Some("cancel"));
            confirm.set_close_response("cancel");
            let app = app.clone();
            let window = window.clone();
            let page = page.clone();
            let state = Rc::clone(&state);
            let startup_error = Rc::clone(&startup_error);
            let parent = window.clone();
            gtk::glib::spawn_future_local(async move {
                let response = confirm.choose_future(Some(&parent)).await;
                if response.as_str() != "reset" {
                    return;
                }
                let outcome = mailin_core::Paths::discover()
                    .and_then(|paths| mailin_core::reset_local_cache(&paths));
                match outcome {
                    Ok(report) => {
                        tracing::info!(?report, "local cache reset from the recovery window");
                        match attempt_open() {
                            Ok(opened) => {
                                *state.borrow_mut() = Some(Rc::new(opened));
                                *startup_error.borrow_mut() = None;
                                window.close();
                                app.activate();
                            }
                            Err(error) => {
                                *startup_error.borrow_mut() = Some(error.clone());
                                note_failure(&page, &error);
                            }
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        *startup_error.borrow_mut() = Some(message.clone());
                        note_failure(&page, &message);
                    }
                }
            });
        }
    });

    quit.connect_clicked({
        let app = application.clone();
        move |_| app.quit()
    });
}

pub fn apply_theme(theme: Theme) {
    let scheme = match theme {
        Theme::System => adw::ColorScheme::Default,
        Theme::Light => adw::ColorScheme::ForceLight,
        Theme::Dark => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

/// Application-wide styles that libadwaita does not provide: the reader's
/// remote-content banner needs a tinted background for its two actions.
fn install_css() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".remote-banner {\
           background-color: alpha(@accent_color, 0.16);\
           border-radius: 8px;\
           padding: 6px;\
         }\
         @keyframes spin {\
           to { transform: rotate(360deg); }\
         }\
         .sync-status-idle {\
           color: @disabled_text_color;\
         }\
         .sync-status-syncing {\
           color: @accent_color;\
           animation: spin 1s linear infinite;\
         }\
         .sync-status-connected {\
           color: @success_color;\
         }\
         .sync-status-offline {\
           color: @warning_color;\
         }\
         .sync-status-error {\
           color: @error_color;\
         }",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
