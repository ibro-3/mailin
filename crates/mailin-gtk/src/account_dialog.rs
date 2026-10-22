//! The "Add Account" wizard.
//!
//! The wizard collects identity data and explicit server settings. Host and
//! port fields are prefilled from [`crate::autoconfig`] as the user types the
//! address, but a field the user edited is never overwritten again.

use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use mailin_core::{Account, AccountId, Security, ServerSettings};

use crate::app::AppState;
use crate::autoconfig::{self, Suggestion};

/// Values read back from the form. Kept separate from the widgets so account
/// construction can be unit tested.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Draft {
    display_name: String,
    email: String,
    password: String,
    imap: ServerDraft,
    smtp: ServerDraft,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerDraft {
    host: String,
    port: String,
    username: String,
    security: Security,
}

impl Draft {
    fn build(&self) -> std::result::Result<Account, String> {
        if self.password.is_empty() {
            return Err("Enter the account password.".to_owned());
        }

        let email = self.email.trim();
        let display_name = if self.display_name.trim().is_empty() {
            email.split_once('@').map_or(email, |(local, _)| local)
        } else {
            self.display_name.trim()
        };

        let imap = self.imap.build(email, "IMAP")?;
        let smtp = self.smtp.build(email, "SMTP")?;
        let account = Account::new(display_name, email, imap, smtp);
        account.validate().map_err(|error| error.to_string())?;
        Ok(account)
    }
}

impl ServerDraft {
    fn build(&self, email: &str, label: &str) -> std::result::Result<ServerSettings, String> {
        let port = self
            .port
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("{label} port must be a number between 1 and 65535."))?;
        let username = if self.username.trim().is_empty() {
            email
        } else {
            self.username.trim()
        };
        Ok(ServerSettings {
            host: self.host.trim().to_owned(),
            port,
            security: self.security,
            username: username.to_owned(),
        })
    }
}

