//! Safe HTML rendering for message bodies.
//!
//! This module is deliberately free of toolkit types: the GTK client hands the
//! document to WebKit and the Windows client hands it to WebView2, but both use
//! the same policy. Email HTML is untrusted input. Mailin never enables
//! JavaScript, plugins or file access, and it injects a Content-Security-Policy
//! that blocks every network request unless the user explicitly allows remote
//! content for the message. Inline `cid:` images are rewritten to `data:` URLs
//! so they work offline without touching the network.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Build the document handed to the platform web view.
pub fn document(
    html: &str,
    allow_remote: bool,
    dark: bool,
    inline_images: &[(String, String)],
) -> String {
    let html = rewrite_cid_images(html, inline_images);
    let image_sources = if allow_remote {
        "data: cid: http: https:"
    } else {
        "data: cid:"
    };
    let nonce = format!("mailin-{}", uuid::Uuid::new_v4().simple());
    let policy = format!(
        "default-src 'none'; img-src {image_sources}; style-src 'unsafe-inline'; \
         font-src data:; media-src 'none'; script-src 'nonce-{nonce}'; frame-src 'none'; \
         base-uri 'none'; form-action 'none';"
    );
    let (background, foreground, link) = if dark {
        ("#1e1e1e", "#eeeeee", "#78aeed")
    } else {
        ("#ffffff", "#1c1c1c", "#1c71d8")
    };
    let head = format!(
        "<meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{policy}\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <style>\
           html, body {{ background: {background}; color: {foreground}; }}\
           body {{ margin: 0; padding: 0; font: 14px/1.5 system-ui, \"Segoe UI\", Cantarell, sans-serif;\
                   overflow-wrap: anywhere; }}\
           img, video {{ max-width: 100%; height: auto; }}\
           table {{ max-width: 100%; }}\
           pre {{ white-space: pre-wrap; }}\
           blockquote {{ margin-left: 1em; padding-left: 1em;\
                         border-left: 3px solid rgba(128, 128, 128, 0.4); }}\
           a {{ color: {link}; }}\
         </style>"
    );
    // Lock down navigation and form submission even though the policy already
    // blocks scripts. The script carries a nonce so mail-authored scripts stay
    // blocked while our own interceptor runs.
    let navigation_guard = format!(
        "<script nonce=\"{nonce}\">\
           document.addEventListener('click', function (event) {{\
             var link = event.target && event.target.closest ? event.target.closest('a') : null;\
             if (link) {{ event.preventDefault(); }}\
           }}, true);\
           document.addEventListener('submit', function (event) {{ event.preventDefault(); }}, true);\
         </script>"
    );
    let head = format!("{head}{navigation_guard}");
    inject_head(html.as_ref(), &head)
}

/// Whether the HTML refers to a network resource that the policy would block.
pub fn has_remote_content(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start..].find('>') else {
            break;
        };
        let tag = &rest[start..start + end];
        // Check for http/https URLs in various attributes
        if tag.contains("http://")
            || tag.contains("https://")
            || tag.contains("src=\"//")
            || tag.contains("src='//")
            || tag.contains("srcset=\"//")
            || tag.contains("srcset='//")
            || tag.contains("href=\"//")
            || tag.contains("href='//")
            || tag.contains("action=\"//")
            || tag.contains("action='//")
            || tag.contains("url(//")
        {
            let resource = tag.contains("src=")
                || tag.contains("srcset=")
                || tag.contains("action=")
                || tag.contains("background=")
                || tag.contains("url(")
                || tag.contains("@import")
                || tag.contains("<link")
                || tag.contains("<iframe")
                || tag.contains("<frame")
                || tag.contains("<video")
                || tag.contains("<audio")
                || tag.contains("<source")
                || tag.contains("<form");
            // Anchor hrefs are user-navigable, not auto-loaded resources
            if resource && !tag.contains("<a ") && !tag.contains("<a>") {
                return true;
            }
        }
        rest = &rest[start + end + 1..];
    }
    // CSS `url(...)` may appear inside a <style> block rather than a tag.
    lower.contains("@import")
        || lower.contains("url(http://")
        || lower.contains("url(https://")
        || lower.contains("url(//")
        || lower.contains("url('http://")
        || lower.contains("url('https://")
        || lower.contains("url('//")
        || lower.contains("url(\"http://")
        || lower.contains("url(\"https://")
        || lower.contains("url(\"//")
        // srcset attributes in style blocks (rare but possible)
        || lower.contains("srcset=\"http://")
        || lower.contains("srcset=\"https://")
        || lower.contains("srcset='http://")
        || lower.contains("srcset='https://")
}

