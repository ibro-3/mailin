//! Compose window: build a message, attach files and queue it in the outbox.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use libadwaita as adw;

use mailin_core::compose::recipient_addresses;
use mailin_core::{Account, AccountId, ComposeAttachment, MessageHeader, OutgoingMessage};

use crate::app::AppState;
use crate::format;

/// Maximum size of a single attached file. Larger files would make the SQLite
/// outbox and the SMTP submission needlessly fragile.
const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

pub fn present(
    parent: &gtk::Window,
    app: Rc<AppState>,
    account_id: AccountId,
    draft: OutgoingMessage,
    on_sent: impl Fn() + 'static,
) {
    let Some(account) = app.mailin.account(&account_id).ok().flatten() else {
        return;
    };
    let draft = OutgoingMessage {
        from_name: account.display_name.clone(),
        from_addr: account.email.clone(),
        ..draft
    };

    let window = adw::Window::builder()
        .title("New Message")
        .transient_for(parent)
        .modal(false)
        .default_width(760)
        .default_height(680)
        .build();

    let title = adw::WindowTitle::new("New Message", &account.email);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));

    let send = gtk::Button::with_label("Send");
    send.add_css_class("suggested-action");
    header.pack_end(&send);

    let attach = gtk::Button::builder()
        .icon_name("mail-attachment-symbolic")
        .tooltip_text("Attach files")
        .build();
    attach.add_css_class("flat");
    header.pack_start(&attach);

    let to_row = adw::EntryRow::builder().title("To").build();
    let cc_row = adw::EntryRow::builder().title("Cc").build();
    let bcc_row = adw::EntryRow::builder().title("Bcc").build();
    let subject_row = adw::EntryRow::builder().title("Subject").build();
    to_row.set_text(&draft.to);
    cc_row.set_text(&draft.cc);
    bcc_row.set_text(&draft.bcc);
    subject_row.set_text(&draft.subject);

    let recipients = adw::PreferencesGroup::builder().title("Recipients").build();
    recipients.add(&to_row);
    recipients.add(&cc_row);
    recipients.add(&bcc_row);
    recipients.add(&subject_row);

    let body_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    body_view.buffer().set_text(&draft.body);
    let body_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .min_content_height(220)
        .child(&body_view)
        .build();
    body_scroller.add_css_class("card");

    let attachment_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    attachment_list.add_css_class("boxed-list");
    let attachments_group = adw::PreferencesGroup::builder()
        .title("Attachments")
        .build();
    attachments_group.add(&attachment_list);
    attachments_group.set_visible(false);

    let page = gtk::Box::new(gtk::Orientation::Vertical, 18);
    page.set_margin_top(18);
    page.set_margin_bottom(18);
    page.set_margin_start(18);
    page.set_margin_end(18);
    page.append(&recipients);
    page.append(&body_scroller);
    page.append(&attachments_group);
    let page_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&page)
        .build();

    let banner = adw::Banner::new("");
    banner.set_revealed(false);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&page_scroller);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    let attachments: Rc<RefCell<Vec<ComposeAttachment>>> =
        Rc::new(RefCell::new(draft.attachments.clone()));
    rebuild_attachments(&attachment_list, &attachments_group, &attachments);

    let picker_window = window.clone();
    let picker_list = attachment_list.clone();
    let picker_group = attachments_group.clone();
    let picker_attachments = Rc::clone(&attachments);
    let picker_banner = banner.clone();
    attach.connect_clicked(move |_| {
        let dialog = gtk::FileDialog::builder().title("Attach files").build();
        let list = picker_list.clone();
        let group = picker_group.clone();
        let attachments = Rc::clone(&picker_attachments);
        let banner = picker_banner.clone();
        dialog.open_multiple(
            Some(&picker_window),
            None::<&gio::Cancellable>,
            move |result| {
                let Ok(files) = result else { return };
                let mut failed = None;
                for index in 0..files.n_items() {
                    let Some(file) = files
                        .item(index)
                        .and_then(|object| object.downcast::<gio::File>().ok())
                    else {
                        continue;
                    };
                    match load_attachment(&file) {
                        Ok(attachment) => attachments.borrow_mut().push(attachment),
                        Err(message) => failed = Some(message),
                    }
                }
                if let Some(message) = failed {
                    banner.set_title(&message);
                    banner.set_revealed(true);
                }
                rebuild_attachments(&list, &group, &attachments);
            },
        );
    });

    let close_window = window.clone();
    let send_to_row = to_row.clone();
    let send_cc_row = cc_row.clone();
    let send_bcc_row = bcc_row.clone();
    let send_subject_row = subject_row.clone();
    send.connect_clicked(move |_| {
        let outgoing = OutgoingMessage {
            from_name: account.display_name.clone(),
            from_addr: account.email.clone(),
            to: send_to_row.text().to_string(),
            cc: send_cc_row.text().to_string(),
            bcc: send_bcc_row.text().to_string(),
            subject: send_subject_row.text().to_string(),
            body: {
                let buffer = body_view.buffer();
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string()
            },
            in_reply_to: draft.in_reply_to.clone(),
            references: draft.references.clone(),
            attachments: attachments.borrow().clone(),
        };
        match outgoing.build() {
            Ok((raw, envelope)) => match app.engine.queue_and_send(account.id, raw, envelope) {
                Ok(_) => {
                    on_sent();
                    close_window.close();
                }
                Err(error) => {
                    banner.set_title(&format!("Could not queue the message: {error}"));
                    banner.set_revealed(true);
                }
            },
            Err(error) => {
                banner.set_title(&format!("Could not send: {error}"));
                banner.set_revealed(true);
            }
        }
    });

    // Ctrl+Enter sends from anywhere in the composer.
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let send_for_key = send.clone();
    controller.connect_key_pressed(move |_, key, _, state| {
        let control = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        if control && matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter) {
            send_for_key.emit_clicked();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(controller);

    to_row.grab_focus();
    window.present();
}

fn rebuild_attachments(
    list: &gtk::ListBox,
    group: &adw::PreferencesGroup,
    attachments: &Rc<RefCell<Vec<ComposeAttachment>>>,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let items = attachments.borrow();
    group.set_visible(!items.is_empty());
    for (index, attachment) in items.iter().enumerate() {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&attachment.filename))
            .subtitle(format!(
                "{} · {}",
                attachment.mime_type,
                format::file_size(u32::try_from(attachment.data.len()).unwrap_or(u32::MAX))
            ))
            .build();
        let remove = gtk::Button::builder()
            .icon_name("list-remove-symbolic")
            .tooltip_text("Remove attachment")
            .valign(gtk::Align::Center)
            .build();
        remove.add_css_class("flat");
        let attachments = Rc::clone(attachments);
        let remove_list = list.clone();
        let remove_group = group.clone();
        remove.connect_clicked(move |_| {
            if index < attachments.borrow().len() {
                attachments.borrow_mut().remove(index);
            }
            rebuild_attachments(&remove_list, &remove_group, &attachments);
        });
        row.add_suffix(&remove);
        list.append(&row);
    }
}

