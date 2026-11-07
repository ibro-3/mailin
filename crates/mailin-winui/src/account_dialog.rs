//! The "Add/Edit Account" wizard.
//!
//! It mirrors the GTK wizard: identity fields, explicit IMAP and SMTP settings,
//! live server suggestions derived from the address domain, and the same
//! validation before anything is written to the store or the keyring.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use mailin_core::autoconfig::{self, Suggestion};
use mailin_core::{
    Account, AccountId, AuthMethod, Contact, OAuth2Tokens, OAuthProvider, PendingAuthorization,
    Security, ServerSettings,
};
use mailin_winui_sys::{ComboBox, Grid, PasswordBox, TextBlock, TextBox, Window};
use secrecy::SecretString;
use windows_core::Interface;

use crate::app::AppState;
use crate::design_tokens;
use crate::ui;

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
            auth_method: AuthMethod::Password,
            oauth2: OAuth2Tokens::default(),
        })
    }
}

struct FormRows {
    name: TextBox,
    email: TextBox,
    password: PasswordBox,
    auth_method: ComboBox,
    oauth_provider: ComboBox,
    oauth_client_id: TextBox,
    oauth_tokens: Arc<Mutex<OAuth2Tokens>>,
    imap_host: TextBox,
    imap_port: TextBox,
    imap_user: TextBox,
    imap_security: ComboBox,
    smtp_host: TextBox,
    smtp_port: TextBox,
    smtp_user: TextBox,
    smtp_security: ComboBox,
    signature: TextBox,
    signature_html: TextBox,
    trust_container: mailin_winui_sys::StackPanel,
    allowlist: Arc<Mutex<Vec<String>>>,
    contacts_container: mailin_winui_sys::StackPanel,
}