fn inject_head(html: &str, head: &str) -> String {
    let lower = html.to_ascii_lowercase();
    if let Some(start) = lower.find("<head") {
        if let Some(offset) = html[start..].find('>') {
            let insert = start + offset + 1;
            let mut output = String::with_capacity(html.len() + head.len());
            output.push_str(&html[..insert]);
            output.push_str(head);
            output.push_str(&html[insert..]);
            return output;
        }
    }
    if let Some(start) = lower.find("<html") {
        if let Some(offset) = html[start..].find('>') {
            let insert = start + offset + 1;
            let mut output = String::with_capacity(html.len() + head.len() + 13);
            output.push_str(&html[..insert]);
            output.push_str("<head>");
            output.push_str(head);
            output.push_str("</head>");
            output.push_str(&html[insert..]);
            return output;
        }
    }
    format!("<!DOCTYPE html><html><head>{head}</head><body>{html}</body></html>")
}

/// Replace `cid:` image references with the matching `data:` URLs.
pub fn rewrite_cid_images(html: &str, images: &[(String, String)]) -> String {
    let mut output = html.to_owned();
    for (content_id, data_url) in images {
        let id = content_id
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>');
        if id.is_empty() {
            continue;
        }
        // Generate all variations of the cid: reference that might appear in HTML:
        // - cid:id, CID:id, Cid:id, etc. (case insensitive prefix)
        // - With/without angle brackets around the content ID
        // - With/without whitespace after the prefix and around the content ID
        let mut patterns = Vec::new();
        let prefixes = [
            "cid:", "CID:", "Cid:", "cId:", "ciD:", "CId:", "CiD:", "cID:",
        ];
        for prefix in prefixes {
            // Without brackets, without whitespace
            patterns.push(format!("{prefix}{id}"));
            // Without brackets, with whitespace after prefix
            patterns.push(format!("{prefix} {id}"));
            patterns.push(format!("{prefix}{id} "));
            patterns.push(format!("{prefix} {id} "));
            // With brackets in HTML (cid:<id>), without whitespace
            patterns.push(format!("{prefix}<{id}>"));
            // With brackets in HTML, with whitespace
            patterns.push(format!("{prefix} <{id}>"));
            patterns.push(format!("{prefix}<{id}> "));
            patterns.push(format!("{prefix} <{id}> "));
            // With whitespace around content ID in HTML (cid:  id  )
            patterns.push(format!("{prefix}  {id}  "));
            patterns.push(format!("{prefix}{id}  "));
            patterns.push(format!("{prefix}  {id}"));
        }
        for pattern in patterns {
            output = output.replace(&pattern, data_url);
        }
    }
    output
}

