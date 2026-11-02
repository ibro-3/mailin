//! Compose window: build a message, attach files and queue it in the outbox.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use adw::prelude::*;
use gtk::{gio, glib};
use libadwaita as adw;

use mailin_core::compose::{
    PgpOptions, apply_template, new_draft_key, recipient_addresses, template_draft,
    with_html_signature_prefix, with_signature_prefix,
};
use mailin_core::html::sanitize_html;
use mailin_core::{
    Account, AccountId, ComposeAttachment, Contact, MessageHeader, OutgoingMessage, Template,
};

use crate::app::AppState;
use crate::format;
use webkit6::javascriptcore::Value as JSValue;
use webkit6::prelude::*;
use webkit6::{UserContentManager, WebView};

/// Maximum size of a single attached file. Larger files would make the SQLite
/// outbox and the SMTP submission needlessly fragile.
const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

pub fn present(
    parent: &gtk::Window,
    app: Rc<AppState>,
    account_id: AccountId,
    draft: OutgoingMessage,
    on_sent: impl Fn() + 'static,
) -> Option<adw::Window> {
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

    let save = gtk::Button::with_label("Save Draft");
    save.set_tooltip_text(Some("Save Draft (auto-saves every 30 seconds)"));
    save.add_css_class("flat");
    header.pack_end(&save);

    let attach = gtk::Button::builder()
        .icon_name("mail-attachment-symbolic")
        .tooltip_text("Attach files")
        .build();
    attach.add_css_class("flat");
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
        // Convert plain text to simple HTML
        draft.body.replace('\n', "<br>")
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
            let _ = webview_for_paste.evaluate_javascript(
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

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&page_scroller);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    // Validate key availability as soon as the user asks for PGP.
    {
        let app = app.clone();
        let state = Rc::clone(&pgp_encrypt);
        let banner = banner.clone();
        let to_row = to_row.clone();
        let cc_row = cc_row.clone();
        let bcc_row = bcc_row.clone();
        encrypt_toggle.connect_toggled(move |button| {
            state.set(button.is_active());
            if !button.is_active() {
                return;
            }
            let mut recipients = Vec::new();
            for row in [&to_row, &cc_row, &bcc_row] {
                if let Ok(addresses) = recipient_addresses(&row.text()) {
                    recipients.extend(addresses);
                }
            }
            match app.mailin.missing_pgp_keys(&account_id, &recipients) {
                Ok(missing) if !missing.is_empty() => {
                    button.set_active(false);
                    state.set(false);
                    banner.set_title(&format!("No public key for: {}", missing.join(", ")));
                    banner.set_revealed(true);
                }
                Err(error) => {
                    button.set_active(false);
                    state.set(false);
                    banner.set_title(&format!("Could not check PGP keys: {error}"));
                    banner.set_revealed(true);
                }
                _ => banner.set_revealed(false),
            }
        });
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
    rebuild_attachments(
        &attachment_list,
        &attachments_group,
        &attachments,
        Some(Rc::clone(&dirty)),
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
                rebuild_attachments(&list, &group, &attachments, Some(Rc::clone(&dirty)));
                let html = format!("<img src=\"cid:{cid}\" alt=\"pasted image\">");
                let script = format!(
                    "__mailinInsert({});",
                    serde_json::to_string(&html).unwrap_or_else(|_| "\"\"".to_owned())
                );
                let _ = webview.evaluate_javascript(
                    &script,
                    None,
                    None,
                    None::<&gio::Cancellable>,
                    |_| {},
                );
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
            rebuild_attachments(&list, &group, &attachments, Some(Rc::clone(&dirty)));
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
    let dirty_for_populate = Rc::clone(&dirty);

    // Populate templates list
    let populate_templates = {
        let app = Rc::clone(&app);
        let account_id = account.id;
        let templates_list = templates_list.clone();
        let templates_popover = templates_popover.clone();
        move || {
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
                        let dirty = Rc::clone(&dirty_for_populate);
                        let templates_popover = templates_popover.clone();
                        let banner = banner_for_populate.clone();
                        row.connect_activated(move |_| {
                            // Only prefill empty fields
                            let vars = std::collections::HashMap::new();
                            let draft = template_draft(&template_clone, &vars);
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
                            let (tx, rx) = std::sync::mpsc::channel();
                            webview.evaluate_javascript(
                                "document.body.innerText",
                                None,
                                None,
                                None::<&gio::Cancellable>,
                                move |result| {
                                    if let Ok(value) = result {
                                        let text = value.to_string();
                                        let _ = tx.send(text);
                                    }
                                },
                            );
                            let current_body = rx
                                .recv_timeout(std::time::Duration::from_millis(100))
                                .unwrap_or_default();
                            if current_body.trim().is_empty() {
                                // For WebView, we need to set HTML content
                                let (tx2, rx2) = std::sync::mpsc::channel();
                                webview.evaluate_javascript(
                                    "document.body.innerHTML",
                                    None,
                                    None,
                                    None::<&gio::Cancellable>,
                                    move |result| {
                                        if let Ok(value) = result {
                                            let html = value.to_string();
                                            let _ = tx2.send(html);
                                        }
                                    },
                                );
                                let current_html = rx2
                                    .recv_timeout(std::time::Duration::from_millis(100))
                                    .unwrap_or_default();
                                if current_html.trim().is_empty() {
                                    webview.evaluate_javascript(
                                        &format!(
                                            "document.body.innerHTML = {}",
                                            serde_json::to_string(&draft.body).unwrap()
                                        ),
                                        None,
                                        None,
                                        None::<&gio::Cancellable>,
                                        |_| {},
                                    );
                                }
                            }
                            dirty.set(true);
                            templates_popover.popdown();
                            banner.set_title("Template applied");
                            banner.set_revealed(true);
                        });
                        templates_list.append(&row);
                    }
                }
            }
            templates_list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            let save_row = adw::ActionRow::builder().title("Save as template…").build();
            let to_row = to_row_for_populate.clone();
            let cc_row = cc_row_for_populate.clone();
            let bcc_row = bcc_row_for_populate.clone();
            let subject_row = subject_row_for_populate.clone();
            let webview = webview_for_populate.clone();
            let app = Rc::clone(&app);
            let account_id = account.id;
            let templates_popover = templates_popover.clone();
            let banner = banner_for_populate.clone();
            let window = window_for_populate.clone();
            save_row.connect_activated(move |_| {
                templates_popover.popdown();
                save_as_template_dialog(
                    &window,
                    &app,
                    account_id,
                    &to_row,
                    &cc_row,
                    &bcc_row,
                    &subject_row,
                    &webview,
                    &banner,
                );
            });
            templates_list.append(&save_row);
        }
    };
    // Show templates when popover opens
    templates_popover.connect_closed({
        let _populate = populate_templates.clone();
        move |_| {
            // Re-populate next time it opens
        }
    });
    // Initial population on first open
    templates_popover.set_autohide(true);
    // We'll populate when the button is clicked by connecting to the popover's show signal
    // For simplicity, populate now
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
                let account_id = account_id;
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
            let entry_for_keys = entry.clone();
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

    // HTML/Plain text mode toggle
    let is_html_mode = Rc::new(Cell::new(draft.html_body.is_some()));

    let snapshot: Rc<dyn Fn() -> OutgoingMessage> = {
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
        Rc::new(move || {
            let html_body = if is_html_mode.get() {
                // Get HTML from WebView
                let (tx, rx) = std::sync::mpsc::channel();
                let webview_clone = webview.clone();
                webview_clone.evaluate_javascript(
                    "document.body.innerHTML",
                    None,
                    None,
                    None::<&gio::Cancellable>,
                    move |result| {
                        if let Ok(value) = result {
                            let html = value.to_string();
                            let _ = tx.send(html);
                        }
                    },
                );
                rx.recv_timeout(std::time::Duration::from_millis(100))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            OutgoingMessage {
                from_name: account.display_name.clone(),
                from_addr: account.email.clone(),
                to: to_row.text().to_string(),
                cc: cc_row.text().to_string(),
                bcc: bcc_row.text().to_string(),
                subject: subject_row.text().to_string(),
                body: {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let webview_clone = webview.clone();
                    webview_clone.evaluate_javascript(
                        "document.body.innerText",
                        None,
                        None,
                        None::<&gio::Cancellable>,
                        move |result| {
                            if let Ok(value) = result {
                                let text = value.to_string();
                                let _ = tx.send(text);
                            }
                        },
                    );
                    rx.recv_timeout(std::time::Duration::from_millis(100))
                        .unwrap_or_default()
                },
                html_body: if is_html_mode.get() {
                    Some(html_body)
                } else {
                    None
                },
                in_reply_to: in_reply_to.clone(),
                references: references.clone(),
                draft_key: Some(draft_key.borrow().clone()),
                attachments: attachments.borrow().clone(),
            }
        })
    };

    let save_draft: Rc<dyn Fn(bool)> = {
        let snapshot = Rc::clone(&snapshot);
        let dirty = Rc::clone(&dirty);
        let app = Rc::clone(&app);
        let account_id = account.id;
        let banner = banner.clone();
        Rc::new(move |force: bool| {
            if !force && !dirty.replace(false) {
                return;
            }
            let outgoing = snapshot();
            match outgoing.build_draft() {
                Ok(_) => {
                    app.engine.save_draft(account_id, outgoing);
                    banner.set_title("Draft saved");
                    banner.set_revealed(true);
                }
                Err(error) => {
                    dirty.set(true);
                    banner.set_title(&format!("Could not save the draft: {error}"));
                    banner.set_revealed(true);
                }
            }
        })
    };

    for row in [&to_row, &cc_row, &bcc_row, &subject_row] {
        let dirty = Rc::clone(&dirty);
        row.connect_changed(move |_| dirty.set(true));
    }
    {
        let dirty = Rc::clone(&dirty);
        let webview = webview.clone();
        // WebView doesn't have a simple "changed" signal; rely on auto-save timer
        // and manual save. We could add a timeout to check for changes if needed.
        let _ = webview;
    }

    save.connect_clicked({
        let save_draft = Rc::clone(&save_draft);
        move |_| save_draft(true)
    });

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
        let save_draft = Rc::clone(&save_draft);
        let closed = Rc::clone(&closed);
        let sent = Rc::clone(&sent);
        move |_| {
            if !closed.get() && !sent.get() {
                save_draft(false);
            }
            closed.set(true);
            glib::Propagation::Proceed
        }
    });

    let picker_window = window.clone();
    let picker_list = attachment_list.clone();
    let picker_group = attachments_group.clone();
    let picker_attachments = Rc::clone(&attachments);
    let picker_banner = banner.clone();
    let picker_dirty = Rc::clone(&dirty);
    attach.connect_clicked(move |_| {
        let dialog = gtk::FileDialog::builder().title("Attach files").build();
        let list = picker_list.clone();
        let group = picker_group.clone();
        let attachments = Rc::clone(&picker_attachments);
        let banner = picker_banner.clone();
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
                rebuild_attachments(&list, &group, &attachments, Some(Rc::clone(&dirty)));
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
    send.connect_clicked(move |_| {
        let outgoing = send_snapshot();
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
    Some(window)
}

fn rebuild_attachments(
    list: &gtk::ListBox,
    group: &adw::PreferencesGroup,
    attachments: &Rc<RefCell<Vec<ComposeAttachment>>>,
    dirty: Option<Rc<Cell<bool>>>,
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
        {
            use gtk::accessible::Property;
            let remove_name = format!("Remove attachment {}", attachment.filename);
            remove.update_property(&[Property::Label(&remove_name)]);
        }
        let attachments = Rc::clone(attachments);
        let remove_list = list.clone();
        let remove_group = group.clone();
        let remove_dirty = dirty.clone();
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
    to_row: &adw::EntryRow,
    cc_row: &adw::EntryRow,
    bcc_row: &adw::EntryRow,
    subject_row: &adw::EntryRow,
    webview: &WebView,
    banner: &adw::Banner,
) {
    let dialog = adw::MessageDialog::builder()
        .heading("Save as template")
        .body("Enter a name for this template:")
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
    let to_text = to_row.text().to_string();
    let cc_text = cc_row.text().to_string();
    let bcc_text = bcc_row.text().to_string();
    let subject_text = subject_row.text().to_string();

    let (tx, rx) = std::sync::mpsc::channel();
    webview.evaluate_javascript(
        "document.body.innerText",
        None,
        None,
        None::<&gio::Cancellable>,
        move |result| {
            if let Ok(value) = result {
                let text = value.to_string();
                let _ = tx.send(text);
            }
        },
    );
    let body_text = rx
        .recv_timeout(std::time::Duration::from_millis(100))
        .unwrap_or_default();

    let app = Rc::clone(app);
    let banner = banner.clone();

    dialog.connect_response(None, move |dialog: &adw::MessageDialog, response| {
        if response == "save" {
            let name = entry_clone.text().to_string().trim().to_owned();
            if !name.is_empty() {
                match app.mailin.store().upsert_template(
                    &account_id,
                    &name,
                    &subject_text,
                    &body_text,
                ) {
                    Ok(_) => {
                        banner.set_title("Template saved");
                        banner.set_revealed(true);
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
