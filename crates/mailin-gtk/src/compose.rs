//! Compose window: build a message, attach files and queue it in the outbox.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use libadwaita as adw;

use mailin_core::compose::{
    PgpOptions, new_draft_key, recipient_addresses, template_draft, with_html_signature_prefix,
    with_signature_prefix,
};
use mailin_core::html::sanitize_html;
use mailin_core::{Account, AccountId, ComposeAttachment, MessageHeader, OutgoingMessage};

fn prune_unresolved_cid_images(html: &str, attachments: &[ComposeAttachment]) -> String {
    let valid_cids: std::collections::HashSet<&str> = attachments
        .iter()
        .filter_map(|a| a.cid.as_deref())
        .collect();
    if valid_cids.is_empty() && !html.contains("cid:") {
        return html.to_owned();
    }
    let mut out = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while let Some(img_pos) = lower[i..].find("<img") {
        let img_start = i + img_pos;
        out.push_str(&html[i..img_start]);
        // Find end of tag
        let rest = &html[img_start..];
        let tag_end = rest.find('>').map(|p| p + 1).unwrap_or(rest.len());
        let tag = &rest[..tag_end];
        let tag_lower = tag.to_ascii_lowercase();
        let unresolved = if let Some(cid_pos) = tag_lower.find("cid:") {
            let cid_start = cid_pos + 4;
            let cid_end = tag[cid_start..]
                .find(|c: char| c == '"' || c == '\'' || c.is_whitespace() || c == '>')
                .map(|p| cid_start + p)
                .unwrap_or(tag.len());
            let cid = tag[cid_start..cid_end].trim_end_matches('>');
            !valid_cids.contains(cid)
        } else {
            false
        };
        if !unresolved {
            out.push_str(tag);
        }
        i = img_start + tag_end;
        if tag_end == 0 {
            break;
        }
    }
    out.push_str(&html[i..]);
    out
}

fn strip_template_placeholders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '{' {
            if let Some(&(_, '{')) = chars.peek() {
                // consume second '{'
                chars.next();
                // skip until '}}'
                let mut closed = false;
                while let Some((_, ch)) = chars.next() {
                    if ch == '}' {
                        if let Some(&(_, '}')) = chars.peek() {
                            chars.next();
                            closed = true;
                            break;
                        }
                    }
                }
                if closed {
                    continue;
                }
                // Unclosed placeholder - keep as-is from original
                out.push_str(&text[i..]);
                return out;
            }
        }
        out.push(c);
    }
    out
}

use crate::app::AppState;
use crate::format;
use webkit6::javascriptcore::Value as JSValue;
use webkit6::prelude::*;
use webkit6::{UserContentManager, WebView};

/// Maximum size of a single attached file. Larger files would make the SQLite
/// outbox and the SMTP submission needlessly fragile.
const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;
/// Soft warning threshold (raw bytes). Base64 encoding adds ~4/3 on the wire (M3a-6).
const LARGE_ATTACHMENT_BYTES: usize = 40 * 1024 * 1024;

/// Collects the composer state and hands a finished message to a one-shot callback.
type SnapshotFn = Rc<dyn Fn(Box<dyn FnOnce(OutgoingMessage)>)>;

/// A one-shot message callback shared behind a take-once slot.
type SnapshotCallback = Rc<RefCell<Option<Box<dyn FnOnce(OutgoingMessage)>>>>;

/// Extract the Rust string from a JavaScript value returned by
/// `WebView::evaluate_javascript`. `Value::to_string` may render a JS string as
/// JSON (surrounded by quotes), so unwrap that form when present.
fn jsc_string(value: &JSValue) -> String {
    let raw = value.to_string();
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        serde_json::from_str::<String>(&raw).unwrap_or(raw)
    } else {
        raw
    }
}