/// `data:` URL for an inline image.
pub fn data_url(mime_type: &str, bytes: &[u8]) -> String {
    let mime = if mime_type.trim().is_empty() {
        "application/octet-stream"
    } else {
        mime_type
    };
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

/// Escape text for HTML element content and quoted attributes.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}


/// Sanitize HTML for safe composition.
/// Removes scripts, event handlers, forms, and other unsafe elements/attributes.
/// Allows a safe subset of HTML for formatted email composition.
pub fn sanitize_html(input: &str) -> String {
    // Quick check - if no suspicious content, return as-is
    if !input.contains('<') {
        return input.to_owned();
    }

    let mut output = String::with_capacity(input.len());
    let mut i = 0;
    let chars: Vec<char> = input.chars().collect();

    while i < chars.len() {
        if chars[i] == '<' {
            // Look ahead to see what kind of tag this is
            if i + 1 < chars.len() && chars[i + 1] == '!' {
                // Comment or doctype - skip to end
                while i < chars.len() && chars[i] != '>' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1; // skip '>'
                }
                continue;
            }

            if i + 1 < chars.len() && chars[i + 1] == '/' {
                // Closing tag - allow all closing tags
                while i < chars.len() && chars[i] != '>' {
                    output.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    output.push(chars[i]);
                    i += 1;
                }
                continue;
            }

            // Opening tag - parse it
            let mut tag_name = String::new();
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_alphabetic() {
                tag_name.push(chars[j]);
                j += 1;
            }
            let tag_lower = tag_name.to_lowercase();

            // Block dangerous tags entirely
            if matches!(
                tag_lower.as_str(),
                "script" | "iframe" | "frame" | "frameset" | "object" | "embed" |
                "applet" | "form" | "input" | "button" | "select" | "textarea" |
                "link" | "meta" | "base" | "style" | "svg" | "math" | "noscript"
            ) {
                // Skip this entire tag and its content
                while i < chars.len() && chars[i] != '>' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                // Skip content until closing tag
                let close_tag = format!("</{}>", tag_lower);
                let remaining: String = chars[i..].iter().collect();
                if let Some(pos) = remaining.find(&close_tag) {
                    i += pos + close_tag.len();
                }
                continue;
            }

            // Allow safe tags - copy the tag with sanitized attributes
            let allowed_tags = [
                "b", "i", "u", "em", "strong", "a", "p", "br", "div", "span",
                "ul", "ol", "li", "blockquote", "pre", "code", "h1", "h2", "h3",
                "h4", "h5", "h6", "img", "table", "thead", "tbody", "tr", "td", "th",
                "hr", "small", "sub", "sup", "del", "ins", "mark", "cite", "q",
            ];

            if allowed_tags.contains(&tag_lower.as_str()) {
                // Copy the tag with sanitized attributes
                let mut tag_end = j;
                let mut in_quotes = false;
                let mut quote_char = '\0';
                while tag_end < chars.len() {
                    let c = chars[tag_end];
                    if (c == '"' || c == '\'') && !in_quotes {
                        in_quotes = true;
                        quote_char = c;
                    } else if c == quote_char && in_quotes {
                        in_quotes = false;
                        quote_char = '\0';
                    } else if c == '>' && !in_quotes {
                        tag_end += 1;
                        break;
                    }
                    tag_end += 1;
                }
                // Copy the raw tag but filter attributes
                let tag_content: String = chars[i..tag_end].iter().collect();
                output.push_str(&sanitize_tag_attributes(&tag_content));
                i = tag_end;
            } else {
                // Unknown tag - skip it (encode as text)
                while i < chars.len() && chars[i] != '>' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
            }
        } else {
            output.push(chars[i]);
            i += 1;
        }
    }
    output
}