pub fn present(
    parent: &impl IsA<gtk::Widget>,
    app: Rc<AppState>,
    on_saved: impl Fn(AccountId) + 'static,
) {
    let name_row = adw::EntryRow::builder().title("Display name").build();
    let email_row = adw::EntryRow::builder().title("Email address").build();
    let password_row = adw::PasswordEntryRow::builder().title("Password").build();

    let imap_host = adw::EntryRow::builder().title("IMAP server").build();
    let imap_port = adw::EntryRow::builder().title("Port").build();
    let imap_user = adw::EntryRow::builder().title("Username").build();
    imap_user.set_tooltip_text(Some("Leave empty to use the email address"));
    let imap_security = security_row("Encryption");

    let smtp_host = adw::EntryRow::builder().title("SMTP server").build();
    let smtp_port = adw::EntryRow::builder().title("Port").build();
    let smtp_user = adw::EntryRow::builder().title("Username").build();
    smtp_user.set_tooltip_text(Some("Leave empty to use the email address"));
    let smtp_security = security_row("Encryption");

    let identity = adw::PreferencesGroup::builder().title("Account").build();
    identity.add(&name_row);
    identity.add(&email_row);
    identity.add(&password_row);

    let incoming = adw::PreferencesGroup::builder()
        .title("Incoming mail (IMAP)")
        .description("Folder discovery, synchronization and message bodies.")
        .build();
    incoming.add(&imap_host);
    incoming.add(&imap_port);
    incoming.add(&imap_security);
    incoming.add(&imap_user);

    let outgoing = adw::PreferencesGroup::builder()
        .title("Outgoing mail (SMTP)")
        .description("Used to submit messages from the outbox.")
        .build();
    outgoing.add(&smtp_host);
    outgoing.add(&smtp_port);
    outgoing.add(&smtp_security);
    outgoing.add(&smtp_user);

    let page = adw::PreferencesPage::new();
    page.add(&identity);
    page.add(&incoming);
    page.add(&outgoing);

    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Add Account", "Connect an IMAP and SMTP mailbox");
    header.set_title_widget(Some(&title));

    let error_label = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .hexpand(true)
        .visible(false)
        .build();
    error_label.add_css_class("error");

    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Add Account");
    save.add_css_class("suggested-action");

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_margin_top(12);
    actions.set_margin_bottom(12);
    actions.set_margin_start(18);
    actions.set_margin_end(18);
    actions.append(&error_label);
    actions.append(&cancel);
    actions.append(&save);

    let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bottom.append(&actions);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    toolbar.add_bottom_bar(&bottom);

    let dialog = adw::Dialog::builder()
        .title("Add Account")
        .content_width(560)
        .content_height(700)
        .child(&toolbar)
        .build();

    // Fill server fields from the address domain while they are still the
    // values we suggested ourselves. Anything the user typed is preserved.
    let last_suggestion: Rc<RefCell<Option<Suggestion>>> = Rc::new(RefCell::new(None));
    email_row.connect_changed({
        let imap_host = imap_host.clone();
        let imap_port = imap_port.clone();
        let imap_security = imap_security.clone();
        let smtp_host = smtp_host.clone();
        let smtp_port = smtp_port.clone();
        let smtp_security = smtp_security.clone();
        let last_suggestion = Rc::clone(&last_suggestion);
        move |row| {
            let Some(suggestion) = autoconfig::suggest(&row.text()) else {
                return;
            };
            let previous = last_suggestion.borrow().clone();

            // A field is "untouched" while it is empty or still contains the
            // value that the previous suggestion put there.
            let imap_text = imap_host.text().to_string();
            let imap_untouched = imap_text.trim().is_empty()
                || previous
                    .as_ref()
                    .is_some_and(|previous| previous.imap_host == imap_text);
            if imap_untouched {
                imap_host.set_text(&suggestion.imap_host);
                imap_port.set_text(&suggestion.imap_port.to_string());
                imap_security.set_selected(security_index(suggestion.imap_security));
            }

            let smtp_text = smtp_host.text().to_string();
            let smtp_untouched = smtp_text.trim().is_empty()
                || previous
                    .as_ref()
                    .is_some_and(|previous| previous.smtp_host == smtp_text);
            if smtp_untouched {
                smtp_host.set_text(&suggestion.smtp_host);
                smtp_port.set_text(&suggestion.smtp_port.to_string());
                smtp_security.set_selected(security_index(suggestion.smtp_security));
            }

            *last_suggestion.borrow_mut() = Some(suggestion);
        }
    });

    cancel.connect_clicked({
        let dialog = dialog.clone();
        move |_| {
            dialog.close();
        }
    });

    let on_saved: Rc<dyn Fn(AccountId)> = Rc::new(on_saved);
    save.connect_clicked({
        let dialog = dialog.clone();
        let app = Rc::clone(&app);
        let rows = FormRows {
            name: name_row.clone(),
            email: email_row.clone(),
            password: password_row.clone(),
            imap_host,
            imap_port,
            imap_user,
            imap_security,
            smtp_host,
            smtp_port,
            smtp_user,
            smtp_security,
        };
        let error_label = error_label.clone();
        let account_id: Rc<std::cell::Cell<Option<AccountId>>> =
            Rc::new(std::cell::Cell::new(None));
        move |_| {
            let draft = rows.draft();
            let password = draft.password.clone();
            let mut account = match draft.build() {
                Ok(account) => account,
                Err(error) => {
                    error_label.set_text(&error);
                    error_label.set_visible(true);
                    return;
                }
            };
            // Keep the identifier across retries: a failed keyring write must
            // not leave an account row behind that a second click duplicates.
            match account_id.get() {
                Some(id) => account.id = id,
                None => account_id.set(Some(account.id)),
            }
            if let Err(error) = app.mailin.save_account(&account, Some(&password)) {
                error_label.set_text(&error.to_string());
                error_label.set_visible(true);
                return;
            }
            app.engine.sync_account(account.id);
            on_saved(account.id);
            dialog.close();
        }
    });

    dialog.present(Some(parent));
}

struct FormRows {
    name: adw::EntryRow,
    email: adw::EntryRow,
    password: adw::PasswordEntryRow,
    imap_host: adw::EntryRow,
    imap_port: adw::EntryRow,
    imap_user: adw::EntryRow,
    imap_security: adw::ComboRow,
    smtp_host: adw::EntryRow,
    smtp_port: adw::EntryRow,
    smtp_user: adw::EntryRow,
    smtp_security: adw::ComboRow,
}

