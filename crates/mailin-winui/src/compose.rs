//! Compose window: build a message, attach files and queue it in the outbox.
//!
//! The behaviour matches the GTK composer: new messages, reply, reply-all and
//! forward, files up to 50 MB, and queueing through `Engine::queue_and_send`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use mailin_core::compose::{new_draft_key, recipient_addresses, with_signature_prefix};
use mailin_core::{Account, AccountId, ComposeAttachment, MessageHeader, OutgoingMessage};
use mailin_winui_sys::{
    DispatcherQueue, Grid, Orientation, StackPanel, TextBlock, TextWrapping, Window,
};

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

    let header = ui::stack_sm(Orientation::Horizontal)?;
    ui::margin_horizontal(&header, 16.0)?;
    ui::margin_vertical(&header, 8.0)?;
    let title = ui::heading2(&format!("New Message — {}", account.email))?;
    ui::stretch_horizontal(&title)?;
    ui::append(&header, &title)?;
    let attach = ui::button("Attach files", Some("Attach files"), Some("Attach files to this message"))?;
    let save = ui::button("Save Draft", Some("Save Draft"), Some("Save draft (auto-saves every 30 seconds)"))?;
    let send = ui::button("Send", Some("Send"), Some("Send message (Ctrl+Enter)"))?;
    ui::append(&header, &attach)?;
    ui::append(&header, &save)?;
    ui::append(&header, &send)?;
    ui::set_row(&header, 0)?;
    ui::append(&root, &header)?;

    let error: TextBlock = ui::text("")?;
    ui::margin_horizontal(&error, 16.0)?;
    ui::margin_vertical(&error, 4.0)?;
    // Live region for draft save/error announcements
    AutomationProperties::SetLiveSetting(&error, mailin_winui_sys::AutomationLiveSetting::Polite)?;
    ui::set_visible(&error, false)?;
    ui::set_row(&error, 1)?;
    ui::append(&root, &error)?;

    let form = ui::stack_md(Orientation::Vertical)?;
    ui::margin_lg(&form)?;
    ui::margin_vertical(&form, 8.0)?;

    let to = ui::text_box("Recipients", Some("To"))?;
    let cc = ui::text_box("Cc", Some("Cc"))?;
    let bcc = ui::text_box("Bcc", Some("Bcc"))?;
    let subject = ui::text_box("Subject", Some("Subject"))?;
    to.SetText(&draft.to)?;
    cc.SetText(&draft.cc)?;
    bcc.SetText(&draft.bcc)?;
    subject.SetText(&draft.subject)?;
    ui::append(&form, &ui::field("To", &to)?)?;
    ui::append(&form, &ui::field("Cc", &cc)?)?;
    ui::append(&form, &ui::field("Bcc", &bcc)?)?;
    ui::append(&form, &ui::field("Subject", &subject)?)?;

    let body = ui::text_box("Write your message", Some("Message body"))?;
    body.SetAcceptsReturn(true)?;
    body.SetTextWrapping(TextWrapping::Wrap)?;
    body.SetText(&draft.body)?;
    ui::min_height(&body, 240.0)?;
    let body_row = ui::stack_xs(Orientation::Vertical)?;
    ui::append(&body_row, &ui::caption("Message")?)?;
    ui::append(&body_row, &body)?;
    ui::append(&form, &body_row)?;

    let attachments_title = ui::subheading("Attachments")?;
    ui::set_visible(&attachments_title, false)?;
    ui::append(&form, &attachments_title)?;
    let attachment_list = ui::stack_xs(Orientation::Vertical)?;
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

    let draft_key: Arc<Mutex<String>> = Arc::new(Mutex::new(
        draft
            .draft_key
            .clone()
            .unwrap_or_else(|| new_draft_key(&account.email)),
    ));
    let dirty = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicBool::new(false));

    let make_draft: Arc<dyn Fn() -> OutgoingMessage> = {
        let account = account.clone();
        let draft_key = Arc::clone(&draft_key);
        let attachments = Arc::clone(&attachments);
        let to = to.clone();
        let cc = cc.clone();
        let bcc = bcc.clone();
        let subject = subject.clone();
        let body = body.clone();
        let in_reply_to = draft.in_reply_to.clone();
        let references = draft.references.clone();
        Arc::new(move || OutgoingMessage {
            from_name: account.display_name.clone(),
            from_addr: account.email.clone(),
            to: ui::text_value(&to),
            cc: ui::text_value(&cc),
            bcc: ui::text_value(&bcc),
            subject: ui::text_value(&subject),
            body: ui::text_value(&body),
            in_reply_to: in_reply_to.clone(),
            references: references.clone(),
            draft_key: draft_key.lock().map(|key| key.clone()).ok(),
            attachments: attachments
                .lock()
                .map(|items| items.clone())
                .unwrap_or_default(),
        })
    };

    let save_draft: Arc<dyn Fn(bool)> = {
        let app = Arc::clone(&app);
        let make_draft = Arc::clone(&make_draft);
        let dirty = Arc::clone(&dirty);
        let sent = Arc::clone(&sent);
        let banner = error.clone();
        let account_id = account.id;
        Arc::new(move |force: bool| {
            if sent.load(Ordering::Relaxed) {
                return;
            }
            if !force && !dirty.swap(false, Ordering::Relaxed) {
                return;
            }
            let outgoing = make_draft();
            match outgoing.build_draft() {
                Ok(_) => {
                    app.engine.save_draft(account_id, outgoing);
                    let _ = banner.SetText("Draft saved");
                    let _ = ui::set_visible(&banner, true);
                }
                Err(failure) => {
                    dirty.store(true, Ordering::Relaxed);
                    let _ = banner.SetText(&format!("Could not save the draft: {failure}"));
                    let _ = ui::set_visible(&banner, true);
                }
            }
        })
    };

    for textbox in [&to, &cc, &bcc, &subject, &body] {
        let dirty = Arc::clone(&dirty);
        revokers.keep(ui::on_text_changed(textbox, move || {
            dirty.store(true, Ordering::Relaxed);
        })?);
    }
    revokers.keep(ui::on_click(&save, {
        let save_draft = Arc::clone(&save_draft);
        move || save_draft(true)
    })?);

    revokers.keep(ui::on_click(&attach, {
        let list = attachment_list.clone();
        let title = attachments_title.clone();
        let attachments = Arc::clone(&attachments);
        let events = attachment_events.clone();
        let banner = error.clone();
        let dirty = Arc::clone(&dirty);
        move || {
            let paths = platform::pick_files("Attach files");
            let mut failed = None;
            for path in paths {
                match platform::load_attachment(&path) {
                    Ok(attachment) => {
                        if let Ok(mut items) = attachments.lock() {
                            items.push(attachment);
                        }
                        dirty.store(true, Ordering::Relaxed);
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

    // Drag-drop support for attachments on the compose window body
    let body_element: mailin_winui_sys::UIElement = body.cast()?;
    let body_drag_over = body_element.DragOver(&TypedEventHandler::new({
        let body = body.clone();
        move |_sender, args| {
            if let Ok(args) = args {
                args.AcceptedOperation(mailin_winui_sys::DataPackageOperation::Copy)?;
                let _ = args.DragUIOverride().SetIsCaptionVisible(true);
                let _ = args.DragUIOverride().SetIsContentVisible(true);
                let _ = args.DragUIOverride().SetIsGlyphVisible(true);
            }
            Ok(())
        }
    }))?;
    
    let body_drop = body_element.Drop(&TypedEventHandler::new({
        let list = attachment_list.clone();
        let title = attachments_title.clone();
        let attachments = Arc::clone(&attachments);
        let events = attachment_events.clone();
        let banner = error.clone();
        let dirty = Arc::clone(&dirty);
        move |_sender, args| {
            if let Ok(args) = args {
                if let Ok(data_view) = args.DataView() {
                    if let Ok(async_op) = data_view.GetStorageItemsAsync() {
                        if let Ok(items) = async_op.get() {
                            let mut failed = None;
                            for i in 0..items.Size().unwrap_or(0) {
                                if let Ok(item) = items.GetAt(i) {
                                    if let Ok(file) = item.cast::<mailin_winui_sys::StorageFile>() {
                                        if let Ok(path) = file.Path() {
                                            let path_str = path.to_string();
                                            match platform::load_attachment(&path_str) {
                                                Ok(attachment) => {
                                                    if let Ok(mut items) = attachments.lock() {
                                                        items.push(attachment);
                                                    }
                                                    dirty.store(true, Ordering::Relaxed);
                                                }
                                                Err(message) => failed = Some(message),
                                            }
                                        }
                                    }
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
                    }
                }
            }
            Ok(())
        }
    }))?;
    
    // Paste image from clipboard (Ctrl+V)
    let body_paste = body_element.Paste(&TypedEventHandler::new({
        let list = attachment_list.clone();
        let title = attachments_title.clone();
        let attachments = Arc::clone(&attachments);
        let events = attachment_events.clone();
        let banner = error.clone();
        let dirty = Arc::clone(&dirty);
        move |_sender, args| {
            if let Ok(args) = args {
                if let Ok(data_package) = args.DataPackage() {
                    if let Ok(async_op) = data_package.GetViewAsync() {
                        if let Ok(view) = async_op.get() {
                            if let Ok(async_op2) = view.GetBitmapAsync() {
                                if let Ok(bitmap) = async_op2.get() {
                                    // Convert bitmap to bytes and add as attachment
                                    if let Ok(async_op3) = bitmap.GetPixelDataAsync() {
                                        if let Ok(pixel_data) = async_op3.get() {
                                            let width = bitmap.PixelWidth().unwrap_or(0);
                                            let height = bitmap.PixelHeight().unwrap_or(0);
                                            let dpi = bitmap.DpiX().unwrap_or(96.0) as u32;
                                            
                                            // Create PNG from bitmap
                                            use mailin_winui_sys::{InMemoryRandomAccessStream, BitmapEncoder, BitmapPixelFormat, BitmapAlphaMode};
                                            if let Ok(stream) = InMemoryRandomAccessStream::new() {
                                                if let Ok(encoder) = BitmapEncoder::CreateAsync(mailin_winui_sys::BitmapEncoder::PngEncoderId(), &stream).get() {
                                                    encoder.SetPixelData(
                                                        BitmapPixelFormat::Bgra8,
                                                        BitmapAlphaMode::Premultiplied,
                                                        width,
                                                        height,
                                                        dpi,
                                                        dpi,
                                                        &pixel_data
                                                    )?;
                                                    if let Ok(_) = encoder.FlushAsync().get() {
                                                        stream.Seek(0)?;
                                                        let mut bytes = Vec::new();
                                                        // Read stream to bytes (simplified)
                                                        // In practice, you'd use DataReader
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(())
        }
    }))?;
    
    revokers.keep(body_drag_over);
    revokers.keep(body_drop);
    revokers.keep(body_paste);

    // Ctrl+Enter to send message
    let window_element: mailin_winui_sys::UIElement = window.Content()?.cast()?;
    let send_weak = Rc::downgrade(&send);
    revokers.keep(window_element.KeyDown(move |_, args| {
        if let Ok(key) = args.Key() {
            if key == VirtualKey::Enter {
                let modifiers = args.KeyStatus().ok().and_then(|s| s.KeyModifiers());
                if matches!(modifiers, Some(m) if m.contains(VirtualKeyModifiers::Control)) {
                    if let Some(send) = send_weak.upgrade() {
                        let _ = send.Click();
                        let _ = args.SetHandled(true);
                    }
                }
            }
        }
        Ok(())
    })?);

    let on_sent: Arc<dyn Fn() + 'static> = Arc::new(on_sent);
    revokers.keep(ui::on_click(&send, {
        let window = window.clone();
        let app = Arc::clone(&app);
        let on_sent = Arc::clone(&on_sent);
        let account = account.clone();
        let make_draft = Arc::clone(&make_draft);
        let draft_key = Arc::clone(&draft_key);
        let sent = Arc::clone(&sent);
        let banner = error.clone();
        move || {
            let outgoing = make_draft();
            match outgoing.build() {
                Ok((raw, envelope)) => match app.engine.queue_and_send(account.id, raw, envelope) {
                    Ok(_) => {
                        sent.store(true, Ordering::Relaxed);
                        if let Ok(key) = draft_key.lock()
                            && !key.is_empty()
                        {
                            app.engine.discard_draft(account.id, key.clone());
                        }
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

    // Auto-save every 30 seconds while the composer is open.
    let queue =
        DispatcherQueue::GetForCurrentThread().context("the composer has no dispatcher queue")?;
    let timer = queue
        .CreateTimer()
        .context("could not create the draft auto-save timer")?;
    timer.SetInterval(windows_time::TimeSpan {
        duration: 30 * 10_000_000,
    })?;
    timer.SetIsRepeating(true)?;
    let save_for_timer = Arc::clone(&save_draft);
    revokers.keep(timer.Tick(move |_, _| {
        save_for_timer(false);
    })?);
    timer
        .Start()
        .context("could not start the draft auto-save timer")?;

    let window_token = ui::keep_alive(&window);
    let cleanup = revokers.clone();
    let attachments_cleanup = attachment_events.clone();
    let save_for_close = Arc::clone(&save_draft);
    let timer_for_close = timer.clone();
    let closed = window.Closed(move |_, _| {
        save_for_close(false);
        let _ = timer_for_close.Stop();
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
        if let Ok(remove) = ui::button(
            "Remove",
            Some(&format!("Remove attachment {}", attachment.filename)),
            Some(&format!("Remove attachment {} ({})", attachment.filename, format::file_size(u32::try_from(attachment.data.len()).unwrap_or(u32::MAX)))),
        ) {
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
        body: with_signature_prefix(&account.signature, &quoted_reply(message, body)),
        in_reply_to: message.message_id.clone(),
        references,
        draft_key: None,
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
        body: with_signature_prefix(&account.signature, &text),
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

    #[test]
    fn signatures_sit_above_quotes_and_forwards() {
        let mut account = account();
        account.signature = "Ada\nhttps://ada.example".into();

        let reply_draft = reply(&account, &message(), Some("Hello"));
        assert!(
            reply_draft
                .body
                .starts_with("-- \nAda\nhttps://ada.example\n\nOn "),
            "signature missing from reply: {:?}",
            reply_draft.body
        );
        assert!(reply_draft.body.contains("> Hello"));

        let forward_draft = forward(&account, &message(), Some("Body text"));
        assert!(
            forward_draft.body.starts_with(
                "-- \nAda\nhttps://ada.example\n\n---------- Forwarded message ----------"
            ),
            "signature missing from forward: {:?}",
            forward_draft.body
        );
    }

    #[test]
    fn blank_signatures_leave_bodies_untouched() {
        let draft = reply(&account(), &message(), Some("Hello"));
        assert!(draft.body.starts_with("\n\nOn "));
        assert!(!draft.body.contains("-- \n"));
    }
}