impl FormRows {
    fn draft(&self) -> Draft {
        let mut oauth2 = self
            .oauth_tokens
            .lock()
            .map(|tokens| tokens.clone())
            .unwrap_or_default();
        oauth2.provider = provider_at(ui::selected_index(&self.oauth_provider));
        oauth2.client_id = ui::text_value(&self.oauth_client_id);
        Draft {
            display_name: ui::text_value(&self.name),
            email: ui::text_value(&self.email),
            password: ui::password_value(&self.password),
            auth_method: auth_at(ui::selected_index(&self.auth_method)),
            oauth2,
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
    app: Arc<AppState>,
    mode: AccountDialogMode,
    existing_account: Option<Account>,
    on_saved: impl Fn(AccountId) + 'static,
) -> Result<Window> {
    let (title_text, subtitle_text, save_label) = match mode {
        AccountDialogMode::Add => (
            "Add Account",
            "Connect an IMAP and SMTP mailbox",
            "Add Account",
        ),
        AccountDialogMode::Edit => ("Edit Account", "Update account settings", "Save Changes"),
    };

    let revokers = ui::Revokers::default();
    let root = Grid::new().context("could not create the wizard layout")?;
    ui::auto_row(&root)?;
    ui::star_row(&root)?;
    ui::auto_row(&root)?;

    let header = ui::stack_xs(mailin_winui_sys::Orientation::Vertical)?;
    let title = ui::heading1(title_text)?;
    ui::margin_horizontal(&title, 20.0)?;
    ui::margin_vertical(&title, 10.0)?;
    let subtitle = ui::text(subtitle_text)?;
    ui::margin_horizontal(&subtitle, 20.0)?;
    ui::margin(&subtitle, 20.0, 0.0, 20.0, 8.0)?;
    ui::append(&header, &title)?;
    ui::append(&header, &subtitle)?;
    ui::set_row(&header, 0)?;
    ui::append(&root, &header)?;

    let form = ui::stack_lg(mailin_winui_sys::Orientation::Vertical)?;
    ui::margin_lg(&form)?;
    ui::margin_vertical(&form, 8.0)?;

    let identity = ui::subheading("Account")?;
    ui::append(&form, &identity)?;

    let name = ui::text_box("Ada Lovelace", Some("Account name"))?;
    let email = ui::text_box("ada@example.com", Some("Email address"))?;
    let password = ui::password_box("Password")?;
    let auth_method = ui::combo(
        &["Password", "OAuth2 (recommended)"],
        0,
        Some("Authentication"),
    )?;
    let password_field = ui::field("Password", &password)?;
    ui::append(&form, &ui::field("Display name", &name)?)?;
    ui::append(&form, &ui::field("Email address", &email)?)?;
    ui::append(&form, &ui::field("Authentication", &auth_method)?)?;
    ui::append(&form, &password_field)?;

    // OAuth2 sign-in section, hidden while password authentication is active.
    let oauth_panel = ui::stack_sm(mailin_winui_sys::Orientation::Vertical)?;
    let oauth_provider = ui::combo(&provider_labels(), 0, Some("OAuth2 provider"))?;
    let oauth_client_id = ui::text_box("OAuth2 client ID", Some("OAuth2 client ID"))?;
    let oauth_sign_in = ui::button(
        "Sign in with browser…",
        Some("Sign in with browser"),
        Some("Sign in with your mail provider via browser"),
    )?;
    let oauth_status = ui::text("")?;
    // Live region for sign-in status announcements
    AutomationProperties::SetLiveSetting(
        &oauth_status,
        mailin_winui_sys::AutomationLiveSetting::Polite,
    )?;
    ui::append(&oauth_panel, &ui::subheading("OAuth2 sign-in")?)?;
    ui::append(&oauth_panel, &ui::field("Provider", &oauth_provider)?)?;
    ui::append(
        &oauth_panel,
        &ui::field("OAuth2 client ID", &oauth_client_id)?,
    )?;
    ui::append(&oauth_panel, &oauth_sign_in)?;
    ui::append(&oauth_panel, &oauth_status)?;
    ui::set_visible(&oauth_panel, false)?;
    ui::append(&form, &oauth_panel)?;

    let incoming = ui::subheading("Incoming mail (IMAP)")?;
    ui::append(&form, &incoming)?;
    let imap_host = ui::text_box("imap.example.com", Some("IMAP server"))?;
    let imap_port = ui::text_box("993", Some("Port"))?;
    let imap_user = ui::text_box("Leave empty to use the email address", Some("Username"))?;
    let imap_security = ui::combo(&["TLS", "STARTTLS", "None"], 0, Some("Encryption"))?;
    ui::append(&form, &ui::field("IMAP server", &imap_host)?)?;
    ui::append(&form, &ui::field("Port", &imap_port)?)?;
    ui::append(&form, &ui::field("Username", &imap_user)?)?;
    ui::append(&form, &ui::field("Encryption", &imap_security)?)?;
    let imap_security_warning =
        ui::body_text("Warning: no encryption. Passwords and mail may be sent in clear text.")?;
    ui::set_visible(&imap_security_warning, false)?;
    ui::append(&form, &imap_security_warning)?;

    let outgoing = ui::subheading("Outgoing mail (SMTP)")?;
    ui::append(&form, &outgoing)?;
    let smtp_host = ui::text_box("smtp.example.com", Some("SMTP server"))?;
    let smtp_port = ui::text_box("465", Some("Port"))?;
    let smtp_user = ui::text_box("Leave empty to use the email address", Some("Username"))?;
    let smtp_security = ui::combo(&["TLS", "STARTTLS", "None"], 0, Some("Encryption"))?;
    ui::append(&form, &ui::field("SMTP server", &smtp_host)?)?;
    ui::append(&form, &ui::field("Port", &smtp_port)?)?;
    ui::append(&form, &ui::field("Username", &smtp_user)?)?;
    ui::append(&form, &ui::field("Encryption", &smtp_security)?)?;
    let smtp_security_warning =
        ui::body_text("Warning: no encryption. Passwords and mail may be sent in clear text.")?;
    ui::set_visible(&smtp_security_warning, false)?;
    ui::append(&form, &smtp_security_warning)?;

    // Signature: plain text, shown under the composer's `-- ` separator.
    let signature_heading = ui::subheading("Signature")?;
    let signature = ui::text_box("", Some("Signature"))?;
    signature.SetPlaceholderText(
        "Appended to messages you send. Mailin adds the “-- ” separator for you.",
    )?;
    signature.SetAcceptsReturn(true)?;
    signature.SetTextWrapping(mailin_winui_sys::TextWrapping::Wrap)?;
    signature.SetHeight(120.0)?;
    let signature_html_heading = ui::subheading("HTML signature")?;
    let signature_html = ui::text_box("", Some("HTML signature"))?;
    signature_html.SetPlaceholderText(
        "Optional HTML signature used by the rich composer. Scripts and event handlers are removed when composing.",
    )?;
    signature_html.SetAcceptsReturn(true)?;
    signature_html.SetTextWrapping(mailin_winui_sys::TextWrapping::Wrap)?;
    signature_html.SetHeight(120.0)?;
    ui::append(&form, &signature_heading)?;
    ui::append(&form, &signature)?;
    ui::append(&form, &signature_html_heading)?;
    ui::append(&form, &signature_html)?;

    // Remote-content allowlist: domains whose images and resources always load.
    let trust_heading = ui::subheading("Trusted senders for remote content")?;
    let trust_desc = ui::body_text(
        "Messages from these domains always load remote content, regardless of the global setting.",
    )?;
    ui::append(&form, &trust_heading)?;
    ui::append(&form, &trust_desc)?;
    let trust_list =
        mailin_winui_sys::ListView::new().context("could not create the trust list")?;
    trust_list.SetSelectionMode(mailin_winui_sys::ListViewSelectionMode::None)?;
    trust_list.SetHeight(120.0)?;
    ui::append(&form, &trust_list)?;

    // Contacts manager: per-account contacts for compose autocomplete.
    let contacts_heading = ui::subheading("Contacts")?;
    let contacts_desc = ui::body_text(
        "Manage contacts for autocomplete in compose. Auto-seeded from mail or added manually.",
    )?;
    ui::append(&form, &contacts_heading)?;
    ui::append(&form, &contacts_desc)?;
    let contacts_list =
        mailin_winui_sys::ListView::new().context("could not create the contacts list")?;
    contacts_list.SetSelectionMode(mailin_winui_sys::ListViewSelectionMode::None)?;
    contacts_list.SetHeight(120.0)?;
    ui::append(&form, &contacts_list)?;

    let scroller = ui::scroll(&form)?;
    ui::set_row(&scroller, 1)?;
    ui::append(&root, &scroller)?;

    let footer = ui::stack_sm(mailin_winui_sys::Orientation::Horizontal)?;
    ui::margin_lg(&footer)?;
    ui::margin_vertical(&footer, 8.0)?;
    let error: TextBlock = ui::text("")?;
    let _ = error.SetIsTextSelectionEnabled(true);
    // Live region for validation/connection error announcements
    AutomationProperties::SetLiveSetting(
        &error,
        mailin_winui_sys::AutomationLiveSetting::Assertive,
    )?;
    ui::set_visible(&error, false)?;
    ui::append(&footer, &error)?;
    let spacer = ui::text("")?;
    ui::stretch_horizontal(&spacer)?;
    ui::append(&footer, &spacer)?;

    let test = ui::button(
        "Test Connection",
        Some("Test Connection"),
        Some("Test IMAP and SMTP connection"),
    )?;
    let cancel = ui::button("Cancel", Some("Cancel"), Some("Cancel without saving"))?;
    let save = ui::button(
        save_label,
        Some("Save account"),
        Some("Save account settings"),
    )?;

    // Remove button for edit mode
    let remove = match mode {
        AccountDialogMode::Edit => Some(ui::destructive_button(
            "Remove Account",
            Some("Remove Account"),
            Some("Permanently delete this account and all its data"),
        )?),
        AccountDialogMode::Add => None,
    };

    if let Some(remove) = &remove {
        ui::append(&footer, remove)?;
    }
    ui::append(&footer, &test)?;
    ui::append(&footer, &cancel)?;
    ui::append(&footer, &save)?;
    ui::set_row(&footer, 2)?;
    ui::append(&root, &footer)?;

    let window = Window::new().context("could not create the wizard window")?;
    window.SetTitle(title_text)?;
    window.SetContent(&root)?;

    let oauth_tokens: Arc<Mutex<OAuth2Tokens>> = Arc::new(Mutex::new(OAuth2Tokens::default()));
    let previous_provider = Arc::new(Mutex::new(OAuthProvider::default()));
    let allowlist: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(
        existing_account
            .as_ref()
            .map(|account| account.remote_allowed_domains.clone())
            .unwrap_or_default(),
    ));

    // Trust list container: a vertical stack of rows (domain + remove button)
    let trust_container = ui::stack_xs(mailin_winui_sys::Orientation::Vertical)?;
    ui::append(&form, &trust_container)?;

    // Helper to rebuild the trust list UI
    fn repopulate_trust_list(
        trust_container: &mailin_winui_sys::StackPanel,
        allowlist: &Arc<Mutex<Vec<String>>>,
    ) -> Result<()> {
        let children = trust_container
            .Children()
            .context("could not read the trust list")?;
        let _ = children.Clear();
        for domain in allowlist
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
        {
            let row = ui::stack_sm(mailin_winui_sys::Orientation::Horizontal)?;
            let label = ui::text(domain)?;
            label.SetVerticalAlignment(mailin_winui_sys::VerticalAlignment::Center)?;
            ui::stretch_horizontal(&label)?;
            ui::append(&row, &label)?;
            let remove = ui::button(
                "Remove",
                Some(&format!("Stop trusting {domain}")),
                Some(&format!("Remove {domain} from trusted senders")),
            )?;
            let tokens = design_tokens::tokens();
            remove.SetPadding(mailin_winui_sys::Thickness {
                left: tokens.spacing.xs,
                top: 2.0,
                right: tokens.spacing.xs,
                bottom: 2.0,
            })?;
            ui::append(&row, &remove)?;
            children.Append(&row)?;

            let allowlist = Arc::clone(allowlist);
            let trust_container = trust_container.clone();
            let domain = domain.clone();
            ui::on_click(&remove, move || {
                allowlist
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .retain(|entry| entry != &domain);
                let _ = repopulate_trust_list(&trust_container, &allowlist);
            })?;
        }
        Ok(())
    }
    repopulate_trust_list(&trust_container, &allowlist)?;

    // Contacts container: a vertical stack of rows (email + name + edit/remove buttons)
    let contacts_container = ui::stack_xs(mailin_winui_sys::Orientation::Vertical)?;
    ui::append(&form, &contacts_container)?;

    // Helper to rebuild the contacts list UI
    fn repopulate_contacts_list(
        contacts_container: &mailin_winui_sys::StackPanel,
        app: &Arc<AppState>,
        account_id: AccountId,
        window: &Window,
        revokers: &ui::Revokers,
    ) -> Result<()> {
        let children = contacts_container
            .Children()
            .context("could not read the contacts list")?;
        let _ = children.Clear();
        if let Ok(contacts) = app.mailin.store().list_contacts(&account_id) {
            for contact in contacts {
                let row = ui::stack_sm(mailin_winui_sys::Orientation::Horizontal)?;
                let labels = ui::stack_xs(mailin_winui_sys::Orientation::Vertical)?;
                let email_label = ui::text(&contact.email)?;
                ui::append(&labels, &email_label)?;
                if !contact.display_name.is_empty() {
                    let name_label = ui::caption(&contact.display_name)?;
                    ui::append(&labels, &name_label)?;
                }
                ui::stretch_horizontal(&labels)?;
                ui::append(&row, &labels)?;

                let edit = ui::button(
                    "Edit",
                    Some(&format!("Edit contact {}", contact.email)),
                    Some("Edit this contact"),
                )?;
                let tokens = design_tokens::tokens();
                edit.SetPadding(mailin_winui_sys::Thickness {
                    left: tokens.spacing.xs,
                    top: 2.0,
                    right: tokens.spacing.xs,
                    bottom: 2.0,
                })?;
                ui::append(&row, &edit)?;

                let remove = ui::button(
                    "Remove",
                    Some(&format!("Remove contact {}", contact.email)),
                    Some("Delete this contact"),
                )?;
                remove.SetPadding(mailin_winui_sys::Thickness {
                    left: tokens.spacing.xs,
                    top: 2.0,
                    right: tokens.spacing.xs,
                    bottom: 2.0,
                })?;
                ui::append(&row, &remove)?;

                children.Append(&row)?;

                let app = Arc::clone(app);
                let contacts_container = contacts_container.clone();
                let window = window.clone();
                let revokers = revokers.clone();
                let account_id = account_id;
                let contact_email = contact.email.clone();
                let contact_name = contact.display_name.clone();
                ui::on_click(&edit, move || {
                    if let Err(failure) = new_contact_dialog(
                        &window,
                        Arc::clone(&app),
                        account_id,
                        &contact_email,
                        &contact_name,
                        true, // is_edit
                    ) {
                        tracing::error!(%failure, "could not open edit contact dialog");
                    }
                })?;

                let app = Arc::clone(app);
                let contacts_container = contacts_container.clone();
                let window = window.clone();
                let revokers = revokers.clone();
                let account_id = account_id;
                let contact_email = contact.email.clone();
                ui::on_click(&remove, move || {
                    if let Err(failure) = app
                        .mailin
                        .store()
                        .delete_contact(&account_id, &contact_email)
                    {
                        tracing::error!(%failure, "could not delete contact");
                    } else {
                        let _ = repopulate_contacts_list(
                            &contacts_container,
                            &app,
                            account_id,
                            &window,
                            &revokers,
                        );
                    }
                })?;
            }
        }
        Ok(())
    }
    repopulate_contacts_list(&contacts_container, &app, account_id, &window, &revokers)?;

    // Pre-fill form if editing
    if let Some(account) = existing_account.clone() {
        let _ = name.SetText(&account.display_name);
        let _ = email.SetText(&account.email);
        // Password left empty - user can enter new one to change it

        let oauth = account.imap.auth_method == AuthMethod::OAuth2;
        ui::select_index(&auth_method, auth_index(account.imap.auth_method));
        let _ = ui::set_visible(&oauth_panel, oauth);
        let _ = ui::set_visible(&password_field, !oauth);
        if oauth {
            if let Ok(mut tokens) = oauth_tokens.lock() {
                *tokens = account.imap.oauth2.clone();
            }
            ui::select_index(
                &oauth_provider,
                provider_index(account.imap.oauth2.provider),
            );
            let _ = oauth_client_id.SetText(&account.imap.oauth2.client_id);
            let _ = oauth_status.SetText("Signed in. Sign in again to replace the token.");
            let _ = ui::set_visible(&oauth_status, true);
            if let Ok(mut previous) = previous_provider.lock() {
                *previous = account.imap.oauth2.provider;
            }
        }

        let _ = imap_host.SetText(&account.imap.host);
        let _ = imap_port.SetText(&account.imap.port.to_string());
        let _ = imap_user.SetText(&account.imap.username);
        ui::select_index(&imap_security, security_index(account.imap.security));
        let _ = ui::set_visible(
            &imap_security_warning,
            account.imap.security == Security::None,
        );

        let _ = smtp_host.SetText(&account.smtp.host);
        let _ = smtp_port.SetText(&account.smtp.port.to_string());
        let _ = smtp_user.SetText(&account.smtp.username);
        ui::select_index(&smtp_security, security_index(account.smtp.security));
        let _ = ui::set_visible(
            &smtp_security_warning,
            account.smtp.security == Security::None,
        );

        // Disable email field in edit mode (account identity shouldn't change)
        let _ = ui::set_enabled(&email, false);

        // Pre-fill signature
        let _ = signature.SetText(&account.signature);
        let _ = signature_html.SetText(&account.signature_html);

        // Pre-fill trust list (already populated from allowlist above)
    }

    revokers.keep(ui::on_selection_changed(&auth_method, {
        let oauth_panel = oauth_panel.clone();
        let password_field = password_field.clone();
        let auth_method = auth_method.clone();
        move || {
            let oauth = ui::selected_index(&auth_method) == 1;
            let _ = ui::set_visible(&oauth_panel, oauth);
            let _ = ui::set_visible(&password_field, !oauth);
        }
    })?);

    revokers.keep(ui::on_selection_changed(&oauth_provider, {
        let oauth_provider = oauth_provider.clone();
        let oauth_client_id = oauth_client_id.clone();
        let oauth_tokens = Arc::clone(&oauth_tokens);
        let previous_provider = Arc::clone(&previous_provider);
        move || {
            let provider = provider_at(ui::selected_index(&oauth_provider));
            let previous = previous_provider
                .lock()
                .map(|mut slot| std::mem::replace(&mut *slot, provider))
                .unwrap_or_default();
            if let Ok(mut tokens) = oauth_tokens.lock()
                && tokens.provider != provider
            {
                tokens.access_token.clear();
                tokens.refresh_token.clear();
                tokens.expires_at = 0;
                tokens.provider = provider;
            }
            let current = ui::text_value(&oauth_client_id);
            if current.trim().is_empty() || current == previous.default_client_id() {
                let _ = oauth_client_id.SetText(&provider.default_client_id());
            }
        }
    })?);

    // M4-F: warn when encryption is disabled.
    revokers.keep(ui::on_selection_changed(&imap_security, {
        let imap_security = imap_security.clone();
        let warning = imap_security_warning.clone();
        move || {
            let none = ui::selected_index(&imap_security) == 2;
            let _ = ui::set_visible(&warning, none);
        }
    })?);
    revokers.keep(ui::on_selection_changed(&smtp_security, {
        let smtp_security = smtp_security.clone();
        let warning = smtp_security_warning.clone();
        move || {
            let none = ui::selected_index(&smtp_security) == 2;
            let _ = ui::set_visible(&warning, none);
        }
    })?);

    revokers.keep(ui::on_click(&oauth_sign_in, {
        let window = window.clone();
        let email = email.clone();
        let oauth_provider = oauth_provider.clone();
        let oauth_client_id = oauth_client_id.clone();
        let oauth_status = oauth_status.clone();
        let oauth_tokens = Arc::clone(&oauth_tokens);
        move || {
            let provider = provider_at(ui::selected_index(&oauth_provider));
            let client_id = ui::text_value(&oauth_client_id);
            let hint = ui::text_value(&email);
            let pending = match PendingAuthorization::begin(provider, &client_id, Some(&hint)) {
                Ok(pending) => pending,
                Err(error) => {
                    let _ = oauth_status.SetText(&error.to_string());
                    let _ = ui::set_visible(&oauth_status, true);
                    return;
                }
            };
            let url = pending.auth_url().to_owned();
            if let Err(error) = std::process::Command::new("cmd")
                .args(["/C", "start", "", &url])
                .spawn()
            {
                let _ = oauth_status.SetText(&format!("Could not open the browser: {error}"));
                let _ = ui::set_visible(&oauth_status, true);
                return;
            }
            let _ = oauth_status.SetText("Waiting for the browser to complete sign-in…");
            let _ = ui::set_visible(&oauth_status, true);

            let window = window.clone();
            let oauth_status = oauth_status.clone();
            let oauth_tokens = Arc::clone(&oauth_tokens);
            std::thread::spawn(move || {
                let result = pending.wait(std::time::Duration::from_secs(300));
                // The dispatcher callback is `Fn`, so hand it owned copies
                // instead of the `Result` itself.
                let (tokens, failure) = match result {
                    Ok(tokens) => (Some(tokens), None),
                    Err(error) => (None, Some(error.to_string())),
                };
                let enqueue = window.DispatcherQueue().and_then(|queue| {
                    let handler = mailin_winui_sys::DispatcherQueueHandler::new(move || {
                        if let Some(tokens) = &tokens {
                            if let Ok(mut slot) = oauth_tokens.lock() {
                                *slot = tokens.clone();
                            }
                            let _ = oauth_status.SetText("Signed in successfully.");
                        } else if let Some(failure) = &failure {
                            let _ = oauth_status.SetText(&format!("Sign-in failed: {failure}"));
                        }
                        let _ = ui::set_visible(&oauth_status, true);
                    });
                    queue.TryEnqueue(handler.to_ref())
                });
                if let Err(error) = enqueue {
                    tracing::warn!(%error, "could not report the sign-in result on the UI thread");
                }
            });
        }
    })?);

    // Fill server fields from the address domain while they are still the
    // values the previous suggestion put there. Anything the user typed wins.
    let last_suggestion: Arc<Mutex<Option<Suggestion>>> = Arc::new(Mutex::new(None));
    let programmatic = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rows = FormRows {
        name,
        email,
        password,
        auth_method,
        oauth_provider,
        oauth_client_id,
        oauth_tokens: Arc::clone(&oauth_tokens),
        imap_host,
        imap_port,
        imap_user,
        imap_security,
        smtp_host,
        smtp_port,
        smtp_user,
        smtp_security,
        signature,
        signature_html,
        trust_container,
        allowlist: Arc::clone(&allowlist),
        contacts_container,
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

    let test_button = test.clone();
    revokers.keep(ui::on_click(&test, {
        let window = window.clone();
        let app = Arc::clone(&app);
        let test_button = test_button.clone();
        let rows = FormRows {
            name: rows.name.clone(),
            email: rows.email.clone(),
            password: rows.password.clone(),
            auth_method: rows.auth_method.clone(),
            oauth_provider: rows.oauth_provider.clone(),
            oauth_client_id: rows.oauth_client_id.clone(),
            oauth_tokens: Arc::clone(&rows.oauth_tokens),
            imap_host: rows.imap_host.clone(),
            imap_port: rows.imap_port.clone(),
            imap_user: rows.imap_user.clone(),
            imap_security: rows.imap_security.clone(),
            smtp_host: rows.smtp_host.clone(),
            smtp_port: rows.smtp_port.clone(),
            smtp_user: rows.smtp_user.clone(),
            smtp_security: rows.smtp_security.clone(),
            signature: rows.signature.clone(),
            signature_html: rows.signature_html.clone(),
            trust_container: rows.trust_container.clone(),
            allowlist: Arc::clone(&rows.allowlist),
        };
        let error = error.clone();
        move || {
            let draft = rows.draft();
            let password = draft.password.clone();
            // In edit mode, we don't require password for test if it's empty
            let require_password = mode == AccountDialogMode::Add;
            let account = match draft.build(require_password) {
                Ok(account) => account,
                Err(message) => {
                    let _ = error.SetText(&message);
                    let _ = ui::set_visible(&error, true);
                    return;
                }
            };
            let _ = error.SetText("Testing connection…");
            let _ = ui::set_visible(&error, true);
            let _ = ui::set_enabled(&test_button, false);
            let app = Arc::clone(&app);
            let error = error.clone();
            let window = window.clone();
            let test_button = test_button.clone();
            std::thread::spawn(move || {
                // The engine owns the shared runtime, so create a short-lived
                // one here (only on the worker thread, never on the UI
                // thread). The button is disabled until this completes, so
                // the runtime cannot be created concurrently.
                let result = tokio::runtime::Runtime::new().map(|runtime| {
                    runtime.block_on(app.mailin.test_connection(
                        &account.imap,
                        &account.smtp,
                        &password,
                    ))
                });
                let (message, _is_error) = match result {
                    Ok(Ok(())) => ("IMAP: ✓ · SMTP: ✓  Connection successful.".to_owned(), false),
                    Ok(Err(e)) => {
                        let msg = e.to_string();
                        let summary = if msg.contains("IMAP connection failed") {
                            format!("IMAP: failed · SMTP: not tested. {msg}")
                        } else if msg.contains("SMTP connection failed") {
                            format!("IMAP: ✓ · SMTP: failed. {msg}")
                        } else {
                            format!("Connection failed: {msg}")
                        };
                        (summary, true)
                    }
                    Err(runtime_error) => (
                        format!("Could not start the connection test: {runtime_error}"),
                        true,
                    ),
                };
                // XAML objects are thread-affine: hand the result back to the
                // UI thread via the dispatcher queue.
                let enqueue = window.DispatcherQueue().and_then(|queue| {
                    let handler = mailin_winui_sys::DispatcherQueueHandler::new(move || {
                        let _ = error.SetText(&message);
                        let _ = ui::set_visible(&error, true);
                        let _ = ui::set_enabled(&test_button, true);
                    });
                    queue.TryEnqueue(handler.to_ref())
                });
                if let Err(enqueue_error) = enqueue {
                    tracing::warn!(%enqueue_error, "could not report the connection test on the UI thread");
                }
            });
        }
    })?);

    // Clone for use in both save and remove closures
    let existing_account_for_save = existing_account.clone();
    let existing_account_for_remove = existing_account.clone();

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
            auth_method: rows.auth_method.clone(),
            oauth_provider: rows.oauth_provider.clone(),
            oauth_client_id: rows.oauth_client_id.clone(),
            oauth_tokens: Arc::clone(&rows.oauth_tokens),
            imap_host: rows.imap_host.clone(),
            imap_port: rows.imap_port.clone(),
            imap_user: rows.imap_user.clone(),
            imap_security: rows.imap_security.clone(),
            smtp_host: rows.smtp_host.clone(),
            smtp_port: rows.smtp_port.clone(),
            smtp_user: rows.smtp_user.clone(),
            smtp_security: rows.smtp_security.clone(),
            signature: rows.signature.clone(),
            signature_html: rows.signature_html.clone(),
            trust_container: rows.trust_container.clone(),
            allowlist: Arc::clone(&rows.allowlist),
        };
        let error = error.clone();
        move || {
            let draft = rows.draft();
            let password = draft.password.clone();
            let is_oauth = draft.auth_method == AuthMethod::OAuth2;
            // In edit mode, we don't require password if it's empty (keep existing)
            let require_password = mode == AccountDialogMode::Add;
            let mut account = match draft.build(require_password) {
                Ok(account) => account,
                Err(message) => {
                    let _ = error.SetText(&message);
                    let _ = ui::set_visible(&error, true);
                    return;
                }
            };
            // Keep the identifier across retries: a failed keyring write must
            // not leave an account row behind that a second click duplicates.
            // In edit mode, preserve the existing account ID and non-form fields:
            // sync settings and the remote-content allowlist are edited elsewhere
            // and must survive a settings save.
            if let Some(existing) = &existing_account_for_save {
                account.id = existing.id;
                account.sync = existing.sync.clone();
                account
                    .remote_allowed_domains
                    .clone_from(&existing.remote_allowed_domains);
            } else {
                if let Ok(mut slot) = account_id.lock() {
                    match *slot {
                        Some(id) => account.id = id,
                        None => *slot = Some(account.id),
                    }
                }
            }
            // Apply signatures from the form fields
            account.signature = ui::text_value(&rows.signature);
            account.signature_html = ui::text_value(&rows.signature_html);
            // Apply allowlist from the shared cell
            account
                .remote_allowed_domains
                .clone_from(&rows.allowlist.lock().unwrap_or_else(|err| err.into_inner()));
            // Only update password if user entered a new one. OAuth2 accounts
            // keep tokens in the account row and do not use the keyring.
            let secret = if password.is_empty() || is_oauth {
                None
            } else {
                Some(SecretString::from(password))
            };
            if let Err(failure) = app.mailin.save_account(&account, secret.as_ref()) {
                let _ = error.SetText(&failure.to_string());
                let _ = ui::set_visible(&error, true);
                return;
            }
            app.engine.sync_account(account.id);
            on_saved(account.id);
            let _ = window.Close();
        }
    })?);

    // Remove account handler (asks for confirmation first: the removal
    // deletes the account and all locally stored mail and cannot be undone)
    if let Some(remove) = remove {
        let window = window.clone();
        let app = Arc::clone(&app);
        let existing = existing_account_for_remove;
        let revokers_for_remove = revokers.clone();
        revokers.keep(ui::on_click(&remove, move || {
            let Some(account) = &existing else { return };
            let account_id = account.id;
            let email = account.email.clone();
            let window = window.clone();
            let app = Arc::clone(&app);
            let revokers_for_remove = revokers_for_remove.clone();

            let shown = (|| -> Result<()> {
                let dialog = mailin_winui_sys::ContentDialog::new()
                    .context("could not create the remove confirmation")?;
                dialog.SetTitle("Remove account?")?;
                let body = ui::text(&format!(
                    "Are you sure you want to remove '{email}'? This deletes the account and all locally stored mail on this device. This cannot be undone."
                ))?;
                ui::set_content(&dialog, &body)?;
                dialog.SetPrimaryButtonText("Remove")?;
                dialog.SetCloseButtonText("Cancel")?;
                dialog.SetDefaultButton(mailin_winui_sys::ContentDialogButton::Close)?;
                let element: mailin_winui_sys::IUIElement = dialog.cast()?;
                element.SetXamlRoot(&ui::window_xaml_root(&window)?)?;

                let window_for_click = window.clone();
                let app_for_click = Arc::clone(&app);
                let error_for_failure = error.clone();
                let revoker = dialog.PrimaryButtonClick(move |_, _| {
                    let window = window_for_click.clone();
                    let app = Arc::clone(&app_for_click);
                    let mailin = Arc::clone(&app.mailin);
                    let error = error_for_failure.clone();
                    std::thread::spawn(move || {
                        let outcome = mailin.remove_account(&account_id);
                        let closed = window.DispatcherQueue().and_then(|queue| {
                            let handler = mailin_winui_sys::DispatcherQueueHandler::new(move || {
                                match outcome {
                                    Ok(()) => {
                                        let _ = window.Close();
                                    }
                                    Err(error) => {
                                        let _ = error.SetText(&format!(
                                            "Could not remove the account: {error}"
                                        ));
                                        let _ = ui::set_visible(&error, true);
                                    }
                                }
                            });
                            queue.TryEnqueue(handler.to_ref())
                        });
                        if let Err(error) = closed {
                            tracing::warn!(%error, "could not report the removal result on the UI thread");
                        }
                    });
                })?;
                revokers_for_remove.keep(revoker);
                let _ = dialog.ShowAsync()?;
                Ok(())
            })();
            if let Err(error) = shown {
                tracing::warn!(%error, "could not ask to remove the account");
            }
        })?);
    }

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