/// Sanitize attributes in an HTML tag - remove event handlers, `javascript:`
/// URLs and other active content, while preserving ordinary attributes.
fn sanitize_tag_attributes(tag: &str) -> String {
    let chars: Vec<char> = tag.chars().collect();
    if chars.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(tag.len());
    out.push('<');
    let mut i = 1;
    // Copy the element name verbatim; only attributes are filtered.
    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == ':') {
        out.push(chars[i]);
        i += 1;
    }

    while i < chars.len() {
        let character = chars[i];
        if character == '>' {
            out.push('>');
            break;
        }
        if character == '/' {
            out.push('/');
            i += 1;
            continue;
        }
        if character.is_whitespace() {
            out.push(' ');
            i += 1;
            continue;
        }

        // Parse the attribute name. Attribute names start only at whitespace
        // boundaries, so `strong` or `one.html` are not mistaken for `on*`.
        let name_start = i;
        while i < chars.len()
            && !chars[i].is_whitespace()
            && !matches!(chars[i], '=' | '>' | '/')
        {
            i += 1;
        }
        if i == name_start {
            // Unexpected character (for example a stray '='); keep it verbatim.
            out.push(character);
            i += 1;
            continue;
        }
        let name: String = chars[name_start..i].iter().collect();

        // Collect the optional value so URL-valued attributes can be checked.
        let mut value: Option<String> = None;
        let mut cursor = i;
        while cursor < chars.len() && chars[cursor].is_whitespace() {
            cursor += 1;
        }
        if cursor < chars.len() && chars[cursor] == '=' {
            cursor += 1;
            while cursor < chars.len() && chars[cursor].is_whitespace() {
                cursor += 1;
            }
            if cursor < chars.len() && (chars[cursor] == '"' || chars[cursor] == '\'') {
                let quote = chars[cursor];
                cursor += 1;
                let value_start = cursor;
                while cursor < chars.len() && chars[cursor] != quote {
                    cursor += 1;
                }
                value = Some(chars[value_start..cursor].iter().collect());
                if cursor < chars.len() {
                    cursor += 1;
                }
            } else {
                let value_start = cursor;
                while cursor < chars.len()
                    && !chars[cursor].is_whitespace()
                    && chars[cursor] != '>'
                {
                    cursor += 1;
                }
                value = Some(chars[value_start..cursor].iter().collect());
            }
            i = cursor;
        }

        let lower = name.to_ascii_lowercase();
        let script_url = value.as_deref().is_some_and(|value| {
            let value = value.trim().to_ascii_lowercase();
            value.starts_with("javascript:")
                || value.starts_with("vbscript:")
                || value.starts_with("data:text/html")
        });
        let dangerous = lower.starts_with("on")
            || (matches!(lower.as_str(), "href" | "src" | "action" | "formaction")
                && script_url);
        if dangerous {
            continue;
        }
        out.extend(chars[name_start..i].iter());
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_gets_the_policy_and_default_styles() {
        let document = document(
            "<html><head><title>x</title></head><body>hi</body></html>",
            false,
            false,
            &[],
        );
        assert!(document.contains("Content-Security-Policy"));
        assert!(document.contains("img-src data: cid:;"));
        assert!(document.contains("<title>x</title>"));
        assert!(
            document.find("Content-Security-Policy").unwrap() < document.find("<title>").unwrap()
        );
        assert!(document.contains("script-src 'nonce-"));
    }

    #[test]
    fn remote_content_is_allowed_only_on_request() {
        let blocked = document("<p>x</p>", false, false, &[]);
        assert!(blocked.contains("img-src data: cid:;"));
        let allowed = document("<p>x</p>", true, true, &[]);
        assert!(allowed.contains("img-src data: cid: http: https:;"));
        assert!(allowed.contains("#1e1e1e"));
    }

    #[test]
    fn fragments_are_wrapped_in_a_complete_document() {
        let document = document("<p>Hello</p>", false, false, &[]);
        assert!(document.starts_with("<!DOCTYPE html><html><head>"));
        assert!(document.contains("<body><p>Hello</p></body>"));
    }

    #[test]
    fn cid_images_become_data_urls() {
        let document = document(
            "<img src=\"cid:logo@example.com\"><img src=\"cid:other\">",
            false,
            false,
            &[(
                "logo@example.com".into(),
                "data:image/png;base64,AAAA".into(),
            )],
        );
        assert!(document.contains("src=\"data:image/png;base64,AAAA\""));
        assert!(document.contains("src=\"cid:other\""));
    }

    #[test]
    fn remote_resources_are_detected() {
        assert!(has_remote_content(
            "<p><img src=\"https://tracker.example/pixel.png\"></p>"
        ));
        assert!(has_remote_content(
            "<style>body { background: url(http://example.com/x) }</style>"
        ));
        assert!(has_remote_content(
            "<img src=\"//tracker.example/pixel.png\">"
        ));
        assert!(has_remote_content(
            "<style>body { background: url(//example.com/x) }</style>"
        ));
        assert!(!has_remote_content("<p>See https://example.com</p>"));
        assert!(!has_remote_content("<img src=\"cid:logo\">"));
        assert!(!has_remote_content(
            "<a href=\"https://example.com\">link</a>"
        ));
    }

    #[test]
    fn data_urls_are_base64() {
        assert_eq!(data_url("image/png", b"PNG"), "data:image/png;base64,UE5H");
    }

    #[test]
    fn escaping_blocks_markup() {
        assert_eq!(
            escape("<img src=\"x\" onerror='y'>&"),
            "&lt;img src=&quot;x&quot; onerror=&#39;y&#39;&gt;&amp;"
        );
    }
    // ============================================================
    // Phase 2 — Hardening: HTML policy edge-case tests
    // ============================================================

    #[test]
    fn cid_rewriting_handles_angle_brackets_in_content_id() {
        // Content IDs in MIME may be wrapped in angle brackets
        let document = document(
            "<img src=\"cid:<logo@example.com>\">",
            false,
            false,
            &[(
                "logo@example.com".into(),
                "data:image/png;base64,AAAA".into(),
            )],
        );
        assert!(document.contains("src=\"data:image/png;base64,AAAA\""));
    }

    #[test]
    fn cid_rewriting_is_case_insensitive() {
        let document = document(
            "<img src=\"CID:logo@example.com\"><img src=\"Cid:other@example.com\">",
            false,
            false,
            &[(
                "logo@example.com".into(),
                "data:image/png;base64,AAAA".into(),
            )],
        );
        assert!(document.contains("src=\"data:image/png;base64,AAAA\""));
        // Unmatched cid: references preserve their original case
        assert!(document.contains("src=\"Cid:other@example.com\""));
    }

    #[test]
    fn cid_rewriting_handles_whitespace_in_content_id() {
        let document = document(
            "<img src=\"cid:  logo@example.com  \">",
            false,
            false,
            &[(
                "logo@example.com".into(),
                "data:image/png;base64,AAAA".into(),
            )],
        );
        // Whitespace in the HTML src attribute should match trimmed content ID
        assert!(document.contains("src=\"data:image/png;base64,AAAA\""));
    }

    #[test]
    fn cid_rewriting_preserves_other_attributes() {
        let document = document(
            "<img src=\"cid:logo@example.com\" alt=\"Logo\" class=\"inline\" width=\"100\">",
            false,
            false,
            &[(
                "logo@example.com".into(),
                "data:image/png;base64,AAAA".into(),
            )],
        );
        assert!(document.contains("src=\"data:image/png;base64,AAAA\""));
        assert!(document.contains("alt=\"Logo\""));
        assert!(document.contains("class=\"inline\""));
        assert!(document.contains("width=\"100\""));
    }

    #[test]
    fn has_remote_content_detects_data_urls_as_local() {
        // data: URLs should NOT trigger remote content detection
        assert!(!has_remote_content(
            "<img src=\"data:image/png;base64,AAAA\">"
        ));
        assert!(!has_remote_content(
            "<img src='data:image/gif;base64,R0lGODlh'>"
        ));
    }

    #[test]
    fn has_remote_content_detects_protocol_relative_urls() {
        // Protocol-relative URLs (//example.com) should be detected
        assert!(has_remote_content(
            "<img src=\"//tracker.example/pixel.png\">"
        ));
        assert!(has_remote_content(
            "<img src='//cdn.example.com/image.jpg'>"
        ));
        assert!(has_remote_content(
            "<style>@import \"//fonts.example.com/style.css\";</style>"
        ));
    }

    #[test]
    fn has_remote_content_detects_various_css_url_formats() {
        assert!(has_remote_content(
            "<style>background: url(http://example.com/x);</style>"
        ));
        assert!(has_remote_content(
            "<style>background: url('https://example.com/x');</style>"
        ));
        assert!(has_remote_content(
            "<style>background: url(\"https://example.com/x\");</style>"
        ));
        assert!(has_remote_content(
            "<style>background-image: url(//example.com/x);</style>"
        ));
    }

    #[test]
    fn has_remote_content_detects_srcset_attributes() {
        assert!(has_remote_content(
            "<img srcset=\"https://example.com/1x.png 1x, https://example.com/2x.png 2x\">"
        ));
        assert!(has_remote_content(
            "<img srcset='//example.com/image.jpg 1x'>"
        ));
    }

    #[test]
    fn has_remote_content_detects_link_tags() {
        assert!(has_remote_content(
            "<link rel=\"stylesheet\" href=\"https://example.com/style.css\">"
        ));
        assert!(has_remote_content(
            "<link rel='preload' href='//example.com/font.woff2' as='font'>"
        ));
    }

    #[test]
    fn has_remote_content_detects_video_audio_sources() {
        assert!(has_remote_content(
            "<video><source src=\"https://example.com/video.mp4\"></video>"
        ));
        assert!(has_remote_content(
            "<audio><source src=\"//example.com/audio.mp3\"></audio>"
        ));
    }

    #[test]
    fn has_remote_content_detects_iframe_and_frame() {
        assert!(has_remote_content(
            "<iframe src=\"https://example.com/frame.html\"></iframe>"
        ));
        assert!(has_remote_content(
            "<frame src=\"//example.com/frame.html\">"
        ));
    }

    #[test]
    fn has_remote_content_detects_form_actions() {
        assert!(has_remote_content(
            "<form action=\"https://example.com/submit\"></form>"
        ));
        assert!(has_remote_content(
            "<form action=\"//example.com/submit\"></form>"
        ));
    }

    #[test]
    fn has_remote_content_ignores_text_links() {
        // Plain text URLs and anchor hrefs should NOT trigger (they're user-navigable)
        assert!(!has_remote_content(
            "Visit https://example.com for more info"
        ));
        assert!(!has_remote_content(
            "<a href=\"https://example.com\">Click here</a>"
        ));
        assert!(!has_remote_content("<a href=\"//example.com\">Link</a>"));
    }

    #[test]
    fn has_remote_content_ignores_cid_images() {
        assert!(!has_remote_content("<img src=\"cid:logo@example.com\">"));
        assert!(!has_remote_content("<img src='CID:logo@example.com'>"));
    }

    #[test]
    fn csp_policy_blocks_scripts_by_default() {
        let doc = document("<script>alert(1)</script>", false, false, &[]);
        assert!(doc.contains("script-src 'nonce-"));
        assert!(doc.contains("document.addEventListener('click'"));
        assert!(doc.contains("event.preventDefault()"));
    }

    #[test]
    fn csp_policy_blocks_forms_by_default() {
        let doc = document("<form action=\"/submit\"><input></form>", false, false, &[]);
        assert!(doc.contains("form-action 'none'"));
    }

    #[test]
    fn csp_policy_blocks_frames_by_default() {
        let doc = document("<iframe src=\"/frame\"></iframe>", false, false, &[]);
        assert!(doc.contains("frame-src 'none'"));
    }

    #[test]
    fn csp_policy_blocks_base_uri_by_default() {
        let doc = document("<base href=\"https://example.com/\">", false, false, &[]);
        assert!(doc.contains("base-uri 'none'"));
    }

    #[test]
    fn csp_policy_allows_inline_styles_only() {
        let doc = document("<div style=\"color: red;\"></div>", false, false, &[]);
        assert!(doc.contains("style-src 'unsafe-inline'"));
    }

    #[test]
    fn csp_policy_allows_data_fonts() {
        let doc = document(
            "<style>@font-face { src: url(data:font/woff2;base64,AAAA); }</style>",
            false,
            false,
            &[],
        );
        assert!(doc.contains("font-src data:"));
    }

    #[test]
    fn csp_nonce_is_unique_per_document() {
        let doc1 = document("<p>test</p>", false, false, &[]);
        let doc2 = document("<p>test</p>", false, false, &[]);
        let nonce1 = extract_nonce(&doc1);
        let nonce2 = extract_nonce(&doc2);
        assert_ne!(nonce1, nonce2);
        assert!(nonce1.starts_with("mailin-"));
        assert!(nonce2.starts_with("mailin-"));
    }

    #[test]
    fn dark_mode_colors_are_applied() {
        let light = document("<p>test</p>", false, false, &[]);
        let dark = document("<p>test</p>", true, true, &[]);
        assert!(light.contains("#ffffff")); // light background
        assert!(light.contains("#1c1c1c")); // light foreground
        assert!(dark.contains("#1e1e1e")); // dark background
        assert!(dark.contains("#eeeeee")); // dark foreground
    }

    #[test]
    fn document_always_has_doctype_and_html() {
        let doc = document("<p>Hello</p>", false, false, &[]);
        assert!(doc.starts_with("<!DOCTYPE html><html><head>"));
        assert!(doc.ends_with("</body></html>"));
    }

    #[test]
    fn document_head_contains_charset_meta() {
        let doc = document("<p>test</p>", false, false, &[]);
        assert!(doc.contains("<meta charset=\"utf-8\">"));
    }

    #[test]
    fn document_head_contains_referrer_policy() {
        let doc = document("<p>test</p>", false, false, &[]);
        assert!(doc.contains("<meta name=\"referrer\" content=\"no-referrer\">"));
    }

    fn extract_nonce(doc: &str) -> String {
        let start =
            doc.find("script-src 'nonce-").expect("nonce not found") + "script-src 'nonce-".len();
        let end = doc[start..].find("'").expect("nonce end not found");
        doc[start..start + end].to_owned()
    }
}
