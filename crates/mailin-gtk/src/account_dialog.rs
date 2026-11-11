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
use gtk::accessible::Property;
use gtk::glib;
use mailin_core::{
    Account, AccountId, AuthMethod, OAuth2Tokens, OAuthProvider, PendingAuthorization, Security,
    ServerSettings,
};
use secrecy::ExposeSecret;

use crate::app::AppState;
use crate::autoconfig::{self, Suggestion};

/// Error/status line at the bottom of the wizard. Both semantic classes are
/// always cleared first so stale styling (e.g. a red "Testing connection…"
/// line that later turns into a success message) never leaks.
fn set_status_message(label: &gtk::Label, text: &str, kind: &str) {
    label.remove_css_class("error");
    label.remove_css_class("success");
    label.set_text(text);
    match kind {
        "error" | "success" => label.add_css_class(kind),
        _ => {} // plain interstitial text, e.g. "Testing connection…"
    }
    label.set_visible(true);
}

/// Hide the status line and drop any styling it carried.
fn clear_status_message(label: &gtk::Label) {
    label.remove_css_class("error");
    label.remove_css_class("success");
    label.set_text("");
    label.set_visible(false);
}

/// A shared, replaceable refresh callback stored behind interior mutability.
type RefreshFn = Rc<RefCell<Option<Box<dyn Fn()>>>>;

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
    let allowlist: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(
        existing_account
            .as_ref()
            .map(|account| account.remote_allowed_domains.clone())
            .unwrap_or_default(),
    ));

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
    // Alert role so sign-in status changes are announced by screen readers.
    oauth_status.set_accessible_role(gtk::AccessibleRole::Alert);

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

    let server_default_subtitle =
        "IMAP and SMTP settings — auto-detected when possible. Edit to override.";
    let server_group = adw::PreferencesGroup::new();
    let server_expander = adw::ExpanderRow::builder()
        .title("Server settings")
        .subtitle(server_default_subtitle)
        .show_enable_switch(false)
        .build();
    let autoconfig_reset = gtk::Button::with_label("Reset to suggested");
    autoconfig_reset.add_css_class("flat");
    autoconfig_reset.set_visible(false);
    autoconfig_reset.set_valign(gtk::Align::Center);
    server_expander.add_suffix(&autoconfig_reset);
    server_group.add(&server_expander);

    let imap_header = adw::ActionRow::builder()
        .title("Incoming mail (IMAP)")
        .subtitle("Folder sync and message bodies")
        .activatable(false)
        .build();
    imap_header.add_css_class("heading");
    let imap_security_warning = adw::ActionRow::builder()
        .title("Unencrypted connection")
        .subtitle("Credentials and mail are sent without encryption. Use only on trusted networks.")
        .build();
    imap_security_warning.add_css_class("warning");
    imap_security_warning.set_visible(false);
    let smtp_header = adw::ActionRow::builder()
        .title("Outgoing mail (SMTP)")
        .subtitle("Sends messages from the outbox")
        .activatable(false)
        .build();
    smtp_header.add_css_class("heading");
    let smtp_security_warning = adw::ActionRow::builder()
        .title("Unencrypted connection")
        .subtitle("Credentials and mail are sent without encryption. Use only on trusted networks.")
        .build();
    smtp_security_warning.add_css_class("warning");
    smtp_security_warning.set_visible(false);

    server_expander.add_row(&imap_header);
    server_expander.add_row(&imap_host);
    server_expander.add_row(&imap_port);
    server_expander.add_row(&imap_security);
    server_expander.add_row(&imap_security_warning);
    server_expander.add_row(&imap_user);
    server_expander.add_row(&smtp_header);
    server_expander.add_row(&smtp_host);
    server_expander.add_row(&smtp_port);
    server_expander.add_row(&smtp_security);
    server_expander.add_row(&smtp_security_warning);
    server_expander.add_row(&smtp_user);

    // Compatibility aliases for existing code that references the groups/captions.
    let incoming = server_expander.clone();
    let outgoing = server_expander.clone();
    let incoming_default_desc = server_default_subtitle;
    let outgoing_default_desc = server_default_subtitle;

    let update_security_warnings = {
        let imap_security = imap_security.clone();
        let smtp_security = smtp_security.clone();
        let imap_security_warning = imap_security_warning.clone();
        let smtp_security_warning = smtp_security_warning.clone();
        move || {
            imap_security_warning
                .set_visible(security_at(imap_security.selected()) == Security::None);
            smtp_security_warning
                .set_visible(security_at(smtp_security.selected()) == Security::None);
        }
    };
    update_security_warnings();
    imap_security.connect_selected_notify({
        let update = update_security_warnings.clone();
        move |_| update()
    });
    smtp_security.connect_selected_notify({
        let update = update_security_warnings;
        move |_| update()
    });

    // Signature: plain text, shown under the composer's `-- ` separator.
    let signature_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .accepts_tab(false)
        .build();
    signature_view.set_tooltip_text(Some(
        "Appended to messages you send. Mailin adds the “-- ” separator for you.",
    ));
    // Bind the buffer to the *visible* view so edit-mode prefill and save both
    // reach the field the user actually sees.
    let signature_buffer = Rc::new(RefCell::new(signature_view.buffer()));
    // Give the editor an accessible name; the bare PreferencesRow has no title.
    {
        signature_view.update_property(&[Property::Label("Signature")]);
    }
    let signature_row = adw::ActionRow::builder().title("Signature").build();
    let signature_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(120)
        .child(&signature_view)
        .build();
    signature_scroll.set_valign(gtk::Align::Start);
    signature_row.add_suffix(&signature_scroll);
    signature_row.set_activatable(false);

    let signatures = adw::PreferencesGroup::builder()
        .title("Signature")
        .description(
            "Plain text and optional HTML signature, appended below the RFC 3676 “-- ” separator.",
        )
        .build();
    signatures.add(&signature_row);

    // HTML signature: sanitized before it is inserted into a rich composer.
    let signature_html_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .accepts_tab(false)
        .build();
    signature_html_view.set_tooltip_text(Some(
        "Optional HTML signature used by the rich composer. Scripts and event handlers are removed when composing.",
    ));
    let signature_html_buffer = Rc::new(RefCell::new(signature_html_view.buffer()));
    {
        signature_html_view.update_property(&[Property::Label("HTML signature")]);
    }
    let signature_html_row = adw::ActionRow::builder().title("HTML signature").build();
    let signature_html_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(120)
        .child(&signature_html_view)
        .build();
    signature_html_scroll.set_valign(gtk::Align::Start);
    signature_html_row.add_suffix(&signature_html_scroll);
    signature_html_row.set_activatable(false);
    signatures.add(&signature_html_row);

    // Remote-content allowlist: domains whose images and resources always load.
    let trust_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let trust_placeholder = gtk::Label::builder()
        .label("No senders trusted yet. Use “Always allow @sender” in the reader to add one.")
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    trust_list.set_placeholder(Some(&trust_placeholder));
    let trust_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(120)
        .child(&trust_list)
        .build();
    let trust_group = adw::PreferencesGroup::builder()
        .title("Trusted senders for remote content")
        .description("Messages from these domains always load remote content, regardless of the global setting.")
        .build();
    trust_group.add(&trust_scroll);

    // Templates manager
    let templates_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let templates_placeholder = gtk::Label::builder()
        .label("No templates yet")
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    templates_list.set_placeholder(Some(&templates_placeholder));
    let templates_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(150)
        .child(&templates_list)
        .build();
    let templates_add = gtk::Button::with_label("New Template");
    templates_add.add_css_class("suggested-action");
    let templates_group = adw::PreferencesGroup::builder()
        .title("Message templates")
        .description(
            "Reusable plain-text message templates. Use {{variable}} placeholders in the composer.",
        )
        .build();
    templates_group.add(&templates_scroll);
    templates_group.set_header_suffix(Some(&templates_add));

    // Contacts manager
    let contacts_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let contacts_placeholder = gtk::Label::builder()
        .label("No contacts yet. Contacts are auto-seeded from sent/received mail, or add them manually.")
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    contacts_list.set_placeholder(Some(&contacts_placeholder));
    let contacts_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(150)
        .child(&contacts_list)
        .build();
    let contacts_add = gtk::Button::with_label("New Contact");
    contacts_add.add_css_class("suggested-action");
    let contacts_group = adw::PreferencesGroup::builder()
        .title("Contacts")
        .description(
            "Manage contacts for autocomplete in compose. Auto-seeded from mail or added manually.",
        )
        .build();
    contacts_group.add(&contacts_scroll);
    contacts_group.set_header_suffix(Some(&contacts_add));

    // M4-F: the wizard is split into two steps — "Account" holds the quick
    // form (identity and sign-in), "Server" holds the server details plus
    // the connection test. Editing an account uses the same two steps so
    // the layout never shifts under the user.
    let account_page = adw::PreferencesPage::new();
    account_page.add(&identity);
    account_page.add(&oauth_group);

    let server_page = adw::PreferencesPage::new();
    server_page.add(&server_group);
    server_page.add(&signatures);
    server_page.add(&trust_group);
    // Templates/contacts are per-account and require a saved account id.
    if mode == AccountDialogMode::Add {
        let hint = adw::PreferencesGroup::builder()
            .title("Templates and contacts")
            .description("Save the account first to manage message templates and contacts.")
            .build();
        server_page.add(&hint);
    }
    templates_group.set_visible(mode == AccountDialogMode::Edit);
    contacts_group.set_visible(mode == AccountDialogMode::Edit);
    server_page.add(&templates_group);
    server_page.add(&contacts_group);

    let stack = adw::ViewStack::new();
    stack.add_titled(&account_page, Some("account"), "Account");
    stack.add_titled(&server_page, Some("server"), "Server");
    stack.set_visible_child_name("account");
    stack.set_vexpand(true);

    let header = adw::HeaderBar::new();
    let window_title = adw::WindowTitle::new(title, subtitle);
    header.set_title_widget(Some(&window_title));

    // Step navigation.
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.add_css_class("flat");
    back.set_tooltip_text(Some("Back to the account details"));
    back.update_property(&[Property::Label("Back to the account details")]);
    let next = gtk::Button::with_label("Continue");
    next.add_css_class("suggested-action");
    header.pack_start(&back);
    header.pack_end(&next);

    let error_label = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .hexpand(true)
        .visible(false)
        .selectable(true)
        .build();
    // Alert role so validation/connection errors are announced.
    error_label.set_accessible_role(gtk::AccessibleRole::Alert);

    // M4-F: one row per connection-test stage (IMAP / SMTP).
    let stage_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    stage_box.set_margin_top(6);
    stage_box.set_margin_bottom(6);
    stage_box.set_margin_start(18);
    stage_box.set_margin_end(18);
    stage_box.set_visible(false);
    let imap_stage_label = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .selectable(true)
        .build();
    let smtp_stage_label = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .selectable(true)
        .build();
    for label in [&imap_stage_label, &smtp_stage_label] {
        label.add_css_class("caption");
        stage_box.append(label);
    }

    let copy_error = gtk::Button::with_label("Copy error details");
    copy_error.add_css_class("flat");
    copy_error.set_visible(false);
    copy_error.connect_clicked({
        let error_label = error_label.clone();
        move |_| {
            let text = error_label.text().to_string();
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
        }
    });

    let cancel = gtk::Button::with_label("Cancel");
    let test = gtk::Button::with_label("Test Connection");
    let save = gtk::Button::with_label(save_label);
    save.add_css_class("suggested-action");

    // Busy indicator for the background save: it replaces the button's
    // label while the keyring and the store are written.
    let save_spinner = gtk::Spinner::new();
    save_spinner.set_visible(false);
    save_spinner.set_valign(gtk::Align::Center);

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
    actions.append(&copy_error);
    actions.append(&test);
    if let Some(remove) = &remove {
        actions.append(remove);
    }
    actions.append(&cancel);
    actions.append(&save_spinner);
    actions.append(&save);

    let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bottom.append(&stage_box);
    bottom.append(&actions);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    toolbar.add_bottom_bar(&bottom);

    // M4-F: the header navigation follows the visible step. In Add mode the
    // connection test and the save button belong to the "Verify" step, so
    // the first step stays a quick form.
    let update_step: Rc<dyn Fn(&str)> = {
        let back = back.clone();
        let next = next.clone();
        let test = test.clone();
        let save = save.clone();
        let window_title = window_title.clone();
        Rc::new(move |page: &str| {
            let on_account = page == "account";
            back.set_visible(!on_account);
            next.set_visible(on_account);
            if mode == AccountDialogMode::Add {
                test.set_visible(!on_account);
                save.set_visible(!on_account);
            }
            // The subtitle names the step so the user never loses their
            // place in the wizard.
            let step_subtitle = match (mode, on_account) {
                (AccountDialogMode::Add, true) => "Step 1 of 2 — enter your email address",
                (AccountDialogMode::Add, false) => {
                    "Step 2 of 2 — check the server settings and connect"
                }
                (AccountDialogMode::Edit, true) => "Update the account details",
                (AccountDialogMode::Edit, false) => "Server settings, templates and contacts",
            };
            window_title.set_subtitle(step_subtitle);
        })
    };
    {
        let stack_for_notify = stack.clone();
        let update_step_for_notify = Rc::clone(&update_step);
        stack.connect_visible_child_name_notify(move |_| {
            if let Some(name) = stack_for_notify.visible_child_name() {
                update_step_for_notify(&name);
            }
        });
        update_step("account");
    }
    {
        let stack = stack.clone();
        let name_row = name_row.clone();
        back.connect_clicked(move |_| {
            stack.set_visible_child_name("account");
            name_row.grab_focus();
        });
    }
    {
        let stack = stack.clone();
        let email_row = email_row.clone();
        let error_label = error_label.clone();
        let imap_host = imap_host.clone();
        next.connect_clicked(move |_| {
            let email = email_row.text().trim().to_owned();
            if email.is_empty() || !email.contains('@') {
                set_status_message(
                    &error_label,
                    "Enter an email address before continuing.",
                    "error",
                );
                email_row.grab_focus();
                return;
            }
            clear_status_message(&error_label);
            stack.set_visible_child_name("server");
            // M3a-5: move the focus into the new step.
            imap_host.grab_focus();
        });
    }

    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(560)
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
        // M4-F: email identifies the account and cannot be changed in place.
        email_row.set_sensitive(false);
        email_row.set_tooltip_text(Some("Email identifies the account and can't be changed."));
        // Password left empty - user can enter new one to change it

        signature_buffer
            .borrow()
            .set_text(account.signature.as_str());
        signature_html_buffer
            .borrow()
            .set_text(account.signature_html.as_str());

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
    let oauth_waiting = Rc::new(std::cell::Cell::new(false));
    let oauth_cancelled = Rc::new(std::cell::Cell::new(false));
    oauth_sign_in.connect_clicked({
        let oauth_tokens = Rc::clone(&oauth_tokens);
        let oauth_status = oauth_status.clone();
        let oauth_sign_in = oauth_sign_in.clone();
        let provider_row = oauth_provider_row.clone();
        let client_id_row = oauth_client_id_row.clone();
        let email_row = email_row.clone();
        let oauth_waiting = Rc::clone(&oauth_waiting);
        let oauth_cancelled = Rc::clone(&oauth_cancelled);
        move |_| {
            if oauth_waiting.get() {
                oauth_cancelled.set(true);
                oauth_waiting.set(false);
                oauth_sign_in.set_label("Sign in with browser…");
                oauth_status.set_text("Sign-in cancelled. You can try again.");
                oauth_status.remove_css_class("error");
                oauth_status.remove_css_class("success");
                oauth_status.set_visible(true);
                return;
            }

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
            oauth_waiting.set(true);
            oauth_cancelled.set(false);
            oauth_sign_in.set_label("Cancel sign-in");
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
            let oauth_waiting = Rc::clone(&oauth_waiting);
            let oauth_cancelled = Rc::clone(&oauth_cancelled);
            let started = std::time::Instant::now();
            glib::timeout_add_local(Duration::from_millis(200), move || {
                if oauth_cancelled.get() {
                    return glib::ControlFlow::Break;
                }
                if started.elapsed() > Duration::from_secs(12)
                    && oauth_status.text().as_str() == "Waiting for the browser to complete sign-in…"
                {
                    oauth_status.set_text(
                        "Still waiting for the browser to complete sign-in… You can cancel and try again.",
                    );
                }
                match receiver.try_recv() {
                    Ok(Ok(tokens)) => {
                        if oauth_cancelled.get() {
                            return glib::ControlFlow::Break;
                        }
                        *oauth_tokens.borrow_mut() = tokens;
                        oauth_waiting.set(false);
                        oauth_status.set_text("Signed in successfully.");
                        oauth_status.remove_css_class("error");
                        oauth_status.add_css_class("success");
                        oauth_status.set_visible(true);
                        oauth_sign_in.set_label("Sign in with browser…");
                        glib::ControlFlow::Break
                    }
                    Ok(Err(error)) => {
                        if oauth_cancelled.get() {
                            return glib::ControlFlow::Break;
                        }
                        oauth_waiting.set(false);
                        oauth_status.set_text(&format!("Sign-in failed: {error}"));
                        oauth_status.remove_css_class("success");
                        oauth_status.add_css_class("error");
                        oauth_status.set_visible(true);
                        oauth_sign_in.set_label("Sign in with browser…");
                        glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if oauth_cancelled.get() {
                            return glib::ControlFlow::Break;
                        }
                        oauth_waiting.set(false);
                        oauth_status.set_text("Sign-in was interrupted.");
                        oauth_status.set_visible(true);
                        oauth_sign_in.set_label("Sign in with browser…");
                        glib::ControlFlow::Break
                    }
                }
            });
        }
    });

    // Fill server fields from the address domain while they are still the
    // values we suggested ourselves. Anything the user typed is preserved.
    let last_suggestion: Rc<RefCell<Option<Suggestion>>> = Rc::new(RefCell::new(None));
    let autoconfig_applying = Rc::new(std::cell::Cell::new(false));
    let autoconfig_caption_active = Rc::new(std::cell::Cell::new(false));
    let clear_autoconfig_caption = {
        let incoming = incoming.clone();
        let _outgoing = outgoing.clone();
        let autoconfig_caption_active = Rc::clone(&autoconfig_caption_active);
        let autoconfig_reset = autoconfig_reset.clone();
        let incoming_default_desc = incoming_default_desc.to_owned();
        let _outgoing_default_desc = outgoing_default_desc.to_owned();
        move || {
            if autoconfig_caption_active.replace(false) {
                incoming.set_subtitle(&incoming_default_desc);
                autoconfig_reset.set_visible(false);
            }
        }
    };

    email_row.connect_changed({
        let imap_host = imap_host.clone();
        let imap_port = imap_port.clone();
        let imap_security = imap_security.clone();
        let smtp_host = smtp_host.clone();
        let smtp_port = smtp_port.clone();
        let smtp_security = smtp_security.clone();
        let last_suggestion = Rc::clone(&last_suggestion);
        let autoconfig_applying = Rc::clone(&autoconfig_applying);
        let autoconfig_caption_active = Rc::clone(&autoconfig_caption_active);
        let autoconfig_reset = autoconfig_reset.clone();
        let incoming = incoming.clone();
        let _outgoing = outgoing.clone();
        let incoming_default_desc = incoming_default_desc.to_owned();
        let _outgoing_default_desc = outgoing_default_desc.to_owned();
        move |row| {
            let email = row.text();
            let Some(suggestion) = autoconfig::suggest(&email) else {
                // No suggestion for this address; don't show stale caption.
                if autoconfig_caption_active.replace(false) {
                    incoming.set_subtitle(&incoming_default_desc);
                }
                return;
            };
            let previous = last_suggestion.borrow().clone();
            let domain = email
                .to_string()
                .split_once('@')
                .map(|(_, d)| d.trim().to_ascii_lowercase())
                .unwrap_or_default();

            autoconfig_applying.set(true);
            let mut applied = false;

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
                applied = true;
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
                applied = true;
            }

            *last_suggestion.borrow_mut() = Some(suggestion);
            autoconfig_applying.set(false);

            if applied && !domain.is_empty() {
                let caption = format!("Suggested for {domain} — edit to override");
                incoming.set_subtitle(&caption);
                autoconfig_caption_active.set(true);
                autoconfig_reset.set_visible(true);
            } else if !applied {
                // Couldn't auto-apply (user edited fields); don't show a stale suggestion.
                if autoconfig_caption_active.replace(false) {
                    incoming.set_subtitle(&incoming_default_desc);
                    autoconfig_reset.set_visible(false);
                }
            }
        }
    });

    // Manual edits to server fields clear the autoconfig caption.
    {
        let wire_clear = {
            let autoconfig_applying = Rc::clone(&autoconfig_applying);
            let clear = clear_autoconfig_caption.clone();
            move || {
                if !autoconfig_applying.get() {
                    clear();
                }
            }
        };
        for row in [&imap_host, &smtp_host, &imap_user, &smtp_user] {
            let c = wire_clear.clone();
            row.connect_changed(move |_| c());
        }
        for row in [&imap_port, &smtp_port] {
            let c = wire_clear.clone();
            row.connect_changed(move |_| c());
        }
        for combo in [&imap_security, &smtp_security] {
            let c = wire_clear.clone();
            combo.connect_selected_notify(move |_| c());
        }
    }

    autoconfig_reset.connect_clicked({
        let last_suggestion = Rc::clone(&last_suggestion);
        let autoconfig_applying = Rc::clone(&autoconfig_applying);
        let autoconfig_caption_active = Rc::clone(&autoconfig_caption_active);
        let autoconfig_reset = autoconfig_reset.clone();
        let incoming = incoming.clone();
        let _outgoing = outgoing.clone();
        let incoming_default_desc = incoming_default_desc.to_owned();
        let _outgoing_default_desc = outgoing_default_desc.to_owned();
        let imap_host = imap_host.clone();
        let imap_port = imap_port.clone();
        let imap_security = imap_security.clone();
        let smtp_host = smtp_host.clone();
        let smtp_port = smtp_port.clone();
        let smtp_security = smtp_security.clone();
        let email_row = email_row.clone();
        move |_| {
            let Some(suggestion) = last_suggestion.borrow().clone() else {
                return;
            };
            autoconfig_applying.set(true);
            imap_host.set_text(&suggestion.imap_host);
            imap_port.set_text(&suggestion.imap_port.to_string());
            imap_security.set_selected(security_index(suggestion.imap_security));
            smtp_host.set_text(&suggestion.smtp_host);
            smtp_port.set_text(&suggestion.smtp_port.to_string());
            smtp_security.set_selected(security_index(suggestion.smtp_security));
            autoconfig_applying.set(false);
            if let Some((_, domain)) = email_row.text().as_str().split_once('@') {
                let domain = domain.trim().to_ascii_lowercase();
                if !domain.is_empty() {
                    let caption = format!("Suggested for {domain} — edit to override");
                    incoming.set_subtitle(&caption);
                    autoconfig_caption_active.set(true);
                    autoconfig_reset.set_visible(true);
                    return;
                }
            }
            autoconfig_caption_active.set(false);
            autoconfig_reset.set_visible(false);
            incoming.set_subtitle(&incoming_default_desc);
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
        let copy_error = copy_error.clone();
        let test = test.clone();
        let stage_box = stage_box.clone();
        let imap_stage_label = imap_stage_label.clone();
        let smtp_stage_label = smtp_stage_label.clone();
        let existing_account_for_test = existing_account.clone();
        move |_| {
            let draft = rows.draft();
            let typed_password = draft.password.clone();
            // In edit mode, we don't require password for test if it's empty
            let require_password = mode == AccountDialogMode::Add;
            let account = match draft.build(require_password) {
                Ok(account) => account,
                Err(error) => {
                    copy_error.set_visible(false);
                    stage_box.set_visible(false);
                    set_status_message(&error_label, &error, "error");
                    return;
                }
            };
            test.set_sensitive(false);
            test.set_label("Testing…");
            copy_error.set_visible(false);
            stage_box.set_visible(true);
            imap_stage_label.set_text("IMAP: testing…");
            smtp_stage_label.set_text("SMTP: waiting…");
            set_status_message(&error_label, "Testing IMAP and SMTP connection…", "info");
            // An empty password field in edit mode means "keep the stored
            // password"; the probe must then use that stored secret, which
            // lives in the keyring. The lookup is blocking, so it runs on a
            // worker (see `Mailin::password`).
            let stored_id = if require_password || !typed_password.is_empty() {
                None
            } else {
                existing_account_for_test.as_ref().map(|account| account.id)
            };
            let run_probe = {
                let app = Rc::clone(&app);
                let error_label = error_label.clone();
                let copy_error = copy_error.clone();
                let test = test.clone();
                let imap_stage_label = imap_stage_label.clone();
                let smtp_stage_label = smtp_stage_label.clone();
                let account = account.clone();
                Rc::new(move |password: String| {
                    let app = Rc::clone(&app);
                    let error_label = error_label.clone();
                    let copy_error = copy_error.clone();
                    let test = test.clone();
                    let imap_stage_label = imap_stage_label.clone();
                    let smtp_stage_label = smtp_stage_label.clone();
                    let account = account.clone();
                    // M4-F: both stages are probed independently.
                    // The probe opens TLS connections: it must not run on the
                    // GTK main loop, which is not a tokio reactor either.
                    let mailin = std::sync::Arc::clone(&app.mailin);
                    crate::background::spawn_async(
                        move |runtime| {
                            runtime.block_on(mailin.test_connection_staged(
                                &account.imap,
                                &account.smtp,
                                &password,
                            ))
                        },
                        move |result| {
                            let report = match result {
                                Ok(report) => report,
                                Err(failed) => {
                                    test.set_sensitive(true);
                                    test.set_label("Test Connection");
                                    set_status_message(
                                        &error_label,
                                        &format!(
                                            "Could not test the connection: {}",
                                            failed.message
                                        ),
                                        "error",
                                    );
                                    copy_error.set_visible(false);
                                    return;
                                }
                            };
                            test.set_sensitive(true);
                            test.set_label("Test Connection");
                            for stage in report.stages() {
                                let label = match stage.stage {
                                    mailin_core::ConnectionStage::Imap => &imap_stage_label,
                                    mailin_core::ConnectionStage::Smtp => &smtp_stage_label,
                                };
                                label.set_text(&format!(
                                    "{}: {}",
                                    stage.stage.as_str(),
                                    if stage.ok {
                                        "✓ connected".to_owned()
                                    } else {
                                        format!("✗ {}", stage.error.clone().unwrap_or_default())
                                    }
                                ));
                                label.remove_css_class("success");
                                label.remove_css_class("error");
                                label.add_css_class(if stage.ok { "success" } else { "error" });
                            }
                            if report.all_ok() {
                                copy_error.set_visible(false);
                                set_status_message(
                                    &error_label,
                                    "IMAP: ✓ · SMTP: ✓  Connection successful.",
                                    "success",
                                );
                            } else {
                                let summary = report
                                    .error_summary()
                                    .unwrap_or_else(|| "Connection failed".to_owned());
                                set_status_message(&error_label, &summary, "error");
                                copy_error.set_visible(true);
                            }
                        },
                    );
                })
            };
            match stored_id {
                None => run_probe(typed_password),
                Some(account_id) => {
                    let mailin = std::sync::Arc::clone(&app.mailin);
                    let run_probe = Rc::clone(&run_probe);
                    let test = test.clone();
                    let stage_box = stage_box.clone();
                    let error_label = error_label.clone();
                    let copy_error = copy_error.clone();
                    crate::background::spawn_result(
                        move || {
                            mailin
                                .password(&account_id)
                                .map(|secret| secret.expose_secret().to_string())
                        },
                        move |result| match result {
                            Ok(Ok(password)) => run_probe(password),
                            Ok(Err(error)) => {
                                copy_error.set_visible(false);
                                stage_box.set_visible(false);
                                test.set_sensitive(true);
                                test.set_label("Test Connection");
                                set_status_message(
                                    &error_label,
                                    &format!("Could not read the stored password: {error}"),
                                    "error",
                                );
                            }
                            Err(worker) => {
                                copy_error.set_visible(false);
                                stage_box.set_visible(false);
                                test.set_sensitive(true);
                                test.set_label("Test Connection");
                                set_status_message(
                                    &error_label,
                                    &format!(
                                        "Could not read the stored password: {}",
                                        worker.message
                                    ),
                                    "error",
                                );
                            }
                        },
                    );
                }
            }
        }
    });

    // Clone existing_account for use in both save and remove closures
    let existing_account_for_save = existing_account.clone();
    let existing_account_for_remove = existing_account.clone();

    // Live view of the allowlist; each row carries a remove button that edits
    // the shared cell and repaints the list.
    fn repaint_trust_list(trust_list: &gtk::ListBox, allowlist: &Rc<RefCell<Vec<String>>>) {
        while let Some(child) = trust_list.row_at_index(0) {
            trust_list.remove(&child);
        }
        for domain in allowlist.borrow().iter() {
            let row = adw::ActionRow::builder().title(domain).build();
            row.set_activatable(false);
            // M2-G/M3a-2: untrust is non-destructive; avoid trash icon.
            // M4-F: share the row-button helper with the other list managers.
            let remove_name = format!("Stop trusting {domain}");
            let remove =
                suffix_icon_button("list-remove-symbolic", &remove_name, &remove_name, false);
            row.add_suffix(&remove);
            trust_list.append(&row);

            let allowlist = Rc::clone(allowlist);
            let trust_list = trust_list.clone();
            let domain_clone = domain.clone();
            remove.connect_clicked(move |_| {
                allowlist
                    .borrow_mut()
                    .retain(|entry| entry != &domain_clone);
                repaint_trust_list(&trust_list, &allowlist);
            });
        }
    }
    let refresh_trust = {
        let trust_list = trust_list.clone();
        let allowlist = Rc::clone(&allowlist);
        move || repaint_trust_list(&trust_list, &allowlist)
    };
    refresh_trust();

    fn suffix_icon_button(icon: &str, tooltip: &str, a11y: &str, destructive: bool) -> gtk::Button {
        let btn = gtk::Button::from_icon_name(icon);
        btn.add_css_class("flat");
        if destructive {
            btn.add_css_class("destructive-action");
        }
        btn.set_valign(gtk::Align::Center);
        // M3a-8: maintain a comfortable touch/click target.
        btn.set_size_request(32, 32);
        btn.set_tooltip_text(Some(tooltip));
        btn.update_property(&[Property::Label(a11y)]);
        btn
    }
    // Templates manager: populate list and handle add/edit/delete
    let refresh_templates: RefreshFn = Rc::new(RefCell::new(None));
    {
        let app = Rc::clone(&app);
        let account_id = existing_account.as_ref().map(|a| a.id);
        let templates_list = templates_list.clone();
        let dialog = dialog.clone();
        let error_label = error_label.clone();
        let refresh_templates_main = Rc::clone(&refresh_templates);
        let refresh_templates_for_edit = Rc::clone(&refresh_templates);
        let refresh_templates_for_delete = Rc::clone(&refresh_templates);
        *refresh_templates_main.borrow_mut() = Some(Box::new(move || {
            if let Some(account_id) = account_id {
                while let Some(child) = templates_list.first_child() {
                    templates_list.remove(&child);
                }
                // The template list comes from the store: read it on a
                // worker and build the rows when the result arrives.
                let mailin = std::sync::Arc::clone(&app.mailin);
                let app_for_rows = Rc::clone(&app);
                let templates_list = templates_list.clone();
                let dialog = dialog.clone();
                let error_label = error_label.clone();
                let refresh_for_edit = Rc::clone(&refresh_templates_for_edit);
                let refresh_for_delete = Rc::clone(&refresh_templates_for_delete);
                crate::background::spawn_result(
                    move || {
                        mailin
                            .store()
                            .list_templates(&account_id)
                            .unwrap_or_default()
                    },
                    move |result| {
                        let Ok(templates) = result else {
                            return;
                        };
                        for template in templates {
                            let row = adw::ActionRow::builder()
                                .title(&template.name)
                                .subtitle(&template.subject)
                                .build();
                            row.set_activatable(false);
                            let edit_a11y = format!("Edit template {}", template.name);
                            let delete_a11y = format!("Delete template {}", template.name);
                            let edit_btn = suffix_icon_button(
                                "document-edit-symbolic",
                                "Edit template",
                                &edit_a11y,
                                false,
                            );
                            let delete_btn = suffix_icon_button(
                                "user-trash-symbolic",
                                "Delete template",
                                &delete_a11y,
                                true,
                            );
                            row.add_suffix(&edit_btn);
                            row.add_suffix(&delete_btn);

                            let app_for_edit = Rc::clone(&app_for_rows);
                            let app_for_delete = Rc::clone(&app_for_rows);
                            let error_label_for_edit = error_label.clone();
                            let error_label_for_delete = error_label.clone();
                            let template_name_for_edit = template.name.clone();
                            let template_name_for_delete = template.name.clone();
                            let template_subject = template.subject.clone();
                            let template_body = template.body.clone();
                            let refresh_for_edit = Rc::clone(&refresh_for_edit);
                            let refresh_for_delete = Rc::clone(&refresh_for_delete);
                            edit_btn.connect_clicked({
                                let dialog = dialog.clone();
                                move |_| {
                                    edit_template_dialog(
                                        &dialog,
                                        &app_for_edit,
                                        account_id,
                                        &template_name_for_edit,
                                        &template_subject,
                                        &template_body,
                                        &error_label_for_edit,
                                        Rc::clone(&refresh_for_edit),
                                    );
                                }
                            });
                            delete_btn.connect_clicked(move |_| {
                                // Deleting writes to the store: keep it off
                                // the UI thread.
                                let mailin = std::sync::Arc::clone(&app_for_delete.mailin);
                                let name = template_name_for_delete.clone();
                                let error_label = error_label_for_delete.clone();
                                let refresh = Rc::clone(&refresh_for_delete);
                                crate::background::spawn_result(
                                    move || {
                                        mailin
                                            .store()
                                            .delete_template(&account_id, &name)
                                            .map_err(|error| error.to_string())
                                    },
                                    move |result| match result {
                                        Ok(Ok(_)) => {
                                            error_label.set_visible(false);
                                            if let Some(refresh) = refresh.borrow().as_ref() {
                                                refresh();
                                            }
                                        }
                                        Ok(Err(error)) => {
                                            error_label.set_text(&format!(
                                                "Could not delete template: {error}"
                                            ));
                                            error_label.set_visible(true);
                                        }
                                        Err(failed) => {
                                            error_label.set_text(&format!(
                                                "Could not delete template: {}",
                                                failed.message
                                            ));
                                            error_label.set_visible(true);
                                        }
                                    },
                                );
                            });
                            templates_list.append(&row);
                        }
                    },
                );
            }
        }));
    }
    if let Some(refresh) = refresh_templates.borrow().as_ref() {
        refresh();
    }

    // Contacts manager: populate list and handle add/edit/delete
    // Use Rc<RefCell> to allow the closure to be called from within itself
    let refresh_contacts: RefreshFn = Rc::new(RefCell::new(None));
    {
        let app = Rc::clone(&app);
        let account_id = existing_account.as_ref().map(|a| a.id);
        let contacts_list = contacts_list.clone();
        let dialog = dialog.clone();
        let error_label = error_label.clone();

        // Clone refresh_contacts for the main closure and for the sub-closures
        let refresh_contacts_main = Rc::clone(&refresh_contacts);
        let refresh_contacts_for_edit = Rc::clone(&refresh_contacts);
        let refresh_contacts_for_delete = Rc::clone(&refresh_contacts);

        *refresh_contacts_main.borrow_mut() = Some(Box::new(move || {
            if let Some(account_id) = account_id {
                while let Some(child) = contacts_list.first_child() {
                    contacts_list.remove(&child);
                }
                // The contact list comes from the store: read it on a worker
                // and build the rows when the result arrives.
                let mailin = std::sync::Arc::clone(&app.mailin);
                let app_for_rows = Rc::clone(&app);
                let contacts_list = contacts_list.clone();
                let dialog = dialog.clone();
                let error_label = error_label.clone();
                let refresh_contacts_for_edit = Rc::clone(&refresh_contacts_for_edit);
                let refresh_contacts_for_delete = Rc::clone(&refresh_contacts_for_delete);
                crate::background::spawn_result(
                    move || {
                        mailin
                            .store()
                            .list_contacts(&account_id)
                            .unwrap_or_default()
                    },
                    move |result| {
                        let Ok(contacts) = result else {
                            return;
                        };
                        if contacts.is_empty() {
                            // Placeholder is shown automatically
                        } else {
                            for contact in contacts {
                                let subtitle = if contact.display_name.is_empty() {
                                    String::new()
                                } else {
                                    contact.display_name.clone()
                                };
                                let row = adw::ActionRow::builder()
                                    .title(&contact.email)
                                    .subtitle(&subtitle)
                                    .build();
                                row.set_activatable(false);
                                let edit_a11y = format!("Edit contact {}", contact.email);
                                let delete_a11y = format!("Delete contact {}", contact.email);
                                let edit_btn = suffix_icon_button(
                                    "document-edit-symbolic",
                                    "Edit contact",
                                    &edit_a11y,
                                    false,
                                );
                                let delete_btn = suffix_icon_button(
                                    "user-trash-symbolic",
                                    "Delete contact",
                                    &delete_a11y,
                                    true,
                                );
                                row.add_suffix(&edit_btn);
                                row.add_suffix(&delete_btn);

                                // Clone values for use in both button closures
                                let app_for_edit = Rc::clone(&app_for_rows);
                                let app_for_delete = Rc::clone(&app_for_rows);
                                let _contacts_list_for_edit = contacts_list.clone();
                                let _contacts_list_for_delete = contacts_list.clone();
                                let error_label_for_edit = error_label.clone();
                                let error_label_for_delete = error_label.clone();
                                let contact_email_for_edit = contact.email.clone();
                                let contact_email_for_delete = contact.email.clone();
                                let contact_name_for_edit = contact.display_name.clone();

                                // Use the pre-cloned refresh_contacts for each button
                                let refresh_contacts_for_edit =
                                    Rc::clone(&refresh_contacts_for_edit);
                                let refresh_contacts_for_delete =
                                    Rc::clone(&refresh_contacts_for_delete);

                                edit_btn.connect_clicked({
                                    let dialog = dialog.clone();
                                    move |_| {
                                        if let Some(refresh) =
                                            refresh_contacts_for_edit.borrow().as_ref()
                                        {
                                            refresh();
                                        }
                                        edit_contact_dialog(
                                            &dialog,
                                            &app_for_edit,
                                            account_id,
                                            &contact_email_for_edit,
                                            &contact_name_for_edit,
                                            &error_label_for_edit,
                                            Rc::clone(&refresh_contacts_for_edit),
                                        );
                                    }
                                });
                                delete_btn.connect_clicked(move |_| {
                                    // Deleting writes to the store: keep it off
                                    // the UI thread.
                                    let mailin = std::sync::Arc::clone(&app_for_delete.mailin);
                                    let email = contact_email_for_delete.clone();
                                    let error_label = error_label_for_delete.clone();
                                    let refresh = Rc::clone(&refresh_contacts_for_delete);
                                    crate::background::spawn_result(
                                        move || {
                                            mailin
                                                .store()
                                                .delete_contact(&account_id, &email)
                                                .map_err(|error| error.to_string())
                                        },
                                        move |result| match result {
                                            Ok(Ok(_)) => {
                                                error_label.set_visible(false);
                                                if let Some(refresh) = refresh.borrow().as_ref() {
                                                    refresh();
                                                }
                                            }
                                            Ok(Err(error)) => {
                                                error_label.set_text(&format!(
                                                    "Could not delete contact: {error}"
                                                ));
                                                error_label.set_visible(true);
                                            }
                                            Err(failed) => {
                                                error_label.set_text(&format!(
                                                    "Could not delete contact: {}",
                                                    failed.message
                                                ));
                                                error_label.set_visible(true);
                                            }
                                        },
                                    );
                                });
                                contacts_list.append(&row);
                            }
                        }
                    },
                );
            }
        }));
    }
    // Initialize by calling the refresh function
    if let Some(refresh) = refresh_contacts.borrow().as_ref() {
        refresh();
    }

    contacts_add.connect_clicked({
        let app = Rc::clone(&app);
        let account_id = existing_account.as_ref().map(|a| a.id);
        let _contacts_list = contacts_list.clone();
        let dialog = dialog.clone();
        let error_label = error_label.clone();
        let refresh = Rc::clone(&refresh_contacts);
        move |_| {
            if let Some(account_id) = account_id {
                let refresh = Rc::clone(&refresh);
                new_contact_dialog(&dialog, &app, account_id, &error_label, refresh);
            }
        }
    });

    templates_add.connect_clicked({
        let app = Rc::clone(&app);
        let account_id = existing_account.as_ref().map(|a| a.id);
        let _templates_list = templates_list.clone();
        let dialog = dialog.clone();
        let error_label = error_label.clone();
        let refresh = Rc::clone(&refresh_templates);
        move |_| {
            if let Some(account_id) = account_id {
                new_template_dialog(&dialog, &app, account_id, &error_label, Rc::clone(&refresh));
            }
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
        let stack = stack.clone();
        let password_row = password_row.clone();
        let email_row = email_row.clone();
        let save = save.clone();
        let save_spinner = save_spinner.clone();
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
                    set_status_message(&error_label, &error, "error");
                    // M4-F: send the user back to the step that holds the
                    // offending field.
                    if mode == AccountDialogMode::Add {
                        stack.set_visible_child_name("account");
                        if error.to_ascii_lowercase().contains("password") {
                            password_row.grab_focus();
                        } else {
                            email_row.grab_focus();
                        }
                    }
                    return;
                }
            };
            // Keep the identifier across retries: a failed keyring write must
            // not leave an account row behind that a second click duplicates.
            // In edit mode, preserve the existing account ID and everything the
            // form does not know about: sync settings and the remote-content
            // allowlist are edited elsewhere and must survive a settings save.
            if let Some(existing) = &existing_account_for_save {
                account.id = existing.id;
                account.sync = existing.sync.clone();
                account
                    .remote_allowed_domains
                    .clone_from(&existing.remote_allowed_domains);
            } else {
                match account_id.get() {
                    Some(id) => account.id = id,
                    None => account_id.set(Some(account.id)),
                }
            }
            // The signature field lives in this dialog, so it is applied here
            // rather than in `Draft::build`.
            let text = {
                let buffer = signature_buffer.borrow();
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string()
            };
            account.signature = text;
            let html = {
                let buffer = signature_html_buffer.borrow();
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string()
            };
            account.signature_html = html;
            account
                .remote_allowed_domains
                .clone_from(&allowlist.borrow());
            // Only update password if user entered a new one. OAuth2 accounts
            // keep tokens in the account row and do not use the keyring.
            let password_opt = if password.is_empty() || is_oauth {
                None
            } else {
                Some(secrecy::SecretString::new(password.into()))
            };
            // Writing the keyring entry and the account row can block for
            // seconds: run it off the main thread and show the state in the
            // dialog instead of freezing the window.
            save.set_sensitive(false);
            save_spinner.set_visible(true);
            save_spinner.start();
            set_status_message(&error_label, "Saving…", "");
            let mailin = std::sync::Arc::clone(&app.mailin);
            let account_for_save = account.clone();
            let save_button = save.clone();
            let error_label_for_save = error_label.clone();
            let dialog_for_save = dialog.clone();
            let app_for_save = Rc::clone(&app);
            let on_saved_for_save = Rc::clone(&on_saved);
            let spinner_for_save = save_spinner.clone();
            crate::background::spawn_result(
                move || mailin.save_account(&account_for_save, password_opt.as_ref()),
                move |result| {
                    spinner_for_save.stop();
                    spinner_for_save.set_visible(false);
                    match result {
                        Ok(Err(error)) => {
                            save_button.set_sensitive(true);
                            set_status_message(&error_label_for_save, &error.to_string(), "error");
                        }
                        Ok(Ok(())) => {
                            app_for_save.engine.sync_account(account.id);
                            on_saved_for_save(account.id);
                            dialog_for_save.close();
                        }
                        Err(failed) => {
                            save_button.set_sensitive(true);
                            set_status_message(
                                &error_label_for_save,
                                &format!("Could not save the account: {}", failed.message),
                                "error",
                            );
                        }
                    }
                },
            );
        }
    });

    // Remove account handler (M3a-2: shared destructive confirm).
    if let Some(remove) = remove {
        let dialog = dialog.clone();
        let app = Rc::clone(&app);
        let existing = existing_account_for_remove.clone();
        let window = parent.root().and_then(|r| r.downcast::<gtk::Window>().ok());
        remove.connect_clicked(move |_| {
            if let Some(account) = &existing {
                let mailin = app.mailin.clone();
                let dialog_ok = dialog.clone();
                let dialog_err = dialog.clone();
                let parent_win = window.clone();
                crate::dialogs::confirm_remove_account(
                    parent_win.as_ref(),
                    mailin,
                    account,
                    move || {
                        dialog_ok.close();
                    },
                    move |e| {
                        let alert = gtk::AlertDialog::builder()
                            .message("Couldn't remove account")
                            .detail(e)
                            .build();
                        alert.set_buttons(&["OK"]);
                        alert.set_default_button(0);
                        let p = dialog_err
                            .root()
                            .and_then(|r| r.downcast::<gtk::Window>().ok());
                        gtk::glib::spawn_future_local(async move {
                            let _ = alert.choose_future(p.as_ref()).await;
                        });
                    },
                );
            }
        });
    }

    // M3a-5: focus the first identity field.
    let focus_row = if existing_account.is_some() {
        name_row.clone()
    } else {
        email_row.clone()
    };
    dialog.present(Some(parent));
    glib::idle_add_local_once(move || {
        let _ = focus_row.grab_focus();
    });
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