fn load_attachment(file: &gio::File) -> std::result::Result<ComposeAttachment, String> {
    let name = file
        .basename()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "attachment".to_owned());
    let info = file
        .query_info(
            "standard::content-type,standard::size",
            gio::FileQueryInfoFlags::NONE,
            None::<&gio::Cancellable>,
        )
        .map_err(|error| format!("Could not read {name}: {error}"))?;
    if info.size() > 0 && info.size() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "{name} is larger than {}",
            format::file_size(MAX_ATTACHMENT_BYTES as u32)
        ));
    }
    let (data, _) = file
        .load_contents(None::<&gio::Cancellable>)
        .map_err(|error| format!("Could not read {name}: {error}"))?;
    if data.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "{name} is larger than {}",
            format::file_size(MAX_ATTACHMENT_BYTES as u32)
        ));
    }
    let mime_type = info
        .content_type()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    Ok(ComposeAttachment::new(name, mime_type, data.to_vec()))
}

// ------------------------------------------------------------------ prefill

/// Build a reply draft for `message`.
pub fn reply(account: &Account, message: &MessageHeader, body: Option<&str>) -> OutgoingMessage {
    let recipients = message
        .from_addr
        .clone()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    reply_with(account, message, body, recipients, String::new())
}

/// Reply to the sender and keep everybody else from the original message in
/// `Cc`, excluding the account's own address.
pub fn reply_all(
    account: &Account,
    message: &MessageHeader,
    body: Option<&str>,
) -> OutgoingMessage {
    let recipients = message.from_addr.clone().into_iter().collect::<Vec<_>>();
    let mut cc = Vec::new();
    for list in [&message.to_addrs, &message.cc_addrs] {
        if let Ok(addresses) = recipient_addresses(list) {
            cc.extend(addresses);
        }
    }
    let mut seen = Vec::new();
    cc.retain(|address| {
        let keep = !address.eq_ignore_ascii_case(&account.email)
            && !recipients
                .iter()
                .any(|recipient| recipient.eq_ignore_ascii_case(address))
            && !seen
                .iter()
                .any(|kept: &String| kept.eq_ignore_ascii_case(address));
        if keep {
            seen.push(address.clone());
        }
        keep
    });
    let to = recipients.join(", ");
    reply_with(account, message, body, to, cc.join(", "))
}