pub fn present(
    parent: &gtk::Window,
    app: Rc<AppState>,
    account_id: AccountId,
    draft: OutgoingMessage,
    on_sent: impl Fn() + 'static,
    on_closed: impl Fn(bool /* draft save attempted */) + 'static,
) -> Option<adw::Window> {
    let on_sent: Rc<dyn Fn()> = Rc::new(on_sent);
    let on_closed: Rc<dyn Fn(bool)> = Rc::new(on_closed);
    let account = app.mailin.account(&account_id).ok().flatten()?;
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

    let base_title = if draft.in_reply_to.is_some() || !draft.references.is_empty() {
        if draft.subject.trim().is_empty() {
            "New Message".to_owned()
        } else {
            draft.subject.trim().to_owned()
        }
    } else if draft.subject.trim().is_empty() {
        "New Message".to_owned()
    } else {
        // Forward or edited draft with subject
        draft.subject.trim().to_owned()
    };
    let base_title = Rc::new(base_title);

    let window = adw::Window::builder()
        .title(base_title.as_str())
        .transient_for(parent)
        .modal(false)
        .default_width(760)
        .default_height(680)
        .build();

    let title = adw::WindowTitle::new(base_title.as_str(), &account.email);
    let title = Rc::new(title);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(title.as_ref()));

    let send = gtk::Button::with_label("Send");
    send.add_css_class("suggested-action");
    send.set_tooltip_text(Some("Send (Ctrl+Enter)"));
    header.pack_end(&send);

    let save = gtk::Button::with_label("Save Draft");
    save.set_tooltip_text(Some("Save draft (Ctrl+S, auto-saves every 30 seconds)"));
    save.add_css_class("flat");
    header.pack_end(&save);

    let attach = gtk::Button::builder()
        .icon_name("mail-attachment-symbolic")
        .tooltip_text("Attach files")
        .build();
    attach.add_css_class("flat");
    attach.set_size_request(32, 32);
    {
        use gtk::accessible::Property;
        attach.update_property(&[Property::Label("Attach files")]);
    }
    header.pack_start(&attach);

    // Templates menu button
    let templates_btn = gtk::MenuButton::builder()
        .icon_name("document-template-symbolic")
        .tooltip_text("Message templates")
        .build();
    templates_btn.add_css_class("flat");
    templates_btn.set_size_request(32, 32);
    {
        use gtk::accessible::Property;
        templates_btn.update_property(&[Property::Label("Message templates")]);
    }
    header.pack_start(&templates_btn);

    // Rich text editor with WebKit WebView (create early so formatting buttons can use it)
    let user_content = UserContentManager::new();
    let webview = WebView::builder()
        .user_content_manager(&user_content)
        .build();
    webview.set_vexpand(true);
    webview.set_hexpand(true);

    // Initialize the contenteditable editor
    let initial_html = if draft.html_body.is_some() {
        draft.html_body.clone().unwrap_or_default()
    } else {
        // Convert plain text to simple HTML, escaping any markup in the
        // original message (the WebView runs JavaScript, so unescaped
        // markup from the quoted body would execute).
        mailin_core::html::escape(&draft.body).replace('\n', "<br>")
    };

    let editor_html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
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
                            window.webkit.messageHandlers.imagePasteHandler.postMessage(JSON.stringify({{
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
                window.webkit.messageHandlers.pasteHandler.postMessage(html);
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
    webview.load_html(&editor_html, None::<&str>);

    // Format menu button with formatting options
    let format_btn = gtk::MenuButton::builder()
        .icon_name("format-text-symbolic")
        .tooltip_text("Formatting options")
        .build();
    format_btn.add_css_class("flat");
    format_btn.set_size_request(32, 32);
    {
        use gtk::accessible::Property;
        format_btn.update_property(&[Property::Label("Formatting options")]);
    }

    // Format popover
    let format_popover = gtk::Popover::builder()
        .has_arrow(true)
        .position(gtk::PositionType::Bottom)
        .build();
    let format_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    format_box.set_margin_top(6);
    format_box.set_margin_bottom(6);
    format_box.set_margin_start(12);
    format_box.set_margin_end(12);
    format_popover.set_child(Some(&format_box));
    format_btn.set_popover(Some(&format_popover));
    header.pack_start(&format_btn);

    // Add formatting buttons to format popover
    let formatting_actions = [
        ("Bold", "format-text-bold-symbolic", "bold"),
        ("Italic", "format-text-italic-symbolic", "italic"),
        ("Underline", "format-text-underline-symbolic", "underline"),
        ("Link", "insert-link-symbolic", "link"),
        (
            "Bulleted List",
            "format-list-bulleted-symbolic",
            "insertUnorderedList",
        ),
        ("Quote", "format-quote-symbolic", "formatBlock"),
        ("Heading", "format-heading-symbolic", "formatBlock"),
    ];
    for (label, icon, cmd) in formatting_actions {
        let btn = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(label)
            .build();
        btn.add_css_class("flat");
        btn.set_size_request(32, 32);
        {
            use gtk::accessible::Property;
            btn.update_property(&[Property::Label(label)]);
        }
        let cmd_str = cmd.to_string();
        let webview_clone = webview.clone();
        btn.connect_clicked(move |_| {
            if cmd_str == "link" {
                webview_clone.evaluate_javascript(
                    "const url = prompt('Enter URL:'); if (url) document.execCommand('createLink', false, url);",
                    None,
                    None,
                    None::<&gio::Cancellable>,
                    |_| {},
                );
            } else if cmd_str == "formatBlock" && label == "Quote" {
                webview_clone.evaluate_javascript(
                    "document.execCommand('formatBlock', false, 'blockquote');",
                    None,
                    None,
                    None::<&gio::Cancellable>,
                    |_| {},
                );
            } else if cmd_str == "formatBlock" && label == "Heading" {
                webview_clone.evaluate_javascript(
                    "document.execCommand('formatBlock', false, 'h2');",
                    None,
                    None,
                    None::<&gio::Cancellable>,
                    |_| {},
                );
            } else {
                webview_clone.evaluate_javascript(
                    &format!("document.execCommand('{}', false);", cmd_str),
                    None,
                    None,
                    None::<&gio::Cancellable>,
                    |_| {},
                );
            }
        });
        format_box.append(&btn);
    }

    // HTML/Plain text mode state
    let is_html_mode = Rc::new(Cell::new(draft.html_body.is_some()));

    // HTML/Plain text toggle
    let html_toggle = gtk::ToggleButton::builder()
        .label("HTML")
        .tooltip_text("Toggle HTML/plain text mode")
        .active(draft.html_body.is_some())
        .build();
    html_toggle.add_css_class("flat");
    header.pack_start(&html_toggle);

    // PGP toggles
    let pgp_encrypt = Rc::new(Cell::new(false));
    let pgp_sign = Rc::new(Cell::new(false));
    let encrypt_toggle = gtk::ToggleButton::builder()
        .icon_name("channel-secure-symbolic")
        .tooltip_text("Encrypt this message (PGP)")
        .build();
    encrypt_toggle.add_css_class("flat");
    encrypt_toggle.set_size_request(32, 32);
    {
        use gtk::accessible::Property;
        encrypt_toggle.update_property(&[Property::Label("Encrypt message")]);
    }
    header.pack_start(&encrypt_toggle);
    let sign_toggle = gtk::ToggleButton::builder()
        .icon_name("application-certificate-symbolic")
        .tooltip_text("Sign this message (PGP)")
        .build();
    sign_toggle.add_css_class("flat");
    sign_toggle.set_size_request(32, 32);
    {
        use gtk::accessible::Property;
        sign_toggle.update_property(&[Property::Label("Sign message")]);
    }
    header.pack_start(&sign_toggle);

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

    // Connect HTML toggle. A full editor rebuild on mode switch would need a
    // reload of the WebView; the plain-text mirror is always captured on save.
    let is_html_mode_clone = is_html_mode.clone();
    html_toggle.connect_toggled(move |btn| {
        is_html_mode_clone.set(btn.is_active());
    });

    // Set up the paste bridge. The selection range was captured by the JS
    // listener before the asynchronous round-trip, so the sanitized fragment is
    // inserted at the caret instead of replacing the whole body.
    let user_content_for_paste = user_content.clone();
    user_content_for_paste.register_script_message_handler("pasteHandler", None);
    let webview_for_paste = webview.clone();
    user_content_for_paste.connect_script_message_received(
        Some("pasteHandler"),
        move |_, value: &JSValue| {
            let sanitized = sanitize_html(&value.to_string());
            let script = format!(
                "__mailinInsert({});",
                serde_json::to_string(&sanitized).unwrap_or_else(|_| "\"\"".to_owned())
            );
            webview_for_paste.evaluate_javascript(
                &script,
                None,
                None,
                None::<&gio::Cancellable>,
                |_| {},
            );
        },
    );

    let body_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .min_content_height(220)
        .child(&webview)
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
    // Banner button action: 0=none, 1=open key manager, 2=send without encryption
    let banner_action: Rc<Cell<u8>> = Rc::new(Cell::new(0));
    {
        let app = Rc::clone(&app);
        let window = window.clone();
        let account_id = account.id;
        let banner_action = Rc::clone(&banner_action);
        let encrypt_toggle = encrypt_toggle.clone();
        let pgp_encrypt = Rc::clone(&pgp_encrypt);
        let banner_for_btn = banner.clone();
        banner.connect_button_clicked(move |_| match banner_action.get() {
            1 => crate::pgp_keys::present(
                window.upcast_ref::<gtk::Window>(),
                Rc::clone(&app),
                account_id,
            ),
            2 => {
                encrypt_toggle.set_active(false);
                pgp_encrypt.set(false);
                encrypt_toggle.remove_css_class("suggested-action");
                banner_for_btn.set_revealed(false);
                banner_for_btn.set_button_label(None);
                banner_action.set(0);
            }
            _ => {}
        });
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&page_scroller);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    // Validate key availability as soon as the user asks for PGP.
    let update_encrypt_state: Rc<dyn Fn()> = {
        let app = Rc::clone(&app);
        let state = Rc::clone(&pgp_encrypt);
        let banner = banner.clone();
        let banner_action = Rc::clone(&banner_action);
        let encrypt_toggle = encrypt_toggle.clone();
        let to_row = to_row.clone();
        let cc_row = cc_row.clone();
        let bcc_row = bcc_row.clone();
        let account_id = account.id;
        Rc::new(move || {
            if !encrypt_toggle.is_active() {
                state.set(false);
                encrypt_toggle.remove_css_class("suggested-action");
                banner.set_button_label(None::<&str>);
                banner_action.set(0);
                banner.set_revealed(false);
                return;
            }
            let mut recipients = Vec::new();
            for row in [&to_row, &cc_row, &bcc_row] {
                if let Ok(addresses) = recipient_addresses(&row.text()) {
                    recipients.extend(addresses);
                }
            }
            if recipients.is_empty() {
                encrypt_toggle.set_active(false);
                state.set(false);
                encrypt_toggle.remove_css_class("suggested-action");
                banner.set_button_label(None::<&str>);
                banner_action.set(0);
                banner.set_title("Add recipients before enabling encryption.");
                banner.set_revealed(true);
                return;
            }
            match app.mailin.missing_pgp_keys(&account_id, &recipients) {
                Ok(missing) if !missing.is_empty() => {
                    encrypt_toggle.set_active(false);
                    state.set(false);
                    encrypt_toggle.remove_css_class("suggested-action");
                    banner.set_title(&format!(
                        "Missing public keys for: {}. Encryption turned off — add keys in Key Manager or send without encryption.",
                        missing.join(", ")
                    ));
                    banner.set_button_label(Some("Open Key Manager"));
                    banner_action.set(1);
                    banner.set_revealed(true);
                }
                Err(error) => {
                    encrypt_toggle.set_active(false);
                    state.set(false);
                    encrypt_toggle.remove_css_class("suggested-action");
                    banner.set_button_label(None::<&str>);
                    banner_action.set(0);
                    banner.set_title(&format!("Could not check PGP keys: {error}"));
                    banner.set_revealed(true);
                }
                Ok(_) => {
                    state.set(true);
                    encrypt_toggle.add_css_class("suggested-action");
                    banner.set_button_label(None::<&str>);
                    banner_action.set(0);
                    banner.set_title(&format!(
                        "This message will be encrypted for {} recipient{}.",
                        recipients.len(),
                        if recipients.len() == 1 { "" } else { "s" }
                    ));
                    banner.set_revealed(true);
                }
            }
        })
    };
    {
        let update = Rc::clone(&update_encrypt_state);
        encrypt_toggle.connect_toggled(move |_| update());
    }
    // Re-check when recipients change (M4-E).
    for row in [&to_row, &cc_row, &bcc_row] {
        let update = Rc::clone(&update_encrypt_state);
        row.connect_changed(move |_| update());
    }
    {
        let app = app.clone();
        let state = Rc::clone(&pgp_sign);
        let banner = banner.clone();
        sign_toggle.connect_toggled(move |button| {
            state.set(button.is_active());
            if !button.is_active() {
                return;
            }
            if !app.mailin.has_signing_key(&account_id) {
                button.set_active(false);
                state.set(false);
                banner.set_title(
                    "No secret key available. Add one in Account Settings → Encryption.",
                );
                banner.set_revealed(true);
            }
        });
    }

    let attachments: Rc<RefCell<Vec<ComposeAttachment>>> =
        Rc::new(RefCell::new(draft.attachments.clone()));
    let dirty = Rc::new(Cell::new(false));
    // M4-E: mark dirty on header edits.
    for row in [&to_row, &cc_row, &bcc_row, &subject_row] {
        let dirty = Rc::clone(&dirty);
        let title = Rc::clone(&title);
        let base_title = Rc::clone(&base_title);
        row.connect_changed(move |_| {
            if !dirty.get() {
                dirty.set(true);
                title.set_title(&format!("● {}", base_title.as_str()));
            }
        });
    }
    rebuild_attachments(
        &attachment_list,
        &attachments_group,
        &attachments,
        Some(Rc::clone(&dirty)),
        banner.clone(),
        Rc::clone(&banner_action),
    );

    // Clipboard images become inline attachments referenced from the HTML body
    // with `cid:`. The JS listener sends a data URL; we store the bytes and
    // insert an <img> element at the saved caret position.
    {
        let user_content = user_content.clone();
        user_content.register_script_message_handler("imagePasteHandler", None);
        let webview = webview.clone();
        let list = attachment_list.clone();
        let group = attachments_group.clone();
        let attachments = Rc::clone(&attachments);
        let dirty = Rc::clone(&dirty);
        let title = Rc::clone(&title);
        let base_title = Rc::clone(&base_title);
        let banner = banner.clone();
        let banner_action = Rc::clone(&banner_action);
        user_content.connect_script_message_received(
            Some("imagePasteHandler"),
            move |_, value: &JSValue| {
                let raw = value.to_string();
                let Ok(payload) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    return;
                };
                let data_url = payload
                    .get("data")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let Some((_, encoded)) = data_url.split_once(',') else {
                    return;
                };
                let Ok(bytes) = STANDARD.decode(encoded) else {
                    return;
                };
                if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
                    return;
                }
                let mime = payload
                    .get("mime")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("image/png");
                let name = payload
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("pasted-image.png");
                let attachment = ComposeAttachment::inline_image(name, mime, bytes);
                let Some(cid) = attachment.cid.clone() else {
                    return;
                };
                attachments.borrow_mut().push(attachment);
                dirty.set(true);
                title.set_title(&format!("● {}", base_title.as_str()));
                rebuild_attachments(
                    &list,
                    &group,
                    &attachments,
                    Some(Rc::clone(&dirty)),
                    banner.clone(),
                    Rc::clone(&banner_action),
                );
                let html = format!("<img src=\"cid:{cid}\" alt=\"pasted image\">");
                let script = format!(
                    "__mailinInsert({});",
                    serde_json::to_string(&html).unwrap_or_else(|_| "\"\"".to_owned())
                );
                webview.evaluate_javascript(&script, None, None, None::<&gio::Cancellable>, |_| {});
            },
        );
    }

    // Files dropped onto the editor are attached just like files chosen with
    // the attach button.
    {
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let list = attachment_list.clone();
        let group = attachments_group.clone();
        let attachments = Rc::clone(&attachments);
        let dirty = Rc::clone(&dirty);
        let banner = banner.clone();
        let banner_action = Rc::clone(&banner_action);
        drop.connect_drop(move |_, value, _, _| {
            let Some(files) = value.get::<gdk::FileList>().ok() else {
                return false;
            };
            let mut failed = None;
            for file in files.files() {
                match load_attachment(&file) {
                    Ok(attachment) => {
                        attachments.borrow_mut().push(attachment);
                        dirty.set(true);
                    }
                    Err(message) => failed = Some(message),
                }
            }
            if let Some(message) = failed {
                banner.set_title(&message);
                banner.set_revealed(true);
            }
            rebuild_attachments(
                &list,
                &group,
                &attachments,
                Some(Rc::clone(&dirty)),
                banner.clone(),
                Rc::clone(&banner_action),
            );
            true
        });
        webview.add_controller(drop);
    }

    // Templates popover
    let templates_popover = gtk::Popover::builder()
        .has_arrow(true)
        .position(gtk::PositionType::Bottom)
        .build();
    let templates_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .build();
    templates_list.add_css_class("boxed-list");
    templates_popover.set_child(Some(&templates_list));
    templates_btn.set_popover(Some(&templates_popover));

    // Clone widgets for use in populate_templates closure
    let to_row_for_populate = to_row.clone();
    let cc_row_for_populate = cc_row.clone();
    let bcc_row_for_populate = bcc_row.clone();
    let subject_row_for_populate = subject_row.clone();
    let webview_for_populate = webview.clone();
    let window_for_populate = window.clone();
    let banner_for_populate = banner.clone();
    let banner_action_for_populate = Rc::clone(&banner_action);
    let dirty_for_populate = Rc::clone(&dirty);

    // Populate templates list
    let populate_templates: Rc<dyn Fn()> = {
        let app = Rc::clone(&app);
        let account_id = account.id;
        let templates_list = templates_list.clone();
        let templates_popover = templates_popover.clone();
        let title = Rc::clone(&title);
        let base_title = Rc::clone(&base_title);
        let banner_action = Rc::clone(&banner_action_for_populate);
        Rc::new(move || {
            while let Some(child) = templates_list.first_child() {
                templates_list.remove(&child);
            }
            if let Ok(templates) = app.mailin.store().list_templates(&account_id) {
                if templates.is_empty() {
                    let empty_row = adw::ActionRow::builder()
                        .title("No templates yet")
                        .subtitle("Create templates from the account settings")
                        .activatable(false)
                        .build();
                    empty_row.add_css_class("dim-label");
                    templates_list.append(&empty_row);
                } else {
                    for template in templates {
                        let row = adw::ActionRow::builder()
                            .title(&template.name)
                            .subtitle(&template.subject)
                            .activatable(true)
                            .build();
                        let template_clone = template.clone();
                        let to_row = to_row_for_populate.clone();
                        let cc_row = cc_row_for_populate.clone();
                        let bcc_row = bcc_row_for_populate.clone();
                        let subject_row = subject_row_for_populate.clone();
                        let webview = webview_for_populate.clone();
                        let window = window_for_populate.clone();
                        let dirty = Rc::clone(&dirty_for_populate);
                        let templates_popover = templates_popover.clone();
                        let banner = banner_for_populate.clone();
                        let banner_action = Rc::clone(&banner_action);
                        let title = Rc::clone(&title);
                        let base_title = Rc::clone(&base_title);
                        row.connect_activated(move |_| {
                            let vars = std::collections::HashMap::new();
                            let base_draft = template_draft(&template_clone, &vars);
                            let has_placeholders =
                                base_draft.subject.contains("{{") || base_draft.body.contains("{{");

                            let to_row = to_row.clone();
                            let cc_row = cc_row.clone();
                            let bcc_row = bcc_row.clone();
                            let subject_row = subject_row.clone();
                            let webview = webview.clone();
                            let dirty = Rc::clone(&dirty);
                            let templates_popover = templates_popover.clone();
                            let banner = banner.clone();
                            let banner_action = Rc::clone(&banner_action);
                            let title = Rc::clone(&title);
                            let base_title = Rc::clone(&base_title);

                            let apply_draft = Rc::new(move |draft: OutgoingMessage, removed: bool| {
                                // Only prefill empty fields
                                if to_row.text().is_empty() {
                                    to_row.set_text(&draft.to);
                                }
                                if cc_row.text().is_empty() {
                                    cc_row.set_text(&draft.cc);
                                }
                                if bcc_row.text().is_empty() {
                                    bcc_row.set_text(&draft.bcc);
                                }
                                if subject_row.text().is_empty() {
                                    subject_row.set_text(&draft.subject);
                                }
                                let draft_body = draft.body.clone();
                                let webview_inner = webview.clone();
                                webview.evaluate_javascript(
                                    "document.body.innerText",
                                    None,
                                    None,
                                    None::<&gio::Cancellable>,
                                    move |result| {
                                        let current_body = result
                                            .map(|value| jsc_string(&value))
                                            .unwrap_or_default();
                                        if !current_body.trim().is_empty() {
                                            return;
                                        }
                                        let draft_body = draft_body.clone();
                                        let webview_outer = webview_inner.clone();
                                        webview_inner.evaluate_javascript(
                                            "document.body.innerHTML",
                                            None,
                                            None,
                                            None::<&gio::Cancellable>,
                                            move |result| {
                                                let current_html = result
                                                    .map(|value| jsc_string(&value))
                                                    .unwrap_or_default();
                                                if current_html.trim().is_empty() {
                                                    let draft_body = sanitize_html(&draft_body);
                                                    webview_outer.evaluate_javascript(
                                                        &format!(
                                                            "document.body.innerHTML = {}",
                                                            serde_json::to_string(&draft_body)
                                                                .unwrap_or_else(|_| "\"\"".to_owned())
                                                        ),
                                                        None,
                                                        None,
                                                        None::<&gio::Cancellable>,
                                                        |_| {},
                                                    );
                                                }
                                            },
                                        );
                                    },
                                );
                                dirty.set(true);
                                title.set_title(&format!("● {}", base_title.as_str()));
                                templates_popover.popdown();
                                banner_action.set(0);
                                banner.set_button_label(None::<&str>);
                                banner.set_title(if removed {
                                    "Template applied (placeholders removed)"
                                } else if has_placeholders {
                                    "Template applied — replace {{…}} placeholders before sending"
                                } else {
                                    "Template applied"
                                });
                                banner.set_revealed(true);
                            });

                            if !has_placeholders {
                                apply_draft(base_draft, false);
                                return;
                            }

                            let dialog = adw::AlertDialog::builder()
                                .heading("Template placeholders")
                                .body("This template contains {{…}} placeholders. Insert them as-is and replace them before sending, or remove them now?")
                                .build();
                            dialog.add_response("cancel", "Cancel");
                            dialog.add_response("keep", "Insert as-is");
                            dialog.add_response("strip", "Remove placeholders");
                            dialog.set_response_appearance("strip", adw::ResponseAppearance::Suggested);
                            dialog.set_default_response(Some("strip"));
                            dialog.set_close_response("cancel");
                            let apply_keep = Rc::clone(&apply_draft);
                            let apply_strip = Rc::clone(&apply_draft);
                            dialog.choose(Some(&window), None::<&gio::Cancellable>, move |response| {
                                match response.as_str() {
                                    "keep" => apply_keep(base_draft, false),
                                    "strip" => {
                                        let mut draft = base_draft;
                                        draft.subject = strip_template_placeholders(&draft.subject);
                                        draft.body = strip_template_placeholders(&draft.body);
                                        apply_strip(draft, true);
                                    }
                                    _ => {}
                                }
                            });
                        });
                        templates_list.append(&row);
                    }
                }
            }
            templates_list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            let save_row = adw::ActionRow::builder().title("Save as template…").build();
            let _to_row = to_row_for_populate.clone();
            let _cc_row = cc_row_for_populate.clone();
            let _bcc_row = bcc_row_for_populate.clone();
            let subject_row = subject_row_for_populate.clone();
            let webview = webview_for_populate.clone();
            let app = Rc::clone(&app);
            let account_id = account.id;
            let templates_popover = templates_popover.clone();
            let banner = banner_for_populate.clone();
            let window = window_for_populate.clone();
            save_row.connect_activated(move |_| {
                templates_popover.popdown();
                save_as_template_dialog(&window, &app, account_id, &subject_row, &webview, &banner);
            });
            templates_list.append(&save_row);
        })
    };
    // Re-populate every time the popover opens so a template saved during this
    // session (or from elsewhere) appears without reopening the composer.
    templates_popover.connect_show({
        let populate = Rc::clone(&populate_templates);
        move |_| populate()
    });
    templates_popover.set_autohide(true);
    populate_templates();

    // Contact autocomplete for recipient fields
    let setup_autocomplete = {
        let app = Rc::clone(&app);
        let account_id = account.id;
        move |entry_row: &adw::EntryRow| {
            let popover = gtk::Popover::builder()
                .has_arrow(true)
                .position(gtk::PositionType::Bottom)
                .build();
            let list_box = gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::Single)
                .build();
            list_box.add_css_class("boxed-list");
            popover.set_child(Some(&list_box));
            entry_row.add_controller(gtk::EventControllerKey::new());
            // We'll use a key controller on the entry itself
            let entry = entry_row.clone();
            let popover_clone = popover.clone();
            let list_box_clone = list_box.clone();
            let app = Rc::clone(&app);
            let account_id = account_id;

            // Store the popover as data on the entry for later access
            let popover_ref = Rc::new(RefCell::new(Some(popover_clone)));
            let list_box_ref = Rc::new(RefCell::new(Some(list_box_clone)));

            // Connect to entry changes to show/filter suggestions
            entry.connect_changed({
                let entry = entry.clone();
                let popover_ref = Rc::clone(&popover_ref);
                let list_box_ref = Rc::clone(&list_box_ref);
                let app = Rc::clone(&app);
                move |_| {
                    let query = entry.text().to_string();
                    if query.trim().is_empty() {
                        if let Some(p) = popover_ref.borrow().as_ref() {
                            p.popdown();
                        }
                        return;
                    }

                    // Search contacts
                    if let Ok(contacts) =
                        app.mailin.store().search_contacts(&account_id, &query, 10)
                    {
                        // Clear list
                        if let Some(list) = list_box_ref.borrow().as_ref() {
                            while let Some(child) = list.first_child() {
                                list.remove(&child);
                            }
                            if contacts.is_empty() {
                                if let Some(p) = popover_ref.borrow().as_ref() {
                                    p.popdown();
                                }
                            } else {
                                for contact in contacts {
                                    let row = adw::ActionRow::builder()
                                        .title(&contact.email)
                                        .subtitle(if contact.display_name.is_empty() {
                                            ""
                                        } else {
                                            &contact.display_name
                                        })
                                        .activatable(true)
                                        .build();
                                    let email = contact.email.clone();
                                    let entry = entry.clone();
                                    let popover_ref = Rc::clone(&popover_ref);
                                    row.connect_activated({
                                        let email = email.clone();
                                        let entry = entry.clone();
                                        let popover_ref = Rc::clone(&popover_ref);
                                        move |_| {
                                            // Append or replace email in the entry
                                            let current = entry.text().to_string();
                                            let new_text = if current.trim().is_empty() {
                                                email.clone()
                                            } else {
                                                // Check if we're completing a partial email
                                                let parts: Vec<&str> = current.split(',').collect();
                                                let mut new_parts = parts.clone();
                                                if let Some(last) = new_parts.last_mut() {
                                                    *last = email.as_str();
                                                } else {
                                                    new_parts.push(&email);
                                                }
                                                new_parts.join(", ")
                                            };
                                            entry.set_text(&new_text);
                                            if let Some(p) = popover_ref.borrow().as_ref() {
                                                p.popdown();
                                            }
                                        }
                                    });
                                    if let Some(list) = list_box_ref.borrow().as_ref() {
                                        list.append(&row);
                                    }
                                }
                                // Position popover under the entry
                                if let Some(p) = popover_ref.borrow().as_ref() {
                                    p.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
                                        0,
                                        entry.height(),
                                        entry.width(),
                                        1,
                                    )));
                                    p.popup();
                                }
                            }
                        }
                    }
                }
            });

            // Handle key events for navigation
            let key_controller = gtk::EventControllerKey::new();
            let _entry_for_keys = entry.clone();
            let list_box_for_keys = Rc::clone(&list_box_ref);
            let popover_for_keys = Rc::clone(&popover_ref);
            key_controller.connect_key_pressed(move |_, key, _, _| {
                if let (Some(list), Some(popover)) = (
                    list_box_for_keys.borrow().as_ref(),
                    popover_for_keys.borrow().as_ref(),
                ) {
                    match key {
                        gtk::gdk::Key::Escape => {
                            popover.popdown();
                            return glib::Propagation::Stop;
                        }
                        gtk::gdk::Key::Up | gtk::gdk::Key::Down => {
                            let selected = list.selected_row();
                            let children: Vec<_> = list
                                .observe_children()
                                .snapshot()
                                .iter()
                                .filter_map(|o| o.downcast_ref::<gtk::ListBoxRow>().cloned())
                                .collect();
                            if children.is_empty() {
                                return glib::Propagation::Proceed;
                            }
                            let current_idx = selected
                                .as_ref()
                                .and_then(|r| {
                                    children.iter().position(|c| {
                                        c.upcast_ref::<glib::Object>()
                                            == r.upcast_ref::<glib::Object>()
                                    })
                                })
                                .unwrap_or(0);
                            let new_idx = match key {
                                gtk::gdk::Key::Up => current_idx.saturating_sub(1),
                                gtk::gdk::Key::Down => (current_idx + 1).min(children.len() - 1),
                                _ => current_idx,
                            };
                            list.select_row(Some(&children[new_idx]));
                            return glib::Propagation::Stop;
                        }
                        gtk::gdk::Key::Return => {
                            if let Some(row) = list.selected_row() {
                                row.emit_by_name::<()>("activated", &[]);
                                return glib::Propagation::Stop;
                            }
                        }
                        _ => {}
                    }
                }
                glib::Propagation::Proceed
            });
            entry.add_controller(key_controller);
        }
    };

    setup_autocomplete(&to_row);
    setup_autocomplete(&cc_row);
    setup_autocomplete(&bcc_row);

    rebuild_attachments(
        &attachment_list,
        &attachments_group,
        &attachments,
        Some(Rc::clone(&dirty)),
        banner.clone(),
        Rc::clone(&banner_action),
    );

    // Drafts are server-side messages whose Message-ID acts as the identity
    // for replacing an earlier auto-save.
    let draft_key = Rc::new(RefCell::new(
        draft
            .draft_key
            .clone()
            .unwrap_or_else(|| new_draft_key(&account.email)),
    ));
    let closed = Rc::new(Cell::new(false));
    let sent = Rc::new(Cell::new(false));

    // `snapshot` gathers the composer state asynchronously. The body extraction
    // runs JavaScript in the WebView, whose result callback is delivered on the
    // GTK main loop. Blocking that thread with `recv_timeout` would deadlock the
    // result it waits for, so instead we query and invoke a callback once the
    // results are in.
    let snapshot: SnapshotFn = {
        let account = account.clone();
        let draft_key = Rc::clone(&draft_key);
        let to_row = to_row.clone();
        let cc_row = cc_row.clone();
        let bcc_row = bcc_row.clone();
        let subject_row = subject_row.clone();
        let webview = webview.clone();
        let attachments = Rc::clone(&attachments);
        let in_reply_to = draft.in_reply_to.clone();
        let references = draft.references.clone();
        let is_html = Rc::clone(&is_html_mode);
        Rc::new(move |callback: Box<dyn FnOnce(OutgoingMessage)>| {
            let html_enabled = is_html.get();
            // Fields that can be read synchronously.
            let to = to_row.text().to_string();
            let cc = cc_row.text().to_string();
            let bcc = bcc_row.text().to_string();
            let subject = subject_row.text().to_string();
            let draft = draft_key.borrow().clone();
            let attachment_list = attachments.borrow().clone();
            let from_name = account.display_name.clone();
            let from_addr = account.email.clone();
            let in_reply_to = in_reply_to.clone();
            let references = references.clone();

            // Shared results from the two JS queries. Whichever of the two
            // callbacks completes last invokes the user callback.
            let html_result = Rc::new(RefCell::new(None::<String>));
            let text_result = Rc::new(RefCell::new(None::<String>));
            let remaining = Rc::new(Cell::new(if html_enabled { 2 } else { 1 }));
            // `FnOnce` cannot be cloned, so share it behind a slot that is
            // taken exactly once when both queries have completed.
            let callback: SnapshotCallback = Rc::new(RefCell::new(Some(callback)));

            let finish: Rc<dyn Fn()> = {
                let html_result = Rc::clone(&html_result);
                let text_result = Rc::clone(&text_result);
                let remaining = Rc::clone(&remaining);
                let callback = Rc::clone(&callback);
                Rc::new(move || {
                    remaining.set(remaining.get() - 1);
                    if remaining.get() > 0 {
                        return;
                    }
                    let Some(callback) = callback.borrow_mut().take() else {
                        return;
                    };
                    let html_body = html_result.borrow().clone();
                    let text_body = text_result.borrow().clone().unwrap_or_default();
                    let html_body = if html_enabled {
                        let html = html_body.unwrap_or_default();
                        Some(prune_unresolved_cid_images(&html, &attachment_list))
                    } else {
                        None
                    };
                    callback(OutgoingMessage {
                        from_name: from_name.clone(),
                        from_addr: from_addr.clone(),
                        to: to.clone(),
                        cc: cc.clone(),
                        bcc: bcc.clone(),
                        subject: subject.clone(),
                        body: text_body,
                        html_body,
                        in_reply_to: in_reply_to.clone(),
                        references: references.clone(),
                        draft_key: Some(draft.clone()),
                        attachments: attachment_list.clone(),
                    });
                })
            };

            // Plain-text mirror is always captured (used when HTML mode is off
            // and as the text alternative when it is on).
            {
                let finish = Rc::clone(&finish);
                let text_result = Rc::clone(&text_result);
                webview.evaluate_javascript(
                    "document.body.innerText",
                    None,
                    None,
                    None::<&gio::Cancellable>,
                    move |result| {
                        if let Ok(value) = result {
                            *text_result.borrow_mut() = Some(jsc_string(&value));
                        }
                        finish();
                    },
                );
            }

            if html_enabled {
                let finish = Rc::clone(&finish);
                let html_result = Rc::clone(&html_result);
                webview.evaluate_javascript(
                    "document.body.innerHTML",
                    None,
                    None,
                    None::<&gio::Cancellable>,
                    move |result| {
                        if let Ok(value) = result {
                            *html_result.borrow_mut() = Some(jsc_string(&value));
                        }
                        finish();
                    },
                );
            }
        })
    };

    let save_draft: Rc<dyn Fn(bool)> = {
        let snapshot = Rc::clone(&snapshot);
        let dirty = Rc::clone(&dirty);
        let app = Rc::clone(&app);
        let account_id = account.id;
        let banner = banner.clone();
        let banner_action = Rc::clone(&banner_action);
        let title = Rc::clone(&title);
        let base_title = Rc::clone(&base_title);
        Rc::new(move |force: bool| {
            if !force && !dirty.replace(false) {
                return;
            }
            let app = Rc::clone(&app);
            let dirty = Rc::clone(&dirty);
            let banner = banner.clone();
            let banner_action = Rc::clone(&banner_action);
            let title = Rc::clone(&title);
            let base_title = Rc::clone(&base_title);
            snapshot(Box::new(move |outgoing| match outgoing.build_draft() {
                Ok(_) => {
                    app.engine.save_draft(account_id, outgoing);
                    banner_action.set(0);
                    banner.set_button_label(None::<&str>);
                    banner.set_title("Draft saved");
                    banner.set_revealed(true);
                    title.set_title(base_title.as_str());
                    // M4-E: auto-hide transient confirmation.
                    let banner_hide = banner.clone();
                    glib::timeout_add_seconds_local_once(3, move || {
                        if banner_hide.title().as_str() == "Draft saved" {
                            banner_hide.set_revealed(false);
                        }
                    });
                }
                Err(error) => {
                    dirty.set(true);
                    banner_action.set(0);
                    banner.set_button_label(None::<&str>);
                    banner.set_title(&format!("Could not save the draft: {error}"));
                    banner.set_revealed(true);
                    title.set_title(&format!("● {}", base_title.as_str()));
                }
            }));
        })
    };

    for row in [&to_row, &cc_row, &bcc_row, &subject_row] {
        let dirty = Rc::clone(&dirty);
        let title = Rc::clone(&title);
        let base_title = Rc::clone(&base_title);
        row.connect_changed(move |_| {
            dirty.set(true);
            title.set_title(&format!("● {}", base_title.as_str()));
        });
    }
    {
        let _dirty = Rc::clone(&dirty);
        let webview = webview.clone();
        // WebView doesn't have a simple "changed" signal; rely on auto-save timer
        // and manual save. We could add a timeout to check for changes if needed.
        let _ = webview;
    }

    save.connect_clicked({
        let save_draft = Rc::clone(&save_draft);
        move |_| save_draft(true)
    });

    // Ctrl+S saves the draft (M4-E).
    {
        let save_draft = Rc::clone(&save_draft);
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, state| {
            if state.contains(gdk::ModifierType::CONTROL_MASK) && key == gdk::Key::s {
                save_draft(true);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(keys);
    }

    // Auto-save while the composer is open. The timeout is cancelled on close.
    {
        let save_draft = Rc::clone(&save_draft);
        let closed = Rc::clone(&closed);
        glib::timeout_add_seconds_local(30, move || {
            if closed.get() {
                return glib::ControlFlow::Break;
            }
            save_draft(false);
            glib::ControlFlow::Continue
        });
    }

    window.connect_close_request({
        let window = window.clone();
        let app = Rc::clone(&app);
        let account_id = account.id;
        let save_draft = Rc::clone(&save_draft);
        let dirty = Rc::clone(&dirty);
        let closed = Rc::clone(&closed);
        let sent = Rc::clone(&sent);
        let on_closed = Rc::clone(&on_closed);
        let draft_key = Rc::clone(&draft_key);
        let confirming = Rc::new(Cell::new(false));
        move |_| {
            if closed.get() || sent.get() {
                if !sent.get() {
                    on_closed(dirty.get());
                }
                closed.set(true);
                return glib::Propagation::Proceed;
            }
            if !dirty.get() {
                on_closed(false);
                closed.set(true);
                return glib::Propagation::Proceed;
            }
            if confirming.replace(true) {
                return glib::Propagation::Stop;
            }
            // M4-E: offer discard, save, and cancel for dirty composers.
            let dialog = adw::AlertDialog::builder()
                .heading("Save draft?")
                .body("This message has unsaved changes.")
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("discard", "Discard");
            dialog.add_response("save", "Save");
            dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
            dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("save"));
            dialog.set_close_response("cancel");
            let window_c = window.clone();
            let app_c = Rc::clone(&app);
            let save_draft_c = Rc::clone(&save_draft);
            let closed_c = Rc::clone(&closed);
            let on_closed_c = Rc::clone(&on_closed);
            let draft_key_c = Rc::clone(&draft_key);
            let confirming_c = Rc::clone(&confirming);
            dialog.choose(Some(&window), None::<&gio::Cancellable>, move |response| {
                confirming_c.set(false);
                match response.as_str() {
                    "save" => {
                        save_draft_c(true);
                        on_closed_c(true);
                        closed_c.set(true);
                        window_c.close();
                    }
                    "discard" => {
                        let key = draft_key_c.borrow().clone();
                        app_c.engine.discard_draft(account_id, key);
                        on_closed_c(false);
                        closed_c.set(true);
                        window_c.close();
                    }
                    _ => {}
                }
            });
            glib::Propagation::Stop
        }
    });

    let picker_window = window.clone();
    let picker_list = attachment_list.clone();
    let picker_group = attachments_group.clone();
    let picker_attachments = Rc::clone(&attachments);
    let picker_banner = banner.clone();
    let picker_banner_action = Rc::clone(&banner_action);
    let picker_dirty = Rc::clone(&dirty);
    attach.connect_clicked(move |_| {
        let dialog = gtk::FileDialog::builder().title("Attach files").build();
        let list = picker_list.clone();
        let group = picker_group.clone();
        let attachments = Rc::clone(&picker_attachments);
        let banner = picker_banner.clone();
        let banner_action = Rc::clone(&picker_banner_action);
        let dirty = Rc::clone(&picker_dirty);
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
                        Ok(attachment) => {
                            attachments.borrow_mut().push(attachment);
                            dirty.set(true);
                        }
                        Err(message) => failed = Some(message),
                    }
                }
                if let Some(message) = failed {
                    banner.set_title(&message);
                    banner.set_revealed(true);
                }
                rebuild_attachments(
                    &list,
                    &group,
                    &attachments,
                    Some(Rc::clone(&dirty)),
                    banner.clone(),
                    Rc::clone(&banner_action),
                );
            },
        );
    });

    let close_window = window.clone();
    let send_snapshot = Rc::clone(&snapshot);
    let send_key = Rc::clone(&draft_key);
    let send_sent = Rc::clone(&sent);
    let send_closed = Rc::clone(&closed);
    let send_encrypt = Rc::clone(&pgp_encrypt);
    let send_sign = Rc::clone(&pgp_sign);
    let send_for_click = send.clone();
    send.connect_clicked(move |_| {
        let app = Rc::clone(&app);
        let send_for_click = send_for_click.clone();
        send_for_click.set_sensitive(false);
        let send_for_report = send_for_click.clone();
        let banner = banner.clone();
        let send_key = Rc::clone(&send_key);
        let send_sent = Rc::clone(&send_sent);
        let send_closed = Rc::clone(&send_closed);
        let send_encrypt = Rc::clone(&send_encrypt);
        let send_sign = Rc::clone(&send_sign);
        let close_window = close_window.clone();
        let on_sent = on_sent.clone();
        send_snapshot(Box::new(move |outgoing| {
            let pgp = PgpOptions {
                encrypt: send_encrypt.get(),
                sign: send_sign.get(),
            };
            match app.mailin.build_outgoing(&account.id, &outgoing, pgp) {
                Ok((raw, envelope)) => match app.engine.queue_and_send(account.id, raw, envelope) {
                    Ok(_) => {
                        let key = send_key.borrow().clone();
                        if !key.is_empty() {
                            app.engine.discard_draft(account.id, key);
                        }
                        send_sent.set(true);
                        send_closed.set(true);
                        on_sent();
                        close_window.close();
                    }
                    Err(error) => {
                        banner.set_title(&format!("Could not queue the message: {error}"));
                        banner.set_revealed(true);
                        send_for_report.set_sensitive(true);
                    }
                },
                Err(error) => {
                    banner.set_title(&format!("Could not send: {error}"));
                    banner.set_revealed(true);
                    send_for_report.set_sensitive(true);
                }
            }
        }));
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
    Some(window)
}

fn rebuild_attachments(
    list: &gtk::ListBox,
    group: &adw::PreferencesGroup,
    attachments: &Rc<RefCell<Vec<ComposeAttachment>>>,
    dirty: Option<Rc<Cell<bool>>>,
    banner: adw::Banner,
    banner_action: Rc<Cell<u8>>,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let items = attachments.borrow();
    group.set_visible(!items.is_empty());
    // M4-E: soft wire-size preflight warning (does not override key-manager actions).
    let total: usize = items.iter().map(|a| a.data.len()).sum();
    drop(items);
    if total >= LARGE_ATTACHMENT_BYTES && banner_action.get() != 1 && banner_action.get() != 2 {
        banner.set_button_label(None::<&str>);
        banner.set_title(&format!(
            "Attachments total ~{} (raw). On the wire this can be ~4/3 larger and may exceed server limits.",
            format::file_size(u32::try_from(total).unwrap_or(u32::MAX))
        ));
        banner.set_revealed(true);
    }
    let items = attachments.borrow();
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
        remove.set_size_request(32, 32);
        {
            use gtk::accessible::Property;
            let remove_name = format!("Remove attachment {}", attachment.filename);
            remove.update_property(&[Property::Label(&remove_name)]);
        }
        let attachments = Rc::clone(attachments);
        let remove_list = list.clone();
        let remove_group = group.clone();
        let remove_dirty = dirty.clone();
        let remove_banner = banner.clone();
        let remove_banner_action = Rc::clone(&banner_action);
        remove.connect_clicked(move |_| {
            if index < attachments.borrow().len() {
                attachments.borrow_mut().remove(index);
            }
            if let Some(dirty) = &remove_dirty {
                dirty.set(true);
            }
            rebuild_attachments(
                &remove_list,
                &remove_group,
                &attachments,
                remove_dirty.clone(),
                remove_banner.clone(),
                Rc::clone(&remove_banner_action),
            );
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
        body: with_signature_prefix(&account.signature, &quoted_reply(message, body)),
        html_body: None,
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

/// Show a dialog to save the current compose content as a template.
fn save_as_template_dialog(
    parent: &adw::Window,
    app: &Rc<AppState>,
    account_id: AccountId,
    subject_row: &adw::EntryRow,
    webview: &WebView,
    banner: &adw::Banner,
) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .heading("Save as template")
        .body("Enter a name for this template. Templates store plain text; formatting and attachments are not saved.")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");

    let entry = adw::EntryRow::builder()
        .title("Template name")
        .show_apply_button(true)
        .build();
    dialog.set_extra_child(Some(&entry));

    let entry_clone = entry.clone();
    let subject_text = subject_row.text().to_string();

    // Query the body asynchronously: the result callback runs on the GTK main
    // loop, so it is populated by the time the user answers the dialog. Blocking
    // here with `recv_timeout` would deadlock the very callback we await.
    let body_text = Rc::new(RefCell::new(String::new()));
    {
        let body_text = Rc::clone(&body_text);
        webview.evaluate_javascript(
            "document.body.innerText",
            None,
            None,
            None::<&gio::Cancellable>,
            move |result| {
                if let Ok(value) = result {
                    *body_text.borrow_mut() = jsc_string(&value);
                }
            },
        );
    }

    let app = Rc::clone(app);
    let banner = banner.clone();

    dialog.connect_response(None, move |dialog: &adw::MessageDialog, response| {
        if response == "save" {
            let name = entry_clone.text().to_string().trim().to_owned();
            if !name.is_empty() {
                let body_text = body_text.borrow().clone();
                match app.mailin.store().upsert_template(
                    &account_id,
                    &name,
                    &subject_text,
                    &body_text,
                ) {
                    Ok(_) => {
                        banner.set_title("Template saved");
                        banner.set_revealed(true);
                        let banner_hide = banner.clone();
                        glib::timeout_add_seconds_local_once(3, move || {
                            if banner_hide.title().as_str() == "Template saved" {
                                banner_hide.set_revealed(false);
                            }
                        });
                    }
                    Err(e) => {
                        banner.set_title(&format!("Could not save template: {e}"));
                        banner.set_revealed(true);
                    }
                }
            }
        }
        dialog.close();
    });

    dialog.present();
    entry.grab_focus();
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
            thread_root_key: Some("root@example.com".into()),
            thread_reply_count: 0,
            thread_depth: 0,
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