/// Dialog to create a new template.
fn new_template_dialog(
    parent: &adw::Dialog,
    app: &Rc<AppState>,
    account_id: AccountId,
    error_label: &gtk::Label,
    refresh: RefreshFn,
) {
    let dialog = adw::MessageDialog::builder()
        .heading("New Template")
        .body("Enter template details:")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");

    let name_entry = adw::EntryRow::builder()
        .title("Name")
        .show_apply_button(true)
        .build();
    let subject_entry = adw::EntryRow::builder()
        .title("Subject")
        .show_apply_button(true)
        .build();
    let body_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .accepts_tab(false)
        .build();
    let body_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(150)
        .child(&body_view)
        .build();
    let body_row = adw::ActionRow::builder().title("Body").build();
    body_row.add_suffix(&body_scroll);
    body_row.set_activatable(false);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
    container.set_margin_top(12);
    container.set_margin_bottom(12);
    container.set_margin_start(12);
    container.set_margin_end(12);
    container.append(&name_entry);
    container.append(&subject_entry);
    container.append(&body_row);
    dialog.set_extra_child(Some(&container));

    let app = Rc::clone(app);
    let error_label = error_label.clone();
    let refresh = refresh.clone();
    let root = parent.root().and_then(|r| r.downcast::<gtk::Window>().ok());
    let prior_focus = root.as_ref().and_then(gtk::prelude::RootExt::focus);
    let name_c = name_entry.clone();
    let subject_c = subject_entry.clone();
    let body_c = body_view.clone();
    dialog.connect_response(None, move |d: &adw::MessageDialog, response| {
        if response == "save" {
            let name = name_c.text().to_string().trim().to_owned();
            let subject = subject_c.text().to_string();
            let buffer = body_c.buffer();
            let body = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string();
            if !name.is_empty() {
                // Writing to the store happens on a worker.
                let mailin = std::sync::Arc::clone(&app.mailin);
                let error_label = error_label.clone();
                let refresh = Rc::clone(&refresh);
                crate::background::spawn_result(
                    move || {
                        mailin
                            .store()
                            .upsert_template(&account_id, &name, &subject, &body)
                            .map(|_| ())
                            .map_err(|error| error.to_string())
                    },
                    move |result| match result {
                        Ok(Ok(())) => {
                            error_label.set_visible(false);
                            if let Some(r) = refresh.borrow().as_ref() {
                                r();
                            }
                        }
                        Ok(Err(error)) => {
                            set_status_message(
                                &error_label,
                                &format!("Could not save template: {error}"),
                                "error",
                            );
                        }
                        Err(failed) => {
                            set_status_message(
                                &error_label,
                                &format!("Could not save template: {}", failed.message),
                                "error",
                            );
                        }
                    },
                );
            }
        }
        d.close();
        if let Some(focus) = prior_focus.clone() {
            let _ = focus.grab_focus();
        }
    });

    dialog.present();
    name_entry.grab_focus();
}

