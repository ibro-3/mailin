//! PGP key manager: list, import, generate and delete keys for an account.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use libadwaita as adw;

use mailin_core::AccountId;

use crate::app::AppState;

/// Show the key manager for `account`.
pub fn present(parent: &gtk::Window, app: Rc<AppState>, account: AccountId) {
    let Some(account_row) = app.mailin.account(&account).ok().flatten() else {
        return;
    };

    let window = adw::Window::builder()
        .title("PGP Keys")
        .transient_for(parent)
        .modal(true)
        .default_width(560)
        .default_height(520)
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("PGP Keys", &account_row.email)));

    let import = gtk::Button::with_label("Import");
    import.set_tooltip_text(Some("Import an armored public or secret key"));
    header.pack_end(&import);

    let generate = gtk::Button::with_label("Generate");
    generate.set_tooltip_text(Some("Generate a new key pair for this account"));
    header.pack_end(&generate);

    let banner = adw::Banner::new("");
    banner.set_revealed(false);

    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Keys")
        .description("Public keys are used to encrypt to this address. Secret keys stay in the system keyring.")
        .build();
    page.add(&group);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&page);
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&content)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroller));
    window.set_content(Some(&toolbar));

    populate(&group, &app, account, &window, &banner);

    {
        let app = Rc::clone(&app);
        let group = group.clone();
        let window = window.clone();
        let banner = banner.clone();
        import.connect_clicked(move |_| {
            import_dialog(&window, &app, account, &group, &banner);
        });
    }
    {
        let app = Rc::clone(&app);
        let group = group.clone();
        let window = window.clone();
        let banner = banner.clone();
        let email = account_row.email.clone();
        generate.connect_clicked(move |_| {
            generate_dialog(&window, &app, account, &email, &group, &banner);
        });
    }

    // M3a-5: initial focus and restore to parent.
    let prior_focus = gtk::prelude::RootExt::focus(parent);
    let parent_c = parent.clone();
    window.connect_close_request(move |_| {
        if let Some(focus) = prior_focus.clone() {
            let _ = focus.grab_focus();
        } else {
            let _ = parent_c.grab_focus();
        }
        gtk::glib::Propagation::Proceed
    });
    window.present();
    import.grab_focus();
}

/// Rebuild the list of keys from the store.
fn populate(
    group: &adw::PreferencesGroup,
    app: &Rc<AppState>,
    account: AccountId,
    window: &adw::Window,
    banner: &adw::Banner,
) {
    while let Some(child) = group.first_child() {
        group.remove(&child);
    }

    let keys = match app.mailin.list_pgp_keys(&account) {
        Ok(keys) => keys,
        Err(error) => {
            banner.set_title(&format!("Could not load keys: {error}"));
            banner.set_revealed(true);
            return;
        }
    };

    if keys.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No keys yet")
            .subtitle("Import a key or generate a new pair to use PGP.")
            .build();
        group.add(&row);
        return;
    }

    for key in keys {
        let has_secret = app.mailin.has_pgp_secret(&account, &key.fingerprint);
        let title = if key.user_ids.is_empty() {
            key.fingerprint.clone()
        } else {
            key.user_ids.join(", ")
        };
        let subtitle = if has_secret {
            format!("{} · secret key available", key.fingerprint)
        } else {
            key.fingerprint.clone()
        };
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&title))
            .subtitle(glib::markup_escape_text(&subtitle))
            .build();

        let delete = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Delete this key")
            .valign(gtk::Align::Center)
            .build();
        delete.add_css_class("flat");
        {
            use gtk::accessible::Property;
            let label = format!("Delete key {}", key.fingerprint);
            delete.update_property(&[Property::Label(&label)]);
        }
        row.add_suffix(&delete);

        let fingerprint = key.fingerprint.clone();
        let app_for_delete = Rc::clone(app);
        let group_for_delete = group.clone();
        let window_for_delete = window.clone();
        let banner_for_delete = banner.clone();
        delete.connect_clicked(move |_| {
            let confirm = adw::MessageDialog::builder()
                .transient_for(&window_for_delete)
                .modal(true)
                .heading("Delete key?")
                .body(format!(
                    "The key {fingerprint} will be removed from this account. This cannot be undone."
                ))
                .build();
            confirm.add_response("cancel", "Cancel");
            confirm.add_response("delete", "Delete");
            confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            confirm.set_close_response("cancel");

            let app = Rc::clone(&app_for_delete);
            let group = group_for_delete.clone();
            let window = window_for_delete.clone();
            let banner = banner_for_delete.clone();
            let fingerprint = fingerprint.clone();
            confirm.connect_response(None, move |dialog, response| {
                dialog.close();
                if response != "delete" {
                    return;
                }
                match app.mailin.delete_pgp_key(&account, &fingerprint) {
                    Ok(_) => populate(&group, &app, account, &window, &banner),
                    Err(error) => {
                        banner.set_title(&format!("Could not delete the key: {error}"));
                        banner.set_revealed(true);
                    }
                }
            });
            confirm.present();
        });

        group.add(&row);
    }
}

