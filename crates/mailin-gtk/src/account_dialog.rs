//! The "Add/Edit Account" wizard.
//!
//! The wizard collects identity data and explicit server settings. Host and
//! port fields are prefilled from [`crate::autoconfig`] as the user types the
//! address, but a field the user edited is never overwritten again.

use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use mailin_core::{
    Account, AccountId, AuthMethod, OAuth2Tokens, OAuthProvider, PendingAuthorization, Security,
    ServerSettings,
};

use crate::app::AppState;
use crate::autoconfig::{self, Suggestion};

/// Values read back from the form. Kept separate from the widgets so account
/// construction can be unit tested.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Draft {
    display_name: String,
    email: String,
    password: String,
    auth_method: AuthMethod,
    oauth2: OAuth2Tokens,
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
    fn build(&self, require_password: bool) -> std::result::Result<Account, String> {
        if require_password {
            match self.auth_method {
                AuthMethod::Password if self.password.is_empty() => {
                    return Err("Enter the account password.".to_owned());
                }
                AuthMethod::OAuth2 if self.oauth2.access_token.is_empty() => {
                    return Err("Sign in with the provider to obtain an access token.".to_owned());
                }
                _ => {}
            }
        }

        let email = self.email.trim();
        let display_name = if self.display_name.trim().is_empty() {
            email.split_once('@').map_or(email, |(local, _)| local)
        } else {
            self.display_name.trim()
        };

        let mut imap = self.imap.build(email, "IMAP")?;
        let mut smtp = self.smtp.build(email, "SMTP")?;
        if self.auth_method == AuthMethod::OAuth2 {
            imap.auth_method = AuthMethod::OAuth2;
            imap.oauth2 = self.oauth2.clone();
            smtp.auth_method = AuthMethod::OAuth2;
            smtp.oauth2 = self.oauth2.clone();
        }
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
            auth_method: AuthMethod::default(),
            oauth2: OAuth2Tokens::default(),
        })
    }
}

/// Mode for the account dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountDialogMode {
    /// Add a new account.
    Add,
    /// Edit an existing account.
    Edit,
}