/// Dialog to edit an existing template.
#[allow(clippy::too_many_arguments)]
fn edit_template_dialog(
    parent: &adw::Dialog,
    app: &Rc<AppState>,
    account_id: AccountId,
    current_name: &str,
    current_subject: &str,
    current_body: &str,
    error_label: &gtk::Label,
    refresh: RefreshFn,
) {
    let dialog = adw::MessageDialog::builder()
        .heading("Edit Template")
        .body("Modify template details:")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");

    let name_entry = adw::EntryRow::builder()
        .title("Name")
        .show_apply_button(true)
        .build();
    name_entry.set_text(current_name);
    let subject_entry = adw::EntryRow::builder()
        .title("Subject")
        .show_apply_button(true)
        .build();
    subject_entry.set_text(current_subject);
    let body_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .accepts_tab(false)
        .build();
    body_view.buffer().set_text(current_body);
    let body_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(150)
        .child(&body_view)
        .build();
    let body_row = adw::ActionRow::builder().title("Body").build();
    body_row.add_suffix(&body_scroll);
    body_row.set_activatable(false);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
    container.set_margin_top(12);
    container.set_margin_bottom(12);
    container.set_margin_start(12);
    container.set_margin_end(12);
    container.append(&name_entry);
    container.append(&subject_entry);
    container.append(&body_row);
    dialog.set_extra_child(Some(&container));

    let app = Rc::clone(app);
    let error_label = error_label.clone();
    let old_name = current_name.to_owned();
    let root = parent.root().and_then(|r| r.downcast::<gtk::Window>().ok());
    let prior_focus = root.as_ref().and_then(gtk::prelude::RootExt::focus);
    let name_c = name_entry.clone();
    let subject_c = subject_entry.clone();
    let body_c = body_view.clone();
    dialog.connect_response(None, move |d: &adw::MessageDialog, response| {
        if response == "save" {
            let name = name_c.text().to_string().trim().to_owned();
            let subject = subject_c.text().to_string();
            let buffer = body_c.buffer();
            let body = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string();
            if !name.is_empty() {
                // Writing to the store happens on a worker.
                let mailin = std::sync::Arc::clone(&app.mailin);
                let old_name = old_name.clone();
                let error_label = error_label.clone();
                let refresh = Rc::clone(&refresh);
                crate::background::spawn_result(
                    move || {
                        // Delete old if name changed
                        if name != old_name {
                            let _ = mailin.store().delete_template(&account_id, &old_name);
                        }
                        mailin
                            .store()
                            .upsert_template(&account_id, &name, &subject, &body)
                            .map(|_| ())
                            .map_err(|error| error.to_string())
                    },
                    move |result| match result {
                        Ok(Ok(())) => {
                            error_label.set_visible(false);
                            if let Some(r) = refresh.borrow().as_ref() {
                                r();
                            }
                        }
                        Ok(Err(error)) => {
                            set_status_message(
                                &error_label,
                                &format!("Could not save template: {error}"),
                                "error",
                            );
                        }
                        Err(failed) => {
                            set_status_message(
                                &error_label,
                                &format!("Could not save template: {}", failed.message),
                                "error",
                            );
                        }
                    },
                );
            }
        }
        d.close();
        if let Some(focus) = prior_focus.clone() {
            let _ = focus.grab_focus();
        }
    });

    dialog.present();
    name_entry.grab_focus();
}