fn import_dialog(
    parent: &adw::Window,
    app: &Rc<AppState>,
    account: AccountId,
    group: &adw::PreferencesGroup,
    banner: &adw::Banner,
) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .heading("Import PGP key")
        .body("Paste an armored public or secret key block.")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("import", "Import");
    dialog.set_response_appearance("import", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("import"));
    dialog.set_close_response("cancel");

    let view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::Char)
        .height_request(180)
        .build();
    view.add_css_class("monospace");
    let scroller = gtk::ScrolledWindow::builder()
        .child(&view)
        .min_content_height(180)
        .build();
    dialog.set_extra_child(Some(&scroller));

    let view_clone = view.clone();
    let app = Rc::clone(app);
    let group = group.clone();
    let banner = banner.clone();
    let parent = parent.clone();
    dialog.connect_response(None, move |dialog, response| {
        dialog.close();
        if response != "import" {
            return;
        }
        let buffer = view_clone.buffer();
        let (start, end) = buffer.bounds();
        let armored = buffer.text(&start, &end, false).to_string();
        if armored.trim().is_empty() {
            banner.set_title("No key data provided");
            banner.set_revealed(true);
            return;
        }

        let result = if armored.contains("PRIVATE KEY BLOCK") {
            app.mailin.import_pgp_secret_key(&account, &armored)
        } else {
            app.mailin.import_pgp_public_key(&account, &armored)
        };
        match result {
            Ok(key) => {
                banner.set_revealed(false);
                populate(&group, &app, account, &parent, &banner);
                banner.set_title(&format!("Imported key {}", key.fingerprint));
                banner.set_revealed(true);
            }
            Err(error) => {
                banner.set_title(&format!("Could not import the key: {error}"));
                banner.set_revealed(true);
            }
        }
    });
    dialog.present();
}

fn generate_dialog(
    parent: &adw::Window,
    app: &Rc<AppState>,
    account: AccountId,
    email: &str,
    group: &adw::PreferencesGroup,
    banner: &adw::Banner,
) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .heading("Generate key pair")
        .body("The secret key is stored in the system keyring.")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("generate", "Generate");
    dialog.set_response_appearance("generate", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("generate"));
    dialog.set_close_response("cancel");

    let entry = adw::EntryRow::builder()
        .title("User ID")
        .show_apply_button(true)
        .build();
    entry.set_text(&format!("Mailin user <{email}>"));
    dialog.set_extra_child(Some(&entry));

    let entry_clone = entry.clone();
    let app = Rc::clone(app);
    let group = group.clone();
    let banner = banner.clone();
    let parent = parent.clone();
    dialog.connect_response(None, move |dialog, response| {
        dialog.close();
        if response != "generate" {
            return;
        }
        let user_id = entry_clone.text().to_string();
        if user_id.trim().is_empty() {
            banner.set_title("Enter a user ID (name and email)");
            banner.set_revealed(true);
            return;
        }
        match app.mailin.generate_pgp_key(&account, user_id.trim()) {
            Ok(key) => {
                populate(&group, &app, account, &parent, &banner);
                banner.set_title(&format!("Generated key {}", key.fingerprint));
                banner.set_revealed(true);
            }
            Err(error) => {
                banner.set_title(&format!("Could not generate a key: {error}"));
                banner.set_revealed(true);
            }
        }
    });
    dialog.present();
}
