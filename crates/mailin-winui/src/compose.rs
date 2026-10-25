//! Compose window: build a message, attach files and queue it in the outbox.
//!
//! The behaviour matches the GTK composer: new messages, reply, reply-all and
//! forward, files up to 50 MB, and queueing through `Engine::queue_and_send`.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use mailin_core::compose::recipient_addresses;
use mailin_core::{Account, AccountId, ComposeAttachment, MessageHeader, OutgoingMessage};
use mailin_winui_sys::{Grid, Orientation, StackPanel, TextBlock, TextWrapping, Window};

use crate::app::AppState;
use crate::format;
use crate::platform;
use crate::ui;

/// Open the composer for `account_id`, prefilled with `draft`.
pub fn present(
    app: Arc<AppState>,
    account_id: AccountId,
    draft: OutgoingMessage,
    on_sent: impl Fn() + 'static,
) -> Result<Window> {
    let Some(account) = app.mailin.account(&account_id).ok().flatten() else {
        anyhow::bail!("the selected account no longer exists");
    };
    let draft = OutgoingMessage {
        from_name: account.display_name.clone(),
        from_addr: account.email.clone(),
        ..draft
    };

    let revokers = ui::Revokers::default();
    let attachment_events = ui::Revokers::default();
    let root = Grid::new().context("could not create the compose layout")?;
    ui::auto_row(&root)?;
    ui::auto_row(&root)?;
    ui::star_row(&root)?;

    let header = ui::stack(Orientation::Horizontal, 8.0)?;
    ui::margin(&header, 16.0, 12.0, 16.0, 4.0)?;
    let title = ui::heading(
        &format!("New Message — {}", account.email),
        Some(18.0),
        Some(600),
    )?;
    ui::stretch_horizontal(&title)?;
    ui::append(&header, &title)?;
    let attach = ui::button("Attach files")?;
    let send = ui::button("Send")?;
    ui::append(&header, &attach)?;
    ui::append(&header, &send)?;
    ui::set_row(&header, 0)?;
    ui::append(&root, &header)?;

    let error: TextBlock = ui::text("")?;
    ui::margin(&error, 16.0, 4.0, 16.0, 4.0)?;
    ui::set_visible(&error, false)?;
    ui::set_row(&error, 1)?;
    ui::append(&root, &error)?;

    let form = ui::stack(Orientation::Vertical, 10.0)?;
    ui::margin(&form, 16.0, 6.0, 16.0, 16.0)?;

    let to = ui::text_box("Recipients")?;
    let cc = ui::text_box("Cc")?;
    let bcc = ui::text_box("Bcc")?;
    let subject = ui::text_box("Subject")?;
    to.SetText(&draft.to)?;
    cc.SetText(&draft.cc)?;
    bcc.SetText(&draft.bcc)?;
    subject.SetText(&draft.subject)?;
    ui::append(&form, &ui::field("To", &to)?)?;
    ui::append(&form, &ui::field("Cc", &cc)?)?;
    ui::append(&form, &ui::field("Bcc", &bcc)?)?;
    ui::append(&form, &ui::field("Subject", &subject)?)?;

    let body = ui::text_box("Write your message")?;
    body.SetAcceptsReturn(true)?;
    body.SetTextWrapping(TextWrapping::Wrap)?;
    body.SetText(&draft.body)?;
    ui::min_height(&body, 240.0)?;
    let body_row = ui::stack(Orientation::Vertical, 4.0)?;
    ui::append(&body_row, &ui::heading("Message", Some(12.0), Some(600))?)?;
    ui::append(&body_row, &body)?;
    ui::append(&form, &body_row)?;

    let attachments_title = ui::heading("Attachments", Some(14.0), Some(600))?;
    ui::set_visible(&attachments_title, false)?;
    ui::append(&form, &attachments_title)?;
    let attachment_list = ui::stack(Orientation::Vertical, 4.0)?;
    ui::append(&form, &attachment_list)?;

    let scroller = ui::scroll(&form)?;
    ui::set_row(&scroller, 2)?;
    ui::append(&root, &scroller)?;

    let window = Window::new().context("could not create the compose window")?;
    window.SetTitle("New Message")?;
    window.SetContent(&root)?;

    let attachments: Arc<Mutex<Vec<ComposeAttachment>>> =
        Arc::new(Mutex::new(draft.attachments.clone()));
    rebuild_attachments(
        &attachment_list,
        &attachments_title,
        &attachments,
        &attachment_events,
    );

    revokers.keep(ui::on_click(&attach, {
        let list = attachment_list.clone();
        let title = attachments_title.clone();
        let attachments = Arc::clone(&attachments);
        let events = attachment_events.clone();
        let banner = error.clone();
        move || {
            let paths = platform::pick_files("Attach files");
            let mut failed = None;
            for path in paths {
                match platform::load_attachment(&path) {
                    Ok(attachment) => {
                        if let Ok(mut items) = attachments.lock() {
                            items.push(attachment);
                        }
                    }
                    Err(message) => failed = Some(message),
                }
            }
            match failed {
                Some(message) => {
                    let _ = banner.SetText(&message);
                    let _ = ui::set_visible(&banner, true);
                }
                None => {
                    let _ = ui::set_visible(&banner, false);
                }
            }
            rebuild_attachments(&list, &title, &attachments, &events);
        }
    })?);

    let on_sent: Arc<dyn Fn() + 'static> = Arc::new(on_sent);
    revokers.keep(ui::on_click(&send, {
        let window = window.clone();
        let app = Arc::clone(&app);
        let on_sent = Arc::clone(&on_sent);
        let account = account.clone();
        let draft = draft.clone();
        let attachments = Arc::clone(&attachments);
        let banner = error.clone();
        let to = to.clone();
        let cc = cc.clone();
        let bcc = bcc.clone();
        let subject = subject.clone();
        let body = body.clone();
        move || {
            let outgoing = OutgoingMessage {
                from_name: account.display_name.clone(),
                from_addr: account.email.clone(),
                to: ui::text_value(&to),
                cc: ui::text_value(&cc),
                bcc: ui::text_value(&bcc),
                subject: ui::text_value(&subject),
                body: ui::text_value(&body),
                in_reply_to: draft.in_reply_to.clone(),
                references: draft.references.clone(),
                attachments: attachments
                    .lock()
                    .map(|items| items.clone())
                    .unwrap_or_default(),
            };
            match outgoing.build() {
                Ok((raw, envelope)) => match app.engine.queue_and_send(account.id, raw, envelope) {
                    Ok(_) => {
                        on_sent();
                        let _ = window.Close();
                    }
                    Err(failure) => {
                        let _ = banner.SetText(&format!("Could not queue the message: {failure}"));
                        let _ = ui::set_visible(&banner, true);
                    }
                },
                Err(failure) => {
                    let _ = banner.SetText(&format!("Could not send: {failure}"));
                    let _ = ui::set_visible(&banner, true);
                }
            }
        }
    })?);

    let window_token = ui::keep_alive(&window);
    let cleanup = revokers.clone();
    let attachments_cleanup = attachment_events.clone();
    let closed = window.Closed(move |_, _| {
        cleanup.clear();
        attachments_cleanup.clear();
        ui::release(window_token);
    })?;
    revokers.keep(closed);
    window.Activate()?;
    Ok(window)
}