/// Dialog to create a new contact.
fn new_contact_dialog(
    parent: &adw::Dialog,
    app: &Rc<AppState>,
    account_id: AccountId,
    error_label: &gtk::Label,
    refresh: RefreshFn,
) {
    let dialog = adw::MessageDialog::builder()
        .heading("New Contact")
        .body("Enter contact details:")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");

    let email_entry = adw::EntryRow::builder()
        .title("Email")
        .show_apply_button(true)
        .build();
    let name_entry = adw::EntryRow::builder()
        .title("Display name (optional)")
        .show_apply_button(true)
        .build();

    let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
    container.set_margin_top(12);
    container.set_margin_bottom(12);
    container.set_margin_start(12);
    container.set_margin_end(12);
    container.append(&email_entry);
    container.append(&name_entry);
    dialog.set_extra_child(Some(&container));

    let app = Rc::clone(app);
    let error_label = error_label.clone();
    let refresh = Rc::clone(&refresh);
    let root = parent.root().and_then(|r| r.downcast::<gtk::Window>().ok());
    let prior_focus = root.as_ref().and_then(gtk::prelude::RootExt::focus);
    let email_c = email_entry.clone();
    let name_c = name_entry.clone();
    dialog.connect_response(None, move |d: &adw::MessageDialog, response| {
        if response == "save" {
            let email = email_c.text().to_string().trim().to_owned();
            let display_name = name_c.text().to_string();
            if !email.is_empty() && email.contains('@') {
                // Writing to the store happens on a worker.
                let mailin = std::sync::Arc::clone(&app.mailin);
                let error_label = error_label.clone();
                let refresh = Rc::clone(&refresh);
                crate::background::spawn_result(
                    move || {
                        mailin
                            .store()
                            .upsert_contact(&account_id, &email, &display_name, true)
                            .map(|_| ())
                            .map_err(|error| error.to_string())
                    },
                    move |result| match result {
                        Ok(Ok(())) => {
                            error_label.set_visible(false);
                            if let Some(r) = refresh.borrow().as_ref() {
                                r();
                            }
                        }
                        Ok(Err(error)) => {
                            set_status_message(
                                &error_label,
                                &format!("Could not save contact: {error}"),
                                "error",
                            );
                        }
                        Err(failed) => {
                            set_status_message(
                                &error_label,
                                &format!("Could not save contact: {}", failed.message),
                                "error",
                            );
                        }
                    },
                );
            }
        }
        d.close();
        if let Some(focus) = prior_focus.clone() {
            let _ = focus.grab_focus();
        }
    });

    dialog.present();
    email_entry.grab_focus();
}

