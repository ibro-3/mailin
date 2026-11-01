//! Folder picker shown when messages are moved with "Move to Folder…".
//!
//! A searchable list of the account's folders inside an `adw::Dialog`.
//! Picking a row (double-click, Enter, or the Move button) hands the folder
//! id back to the caller, which issues the move.

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita as adw;

use adw::prelude::*;
use gtk::pango;
use gtk::prelude::*;
use mailin_core::{Folder, FolderId};

use crate::window::{folder_title, role_icon};

/// Show the picker for `parent`. `exclude` is the folder the messages are
/// already in; the outbox (`FolderId(-1)`) is never offered.
pub fn show(
    parent: &adw::ApplicationWindow,
    folders: &[Folder],
    exclude: Option<FolderId>,
    on_pick: impl Fn(FolderId) + 'static,
) {
    let title = adw::WindowTitle::new("Move to Folder", "");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));

    let move_button = gtk::Button::with_label("Move");
    move_button.add_css_class("suggested-action");
    move_button.set_sensitive(false);
    header.pack_end(&move_button);

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search folders")
        .width_chars(20)
        .build();
    // Placeholder text isn't reliably announced as a name; set it explicitly.
    {
        use gtk::accessible::Property;
        search.update_property(&[Property::Label("Search folders")]);
    }

    // Only real folders can be destinations, and only one row is selected
    // at a time, so a row index maps straight into this vector.
    let candidates: Vec<Folder> = folders
        .iter()
        .filter(|folder| folder.id.0 >= 0 && Some(folder.id) != exclude)
        .cloned()
        .collect();

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(false)
        .build();
    list.add_css_class("navigation-sidebar");
    for folder in &candidates {
        list.append(&folder_row(folder));
    }

    let empty = adw::StatusPage::builder()
        .icon_name("folder-symbolic")
        .title("No folders")
        .description("This account has no other folders to move messages into.")
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&list, Some("list"));
    stack.add_named(&empty, Some("empty"));
    stack.set_visible_child_name(if candidates.is_empty() {
        "empty"
    } else {
        "list"
    });

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&stack)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&search);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&scroller);

    let dialog = adw::Dialog::builder()
        .child(&content)
        .content_width(380)
        .content_height(480)
        .build();

    let chosen: Rc<RefCell<Option<FolderId>>> = Rc::new(RefCell::new(None));
    let on_pick = Rc::new(on_pick);

    list.connect_row_selected({
        let candidates = candidates.clone();
        let chosen = Rc::clone(&chosen);
        let move_button = move_button.clone();
        move |list_box, row| {
            let folder_id = row
                .as_ref()
                .and_then(|row| candidates.get(usize::try_from(row.index()).ok()?))
                .map(|folder| folder.id);
            *chosen.borrow_mut() = folder_id;
            move_button.set_sensitive(folder_id.is_some() && list_box.selected_row().is_some());
        }
    });

    // Keep the rows and the selection in sync with the filter text. Rows are
    // hidden rather than removed so the index mapping stays intact.
    search.connect_search_changed({
        let list = list.clone();
        let candidates = candidates.clone();
        let chosen = Rc::clone(&chosen);
        move |entry| {
            let needle = entry.text().to_lowercase();
            for (index, folder) in candidates.iter().enumerate() {
                let matches = needle.is_empty()
                    || folder_title(folder).to_lowercase().contains(&needle)
                    || folder.name.to_lowercase().contains(&needle);
                if let Some(row) = list.row_at_index(index as i32) {
                    row.set_visible(matches);
                }
            }
            // A hidden selected row would leave a stale destination behind.
            if let Some(row) = list.selected_row()
                && !row.is_visible()
            {
                list.unselect_row(&row);
            }
            *chosen.borrow_mut() = None;
        }
    });

    let apply = {
        let dialog = dialog.clone();
        let chosen = Rc::clone(&chosen);
        let on_pick = Rc::clone(&on_pick);
        move || {
            if let Some(folder_id) = chosen.borrow_mut().take() {
                dialog.close();
                on_pick(folder_id);
            }
        }
    };

    move_button.connect_clicked({
        let apply = apply.clone();
        move |_| apply()
    });
    list.connect_row_activated({
        let apply = apply.clone();
        move |_, _| apply()
    });

    dialog.present(Some(parent));
}

fn folder_row(folder: &Folder) -> gtk::ListBoxRow {
    let icon = gtk::Image::from_icon_name(role_icon(folder.role));
    let label = gtk::Label::builder()
        .label(folder_title(folder))
        .xalign(0.0)
        .ellipsize(pango::EllipsizeMode::End)
        .hexpand(true)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(10);
    content.set_margin_end(10);
    content.append(&icon);
    content.append(&label);

    let row = gtk::ListBoxRow::builder().child(&content).build();
    row.set_tooltip_text(Some(&folder.name));
    row
}