/// Present the account dialog for adding or editing an account.
///
/// In edit mode, the form is pre-filled with the existing account's settings.
/// The password field is left empty; if the user enters a new password, it will
/// be updated. If left empty, the existing password is kept.
pub fn present(
    parent: &impl IsA<gtk::Widget>,
    app: Rc<AppState>,
    mode: AccountDialogMode,
    existing_account: Option<Account>,
    on_saved: impl Fn(AccountId) + 'static,
) {
    let (title, subtitle, save_label) = match mode {
        AccountDialogMode::Add => (
            "Add Account",
            "Connect an IMAP and SMTP mailbox",
            "Add Account",
        ),
        AccountDialogMode::Edit => ("Edit Account", "Update account settings", "Save Changes"),
    };

    // Create form fields
    let name_row = adw::EntryRow::builder().title("Display name").build();
    let email_row = adw::EntryRow::builder().title("Email address").build();
    let password_row = adw::PasswordEntryRow::builder().title("Password").build();
    let auth_method_row = adw::ComboRow::builder()
        .title("Authentication")
        .model(&gtk::StringList::new(&["Password", "OAuth2 (recommended)"]))
        .build();
    let auth_method_row = auth_method_row;
    let oauth_provider_row = adw::ComboRow::builder()
        .title("Provider")
        .model(&gtk::StringList::new(&provider_labels()))
        .build();
    let oauth_client_id_row = adw::EntryRow::builder().title("OAuth2 client ID").build();
    oauth_client_id_row.set_tooltip_text(Some(
        "Public client identifier registered by your mail provider. \
         Deployment builds can also set MAILIN_*_CLIENT_ID.",
    ));
    let oauth_sign_in = gtk::Button::with_label("Sign in with browser…");
    oauth_sign_in.add_css_class("suggested-action");
    let oauth_status = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .hexpand(true)
        .visible(false)
        .build();

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
    identity.add(&auth_method_row);
    identity.add(&password_row);

    let oauth_group = adw::PreferencesGroup::builder()
        .title("OAuth2 sign-in")
        .description(
            "Authenticate without storing a password. A browser window opens; \
             sign in there and return to Mailin.",
        )
        .build();
    oauth_group.add(&oauth_provider_row);
    oauth_group.add(&oauth_client_id_row);
    oauth_group.add(&oauth_sign_in);
    oauth_group.add(&oauth_status);

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
    page.add(&oauth_group);
    page.add(&incoming);
    page.add(&outgoing);

    let header = adw::HeaderBar::new();
    let window_title = adw::WindowTitle::new(title, subtitle);
    header.set_title_widget(Some(&window_title));

    let error_label = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .hexpand(true)
        .visible(false)
        .build();
    error_label.add_css_class("error");

    let cancel = gtk::Button::with_label("Cancel");
    let test = gtk::Button::with_label("Test Connection");
    let save = gtk::Button::with_label(save_label);
    save.add_css_class("suggested-action");

    // Remove button for edit mode
    let remove = match mode {
        AccountDialogMode::Edit => {
            let btn = gtk::Button::with_label("Remove Account");
            btn.add_css_class("destructive-action");
            Some(btn)
        }
        AccountDialogMode::Add => None,
    };

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_margin_top(12);
    actions.set_margin_bottom(12);
    actions.set_margin_start(18);
    actions.set_margin_end(18);
    actions.append(&error_label);
    actions.append(&test);
    if let Some(remove) = &remove {
        actions.append(remove);
    }
    actions.append(&cancel);
    actions.append(&save);

    let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bottom.append(&actions);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    toolbar.add_bottom_bar(&bottom);

    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(560)
        .content_height(700)
        .child(&toolbar)
        .build();

    let oauth_tokens: Rc<RefCell<OAuth2Tokens>> = Rc::new(RefCell::new(OAuth2Tokens::default()));
    let previous_provider: Rc<std::cell::Cell<OAuthProvider>> =
        Rc::new(std::cell::Cell::new(OAuthProvider::default()));

    // Toggle the password and OAuth2 sections.
    auth_method_row.connect_selected_notify({
        let oauth_group = oauth_group.clone();
        let password_row = password_row.clone();
        move |row| {
            let oauth = row.selected() == auth_index(AuthMethod::OAuth2);
            oauth_group.set_visible(oauth);
            password_row.set_visible(!oauth);
        }
    });

    oauth_provider_row.connect_selected_notify({
        let oauth_client_id_row = oauth_client_id_row.clone();
        let oauth_tokens = Rc::clone(&oauth_tokens);
        let previous_provider = Rc::clone(&previous_provider);
        move |row| {
            let provider = provider_at(row.selected());
            let previous = previous_provider.replace(provider);
            if let Ok(mut tokens) = oauth_tokens.try_borrow_mut() {
                if tokens.provider != provider {
                    // Tokens from a different provider cannot be reused.
                    tokens.access_token.clear();
                    tokens.refresh_token.clear();
                    tokens.expires_at = 0;
                    tokens.provider = provider;
                }
            }
            let current = oauth_client_id_row.text().to_string();
            let previous_default = previous.default_client_id();
            if current.trim().is_empty() || current == previous_default {
                oauth_client_id_row.set_text(&provider.default_client_id());
            }
        }
    });

    // Pre-fill form if editing
    if let Some(account) = existing_account.clone() {
        name_row.set_text(&account.display_name);
        email_row.set_text(&account.email);
        // Password left empty - user can enter new one to change it

        let oauth = account.imap.auth_method == AuthMethod::OAuth2;
        auth_method_row.set_selected(auth_index(account.imap.auth_method));
        oauth_group.set_visible(oauth);
        password_row.set_visible(!oauth);
        if oauth {
            *oauth_tokens.borrow_mut() = account.imap.oauth2.clone();
            oauth_provider_row.set_selected(provider_index(account.imap.oauth2.provider));
            oauth_client_id_row.set_text(&account.imap.oauth2.client_id);
            oauth_status.set_text("Signed in. Sign in again to replace the token.");
            oauth_status.set_visible(true);
            previous_provider.set(account.imap.oauth2.provider);
        } else {
            previous_provider.set(provider_at(oauth_provider_row.selected()));
        }

        imap_host.set_text(&account.imap.host);
        imap_port.set_text(&account.imap.port.to_string());
        imap_user.set_text(&account.imap.username);
        imap_security.set_selected(security_index(account.imap.security));

        smtp_host.set_text(&account.smtp.host);
        smtp_port.set_text(&account.smtp.port.to_string());
        smtp_user.set_text(&account.smtp.username);
        smtp_security.set_selected(security_index(account.smtp.security));

        // Disable email field in edit mode (account identity shouldn't change)
        email_row.set_sensitive(false);
    }

    // Sign in with the system browser and wait for the loopback redirect on a
    // background thread. Poll the channel from the GTK main loop.
    oauth_sign_in.connect_clicked({
        let oauth_tokens = Rc::clone(&oauth_tokens);
        let oauth_status = oauth_status.clone();
        let oauth_sign_in = oauth_sign_in.clone();
        let provider_row = oauth_provider_row.clone();
        let client_id_row = oauth_client_id_row.clone();
        let email_row = email_row.clone();
        move |_| {
            let provider = provider_at(provider_row.selected());
            let client_id = client_id_row.text().to_string();
            let email = email_row.text().to_string();
            let pending = match PendingAuthorization::begin(provider, &client_id, Some(&email)) {
                Ok(pending) => pending,
                Err(error) => {
                    oauth_status.set_text(&error.to_string());
                    oauth_status.remove_css_class("success");
                    oauth_status.add_css_class("error");
                    oauth_status.set_visible(true);
                    return;
                }
            };
            let url = pending.auth_url().to_owned();
            if let Err(error) =
                gtk::gio::AppInfo::launch_default_for_uri(&url, None::<&gtk::gio::AppLaunchContext>)
            {
                oauth_status.set_text(&format!("Could not open the browser: {error}"));
                oauth_status.remove_css_class("success");
                oauth_status.add_css_class("error");
                oauth_status.set_visible(true);
                return;
            }
            oauth_sign_in.set_sensitive(false);
            oauth_status.set_text("Waiting for the browser to complete sign-in…");
            oauth_status.remove_css_class("error");
            oauth_status.remove_css_class("success");
            oauth_status.set_visible(true);

            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                let result = pending.wait(Duration::from_secs(300));
                let _ = sender.send(result);
            });

            let oauth_tokens = Rc::clone(&oauth_tokens);
            let oauth_status = oauth_status.clone();
            let oauth_sign_in = oauth_sign_in.clone();
            glib::timeout_add_local(Duration::from_millis(200), move || {
                match receiver.try_recv() {
                    Ok(Ok(tokens)) => {
                        *oauth_tokens.borrow_mut() = tokens;
                        oauth_status.set_text("Signed in successfully.");
                        oauth_status.remove_css_class("error");
                        oauth_status.add_css_class("success");
                        oauth_status.set_visible(true);
                        oauth_sign_in.set_sensitive(true);
                        glib::ControlFlow::Break
                    }
                    Ok(Err(error)) => {
                        oauth_status.set_text(&format!("Sign-in failed: {error}"));
                        oauth_status.remove_css_class("success");
                        oauth_status.add_css_class("error");
                        oauth_status.set_visible(true);
                        oauth_sign_in.set_sensitive(true);
                        glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        oauth_status.set_text("Sign-in was interrupted.");
                        oauth_status.set_visible(true);
                        oauth_sign_in.set_sensitive(true);
                        glib::ControlFlow::Break
                    }
                }
            });
        }
    });

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

    test.connect_clicked({
        let app = Rc::clone(&app);
        let rows = FormRows {
            name: name_row.clone(),
            email: email_row.clone(),
            password: password_row.clone(),
            auth_method: auth_method_row.clone(),
            oauth_provider: oauth_provider_row.clone(),
            oauth_client_id: oauth_client_id_row.clone(),
            oauth_tokens: Rc::clone(&oauth_tokens),
            imap_host: imap_host.clone(),
            imap_port: imap_port.clone(),
            imap_user: imap_user.clone(),
            imap_security: imap_security.clone(),
            smtp_host: smtp_host.clone(),
            smtp_port: smtp_port.clone(),
            smtp_user: smtp_user.clone(),
            smtp_security: smtp_security.clone(),
        };
        let error_label = error_label.clone();
        move |_| {
            let draft = rows.draft();
            let password = draft.password.clone();
            // In edit mode, we don't require password for test if it's empty
            let require_password = mode == AccountDialogMode::Add;
            let account = match draft.build(require_password) {
                Ok(account) => account,
                Err(error) => {
                    error_label.set_text(&error);
                    error_label.set_visible(true);
                    return;
                }
            };
            error_label.set_visible(false);
            error_label.set_text("Testing connection…");
            error_label.set_visible(true);
            let app = Rc::clone(&app);
            let error_label = error_label.clone();
            glib::spawn_future_local(async move {
                match app
                    .mailin
                    .test_connection(&account.imap, &account.smtp, &password)
                    .await
                {
                    Ok(()) => {
                        error_label.set_text("Connection successful!");
                        error_label.add_css_class("success");
                        error_label.set_visible(true);
                    }
                    Err(error) => {
                        error_label.set_text(&format!("Connection failed: {error}"));
                        error_label.add_css_class("error");
                        error_label.set_visible(true);
                    }
                }
            });
        }
    });

    // Clone existing_account for use in both save and remove closures
    let existing_account_for_save = existing_account.clone();
    let existing_account_for_remove = existing_account.clone();

    let on_saved: Rc<dyn Fn(AccountId)> = Rc::new(on_saved);
    save.connect_clicked({
        let dialog = dialog.clone();
        let app = Rc::clone(&app);
        let rows = FormRows {
            name: name_row.clone(),
            email: email_row.clone(),
            password: password_row.clone(),
            auth_method: auth_method_row.clone(),
            oauth_provider: oauth_provider_row.clone(),
            oauth_client_id: oauth_client_id_row.clone(),
            oauth_tokens: Rc::clone(&oauth_tokens),
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
            let is_oauth = draft.auth_method == AuthMethod::OAuth2;
            // In edit mode, we don't require password if it's empty (keep existing)
            let require_password = mode == AccountDialogMode::Add;
            let mut account = match draft.build(require_password) {
                Ok(account) => account,
                Err(error) => {
                    error_label.set_text(&error);
                    error_label.set_visible(true);
                    return;
                }
            };
            // Keep the identifier across retries: a failed keyring write must
            // not leave an account row behind that a second click duplicates.
            // In edit mode, preserve the existing account ID.
            if let Some(existing) = &existing_account_for_save {
                account.id = existing.id;
            } else {
                match account_id.get() {
                    Some(id) => account.id = id,
                    None => account_id.set(Some(account.id)),
                }
            }
            // Only update password if user entered a new one. OAuth2 accounts
            // keep tokens in the account row and do not use the keyring.
            let password_opt = if password.is_empty() || is_oauth {
                None
            } else {
                Some(secrecy::SecretString::new(password.into()))
            };
            if let Err(error) = app.mailin.save_account(&account, password_opt.as_ref()) {
                error_label.set_text(&error.to_string());
                error_label.set_visible(true);
                return;
            }
            app.engine.sync_account(account.id);
            on_saved(account.id);
            dialog.close();
        }
    });

    // Remove account handler
    if let Some(remove) = remove {
        let dialog = dialog.clone();
        let app = Rc::clone(&app);
        let existing = existing_account_for_remove.clone();
        // Get parent window before the closure to avoid lifetime issues
        let window = parent.root().and_then(|r| r.downcast::<gtk::Window>().ok());
        remove.connect_clicked(move |_| {
            if let Some(account) = &existing {
                // Show confirmation dialog
                let confirm = gtk::AlertDialog::builder()
                    .message("Remove account?")
                    .detail(format!(
                        "Are you sure you want to remove '{}' ({})? This will delete all local data for this account.",
                        account.display_name, account.email
                    ))
                    .build();
                confirm.set_buttons(&["Cancel", "Remove"]);
                confirm.set_cancel_button(0);
                confirm.set_default_button(0);

                // Extract the mailin from app for use in callback
                let mailin = app.mailin.clone();
                let dialog = dialog.clone();
                let account_id = account.id;
                let window = window.clone();

                // Use choose_future to handle the response asynchronously
                glib::spawn_future_local(async move {
                    let response = confirm.choose_future(window.as_ref()).await;
                    match response {
                        Ok(1) => { // "Remove" button (index 1)
                            if let Err(e) = mailin.remove_account(&account_id) {
                                eprintln!("Failed to remove account: {e}");
                            } else {
                                // sync_account is called by the remove_account internally
                            }
                            dialog.close();
                        }
                        _ => {
                            // Cancel or other - do nothing
                        }
                    }
                });
            }
        });
    }

    dialog.present(Some(parent));
}