/// Dialog to edit an existing contact.
fn edit_contact_dialog(
    parent: &adw::Dialog,
    app: &Rc<AppState>,
    account_id: AccountId,
    current_email: &str,
    current_name: &str,
    error_label: &gtk::Label,
    refresh: RefreshFn,
) {
    let dialog = adw::MessageDialog::builder()
        .heading("Edit Contact")
        .body("Modify contact details:")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");

    let email_entry = adw::EntryRow::builder()
        .title("Email")
        .show_apply_button(true)
        .build();
    email_entry.set_text(current_email);
    let name_entry = adw::EntryRow::builder()
        .title("Display name (optional)")
        .show_apply_button(true)
        .build();
    name_entry.set_text(current_name);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
    container.set_margin_top(12);
    container.set_margin_bottom(12);
    container.set_margin_start(12);
    container.set_margin_end(12);
    container.append(&email_entry);
    container.append(&name_entry);
    dialog.set_extra_child(Some(&container));

    let app = Rc::clone(app);
    let error_label = error_label.clone();
    let refresh = Rc::clone(&refresh);
    let old_email = current_email.to_owned();
    let root = parent.root().and_then(|r| r.downcast::<gtk::Window>().ok());
    let prior_focus = root.as_ref().and_then(gtk::prelude::RootExt::focus);
    let email_c = email_entry.clone();
    let name_c = name_entry.clone();
    dialog.connect_response(None, move |d: &adw::MessageDialog, response| {
        if response == "save" {
            let email = email_c.text().to_string().trim().to_owned();
            let display_name = name_c.text().to_string();
            if !email.is_empty() && email.contains('@') {
                // Writing to the store happens on a worker.
                let mailin = std::sync::Arc::clone(&app.mailin);
                let old_email = old_email.clone();
                let error_label = error_label.clone();
                let refresh = Rc::clone(&refresh);
                crate::background::spawn_result(
                    move || {
                        // Delete old if email changed
                        if email != old_email {
                            let _ = mailin.store().delete_contact(&account_id, &old_email);
                        }
                        mailin
                            .store()
                            .upsert_contact(&account_id, &email, &display_name, true)
                            .map(|_| ())
                            .map_err(|error| error.to_string())
                    },
                    move |result| match result {
                        Ok(Ok(())) => {
                            error_label.set_visible(false);
                            if let Some(r) = refresh.borrow().as_ref() {
                                r();
                            }
                        }
                        Ok(Err(error)) => {
                            set_status_message(
                                &error_label,
                                &format!("Could not save contact: {error}"),
                                "error",
                            );
                        }
                        Err(failed) => {
                            set_status_message(
                                &error_label,
                                &format!("Could not save contact: {}", failed.message),
                                "error",
                            );
                        }
                    },
                );
            }
        }
        d.close();
        if let Some(focus) = prior_focus.clone() {
            let _ = focus.grab_focus();
        }
    });

    dialog.present();
    email_entry.grab_focus();
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
        // A client_id is also required for OAuth2; supply one so this test
        // isolates the access-token requirement.
        draft.oauth2.client_id = "client-id".into();
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

        let paths =
            mailin_core::config::Paths::with_root(tempfile::tempdir().expect("temp dir").path());
        let config = Config::default();
        let config_cache = Arc::new(std::sync::RwLock::new(Some(config)));
        let config_paths = Arc::new(paths);
        let mailin = Arc::new(Mailin::with_parts(
            Arc::new(Store::open_in_memory().expect("store")),
            Arc::new(MemoryCredentials::new()),
            config_cache,
            config_paths,
        ));
        let engine = Engine::start(Arc::clone(&mailin)).expect("engine");
        let app = Rc::new(crate::app::AppState {
            config: Rc::new(RefCell::new(Config::default())),
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
