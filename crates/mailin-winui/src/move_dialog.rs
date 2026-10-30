//! Folder picker shown when messages are moved with "Move to Folder…".
//!
//! A searchable list of the account's folders inside a `ContentDialog`.
//! Picking a folder hands its id straight back to the caller, which issues
//! the move. The dialog only needs the window's XAML root; its event revokers
//! are kept alive by the caller (the main window).

use std::rc::Rc;

use anyhow::{Context, Result};
use mailin_core::{Folder, FolderId};
use mailin_winui_sys::{Button, ContentDialog, XamlRoot};
use windows_core::Interface;

use crate::ui;
use crate::window::folder_title;

/// Show the picker attached to `xaml_root`. `exclude` is the folder the
/// messages are already in; the outbox (`FolderId(-1)`) is never offered.
pub fn show(
    xaml_root: &XamlRoot,
    events: &ui::Revokers,
    folders: &[Folder],
    exclude: Option<FolderId>,
    on_pick: impl Fn(FolderId) + 'static,
) -> Result<()> {
    // Only real folders can be destinations.
    let candidates: Vec<Folder> = folders
        .iter()
        .filter(|folder| folder.id.0 >= 0 && Some(folder.id) != exclude)
        .cloned()
        .collect();

    let dialog = ContentDialog::new().context("could not create the folder picker")?;
    let title: windows_core::IInspectable = ui::text("Move to Folder")?.cast()?;
    dialog
        .SetTitle(&title)
        .context("could not title the folder picker")?;

    let panel = ui::stack_sm(mailin_winui_sys::Orientation::Vertical)?;
    ui::margin_lg(&panel)?;
    ui::margin_vertical(&panel, 8.0)?;

    let search = ui::text_box("Search folders")?;
    ui::append(&panel, &search)?;

    // Each folder is a button; the search hides rows rather than removing
    // them, so nothing needs to be rebuilt while typing.
    let entries: Rc<Vec<(Button, FolderId, String)>> = Rc::new(
        candidates
            .iter()
            .map(|folder| {
                let label = folder_title(folder);
                let haystack = format!("{label} {}", folder.name).to_lowercase();
                (label, folder.id, haystack)
            })
            .filter_map(|(label, id, haystack)| {
                ui::button(&label).ok().map(|button| (button, id, haystack))
            })
            .collect(),
    );

    let list = ui::stack_xs(mailin_winui_sys::Orientation::Vertical)?;
    for (button, _, _) in entries.iter() {
        ui::stretch_horizontal(button)?;
        ui::append(&list, button)?;
    }
    ui::append(&panel, &list)?;

    let empty = ui::text(if candidates.is_empty() {
        "This account has no other folders to move messages into."
    } else {
        "No folders match your search."
    })?;
    ui::set_visible(&empty, candidates.is_empty())?;
    ui::append(&panel, &empty)?;

    // Picking a folder closes the dialog and hands the id to the caller.
    let on_pick = Rc::new(on_pick);
    for (button, id, _) in entries.iter() {
        // Copy the id out: the handler must be `'static`, not borrow `entries`.
        let id = *id;
        let on_pick = Rc::clone(&on_pick);
        let dialog_for_click = dialog.clone();
        events.keep(ui::on_click(button, move || {
            let _ = dialog_for_click.Hide();
            on_pick(id);
        })?);
    }

    let filter_entries = Rc::clone(&entries);
    events.keep(ui::on_text_changed(&search, {
        let search = search.clone();
        let empty = empty.clone();
        move || {
            let needle = ui::text_value(&search).to_lowercase();
            let mut visible = 0usize;
            for (button, _, haystack) in filter_entries.iter() {
                let matches = needle.is_empty() || haystack.contains(&needle);
                let _ = ui::set_visible(button, matches);
                if matches {
                    visible += 1;
                }
            }
            let _ = ui::set_visible(&empty, visible == 0);
        }
    })?);

    ui::set_content(&dialog, &panel)?;
    dialog
        .SetCloseButtonText("Cancel")
        .context("could not set the cancel button")?;
    let dialog_element: mailin_winui_sys::IUIElement = dialog.cast()?;
    dialog_element
        .SetXamlRoot(xaml_root)
        .context("could not attach the folder picker to the window")?;
    let _ = dialog
        .ShowAsync()
        .context("could not show the folder picker")?;
    Ok(())
}