struct FormRows {
    name: adw::EntryRow,
    email: adw::EntryRow,
    password: adw::PasswordEntryRow,
    auth_method: adw::ComboRow,
    oauth_provider: adw::ComboRow,
    oauth_client_id: adw::EntryRow,
    oauth_tokens: Rc<RefCell<OAuth2Tokens>>,
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
        let mut oauth2 = self.oauth_tokens.borrow().clone();
        oauth2.provider = provider_at(self.oauth_provider.selected());
        oauth2.client_id = self.oauth_client_id.text().to_string();
        Draft {
            display_name: self.name.text().to_string(),
            email: self.email.text().to_string(),
            password: self.password.text().to_string(),
            auth_method: auth_at(self.auth_method.selected()),
            oauth2,
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

fn auth_index(method: AuthMethod) -> u32 {
    match method {
        AuthMethod::Password => 0,
        AuthMethod::OAuth2 => 1,
    }
}

fn auth_at(index: u32) -> AuthMethod {
    match index {
        1 => AuthMethod::OAuth2,
        _ => AuthMethod::Password,
    }
}

fn provider_labels() -> [&'static str; 5] {
    [
        OAuthProvider::Google.label(),
        OAuthProvider::Microsoft.label(),
        OAuthProvider::Yahoo.label(),
        OAuthProvider::Fastmail.label(),
        OAuthProvider::Custom.label(),
    ]
}

fn provider_index(provider: OAuthProvider) -> u32 {
    match provider {
        OAuthProvider::Google => 0,
        OAuthProvider::Microsoft => 1,
        OAuthProvider::Yahoo => 2,
        OAuthProvider::Fastmail => 3,
        OAuthProvider::Custom => 4,
    }
}

fn provider_at(index: u32) -> OAuthProvider {
    match index {
        1 => OAuthProvider::Microsoft,
        2 => OAuthProvider::Yahoo,
        3 => OAuthProvider::Fastmail,
        4 => OAuthProvider::Custom,
        _ => OAuthProvider::Google,
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
            auth_method: AuthMethod::Password,
            oauth2: OAuth2Tokens::default(),
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
        let account = sample().build(true).expect("valid draft");
        assert_eq!(account.display_name, "ada");
        assert_eq!(account.imap.username, "ada@example.com");
        assert_eq!(account.smtp.username, "ada@example.com");
        assert_eq!(account.imap.port, 993);
    }

    #[test]
    fn missing_password_is_rejected() {
        let mut draft = sample();
        draft.password.clear();
        assert!(draft.build(true).is_err());
    }

    #[test]
    fn oauth2_requires_an_access_token() {
        let mut draft = sample();
        draft.password.clear();
        draft.auth_method = AuthMethod::OAuth2;
        assert!(draft.build(true).is_err());

        draft.oauth2.access_token = "token".into();
        draft.oauth2.refresh_token = "refresh".into();
        let account = draft.build(true).expect("OAuth2 draft");
        assert_eq!(account.imap.auth_method, AuthMethod::OAuth2);
        assert_eq!(account.smtp.auth_method, AuthMethod::OAuth2);
        assert_eq!(account.imap.oauth2.access_token, "token");
    }

    #[test]
    fn oauth_provider_round_trips_through_combo_index() {
        for provider in [
            OAuthProvider::Google,
            OAuthProvider::Microsoft,
            OAuthProvider::Yahoo,
            OAuthProvider::Fastmail,
            OAuthProvider::Custom,
        ] {
            assert_eq!(provider_at(provider_index(provider)), provider);
        }
    }

    #[test]
    fn edit_mode_allows_empty_password() {
        let mut draft = sample();
        draft.password.clear();
        // In edit mode (require_password = false), empty password should be OK
        assert!(draft.build(false).is_ok());
    }

    #[test]
    fn invalid_port_and_host_are_rejected() {
        let mut draft = sample();
        draft.imap.port = "nope".into();
        assert!(draft.build(true).is_err());

        let mut draft = sample();
        draft.smtp.host = "  ".into();
        assert!(draft.build(true).is_err());
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

        present(&window, app, AccountDialogMode::Add, None, |_| {});

        let main_loop = gtk::glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        gtk::glib::timeout_add_local_once(Duration::from_secs(5), move || quit.quit());
        main_loop.run();
    }
}