fn auth_index(method: AuthMethod) -> usize {
    match method {
        AuthMethod::Password => 0,
        AuthMethod::OAuth2 => 1,
    }
}

fn auth_at(index: usize) -> AuthMethod {
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

fn provider_index(provider: OAuthProvider) -> usize {
    match provider {
        OAuthProvider::Google => 0,
        OAuthProvider::Microsoft => 1,
        OAuthProvider::Yahoo => 2,
        OAuthProvider::Fastmail => 3,
        OAuthProvider::Custom => 4,
    }
}

fn provider_at(index: usize) -> OAuthProvider {
    match index {
        1 => OAuthProvider::Microsoft,
        2 => OAuthProvider::Yahoo,
        3 => OAuthProvider::Fastmail,
        4 => OAuthProvider::Custom,
        _ => OAuthProvider::Google,
    }
}

/// Dialog to create or edit a contact.
fn new_contact_dialog(
    parent: &Window,
    app: &Arc<AppState>,
    account_id: AccountId,
    current_email: &str,
    current_name: &str,
    is_edit: bool,
) -> Result<()> {
    let (title, save_label) = if is_edit {
        ("Edit Contact", "Save")
    } else {
        ("New Contact", "Add")
    };

    let dialog = mailin_winui_sys::ContentDialog::new()?;
    dialog.SetTitle(title)?;
    dialog.SetPrimaryButtonText(save_label)?;
    dialog.SetCloseButtonText("Cancel")?;
    dialog.SetDefaultButton(mailin_winui_sys::ContentDialogButton::Primary)?;

    let panel = ui::stack_xs(mailin_winui_sys::Orientation::Vertical)?;
    ui::margin_lg(&panel)?;
    ui::margin_vertical(&panel, 8.0)?;

    let email_label = ui::text("Email")?;
    let email_box = ui::text_box(current_email, Some("Email address"))?;
    ui::append(&panel, &email_label)?;
    ui::append(&panel, &email_box)?;

    let name_label = ui::text("Display name (optional)")?;
    let name_box = ui::text_box(current_name, Some("Display name"))?;
    ui::append(&panel, &name_label)?;
    ui::append(&panel, &name_box)?;

    ui::set_content(&dialog, &panel)?;

    let app = Arc::clone(app);
    let parent = parent.clone();
    let account_id = account_id;
    let current_email = current_email.to_owned();

    dialog.PrimaryButtonClick({
        let dialog = dialog.clone();
        let email_box = email_box.clone();
        let name_box = name_box.clone();
        let app = Arc::clone(&app);
        let parent = parent.clone();
        let account_id = account_id;
        let current_email = current_email.clone();
        move |_, _| {
            let email = ui::text_value(&email_box).trim().to_owned();
            let display_name = ui::text_value(&name_box);
            if !email.is_empty() && email.contains('@') {
                // If editing and email changed, delete old contact
                if is_edit && email != current_email {
                    let _ = app
                        .mailin
                        .store()
                        .delete_contact(&account_id, &current_email);
                }
                let _ = app
                    .mailin
                    .store()
                    .upsert_contact(&account_id, &email, &display_name, true);
            }
            let _ = dialog.Hide();
        }
    })?;

    let _ = dialog.ShowAsync()?;
    Ok(())
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
    fn oauth2_requires_an_access_token() {
        let mut draft = sample();
        draft.password.clear();
        draft.auth_method = AuthMethod::OAuth2;
        assert!(draft.build(true).is_err());
        draft.oauth2.access_token = "token".into();
        let account = draft.build(true).expect("OAuth2 draft");
        assert_eq!(account.imap.auth_method, AuthMethod::OAuth2);
        assert_eq!(account.smtp.auth_method, AuthMethod::OAuth2);
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
}
