//! Application bootstrap: open the core, start the network engine and own the
//! top level window.

use gtk::glib;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use mailin_core::{Config, Engine, Mailin, OutgoingMessage, Paths, Theme};

use crate::window::MainWindow;

const APP_ID: &str = "dev.mailin.Mailin";

/// Everything that lives for the whole process. The UI keeps an `Rc` of this.
///
/// The config sits behind an `Rc<RefCell<…>>` so the preferences dialog can
/// write changes back into the running application; the UI reads it on every
/// use instead of caching values that would go stale.
pub struct AppState {
    pub config: Rc<RefCell<Config>>,
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
            config: Rc::new(RefCell::new(config)),
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
                apply_theme(opened.config.borrow().ui.theme);
                install_css();
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

    // Handle mailto: URI activation (e.g., from browser links)
    application.connect_open({
        move |_application, files, _hint| {
            for file in files {
                let uri = file.uri();
                if let Some(draft) = parse_mailto_uri(&uri) {
                    crate::window::set_mailto_draft(draft);
                    return;
                }
            }
        }
    });

    application.run()
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

/// Parse a mailto: URI into an OutgoingMessage draft.
/// Supports: mailto:addr?subject=...&body=...&cc=...&bcc=...
fn parse_mailto_uri(uri: &str) -> Option<OutgoingMessage> {
    let uri = uri.strip_prefix("mailto:")?;
    let (addr_part, query_part) = uri.split_once('?').unwrap_or((uri, ""));
    let to = percent_decode(addr_part)?;
    let mut draft = OutgoingMessage {
        to,
        ..OutgoingMessage::default()
    };
    // Best-effort: a single malformed component must not discard the fields
    // that were already parsed.
    for pair in query_part.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let Some(value) = percent_decode(value) else {
            continue;
        };
        match key {
            "subject" => draft.subject = value,
            "body" => draft.body = value,
            "cc" => draft.cc = value,
            "bcc" => draft.bcc = value,
            _ => {}
        }
    }
    Some(draft)
}

/// Percent-decode an URI component, assembling bytes before interpreting them
/// as UTF-8 so multi-byte characters survive intact.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                let byte = u8::from_str_radix(hex, 16).ok()?;
                out.push(byte);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailto_parses_recipient_and_fields() {
        let draft = parse_mailto_uri("mailto:ada@example.com?subject=Hi&body=Hello+there&cc=a@b.c")
            .expect("parsed");
        assert_eq!(draft.to, "ada@example.com");
        assert_eq!(draft.subject, "Hi");
        assert_eq!(draft.body, "Hello there");
        assert_eq!(draft.cc, "a@b.c");
    }

    #[test]
    fn mailto_decodes_multibyte_utf8() {
        // Percent-encoded "Grüße" = Gr%C3%BC%C3%9Fe
        let draft =
            parse_mailto_uri("mailto:ada@example.com?subject=Gr%C3%BC%C3%9Fe").expect("parsed");
        assert_eq!(draft.subject, "Grüße");
    }

    #[test]
    fn mailto_keeps_fields_when_a_later_component_is_malformed() {
        // The trailing pair has no '='; the earlier fields must survive.
        let draft = parse_mailto_uri("mailto:ada@example.com?subject=Hi&broken").expect("parsed");
        assert_eq!(draft.subject, "Hi");
        assert_eq!(draft.to, "ada@example.com");
    }

    #[test]
    fn percent_decode_handles_plus_and_plain_text() {
        assert_eq!(percent_decode("a+b%20c").as_deref(), Some("a b c"));
    }
}
