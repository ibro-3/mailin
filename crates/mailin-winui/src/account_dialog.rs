//! The "Add Account" wizard.
//!
//! It mirrors the GTK wizard: identity fields, explicit IMAP and SMTP settings,
//! live server suggestions derived from the address domain, and the same
//! validation before anything is written to the store or the keyring.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use mailin_core::autoconfig::{self, Suggestion};
use mailin_core::{Account, AccountId, Security, ServerSettings};
use mailin_winui_sys::{ComboBox, Grid, PasswordBox, TextBlock, TextBox, Window};

use crate::app::AppState;
use crate::ui;

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

struct FormRows {
    name: TextBox,
    email: TextBox,
    password: PasswordBox,
    imap_host: TextBox,
    imap_port: TextBox,
    imap_user: TextBox,
    imap_security: ComboBox,
    smtp_host: TextBox,
    smtp_port: TextBox,
    smtp_user: TextBox,
    smtp_security: ComboBox,
}

impl FormRows {
    fn draft(&self) -> Draft {
        Draft {
            display_name: ui::text_value(&self.name),
            email: ui::text_value(&self.email),
            password: ui::password_value(&self.password),
            imap: ServerDraft {
                host: ui::text_value(&self.imap_host),
                port: ui::text_value(&self.imap_port),
                username: ui::text_value(&self.imap_user),
                security: security_at(ui::selected_index(&self.imap_security)),
            },
            smtp: ServerDraft {
                host: ui::text_value(&self.smtp_host),
                port: ui::text_value(&self.smtp_port),
                username: ui::text_value(&self.smtp_user),
                security: security_at(ui::selected_index(&self.smtp_security)),
            },
        }
    }
}

struct SuggestionWidgets {
    imap_host: TextBox,
    imap_port: TextBox,
    imap_security: ComboBox,
    smtp_host: TextBox,
    smtp_port: TextBox,
    smtp_security: ComboBox,
}

