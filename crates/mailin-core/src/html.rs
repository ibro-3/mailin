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
        if tag.contains("http://")
            || tag.contains("https://")
            || tag.contains("src=\"//")
            || tag.contains("src='//")
            || tag.contains("srcset=\"//")
            || tag.contains("srcset='//")
            || tag.contains("url(//")
        {
            let resource = tag.contains("src=")
                || tag.contains("background=")
                || tag.contains("url(")
                || tag.contains("@import")
                || tag.contains("<link");
            if resource {
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
        for prefix in ["cid:", "CID:"] {
            output = output.replace(&format!("{prefix}{id}"), data_url);
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
}