fn reply_with(
    account: &Account,
    message: &MessageHeader,
    body: Option<&str>,
    to: String,
    cc: String,
) -> OutgoingMessage {
    let mut references = Vec::new();
    if let Some(root) = message.thread_key.clone() {
        references.push(root);
    }
    if let Some(id) = message.message_id.clone() {
        references.push(id);
    }
    OutgoingMessage {
        from_name: account.display_name.clone(),
        from_addr: account.email.clone(),
        to,
        cc,
        bcc: String::new(),
        subject: reply_subject(&message.subject),
        body: quoted_reply(message, body),
        in_reply_to: message.message_id.clone(),
        references,
        attachments: Vec::new(),
    }
}

/// Build a forwarding draft with the original headers and body quoted below a
/// separator.
pub fn forward(account: &Account, message: &MessageHeader, body: Option<&str>) -> OutgoingMessage {
    let timestamp = message.sent_at.unwrap_or(message.received_at);
    let mut text = String::new();
    text.push_str("\n\n---------- Forwarded message ----------\n");
    text.push_str(&format!("From: {}\n", message.from_display()));
    text.push_str(&format!("Date: {}\n", format::full_date(timestamp)));
    text.push_str(&format!("Subject: {}\n", format::subject_label(message)));
    if !message.to_addrs.is_empty() {
        text.push_str(&format!("To: {}\n", message.to_addrs));
    }
    if !message.cc_addrs.is_empty() {
        text.push_str(&format!("Cc: {}\n", message.cc_addrs));
    }
    text.push('\n');
    match body {
        Some(body) if !body.trim().is_empty() => text.push_str(body.trim_end()),
        _ => text.push_str("(no text body)"),
    }
    OutgoingMessage {
        from_name: account.display_name.clone(),
        from_addr: account.email.clone(),
        subject: forward_subject(&message.subject),
        body: text,
        references: Vec::new(),
        ..OutgoingMessage::default()
    }
}

fn reply_subject(subject: &str) -> String {
    let subject = subject.trim();
    if subject.to_ascii_lowercase().starts_with("re:") {
        subject.to_owned()
    } else if subject.is_empty() {
        "Re:".to_owned()
    } else {
        format!("Re: {subject}")
    }
}

