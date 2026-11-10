//! Shared confirmation dialogs (M3a-2).

use std::sync::Arc;

use libadwaita as adw;
use libadwaita::prelude::*;
use mailin_core::Account;

/// Confirm and remove an account (destructive, irreversible local data loss).
pub fn confirm_remove_account(
    parent: Option<&gtk::Window>,
    mailin: Arc<mailin_core::client::Mailin>,
    account: &Account,
    on_success: impl FnOnce() + 'static,
    on_error: impl FnOnce(String) + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Remove account?")
        .body(format!(
            "Remove “{}” ({})? This deletes all local data for this account and cannot be undone.",
            account.display_name, account.email
        ))
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let account_id = account.id;
    let parent = parent.cloned();
    gtk::glib::spawn_future_local(async move {
        let response = dialog.choose_future(parent.as_ref()).await;
        if response.as_str() == "remove" {
            // Removing the account deletes every local row and the keyring
            // entry: run it off the main thread so the window stays alive.
            crate::background::spawn_result(
                move || mailin.remove_account(&account_id),
                move |result| match result {
                    Ok(Ok(())) => on_success(),
                    Ok(Err(error)) => on_error(error.to_string()),
                    Err(failed) => on_error(failed.message),
                },
            );
        }
    });
}
