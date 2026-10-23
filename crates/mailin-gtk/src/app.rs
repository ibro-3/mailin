//! Application bootstrap: open the core, start the network engine and own the
//! top level window.

use gtk::glib;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use mailin_core::{Config, Engine, Mailin, Paths, Theme};

use crate::window::MainWindow;

const APP_ID: &str = "dev.mailin.Mailin";

/// Everything that lives for the whole process. The UI keeps an `Rc` of this.
pub struct AppState {
    pub config: Config,
    pub mailin: std::sync::Arc<Mailin>,
    pub engine: Engine,
}

impl AppState {
    fn open() -> anyhow::Result<Self> {
        let paths = Paths::discover()?;
        paths.ensure()?;
        let config = Config::load(&paths)?;
        config.validate()?;
        let mailin = std::sync::Arc::new(Mailin::open(&paths)?);
        let engine = Engine::start(std::sync::Arc::clone(&mailin))?;
        Ok(Self {
            config,
            mailin,
            engine,
        })
    }
}

pub fn run() -> glib::ExitCode {
    let application = adw::Application::builder().application_id(APP_ID).build();

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
        move |_| match AppState::open() {
            Ok(opened) => {
                apply_theme(opened.config.ui.theme);
                *state.borrow_mut() = Some(Rc::new(opened));
            }
            Err(error) => {
                *startup_error.borrow_mut() = Some(format!("{error:#}"));
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
                let dialog = gtk::AlertDialog::builder()
                    .message("Mailin could not start")
                    .detail(error)
                    .build();
                let parent = application.active_window();
                dialog.show(parent.as_ref());
                return;
            }
            let Some(state) = state.borrow().clone() else {
                return;
            };
            MainWindow::new(application, state).present();
        }
    });

    application.run()
}

fn apply_theme(theme: Theme) {
    let scheme = match theme {
        Theme::System => adw::ColorScheme::Default,
        Theme::Light => adw::ColorScheme::ForceLight,
        Theme::Dark => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}