fn rebuild_attachments(
    list: &StackPanel,
    title: &TextBlock,
    attachments: &Arc<Mutex<Vec<ComposeAttachment>>>,
    events: &ui::Revokers,
) {
    events.clear();
    let _ = ui::set_visible(
        title,
        attachments.lock().map(|a| !a.is_empty()).unwrap_or(false),
    );
    let _ = ui::clear(list);
    let items = attachments
        .lock()
        .map(|items| items.clone())
        .unwrap_or_default();
    for (index, attachment) in items.iter().enumerate() {
        let row = ui::stack(Orientation::Horizontal, 8.0).ok();
        let Some(row) = row else { continue };
        if let Ok(name) = ui::text(&attachment.filename) {
            let _ = ui::stretch_horizontal(&name);
            let _ = ui::append(&row, &name);
        }
        if let Ok(details) = ui::text(&format!(
            "{} · {}",
            attachment.mime_type,
            format::file_size(u32::try_from(attachment.data.len()).unwrap_or(u32::MAX))
        )) {
            let _ = ui::append(&row, &details);
        }
        if let Ok(remove) = ui::button("Remove") {
            let attachments = Arc::clone(attachments);
            let list = list.clone();
            let title = title.clone();
            let events_for_row = events.clone();
            let events_for_remove = events.clone();
            if let Ok(revoker) = ui::on_click(&remove, move || {
                if let Ok(mut items) = attachments.lock()
                    && index < items.len()
                {
                    items.remove(index);
                }
                rebuild_attachments(&list, &title, &attachments, &events_for_remove);
            }) {
                events_for_row.keep(revoker);
            }
            let _ = ui::append(&row, &remove);
        }
        let _ = ui::append(list, &row);
    }
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
    fn subjects_do_not_get_double_prefixes() {
        assert_eq!(reply_subject("Re: Hi"), "Re: Hi");
        assert_eq!(reply_subject(""), "Re:");
        assert_eq!(forward_subject("Fwd: Hi"), "Fwd: Hi");
        assert_eq!(forward_subject("Hi"), "Fwd: Hi");
    }
}