impl FormRows {
    fn draft(&self) -> Draft {
        Draft {
            display_name: self.name.text().to_string(),
            email: self.email.text().to_string(),
            password: self.password.text().to_string(),
            imap: ServerDraft {
                host: self.imap_host.text().to_string(),
                port: self.imap_port.text().to_string(),
                username: self.imap_user.text().to_string(),
                security: security_at(self.imap_security.selected()),
            },
            smtp: ServerDraft {
                host: self.smtp_host.text().to_string(),
                port: self.smtp_port.text().to_string(),
                username: self.smtp_user.text().to_string(),
                security: security_at(self.smtp_security.selected()),
            },
        }
    }
}

fn security_row(title: &str) -> adw::ComboRow {
    adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(&["TLS", "STARTTLS", "None"]))
        .build()
}

fn security_index(security: Security) -> u32 {
    match security {
        Security::Tls => 0,
        Security::StartTls => 1,
        Security::None => 2,
    }
}

fn security_at(index: u32) -> Security {
    match index {
        1 => Security::StartTls,
        2 => Security::None,
        _ => Security::Tls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Draft {
        Draft {
            display_name: String::new(),
            email: "ada@example.com".into(),
            password: "secret".into(),
            imap: ServerDraft {
                host: "imap.example.com".into(),
                port: "993".into(),
                username: String::new(),
                security: Security::Tls,
            },
            smtp: ServerDraft {
                host: "smtp.example.com".into(),
                port: "465".into(),
                username: String::new(),
                security: Security::Tls,
            },
        }
    }

    #[test]
    fn empty_optional_fields_get_sensible_defaults() {
        let account = sample().build().expect("valid draft");
        assert_eq!(account.display_name, "ada");
        assert_eq!(account.imap.username, "ada@example.com");
        assert_eq!(account.smtp.username, "ada@example.com");
        assert_eq!(account.imap.port, 993);
    }

    #[test]
    fn missing_password_is_rejected() {
        let mut draft = sample();
        draft.password.clear();
        assert!(draft.build().is_err());
    }

    #[test]
    fn invalid_port_and_host_are_rejected() {
        let mut draft = sample();
        draft.imap.port = "nope".into();
        assert!(draft.build().is_err());

        let mut draft = sample();
        draft.smtp.host = "  ".into();
        assert!(draft.build().is_err());
    }

    #[test]
    fn security_round_trips_through_combo_index() {
        for security in [Security::Tls, Security::StartTls, Security::None] {
            assert_eq!(security_at(security_index(security)), security);
        }
    }

    /// Manual smoke test: renders the wizard for a few seconds. Ignored by
    /// default because CI has no display server.
    #[test]
    #[ignore = "requires a display; run manually to inspect the wizard"]
    fn wizard_renders_on_a_desktop() {
        use std::sync::Arc;
        use std::time::Duration;

        use mailin_core::{Config, Engine, Mailin, MemoryCredentials, Store};

        gtk::init().expect("GTK initializes");
        libadwaita::init().expect("libadwaita initializes");

        let gtk_app = adw::Application::builder()
            .application_id("dev.mailin.MailinWizardTest")
            .build();
        gtk_app.register(None::<&gtk::gio::Cancellable>).ok();
        let window = adw::ApplicationWindow::builder()
            .application(&gtk_app)
            .default_width(900)
            .default_height(700)
            .build();
        window.present();

        let mailin = Arc::new(Mailin::with_parts(
            Arc::new(Store::open_in_memory().expect("store")),
            Arc::new(MemoryCredentials::new()),
        ));
        let engine = Engine::start(Arc::clone(&mailin)).expect("engine");
        let app = Rc::new(crate::app::AppState {
            config: Config::default(),
            mailin,
            engine,
        });

        present(&window, app, |_| {});

        let main_loop = gtk::glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        gtk::glib::timeout_add_local_once(Duration::from_secs(5), move || quit.quit());
        main_loop.run();
    }
}