fn forward_subject(subject: &str) -> String {
    let subject = subject.trim();
    if subject.to_ascii_lowercase().starts_with("fwd:")
        || subject.to_ascii_lowercase().starts_with("fw:")
    {
        subject.to_owned()
    } else if subject.is_empty() {
        "Fwd:".to_owned()
    } else {
        format!("Fwd: {subject}")
    }
}

fn quoted_reply(message: &MessageHeader, body: Option<&str>) -> String {
    let timestamp = message.sent_at.unwrap_or(message.received_at);
    let mut text = format!(
        "\n\nOn {}, {} wrote:\n",
        format::full_date(timestamp),
        message.from_display()
    );
    let quoted = body
        .map(|body| {
            body.lines()
                .map(|line| {
                    let line = line.trim_end();
                    if line.is_empty() {
                        ">".to_owned()
                    } else {
                        format!("> {line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    text.push_str(&quoted);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use mailin_core::{FolderId, MessageId};

    fn account() -> Account {
        Account::new(
            "Ada",
            "ada@example.com",
            mailin_core::ServerSettings::imap("imap.example.com", "ada@example.com"),
            mailin_core::ServerSettings::smtp("smtp.example.com", "ada@example.com"),
        )
    }

    fn message() -> MessageHeader {
        MessageHeader {
            id: MessageId(1),
            folder_id: FolderId(1),
            uid: 1,
            message_id: Some("parent@example.com".into()),
            thread_key: Some("root@example.com".into()),
            subject: "Quarterly report".into(),
            from_name: Some("Grace Hopper".into()),
            from_addr: Some("grace@example.com".into()),
            to_addrs: "Ada <ada@example.com>, bob@example.com".into(),
            cc_addrs: "carol@example.com".into(),
            sent_at: Some(1_700_000_000),
            received_at: 1_700_000_000,
            snippet: None,
            size: None,
            flags: Vec::new(),
            has_attachments: false,
            body_fetched: true,
        }
    }

    #[test]
    fn reply_goes_to_the_sender_and_quotes_the_body() {
        let draft = reply(&account(), &message(), Some("Hello\nworld"));
        assert_eq!(draft.to, "grace@example.com");
        assert!(draft.cc.is_empty());
        assert_eq!(draft.subject, "Re: Quarterly report");
        assert!(draft.body.contains("> Hello\n> world"));
        assert_eq!(draft.in_reply_to.as_deref(), Some("parent@example.com"));
        assert_eq!(
            draft.references,
            vec!["root@example.com", "parent@example.com"]
        );
    }

    #[test]
    fn reply_all_keeps_other_recipients_but_not_the_account() {
        let draft = reply_all(&account(), &message(), None);
        assert_eq!(draft.to, "grace@example.com");
        assert_eq!(draft.cc, "bob@example.com, carol@example.com");
    }

    #[test]
    fn reply_all_keeps_every_sender_and_deduplicates_cc() {
        let mut original = message();
        original.from_addr = Some("grace@example.com, alan@example.com".into());
        original.to_addrs = "ada@example.com, bob@example.com, BOB@example.com".into();
        let draft = reply_all(&account(), &original, None);
        assert_eq!(draft.to, "grace@example.com, alan@example.com");
        assert_eq!(draft.cc, "bob@example.com, carol@example.com");
    }

    #[test]
    fn subjects_do_not_get_double_prefixes() {
        assert_eq!(reply_subject("Re: Hi"), "Re: Hi");
        assert_eq!(reply_subject(""), "Re:");
        assert_eq!(forward_subject("Fwd: Hi"), "Fwd: Hi");
        assert_eq!(forward_subject("Hi"), "Fwd: Hi");
    }

    #[test]
    fn forward_includes_the_original_headers() {
        let draft = forward(&account(), &message(), Some("Body text"));
        assert_eq!(draft.subject, "Fwd: Quarterly report");
        assert!(
            draft
                .body
                .contains("From: Grace Hopper <grace@example.com>")
        );
        assert!(draft.body.contains("Body text"));
        assert!(!draft.body.contains("root@example.com"));
    }
}