/// Open the wizard. `on_saved` runs after the account has been stored and the
/// first synchronization has been requested.
pub fn present(app: Arc<AppState>, on_saved: impl Fn(AccountId) + 'static) -> Result<Window> {
    let revokers = ui::Revokers::default();
    let root = Grid::new().context("could not create the wizard layout")?;
    ui::auto_row(&root)?;
    ui::star_row(&root)?;
    ui::auto_row(&root)?;

    let title = ui::heading("Add Account", Some(24.0), Some(700))?;
    ui::margin(&title, 20.0, 16.0, 20.0, 4.0)?;
    ui::set_row(&title, 0)?;
    ui::append(&root, &title)?;

    let form = ui::stack(mailin_winui_sys::Orientation::Vertical, 14.0)?;
    ui::margin(&form, 20.0, 8.0, 20.0, 16.0)?;

    let identity = ui::heading("Account", Some(16.0), Some(600))?;
    ui::append(&form, &identity)?;

    let name = ui::text_box("Ada Lovelace")?;
    let email = ui::text_box("ada@example.com")?;
    let password = ui::password_box("Password")?;
    ui::append(&form, &ui::field("Display name", &name)?)?;
    ui::append(&form, &ui::field("Email address", &email)?)?;
    ui::append(&form, &ui::field("Password", &password)?)?;

    let incoming = ui::heading("Incoming mail (IMAP)", Some(16.0), Some(600))?;
    ui::append(&form, &incoming)?;
    let imap_host = ui::text_box("imap.example.com")?;
    let imap_port = ui::text_box("993")?;
    let imap_user = ui::text_box("Leave empty to use the email address")?;
    let imap_security = ui::combo(&["TLS", "STARTTLS", "None"], 0)?;
    ui::append(&form, &ui::field("IMAP server", &imap_host)?)?;
    ui::append(&form, &ui::field("Port", &imap_port)?)?;
    ui::append(&form, &ui::field("Username", &imap_user)?)?;
    ui::append(&form, &ui::field("Encryption", &imap_security)?)?;

    let outgoing = ui::heading("Outgoing mail (SMTP)", Some(16.0), Some(600))?;
    ui::append(&form, &outgoing)?;
    let smtp_host = ui::text_box("smtp.example.com")?;
    let smtp_port = ui::text_box("465")?;
    let smtp_user = ui::text_box("Leave empty to use the email address")?;
    let smtp_security = ui::combo(&["TLS", "STARTTLS", "None"], 0)?;
    ui::append(&form, &ui::field("SMTP server", &smtp_host)?)?;
    ui::append(&form, &ui::field("Port", &smtp_port)?)?;
    ui::append(&form, &ui::field("Username", &smtp_user)?)?;
    ui::append(&form, &ui::field("Encryption", &smtp_security)?)?;

    let scroller = ui::scroll(&form)?;
    ui::set_row(&scroller, 1)?;
    ui::append(&root, &scroller)?;

    let footer = ui::stack(mailin_winui_sys::Orientation::Horizontal, 8.0)?;
    ui::margin(&footer, 20.0, 10.0, 20.0, 16.0)?;
    let error: TextBlock = ui::text("")?;
    ui::set_visible(&error, false)?;
    ui::append(&footer, &error)?;
    let spacer = ui::text("")?;
    ui::stretch_horizontal(&spacer)?;
    ui::append(&footer, &spacer)?;
    let cancel = ui::button("Cancel")?;
    let save = ui::button("Add Account")?;
    ui::append(&footer, &cancel)?;
    ui::append(&footer, &save)?;
    ui::set_row(&footer, 2)?;
    ui::append(&root, &footer)?;

    let window = Window::new().context("could not create the wizard window")?;
    window.SetTitle("Add Account")?;
    window.SetContent(&root)?;

    // Fill server fields from the address domain while they are still the
    // values the previous suggestion put there. Anything the user typed wins.
    let last_suggestion: Arc<Mutex<Option<Suggestion>>> = Arc::new(Mutex::new(None));
    let programmatic = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rows = FormRows {
        name,
        email,
        password,
        imap_host,
        imap_port,
        imap_user,
        imap_security,
        smtp_host,
        smtp_port,
        smtp_user,
        smtp_security,
    };
    revokers.keep(ui::on_text_changed(&rows.email, {
        let rows_email = rows.email.clone();
        let widgets = SuggestionWidgets {
            imap_host: rows.imap_host.clone(),
            imap_port: rows.imap_port.clone(),
            imap_security: rows.imap_security.clone(),
            smtp_host: rows.smtp_host.clone(),
            smtp_port: rows.smtp_port.clone(),
            smtp_security: rows.smtp_security.clone(),
        };
        let last_suggestion = Arc::clone(&last_suggestion);
        let programmatic = Arc::clone(&programmatic);
        move || {
            if programmatic.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let Some(suggestion) = autoconfig::suggest(&ui::text_value(&rows_email)) else {
                return;
            };
            let previous = last_suggestion.lock().ok().and_then(|slot| slot.clone());

            programmatic.store(true, std::sync::atomic::Ordering::Relaxed);
            let imap_text = ui::text_value(&widgets.imap_host);
            let imap_untouched = imap_text.trim().is_empty()
                || previous
                    .as_ref()
                    .is_some_and(|previous| previous.imap_host == imap_text);
            if imap_untouched {
                let _ = widgets.imap_host.SetText(&suggestion.imap_host);
                let _ = widgets.imap_port.SetText(&suggestion.imap_port.to_string());
                ui::select_index(
                    &widgets.imap_security,
                    security_index(suggestion.imap_security),
                );
            }

            let smtp_text = ui::text_value(&widgets.smtp_host);
            let smtp_untouched = smtp_text.trim().is_empty()
                || previous
                    .as_ref()
                    .is_some_and(|previous| previous.smtp_host == smtp_text);
            if smtp_untouched {
                let _ = widgets.smtp_host.SetText(&suggestion.smtp_host);
                let _ = widgets.smtp_port.SetText(&suggestion.smtp_port.to_string());
                ui::select_index(
                    &widgets.smtp_security,
                    security_index(suggestion.smtp_security),
                );
            }
            programmatic.store(false, std::sync::atomic::Ordering::Relaxed);

            if let Ok(mut slot) = last_suggestion.lock() {
                *slot = Some(suggestion);
            }
        }
    })?);

    revokers.keep(ui::on_click(&cancel, {
        let window = window.clone();
        move || {
            let _ = window.Close();
        }
    })?);

    let on_saved: Arc<dyn Fn(AccountId) + 'static> = Arc::new(on_saved);
    let account_id: Arc<Mutex<Option<AccountId>>> = Arc::new(Mutex::new(None));
    revokers.keep(ui::on_click(&save, {
        let window = window.clone();
        let app = Arc::clone(&app);
        let on_saved = Arc::clone(&on_saved);
        let account_id = Arc::clone(&account_id);
        let rows = FormRows {
            name: rows.name.clone(),
            email: rows.email.clone(),
            password: rows.password.clone(),
            imap_host: rows.imap_host.clone(),
            imap_port: rows.imap_port.clone(),
            imap_user: rows.imap_user.clone(),
            imap_security: rows.imap_security.clone(),
            smtp_host: rows.smtp_host.clone(),
            smtp_port: rows.smtp_port.clone(),
            smtp_user: rows.smtp_user.clone(),
            smtp_security: rows.smtp_security.clone(),
        };
        let error = error.clone();
        move || {
            let draft = rows.draft();
            let password = draft.password.clone();
            let mut account = match draft.build() {
                Ok(account) => account,
                Err(message) => {
                    let _ = error.SetText(&message);
                    let _ = ui::set_visible(&error, true);
                    return;
                }
            };
            // Keep the identifier across retries: a failed keyring write must
            // not leave an account row behind that a second click duplicates.
            if let Ok(mut slot) = account_id.lock() {
                match *slot {
                    Some(id) => account.id = id,
                    None => *slot = Some(account.id),
                }
            }
            if let Err(failure) = app.mailin.save_account(&account, Some(&password)) {
                let _ = error.SetText(&failure.to_string());
                let _ = ui::set_visible(&error, true);
                return;
            }
            app.engine.sync_account(account.id);
            on_saved(account.id);
            let _ = window.Close();
        }
    })?);

    let window_token = ui::keep_alive(&window);
    let cleanup = revokers.clone();
    let closed = window.Closed(move |_, _| {
        cleanup.clear();
        ui::release(window_token);
    })?;
    revokers.keep(closed);
    window.Activate()?;
    Ok(window)
}

fn security_index(security: Security) -> usize {
    match security {
        Security::Tls => 0,
        Security::StartTls => 1,
        Security::None => 2,
    }
}

fn security_at(index: usize) -> Security {
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
}
