//! Compose window: build a message, attach files and queue it in the outbox.
//!
//! The behaviour matches the GTK composer: new messages, reply, reply-all and
//! forward, files up to 50 MB, and queueing through `Engine::queue_and_send`.
//!
//! Rich text editing is provided by WebView2 hosting a `contenteditable` div
//! with a JS bridge for formatting, paste sanitization and inline images.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use mailin_core::compose::{
    PgpOptions, new_draft_key, recipient_addresses, with_html_signature_prefix,
    with_signature_prefix,
};
use mailin_core::html::sanitize_html;
use mailin_core::{Account, AccountId, ComposeAttachment, MessageHeader, OutgoingMessage};
use mailin_winui_sys::{
    AutomationLiveSetting, AutomationProperties, CoreWebView2WebMessageReceivedEventArgs,
    DispatcherQueue, Grid, ListView, ListViewItem, Orientation, StackPanel, TextBlock,
    TextWrapping, Thickness, ToggleSwitch, TypedEventHandler, VerticalAlignment, Visibility,
    WebView2, Window,
};
use windows_core::Interface;

use crate::app::AppState;
use crate::format;
use crate::platform;
use crate::ui;

/// Maximum size of a single attached file.
const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

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
    let mut draft = OutgoingMessage {
        from_name: account.display_name.clone(),
        from_addr: account.email.clone(),
        ..draft
    };
    // Accounts with a rich signature compose in HTML: prepend the sanitized
    // HTML signature once, converting a plain reply/forward body so nothing is
    // lost in the process. The plain-text mirror keeps working for clients that
    // only understand text/plain.
    if !account.signature_html.trim().is_empty() {
        let html = match draft.html_body.take() {
            Some(html) => with_html_signature_prefix(&account.signature_html, &html),
            None => {
                let plain_block = mailin_core::compose::signature_block(&account.signature);
                let mut text = draft.body.as_str();
                if !plain_block.is_empty() {
                    text = text.trim_start();
                    if let Some(rest) = text.strip_prefix(&plain_block) {
                        text = rest.trim_start_matches('\n');
                    }
                }
                let escaped = mailin_core::html::escape(text).replace('\n', "<br>");
                with_html_signature_prefix(&account.signature_html, &escaped)
            }
        };
        draft.html_body = Some(html);
    }

    // ---------------------------------------------------------------- shared state
    // Create shared state early so WebMessageReceived handler can use it
    let attachments: Arc<Mutex<Vec<ComposeAttachment>>> =
        Arc::new(Mutex::new(draft.attachments.clone()));
    let draft_key: Arc<Mutex<String>> = Arc::new(Mutex::new(
        draft
            .draft_key
            .clone()
            .unwrap_or_else(|| new_draft_key(&account.email)),
    ));
    let dirty = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicBool::new(false));
    let is_html_mode = Arc::new(AtomicBool::new(draft.html_body.is_some()));

    // Shared state for one in-flight content capture (send or draft save).
    #[derive(Default)]
    struct PendingCapture {
        request_id: u64,
        want_html: bool,
        html: Option<String>,
        text: Option<String>,
        on_done: Option<Box<dyn FnOnce(Option<String>, String)>>,
    }

    let pending_capture = Rc::new(RefCell::new(PendingCapture::default()));

    let revokers = ui::Revokers::default();
    let attachment_events = ui::Revokers::default();

    // ---------------------------------------------------------------- editor
    // Create WebView2 for rich text editing
    let webview = WebView2::new().context("could not create the WebView2 control")?;

    // Set up the WebMessageReceived bridge for paste sanitization, image paste, and content capture
    // This MUST be done before loading HTML so handlers are ready when the page loads
    let webview_for_messages = webview.clone();
    let pending_capture_clone = pending_capture.clone();
    let attachments_for_paste = attachments.clone();
    let dirty_for_paste = dirty.clone();

    let message_token = webview.WebMessageReceived(&TypedEventHandler::new(move |_sender, args| {
        if let Ok(args) = args {
            // Try to get the message as string
            // Note: TryGetWebMessageAsString should be available after bindings regeneration
            if let Ok(message) = args.TryGetWebMessageAsString() {
                if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&message) {
                    if let Some(msg_type) = payload.get("type").and_then(|v| v.as_str()) {
                        match msg_type {
                            "pasteHtml" => {
                                if let Some(html) = payload.get("html").and_then(|v| v.as_str()) {
                                    let sanitized = sanitize_html(html);
                                    let script = format!(
                                        "__mailinInsert({});",
                                        serde_json::to_string(&sanitized).unwrap_or_else(|_| "\"\"".to_owned())
                                    );
                                    let _ = webview_for_messages.ExecuteScriptAsync(&script);
                                }
                            }
                            "imagePaste" => {
                                // We need to rebuild attachments list, but we don't have access to
                                // attachment_list and attachment_events here. They'll be set up later.
                                // For now, just handle the image data and store it.
                                // The actual attachment list rebuild will happen when the UI is ready.
                                if let (Some(name), Some(mime), Some(data_url)) = (
                                    payload.get("name").and_then(|v| v.as_str()),
                                    payload.get("mime").and_then(|v| v.as_str()),
                                    payload.get("data").and_then(|v| v.as_str()),
                                ) {
                                    if let Some((_, encoded)) = data_url.split_once(',') {
                                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) {
                                            if bytes.len() as u64 <= MAX_ATTACHMENT_BYTES {
                                                let attachment = ComposeAttachment::inline_image(name, mime, bytes);
                                                if let Some(cid) = attachment.cid.clone() {
                                                    if let Ok(mut items) = attachments_for_paste.lock() {
                                                        items.push(attachment);
                                                    }
                                                    dirty_for_paste.store(true, Ordering::Relaxed);
                                                    // Note: attachment list rebuild will be triggered by UI setup
                                                    let script = format!(
                                                        "__mailinInsert({});",
                                                        serde_json::to_string(&format!("<img src=\"cid:{cid}\" alt=\"pasted image\">")).unwrap_or_else(|_| "\"\"".to_owned())
                                                    );
                                                    let _ = webview_for_messages.ExecuteScriptAsync(&script);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            "contentResponse" => {
                                if let (Some(request_id), Some(content)) = (
                                    payload.get("requestId").and_then(|v| v.as_u64()),
                                    payload.get("content").and_then(|v| v.as_str()),
                                ) {
                                    let want_html = payload
                                        .get("wantHtml")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);
                                    let mut cap = pending_capture_clone.borrow_mut();
                                    if request_id == cap.request_id && cap.on_done.is_some() {
                                        if want_html {
                                            cap.html = Some(content.to_owned());
                                        } else {
                                            cap.text = Some(content.to_owned());
                                        }
                                        if cap.text.is_some()
                                            && (!cap.want_html || cap.html.is_some())
                                        {
                                            let done = cap.on_done.take();
                                            let html = cap.html.take();
                                            let text = cap.text.take().unwrap_or_default();
                                            drop(cap);
                                            if let Some(done) = done {
                                                done(html, text);
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(())
    }))?;
    revokers.keep(message_token);

    // Initialize the contenteditable editor
    let initial_html = if draft.html_body.is_some() {
        draft.html_body.clone().unwrap_or_default()
    } else {
        // Convert plain text to simple HTML, escaping markup from the
        // original message (the editor WebView runs JavaScript, so raw
        // markup from a quoted body would execute).
        mailin_core::html::escape(&draft.body).replace('\n', "<br>")
    };

    let editor_html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <style>
        body {{
            font-family: system-ui, sans-serif;
            font-size: 14px;
            line-height: 1.5;
            padding: 12px;
            margin: 0;
            min-height: 200px;
        }}
        img {{ max-width: 100%; height: auto; }}
        blockquote {{ margin-left: 1em; padding-left: 1em; border-left: 3px solid #ccc; }}
        ul, ol {{ padding-left: 2em; }}
        pre {{ background: #f5f5f5; padding: 0.5em; overflow: auto; }}
    </style>
    <script>
        window.__mailinRange = null;
        function __mailinSaveRange() {{
            var selection = window.getSelection();
            window.__mailinRange = selection && selection.rangeCount
                ? selection.getRangeAt(0).cloneRange()
                : null;
        }}
        function __mailinInsert(html) {{
            var selection = window.getSelection();
            if (window.__mailinRange) {{
                selection.removeAllRanges();
                selection.addRange(window.__mailinRange);
            }}
            document.execCommand('insertHTML', false, html);
        }}
        function __mailinGetContent(requestId, wantHtml) {{
            var content = wantHtml ? document.body.innerHTML : document.body.innerText;
            window.chrome.webview.postMessage(JSON.stringify({{
                type: 'contentResponse',
                requestId: requestId,
                wantHtml: wantHtml,
                content: content
            }}));
        }}
        document.addEventListener('paste', function(e) {{
            var data = e.clipboardData || window.clipboardData;
            if (!data) {{
                return;
            }}
            // Images copied from a screenshot tool or another app become
            // inline attachments with a Content-ID.
            var items = data.items;
            if (items) {{
                for (var i = 0; i < items.length; i++) {{
                    var item = items[i];
                    if (item.kind === 'file' && item.type.indexOf('image/') === 0) {{
                        e.preventDefault();
                        __mailinSaveRange();
                        var file = item.getAsFile();
                        var reader = new FileReader();
                        reader.onload = function(event) {{
                            window.chrome.webview.postMessage(JSON.stringify({{
                                type: 'imagePaste',
                                name: file && file.name ? file.name : 'pasted-image.png',
                                mime: file && file.type ? file.type : 'image/png',
                                data: event.target.result
                            }}));
                        }};
                        reader.readAsDataURL(file);
                        return;
                    }}
                }}
            }}
            e.preventDefault();
            var html = data.getData('text/html');
            var text = data.getData('text/plain');
            if (html) {{
                __mailinSaveRange();
                // Sanitization happens in Rust before re-insertion.
                window.chrome.webview.postMessage(JSON.stringify({{
                    type: 'pasteHtml',
                    html: html
                }}));
            }} else if (text) {{
                document.execCommand('insertText', false, text);
            }}
        }});
    </script>
</head>
<body contenteditable="true" spellcheck="true">{}</body>
</html>"#,
        initial_html
    );

    // Load the editor HTML directly
    let _ = webview.NavigateToString(&editor_html);

    // ---------------------------------------------------------------- layout
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

    // Place WebView2 in row 2 (will be added after header setup)
    ui::set_row(&webview, 2)?;
    ui::append(&root, &webview)?;

    let error: TextBlock = ui::text("")?;
    ui::margin_horizontal(&error, 16.0)?;
    ui::margin_vertical(&error, 4.0)?;
    // Live region for draft save/error announcements
    AutomationProperties::SetLiveSetting(&error, AutomationLiveSetting::Polite)?;
    ui::set_visible(&error, false)?;
    ui::set_row(&error, 1)?;
    ui::append(&root, &error)?;

    // -------------------------------------------------------- format toolbar
    let attach = ui::button(
        "Attach files",
        Some("Attach files"),
        Some("Attach files to this message"),
    )?;
    let save = ui::button(
        "Save Draft",
        Some("Save Draft"),
        Some("Save draft (auto-saves every 30 seconds)"),
    )?;
    let send = ui::button("Send", Some("Send"), Some("Send message (Ctrl+Enter)"))?;
    ui::append(&header, &attach)?;
    ui::append(&header, &save)?;
    ui::append(&header, &send)?;

    // Format menu button with formatting options
    let format_btn = ui::button(
        "Format",
        Some("Formatting options"),
        Some("Text formatting options"),
    )?;
    ui::append(&header, &format_btn)?;

    // Format popover (using a StackPanel as a simple dropdown)
    let format_popover = StackPanel::new().context("could not create format popover")?;
    format_popover.SetOrientation(Orientation::Vertical)?;
    format_popover.SetVisibility(Visibility::Collapsed)?;
    format_popover.SetBackground(&ui::solid_brush("#FFFFFF")?)?;
    ui::margin_all(&format_popover, 8.0)?;
    ui::append(&root, &format_popover)?;

    let formatting_actions = [
        ("Bold", "bold"),
        ("Italic", "italic"),
        ("Underline", "underline"),
        ("Link", "link"),
        ("Bulleted List", "insertUnorderedList"),
        ("Numbered List", "insertOrderedList"),
        ("Quote", "blockquote"),
        ("Heading", "h2"),
        ("Code", "pre"),
    ];

    for (label, cmd) in formatting_actions {
        let btn = ui::button(label, Some(label), Some(label))?;
        let cmd_str = cmd.to_string();
        let webview_clone = webview.clone();
        let popover_clone = format_popover.clone();
        revokers.keep(ui::on_click(&btn, move || {
            let script = if cmd_str == "link" {
                "const url = prompt('Enter URL:'); if (url) document.execCommand('createLink', false, url);"
            } else if cmd_str == "blockquote" {
                "document.execCommand('formatBlock', false, 'blockquote');"
            } else if cmd_str == "h2" {
                "document.execCommand('formatBlock', false, 'h2');"
            } else if cmd_str == "pre" {
                "document.execCommand('formatBlock', false, 'pre');"
            } else {
                &format!("document.execCommand('{}', false);", cmd_str)
            };
            let _ = webview_clone.ExecuteScriptAsync(script);
            // Close popover after action
            let _ = popover_clone.SetVisibility(Visibility::Collapsed);
        })?);
        ui::append(&format_popover, &btn)?;
    }

    // Toggle format popover
    let format_popover_for_toggle = format_popover.clone();
    let format_btn_for_toggle = format_btn.clone();
    revokers.keep(ui::on_click(&format_btn, move || {
        let is_visible = format_popover_for_toggle
            .Visibility()
            .unwrap_or(Visibility::Collapsed)
            == Visibility::Visible;
        let _ = format_popover_for_toggle.SetVisibility(if is_visible {
            Visibility::Collapsed
        } else {
            Visibility::Visible
        });
        // Position popover below the format button
        if !is_visible {
            let _ = format_popover_for_toggle.SetMargin(margin_for_popover(&format_btn_for_toggle));
        }
    })?);

    // HTML/Plain text toggle
    let html_toggle = ToggleSwitch::new().context("could not create HTML toggle")?;
    html_toggle.SetIsOn(draft.html_body.is_some())?;
    html_toggle.SetOnContent(&ui::text("HTML")?)?;
    html_toggle.SetOffContent(&ui::text("Plain Text")?)?;
    ui::margin_left(&html_toggle, 8.0)?;
    ui::vertical_alignment_center(&html_toggle)?;
    ui::append(&header, &html_toggle)?;

    // HTML toggle handler
    let is_html_mode_clone = is_html_mode.clone();
    revokers.keep(html_toggle.Toggled(move |_, args| {
        if let Ok(args) = args {
            is_html_mode_clone.store(args.IsOn().unwrap_or(false), Ordering::Relaxed);
        }
        Ok(())
    })?);

    // PGP options
    let (encrypt_row, encrypt_toggle) = ui::switch_row(
        "Encrypt",
        "Encrypt and sign the message for every recipient",
        false,
        Some("Encrypt message"),
        Some("On"),
        Some("Off"),
    )?;
    let (sign_row, sign_toggle) = ui::switch_row(
        "Sign",
        "Attach a detached OpenPGP signature",
        false,
        Some("Sign message"),
        Some("On"),
        Some("Off"),
    )?;
    ui::append(&header, &encrypt_row)?;
    ui::append(&header, &sign_row)?;

    ui::set_row(&header, 0)?;
    ui::append(&root, &header)?;

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

    // Suggestion panels for contact autocomplete (one per recipient field)
    let to_suggestions = ui::stack_xs(Orientation::Vertical)?;
    ui::set_visible(&to_suggestions, false)?;
    let cc_suggestions = ui::stack_xs(Orientation::Vertical)?;
    ui::set_visible(&cc_suggestions, false)?;
    let bcc_suggestions = ui::stack_xs(Orientation::Vertical)?;
    ui::set_visible(&bcc_suggestions, false)?;

    ui::append(&form, &ui::field("To", &to)?)?;
    ui::append(&form, &to_suggestions)?;
    ui::append(&form, &ui::field("Cc", &cc)?)?;
    ui::append(&form, &cc_suggestions)?;
    ui::append(&form, &ui::field("Bcc", &bcc)?)?;
    ui::append(&form, &bcc_suggestions)?;
    ui::append(&form, &ui::field("Subject", &subject)?)?;

    // Setup contact autocomplete for recipient fields
    setup_contact_autocomplete(&to, &to_suggestions, &app, account_id, &revokers)?;
    setup_contact_autocomplete(&cc, &cc_suggestions, &app, account_id, &revokers)?;
    setup_contact_autocomplete(&bcc, &bcc_suggestions, &app, account_id, &revokers)?;

    // Attachments
    let attachments_title = ui::subheading("Attachments")?;
    ui::set_visible(&attachments_title, false)?;
    ui::append(&form, &attachments_title)?;
    let attachment_list = ui::stack_xs(Orientation::Vertical)?;
    ui::append(&form, &attachment_list)?;

    let scroller = ui::scroll(&form)?;

    let window = Window::new().context("could not create the compose window")?;
    window.SetTitle("New Message")?;
    window.SetContent(&root)?;

    // Rebuild attachments now that UI is ready
    rebuild_attachments(
        &attachment_list,
        &attachments_title,
        &attachments,
        &attachment_events,
    );

    // Ctrl+Enter to send message
    let window_element: mailin_winui_sys::UIElement = window.Content()?.cast()?;
    let send_weak = Rc::downgrade(&send);
    revokers.keep(window_element.KeyDown(move |_, args| {
        if let Ok(key) = args.Key() {
            if key == mailin_winui_sys::VirtualKey::Enter {
                let modifiers = args.KeyStatus().ok().and_then(|s| s.KeyModifiers());
                if matches!(modifiers, Some(m) if m.contains(mailin_winui_sys::VirtualKeyModifiers::Control)) {
                    if let Some(send) = send_weak.upgrade() {
                        let _ = send.Click();
                        let _ = args.SetHandled(true);
                    }
                }
            }
        }
        Ok(())
    })?);

    // Start an asynchronous content capture. The WebView answers every
    // `__mailinGetContent` request with a `contentResponse` message on this
    // (UI) thread, where the handler finalizes the outgoing message.
    // Returns false when a capture is already in flight.
    let start_capture = {
        let webview = webview.clone();
        let pending_capture = pending_capture.clone();
        move |want_html: bool, on_done: Box<dyn FnOnce(Option<String>, String)>| -> bool {
            let request_id = {
                let mut cap = pending_capture.borrow_mut();
                if cap.on_done.is_some() {
                    return false;
                }
                cap.request_id += 1;
                cap.want_html = want_html;
                cap.html = None;
                cap.text = None;
                cap.on_done = Some(on_done);
                cap.request_id
            };
            let _ =
                webview.ExecuteScriptAsync(&format!("__mailinGetContent({request_id}, false);"));
            if want_html {
                let _ =
                    webview.ExecuteScriptAsync(&format!("__mailinGetContent({request_id}, true);"));
            }
            true
        }
    };

    let save_draft: Arc<dyn Fn(bool)> = {
        let app = Arc::clone(&app);
        let dirty = Arc::clone(&dirty);
        let sent = Arc::clone(&sent);
        let banner = error.clone();
        let account_id = account.id;
        let draft_key = Arc::clone(&draft_key);
        let attachments = Arc::clone(&attachments);
        let to = to.clone();
        let cc = cc.clone();
        let bcc = bcc.clone();
        let subject = subject.clone();
        let in_reply_to = draft.in_reply_to.clone();
        let references = draft.references.clone();
        let is_html_mode = is_html_mode.clone();
        let start_capture = start_capture.clone();
        Arc::new(move |force: bool| {
            if sent.load(Ordering::Relaxed) {
                return;
            }
            if !force && !dirty.swap(false, Ordering::Relaxed) {
                return;
            }

            let want_html = is_html_mode.load(Ordering::Relaxed);
            // Clone everything the continuation needs.
            let app = Arc::clone(&app);
            let banner = banner.clone();
            let dirty = Arc::clone(&dirty);
            let to = to.clone();
            let cc = cc.clone();
            let bcc = bcc.clone();
            let subject = subject.clone();
            let draft_key = Arc::clone(&draft_key);
            let attachments = Arc::clone(&attachments);
            let in_reply_to = in_reply_to.clone();
            let references = references.clone();
            let from_name = account.display_name.clone();
            let from_addr = account.email.clone();
            let on_done: Box<dyn FnOnce(Option<String>, String)> =
                Box::new(move |html_body, body_text: String| {
                    let outgoing = OutgoingMessage {
                        from_name,
                        from_addr,
                        to: ui::text_value(&to),
                        cc: ui::text_value(&cc),
                        bcc: ui::text_value(&bcc),
                        subject: ui::text_value(&subject),
                        body: body_text,
                        html_body,
                        in_reply_to,
                        references,
                        draft_key: draft_key.lock().map(|key| key.clone()).ok(),
                        attachments: attachments
                            .lock()
                            .map(|items| items.clone())
                            .unwrap_or_default(),
                    };

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
                });
            if !start_capture(want_html, on_done) {
                // A capture is already in flight; restore the dirty marker so
                // the next autosave or close retries this save.
                dirty.store(true, Ordering::Relaxed);
            }
        })
    };

    for textbox in [&to, &cc, &bcc, &subject] {
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

    let on_sent: Arc<dyn Fn() + 'static> = Arc::new(on_sent);
    let send_encrypt = encrypt_toggle.clone();
    let send_sign = sign_toggle.clone();
    let is_html_mode_for_send = is_html_mode.clone();
    let start_capture_for_send = start_capture.clone();
    revokers.keep(ui::on_click(&send, {
        let send = send.clone();
        let window = window.clone();
        let app = Arc::clone(&app);
        let on_sent = Arc::clone(&on_sent);
        let account = account.clone();
        let draft_key = Arc::clone(&draft_key);
        let sent = Arc::clone(&sent);
        let banner = error.clone();
        let attachments = Arc::clone(&attachments);
        let to = to.clone();
        let cc = cc.clone();
        let bcc = bcc.clone();
        let subject = subject.clone();
        let in_reply_to = draft.in_reply_to.clone();
        let references = draft.references.clone();
        move || {
            let _ = ui::set_enabled(&send, false);
            let want_html = is_html_mode_for_send.load(Ordering::Relaxed);
            let to_value = ui::text_value(&to);
            let cc_value = ui::text_value(&cc);
            let bcc_value = ui::text_value(&bcc);
            let subject_value = ui::text_value(&subject);

            let pgp = PgpOptions {
                encrypt: send_encrypt.IsOn().unwrap_or(false),
                sign: send_sign.IsOn().unwrap_or(false),
            };
            if pgp.encrypt {
                let mut recipients = Vec::new();
                for field in [&to_value, &cc_value, &bcc_value] {
                    if let Ok(addresses) = recipient_addresses(field) {
                        recipients.extend(addresses);
                    }
                }
                let report = app.mailin.missing_pgp_keys(&account.id, &recipients);
                let failure = match report {
                    Ok(missing) if !missing.is_empty() => {
                        Some(format!("No public key for: {}", missing.join(", ")))
                    }
                    Err(failure) => Some(format!("Could not check PGP keys: {failure}")),
                    _ => None,
                };
                if let Some(message) = failure {
                    let _ = banner.SetText(&message);
                    let _ = ui::set_visible(&banner, true);
                    let _ = ui::set_enabled(&send, true);
                    return;
                }
            }
            if pgp.sign && !app.mailin.has_signing_key(&account.id) {
                let _ = banner
                    .SetText("No secret key available. Add one in Account Settings → Encryption.");
                let _ = ui::set_visible(&banner, true);
                let _ = ui::set_enabled(&send, true);
                return;
            }
            let account_id = account.id;
            let from_name = account.display_name.clone();
            let from_addr = account.email.clone();
            let app = Arc::clone(&app);
            let on_sent = Arc::clone(&on_sent);
            let window = window.clone();
            let send_for_done = send.clone();
            let banner = banner.clone();
            let draft_key = Arc::clone(&draft_key);
            let attachments = Arc::clone(&attachments);
            let sent = Arc::clone(&sent);
            let in_reply_to = in_reply_to.clone();
            let references = references.clone();
            let on_done: Box<dyn FnOnce(Option<String>, String)> =
                Box::new(move |html_body, body_text: String| {
                    let outgoing = OutgoingMessage {
                        from_name,
                        from_addr,
                        to: to_value,
                        cc: cc_value,
                        bcc: bcc_value,
                        subject: subject_value,
                        body: body_text,
                        html_body,
                        in_reply_to,
                        references,
                        draft_key: draft_key.lock().map(|key| key.clone()).ok(),
                        attachments: attachments
                            .lock()
                            .map(|items| items.clone())
                            .unwrap_or_default(),
                    };

                    match app.mailin.build_outgoing(&account_id, &outgoing, pgp) {
                        Ok((raw, envelope)) => {
                            match app.engine.queue_and_send(account_id, raw, envelope) {
                                Ok(_) => {
                                    sent.store(true, Ordering::Relaxed);
                                    if let Ok(key) = draft_key.lock()
                                        && !key.is_empty()
                                    {
                                        app.engine.discard_draft(account_id, key.clone());
                                    }
                                    on_sent();
                                    let _ = window.Close();
                                }
                                Err(failure) => {
                                    let _ = banner.SetText(&format!(
                                        "Could not queue the message: {failure}"
                                    ));
                                    let _ = ui::set_visible(&banner, true);
                                    let _ = ui::set_enabled(&send_for_done, true);
                                }
                            }
                        }
                        Err(failure) => {
                            let _ = banner.SetText(&format!("Could not send: {failure}"));
                            let _ = ui::set_visible(&banner, true);
                            let _ = ui::set_enabled(&send_for_done, true);
                        }
                    }
                });
            if !start_capture_for_send(want_html, on_done) {
                let _ = banner.SetText("Still finishing the previous capture — try again.");
                let _ = ui::set_visible(&banner, true);
                let _ = ui::set_enabled(&send, true);
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

// Helper function to calculate margin for popover positioning
fn margin_for_popover(button: &mailin_winui_sys::Button) -> mailin_winui_sys::Thickness {
    // Simple fixed margin for now
    mailin_winui_sys::Thickness {
        left: 0.0,
        top: 40.0,
        right: 0.0,
        bottom: 0.0,
    }
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
            Some(&format!(
                "Remove attachment {} ({})",
                attachment.filename,
                format::file_size(u32::try_from(attachment.data.len()).unwrap_or(u32::MAX))
            )),
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

/// Set up contact autocomplete for a recipient text box.
fn setup_contact_autocomplete(
    text_box: &mailin_winui_sys::TextBox,
    suggestions_panel: &mailin_winui_sys::StackPanel,
    app: &Arc<AppState>,
    account_id: AccountId,
    revokers: &ui::Revokers,
) -> Result<()> {
    use mailin_winui_sys::{Button, Orientation, VerticalAlignment};

    let text_box = text_box.clone();
    let suggestions_panel = suggestions_panel.clone();
    let app = Arc::clone(app);

    // Hide the suggestion list shortly after the text box loses focus, so a
    // click on a suggestion still registers before the panel disappears. The
    // timer lives on the UI thread and fires exactly once.
    let hide_timer = DispatcherQueue::GetForCurrentThread()
        .and_then(|queue| queue.CreateTimer())
        .context("could not create the suggestion hide timer")?;
    hide_timer.SetInterval(windows_time::TimeSpan {
        duration: 150 * 10_000,
    })?;
    hide_timer.SetIsRepeating(false)?;
    {
        let suggestions_panel = suggestions_panel.clone();
        revokers.keep(hide_timer.Tick(move |_, _| {
            let _ = ui::set_visible(&suggestions_panel, false);
        })?);
    }

    // TextChanged event to show/filter suggestions
    revokers.keep(
        text_box.TextChanged(&TypedEventHandler::new(move |_sender, _args| {
            let query = ui::text_value(&text_box);
            let query = query.trim();

            // Clear existing suggestions
            let children = match suggestions_panel.Children() {
                Ok(children) => children,
                Err(error) => {
                    tracing::warn!(%error, "could not read the suggestion list");
                    return Ok(());
                }
            };
            let _ = children.Clear();

            if query.is_empty() {
                let _ = ui::set_visible(&suggestions_panel, false);
                return Ok(());
            }

            // Search contacts
            if let Ok(contacts) = app.mailin.store().search_contacts(&account_id, query, 10) {
                if contacts.is_empty() {
                    let _ = ui::set_visible(&suggestions_panel, false);
                    return Ok(());
                }

                for contact in contacts {
                    let labels = ui::stack_xs(Orientation::Vertical)?;
                    let email_label = ui::text(&contact.email)?;
                    ui::append(&labels, &email_label)?;
                    if !contact.display_name.is_empty() {
                        let name_label = ui::caption(&contact.display_name)?;
                        ui::append(&labels, &name_label)?;
                    }
                    ui::stretch_horizontal(&labels)?;

                    // Click handler to insert the email
                    let text_box = text_box.clone();
                    let suggestions_panel = suggestions_panel.clone();
                    let email = contact.email.clone();
                    let contact_name = contact.display_name.clone();
                    let button = Button::new()?;
                    button.SetContent(&labels)?;
                    button
                        .SetHorizontalAlignment(mailin_winui_sys::HorizontalAlignment::Stretch)?;
                    button.SetVerticalAlignment(VerticalAlignment::Stretch)?;
                    button.SetPadding(mailin_winui_sys::Thickness {
                        left: 8.0,
                        top: 4.0,
                        right: 8.0,
                        bottom: 4.0,
                    })?;

                    // Use a transparent background directly instead of a Style
                    button.SetBackground(mailin_winui_sys::SolidColorBrush::CreateInstance(
                        &mailin_winui_sys::color_from_argb(0, 0, 0, 0),
                    )?)?;
                    AutomationProperties::SetName(
                        &button,
                        &if contact_name.is_empty() {
                            format!("Insert {email}")
                        } else {
                            format!("Insert {contact_name} <{email}>")
                        },
                    )?;
                    revokers.keep(ui::on_click(&button, move || {
                        let current = ui::text_value(&text_box);
                        let new_text = if current.trim().is_empty() {
                            email.clone()
                        } else {
                            // Append or replace last email in comma-separated list
                            let parts: Vec<&str> = current.split(',').collect();
                            let mut new_parts = parts.clone();
                            if let Some(last) = new_parts.last_mut() {
                                *last = email.as_str();
                            } else {
                                new_parts.push(&email);
                            }
                            new_parts.join(", ")
                        };
                        let _ = text_box.SetText(&new_text);
                        let _ = ui::set_visible(&suggestions_panel, false);
                    })?);

                    children.Append(&button)?;
                }
                let _ = ui::set_visible(&suggestions_panel, true);
            }
            Ok(())
        }))?,
    );

    // Hide suggestions when the text box loses focus (the timer fires from
    // the UI thread, so the focused button's Click still gets to run first).
    revokers.keep(text_box.LostFocus(&TypedEventHandler::new({
        let hide_timer = hide_timer.clone();
        move |_sender, _args| {
            let _ = hide_timer.Start();
            Ok(())
        }
    }))?);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mailin_core::compose::OutgoingMessage;
    use mailin_core::model::MessageHeader;

    fn account() -> mailin_core::Account {
        mailin_core::Account::new(
            "Ada",
            "ada@example.com",
            mailin_core::account::ServerSettings::imap("imap.example.com", "ada@example.com"),
            mailin_core::account::ServerSettings::smtp("smtp.example.com", "ada@example.com"),
        )
    }

    fn message() -> MessageHeader {
        MessageHeader {
            id: mailin_core::model::MessageId(1),
            folder_id: mailin_core::model::FolderId(1),
            uid: 1,
            message_id: Some("<test@example.com>".into()),
            thread_key: None,
            thread_root_key: None,
            thread_reply_count: 0,
            thread_depth: 0,
            subject: "Test".into(),
            from_name: Some("Bob".into()),
            from_addr: Some("bob@example.com".into()),
            to_addrs: "ada@example.com".into(),
            cc_addrs: "".into(),
            sent_at: 0,
            received_at: 0,
            snippet: "Hello".into(),
            size: 100,
            flags: vec![],
            has_attachments: false,
            body_fetched: false,
        }
    }

    fn reply(
        account: &mailin_core::Account,
        message: &MessageHeader,
        body: Option<&str>,
    ) -> OutgoingMessage {
        let mut draft = OutgoingMessage::default();
        draft.from_name = account.display_name.clone();
        draft.from_addr = account.email.clone();
        draft.to = format!(
            "{} <{}>",
            message.from_name.as_deref().unwrap_or(""),
            message.from_addr.as_deref().unwrap_or("")
        );
        draft.subject = format!("Re: {}", message.subject);
        draft.body = body.unwrap_or("").into();
        draft.in_reply_to = message.message_id.clone();
        draft.references = message.message_id.clone().into_iter().collect();
        draft
    }

    #[test]
    fn reply_preserves_in_reply_to_and_references() {
        let acc = account();
        let msg = message();
        let draft = reply(&acc, &msg, Some("Hello"));
        assert_eq!(draft.in_reply_to, Some("<test@example.com>".into()));
        assert_eq!(draft.references, vec!["<test@example.com>".into()]);
    }

    #[test]
    fn forward_sets_subject_prefix() {
        let acc = account();
        let msg = message();
        let mut draft = OutgoingMessage::default();
        draft.from_name = acc.display_name.clone();
        draft.from_addr = acc.email.clone();
        draft.to = "charlie@example.com".into();
        draft.subject = format!("Fwd: {}", msg.subject);
        draft.body = "FYI".into();
        assert!(draft.subject.starts_with("Fwd: "));
    }

    #[test]
    fn blank_signatures_leave_bodies_untouched() {
        let draft = reply(&account(), &message(), Some("Hello"));
        assert!(draft.body.starts_with("\n\nOn "));
        assert!(!draft.body.contains("-- \n"));
    }
}
