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
///
/// Colors follow the system `color-scheme` through `light-dark()`, so the
/// document always renders with the palette the reader is also themed with
/// and no palette values are duplicated in the core.
pub fn document(html: &str, allow_remote: bool, inline_images: &[(String, String)]) -> String {
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
    let head = format!(
        "<meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{policy}\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <style>\
           html, body {{ background: light-dark(#ffffff, #1e1e1e); color: light-dark(#1c1c1c, #eeeeee); }}\
           body {{ margin: 0; padding: 0; font: 14px/1.5 system-ui, \"Segoe UI\", Cantarell, sans-serif;\
                   overflow-wrap: anywhere; }}\
           img, video {{ max-width: 100%; height: auto; }}\
           table {{ max-width: 100%; }}\
           pre {{ white-space: pre-wrap; }}\
           blockquote {{ margin-left: 1em; padding-left: 1em;\
                         border-left: 3px solid rgba(128, 128, 128, 0.4); }}\
           a {{ color: light-dark(#1c71d8, #78aeed); }}\
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
    let bytes = lower.as_bytes();

    // CSS `url(...)` (with optional whitespace and quotes) and `@import` may
    // appear inside a <style> block or an inline style attribute.
    if lower.contains("@import") || has_remote_css_url(&lower) {
        return true;
    }

    // Tag-level attributes: `attr = "value"` with arbitrary whitespace and an
    // optional quote, and absolute http(s) or protocol-relative values.
    let mut i = 0;
    while let Some(open) = find_bytes(bytes, i, b"<") {
        let end = scan_tag_end(bytes, open);
        if end == bytes.len() {
            break;
        }
        let tag = &lower[open..end];
        if is_remote_tag(tag) {
            return true;
        }
        i = end + 1;
    }
    false
}

fn find_bytes(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let mut i = from;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Index of the `>` closing the tag starting at `open`, aware of quoted
/// attribute values. Returns `bytes.len()` when the tag is unterminated.
fn scan_tag_end(bytes: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    let mut quote = 0u8;
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' => {
                quote = c;
                i += 1;
            }
            b'>' => return i,
            _ => i += 1,
        }
    }
    i
}

/// Detect `url( ... )` with an absolute http(s) or protocol-relative target,
/// allowing whitespace and optional quotes around the URL.
fn has_remote_css_url(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while let Some(start) = find_bytes(bytes, i, b"url(") {
        let mut j = start + 4;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
            j += 1;
        }
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if lower[j..].starts_with("http://")
            || lower[j..].starts_with("https://")
            || lower[j..].starts_with("//")
        {
            return true;
        }
        i = start + 4;
    }
    false
}

/// Check whether one of the given attribute names on this tag holds an
/// absolute http(s) or protocol-relative URL.
fn attr_holds_remote_url(tag: &str, attr: &str) -> bool {
    let bytes = tag.as_bytes();
    let attr_bytes = attr.as_bytes();
    let mut i = 0;
    while let Some(rel) = find_bytes(bytes, i, attr_bytes) {
        // Left boundary: must not be part of a longer attribute name, e.g.
        // "data-x" must not match "data", or "srcset" match "src".
        let left_ok = rel == 0
            || !(bytes[rel - 1].is_ascii_alphanumeric()
                || bytes[rel - 1] == b'-'
                || bytes[rel - 1] == b'_');
        let after = rel + attr_bytes.len();
        if left_ok {
            let remote = if attr == "srcset" {
                attr_srcset_is_remote(bytes, after)
            } else {
                attr_tail_is_remote(bytes, after)
            };
            if remote {
                return true;
            }
        }
        i = rel + attr_bytes.len();
    }
    false
}

/// After the attribute name, expect optional whitespace, `=`, optional
/// whitespace, then the attribute value (quoted or unquoted).
fn attr_value_span(bytes: &[u8], mut i: usize) -> Option<(usize, usize)> {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if bytes.get(i) != Some(&b'=') {
        return None;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    match bytes.get(i) {
        Some(quote @ (b'"' | b'\'')) => {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != *quote {
                end += 1;
            }
            Some((start, end))
        }
        _ => {
            let start = i;
            let mut end = i;
            while end < bytes.len() && !bytes[end].is_ascii_whitespace() && bytes[end] != b'>' {
                end += 1;
            }
            Some((start, end))
        }
    }
}

fn is_remote_url(value: &[u8]) -> bool {
    value.starts_with(b"http://") || value.starts_with(b"https://") || value.starts_with(b"//")
}

/// After the attribute name, expect the value to consist of a single
/// http(s)/protocol-relative URL.
fn attr_tail_is_remote(bytes: &[u8], after: usize) -> bool {
    let Some((start, end)) = attr_value_span(bytes, after) else {
        return false;
    };
    is_remote_url(&bytes[start..end])
}

/// `srcset` values are comma-separated `<url> <descriptor>` candidates; a
/// remote candidate anywhere in the list will be fetched by the browser.
fn attr_srcset_is_remote(bytes: &[u8], after: usize) -> bool {
    let Some((start, end)) = attr_value_span(bytes, after) else {
        return false;
    };
    bytes[start..end].split(|b| *b == b',').any(|part| {
        let trimmed = trim_ascii(part);
        let token_end = trimmed
            .iter()
            .position(|b| b.is_ascii_whitespace())
            .unwrap_or(trimmed.len());
        is_remote_url(&trimmed[..token_end])
    })
}

fn trim_ascii(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |end| end + 1);
    &value[start..end]
}

/// A tag that will fetch a remote resource when rendered. `<a>`/`<area>` hrefs
/// are user-navigable links, not auto-loaded resources, so `href` on those
/// tags does not count.
fn is_remote_tag(tag: &str) -> bool {
    let navigable = tag.starts_with("<a") || tag.starts_with("<a>") || tag.starts_with("<area");

    for attr in [
        "src",
        "srcset",
        "action",
        "formaction",
        "background",
        "poster",
        "data",
    ] {
        if attr_holds_remote_url(tag, attr) {
            return true;
        }
    }
    if !navigable && attr_holds_remote_url(tag, "href") {
        return true;
    }
    has_remote_css_url(tag)
}

fn inject_head(html: &str, head: &str) -> String {
    // `to_ascii_lowercase` only changes ASCII bytes, so the lowercased
    // string has the same length and byte offsets as the original.
    let lower = html.to_ascii_lowercase();
    debug_assert_eq!(lower.len(), html.len());
    let mask = structural_mask(lower.as_bytes());

    if let Some(start) = find_tag(&mask, b"<head")
        && let Some(end) = find_byte_from(&mask, start, b'>')
    {
        let insert = end + 1;
        let mut output = String::with_capacity(html.len() + head.len());
        output.push_str(&html[..insert]);
        output.push_str(head);
        output.push_str(&html[insert..]);
        return output;
    }
    if let Some(start) = find_tag(&mask, b"<html")
        && let Some(end) = find_byte_from(&mask, start, b'>')
    {
        let insert = end + 1;
        let mut output = String::with_capacity(html.len() + head.len() + 13);
        output.push_str(&html[..insert]);
        output.push_str("<head>");
        output.push_str(head);
        output.push_str("</head>");
        output.push_str(&html[insert..]);
        return output;
    }
    format!("<!DOCTYPE html><html><head>{head}</head><body>{html}</body></html>")
}

/// Mask out the parts of the document where `<head` / `<html` matches would
/// be meaningless: HTML comments, raw-text elements (`<script>`, `<style>`,
/// `<textarea>`, `<title>`) and quoted attribute values. The returned vector
/// has the same length as `bytes`; masked bytes become ASCII spaces so byte
/// offsets line up with the original string.
fn structural_mask(bytes: &[u8]) -> Vec<u8> {
    let mut mask = bytes.to_vec();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Comment: <!-- ... --> (an unterminated comment masks to EOF).
        if bytes[i..].starts_with(b"<!--") {
            let end = find_bytes(bytes, i + 4, b"-->")
                .map(|end| end + 3)
                .unwrap_or(bytes.len());
            for b in &mut mask[i..end] {
                *b = b' ';
            }
            i = end;
            continue;
        }
        // Raw-text element: <script>...</script>, <style>...</style>,
        // <textarea>...</textarea>, <title>...</title>.
        if let Some(raw) = raw_text_open(bytes, i) {
            let tag_end = scan_tag_end(bytes, i);
            // A self-closing raw-text element has no content to mask.
            let end = if tag_end < bytes.len() && tag_end > i && bytes[tag_end - 1] == b'/' {
                tag_end + 1
            } else {
                find_raw_text_close(bytes, tag_end, raw)
                    .map_or(bytes.len(), |close| scan_tag_end(bytes, close) + 1)
            };
            for b in &mut mask[i..end] {
                *b = b' ';
            }
            i = end;
            continue;
        }
        // Ordinary tag: blank quoted attribute values so e.g.
        // <div data-x="<head>"> cannot be mistaken for an element.
        if i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() {
            let end = scan_tag_end(bytes, i);
            mask_quoted_values(bytes, &mut mask, i, end);
            i = end + 1;
            continue;
        }
        i += 1;
    }
    mask
}

/// If `bytes[i..]` opens a raw-text element, return the lowercased tag name
/// (without the leading `<`).
fn raw_text_open(bytes: &[u8], i: usize) -> Option<&'static [u8]> {
    for name in [b"script".as_slice(), b"style", b"textarea", b"title"] {
        let rest = &bytes[i + 1..];
        if rest.len() >= name.len()
            && rest[..name.len()].eq_ignore_ascii_case(name)
            && rest
                .get(name.len())
                .is_none_or(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
        {
            return Some(name);
        }
    }
    None
}

/// Index of `</name` (case-insensitive) starting the raw-text close tag.
fn find_raw_text_close(bytes: &[u8], from: usize, name: &[u8]) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && bytes.get(i + 1) == Some(&b'/')
            && bytes.len() >= i + 2 + name.len()
            && bytes[i + 2..i + 2 + name.len()].eq_ignore_ascii_case(name)
            && bytes
                .get(i + 2 + name.len())
                .is_none_or(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Blank the quoted attribute values inside the tag starting at `open`
/// (indices up to `end`, i.e. the position of `>`).
fn mask_quoted_values(bytes: &[u8], mask: &mut [u8], open: usize, end: usize) {
    let mut i = open + 1;
    while i < end {
        let quote = bytes[i];
        if quote == b'"' || quote == b'\'' {
            let start = i;
            i += 1;
            while i < end && bytes[i] != quote {
                i += 1;
            }
            let stop = i.min(end);
            for b in &mut mask[start..stop] {
                *b = b' ';
            }
            if i < end {
                i += 1; // Skip the closing quote.
            }
        } else {
            i += 1;
        }
    }
}

/// Find a real tag `<name` in the mask: the match must end on a tag boundary
/// (whitespace, `/`, `>` or end of input) so `<head` does not match `<header>`.
fn find_tag(mask: &[u8], needle: &[u8]) -> Option<usize> {
    let mut i = 0;
    while let Some(start) = find_bytes(mask, i, needle) {
        let after = start + needle.len();
        let boundary = mask
            .get(after)
            .is_none_or(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/');
        if boundary {
            return Some(start);
        }
        i = after;
    }
    None
}

fn find_byte_from(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
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
                "script"
                    | "iframe"
                    | "frame"
                    | "frameset"
                    | "object"
                    | "embed"
                    | "applet"
                    | "form"
                    | "input"
                    | "button"
                    | "select"
                    | "textarea"
                    | "link"
                    | "meta"
                    | "base"
                    | "style"
                    | "svg"
                    | "math"
                    | "noscript"
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
                "b",
                "i",
                "u",
                "em",
                "strong",
                "a",
                "p",
                "br",
                "div",
                "span",
                "ul",
                "ol",
                "li",
                "blockquote",
                "pre",
                "code",
                "h1",
                "h2",
                "h3",
                "h4",
                "h5",
                "h6",
                "img",
                "table",
                "thead",
                "tbody",
                "tr",
                "td",
                "th",
                "hr",
                "small",
                "sub",
                "sup",
                "del",
                "ins",
                "mark",
                "cite",
                "q",
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
        while i < chars.len() && !chars[i].is_whitespace() && !matches!(chars[i], '=' | '>' | '/') {
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
                while cursor < chars.len() && !chars[cursor].is_whitespace() && chars[cursor] != '>'
                {
                    cursor += 1;
                }
                value = Some(chars[value_start..cursor].iter().collect());
            }
            i = cursor;
        }

        let lower = name.to_ascii_lowercase();
        let script_url = value.as_deref().is_some_and(is_dangerous_url);
        let dangerous = lower.starts_with("on")
            || (matches!(lower.as_str(), "href" | "src" | "action" | "formaction") && script_url);
        if dangerous {
            continue;
        }
        out.extend(chars[name_start..i].iter());
    }
    out
}

/// Decode the HTML character references that browsers accept in attribute
/// values: numeric (`&#115;`, `&#x73;`), a small set of named entities, and
/// the no-op forms browsers tolerate (NUL bytes, `&nbsp;`-style whitespace,
/// and HTML comments like `java<!-- -->script:`).
fn decode_entity(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // Expect `&[#]?name;` — a missing `;` is also tolerated by browsers.
        let mut j = i + 1;
        let numeric = j < chars.len() && chars[j] == '#';
        if numeric {
            j += 1;
        }
        let hex = numeric && j < chars.len() && matches!(chars[j], 'x' | 'X');
        if hex {
            j += 1;
        }
        let digits_start = j;
        // Consume only actual digits: decimal NCRs stop at a non-digit and
        // hex NCRs stop at a non-hex-digit — exactly how browsers tokenize.
        // Anything else (e.g. `&#58alert(1)`) is decoded as `:` + `alert(1)`,
        // which is what the receiving browser will render.
        while j < chars.len()
            && j - digits_start < 8
            && if hex {
                chars[j].is_ascii_hexdigit()
            } else {
                chars[j].is_ascii_digit()
            }
        {
            j += 1;
        }
        if j > digits_start {
            let had_semicolon = j < chars.len() && chars[j] == ';';
            let digits: String = chars[digits_start..j].iter().collect();
            let decoded = if numeric {
                let radix = if hex { 16 } else { 10 };
                u32::from_str_radix(&digits, radix)
                    .ok()
                    .and_then(char::from_u32)
            } else {
                decode_named_entity(&digits)
            };
            if let Some(ch) = decoded {
                out.push(ch);
                i = if had_semicolon { j + 1 } else { j };
                continue;
            }
        }
        // Not a recognised entity; keep the '&' verbatim.
        out.push('&');
        i += 1;
    }
    out
}

/// The handful of named entities that show up in scheme-smuggling payloads.
fn decode_named_entity(name: &str) -> Option<char> {
    match name.to_ascii_lowercase().as_str() {
        "tab" => Some('\t'),
        "newline" => Some('\n'),
        "sol" => Some('/'),
        "colon" => Some(':'),
        "amp" => Some('&'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => None,
    }
}

/// Strip characters browsers ignore when resolving a URL scheme (leading
/// control characters, embedded tabs/newlines, and comment markers).
fn normalize_url_scheme(value: &str) -> String {
    let decoded = decode_entity(value);
    // Remove anything that browsers strip before comparing the scheme:
    // ASCII control chars, whitespace, and comment delimiters.
    let mut cleaned = String::with_capacity(decoded.len());
    for ch in decoded.chars() {
        if ch.is_ascii_control() || ch == ' ' {
            continue;
        }
        cleaned.push(ch);
    }
    // Strip HTML comment markers that can be embedded mid-scheme.
    while let Some(start) = cleaned.find("<!--") {
        if let Some(end) = cleaned[start..].find("-->") {
            cleaned.replace_range(start..start + end + 3, "");
        } else {
            break;
        }
    }
    cleaned.trim().to_ascii_lowercase()
}

/// Return true when the attribute value resolves to a dangerous URL scheme.
fn is_dangerous_url(value: &str) -> bool {
    let normalized = normalize_url_scheme(value);
    normalized.starts_with("javascript:")
        || normalized.starts_with("vbscript:")
        || normalized.starts_with("data:")
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_gets_the_policy_and_default_styles() {
        let document = document(
            "<html><head><title>x</title></head><body>hi</body></html>",
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
        let blocked = document("<p>x</p>", false, &[]);
        assert!(blocked.contains("img-src data: cid:;"));
        let allowed = document("<p>x</p>", true, &[]);
        assert!(allowed.contains("img-src data: cid: http: https:;"));
        assert!(allowed.contains("#1e1e1e"));
    }

    #[test]
    fn fragments_are_wrapped_in_a_complete_document() {
        let document = document("<p>Hello</p>", false, &[]);
        assert!(document.starts_with("<!DOCTYPE html><html><head>"));
        assert!(document.contains("<body><p>Hello</p></body>"));
    }

    #[test]
    fn cid_images_become_data_urls() {
        let document = document(
            "<img src=\"cid:logo@example.com\"><img src=\"cid:other\">",
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
        let doc = document("<script>alert(1)</script>", false, &[]);
        assert!(doc.contains("script-src 'nonce-"));
        assert!(doc.contains("document.addEventListener('click'"));
        assert!(doc.contains("event.preventDefault()"));
    }

    #[test]
    fn csp_policy_blocks_forms_by_default() {
        let doc = document("<form action=\"/submit\"><input></form>", false, &[]);
        assert!(doc.contains("form-action 'none'"));
    }

    #[test]
    fn csp_policy_blocks_frames_by_default() {
        let doc = document("<iframe src=\"/frame\"></iframe>", false, &[]);
        assert!(doc.contains("frame-src 'none'"));
    }

    #[test]
    fn csp_policy_blocks_base_uri_by_default() {
        let doc = document("<base href=\"https://example.com/\">", false, &[]);
        assert!(doc.contains("base-uri 'none'"));
    }

    #[test]
    fn csp_policy_allows_inline_styles_only() {
        let doc = document("<div style=\"color: red;\"></div>", false, &[]);
        assert!(doc.contains("style-src 'unsafe-inline'"));
    }

    #[test]
    fn csp_policy_allows_data_fonts() {
        let doc = document(
            "<style>@font-face { src: url(data:font/woff2;base64,AAAA); }</style>",
            false,
            &[],
        );
        assert!(doc.contains("font-src data:"));
    }

    #[test]
    fn sanitizer_rejects_entity_encoded_javascript_urls() {
        // `&#115;` decodes to `s`, so a naive `starts_with` check misses it.
        let encoded = sanitize_html("<a href=\"java&#115;cript:alert(1)\">x</a>");
        assert!(
            !encoded.contains("javascript:") && !encoded.contains("java&#115;cript:"),
            "entity-encoded javascript URL survived: {encoded}"
        );

        let hex = sanitize_html("<a href=\"java&#x73;cript:alert(1)\">x</a>");
        assert!(
            !hex.contains("javascript:") && !hex.contains("&#x73;cript:"),
            "hex-encoded javascript URL survived: {hex}"
        );

        // A missing semicolon is tolerated by browsers and must be caught.
        let no_semicolon = sanitize_html("<a href=\"javascript&#58;alert(1)\">x</a>");
        assert!(
            !no_semicolon.contains("javascript&#58;"),
            "entity-encoded colon survived: {no_semicolon}"
        );
    }

    #[test]
    fn sanitizer_rejects_whitespace_and_comment_smuggling() {
        let tabbed = sanitize_html("<a href=\"java\tscript:alert(1)\">x</a>");
        assert!(
            !tabbed.contains("java\tscript:") && !tabbed.contains("javascript:"),
            "tab-smuggled URL survived: {tabbed}"
        );

        let commented = sanitize_html("<a href=\"java<!-- -->script:alert(1)\">x</a>");
        assert!(
            !commented.contains("java<!-- -->script:"),
            "comment-smuggled URL survived: {commented}"
        );

        let newline_entity = sanitize_html("<a href=\"java&#10;script:alert(1)\">x</a>");
        assert!(
            !newline_entity.contains("javascript:") && !newline_entity.contains("&#10;script:"),
            "newline-entity-smuggled URL survived: {newline_entity}"
        );
    }

    #[test]
    fn sanitizer_rejects_data_and_vbscript_schemes() {
        for payload in [
            "<a href=\"data:text/html,<script>alert(1)</script>\">x</a>",
            "<a href=\"vbscript:msgbox(1)\">x</a>",
            "<img src=\"vbscript:msgbox(1)\">",
        ] {
            let sanitized = sanitize_html(payload);
            let lower = sanitized.to_ascii_lowercase();
            assert!(
                !lower.contains("vbscript:") && !lower.contains("data:text/html"),
                "dangerous scheme survived for {payload}: {sanitized}"
            );
        }
    }

    #[test]
    fn sanitizer_keeps_safe_urls() {
        let safe = sanitize_html("<a href=\"https://example.com/a?b=1\">x</a>");
        assert!(safe.contains("href=\"https://example.com/a?b=1\""));
        let relative = sanitize_html("<a href=\"/relative/path\">x</a>");
        assert!(relative.contains("href=\"/relative/path\""));
    }

    #[test]
    fn csp_nonce_is_unique_per_document() {
        let doc1 = document("<p>test</p>", false, &[]);
        let doc2 = document("<p>test</p>", false, &[]);
        let nonce1 = extract_nonce(&doc1);
        let nonce2 = extract_nonce(&doc2);
        assert_ne!(nonce1, nonce2);
        assert!(nonce1.starts_with("mailin-"));
        assert!(nonce2.starts_with("mailin-"));
    }

    #[test]
    fn document_follows_the_system_color_scheme() {
        let doc = document("<p>test</p>", false, &[]);
        assert!(doc.contains("<meta name=\"color-scheme\" content=\"light dark\">"));
        assert!(doc.contains("light-dark(#ffffff, #1e1e1e)")); // background
        assert!(doc.contains("light-dark(#1c1c1c, #eeeeee)")); // foreground
        assert!(doc.contains("light-dark(#1c71d8, #78aeed)")); // link
    }

    #[test]
    fn document_always_has_doctype_and_html() {
        let doc = document("<p>Hello</p>", false, &[]);
        assert!(doc.starts_with("<!DOCTYPE html><html><head>"));
        assert!(doc.ends_with("</body></html>"));
    }

    #[test]
    fn document_head_contains_charset_meta() {
        let doc = document("<p>test</p>", false, &[]);
        assert!(doc.contains("<meta charset=\"utf-8\">"));
    }

    #[test]
    fn document_head_contains_referrer_policy() {
        let doc = document("<p>test</p>", false, &[]);
        assert!(doc.contains("<meta name=\"referrer\" content=\"no-referrer\">"));
    }

    fn extract_nonce(doc: &str) -> String {
        let start =
            doc.find("script-src 'nonce-").expect("nonce not found") + "script-src 'nonce-".len();
        let end = doc[start..].find("'").expect("nonce end not found");
        doc[start..start + end].to_owned()
    }

    // ------------------------- inject_head hardening -------------------------

    fn csp_position(doc: &str) -> usize {
        doc.find("Content-Security-Policy").expect("CSP present")
    }

    #[test]
    fn inject_head_ignores_head_inside_a_comment() {
        let doc = document("<body><!-- <head> decoy --><p>hello</p></body>", false, &[]);
        let csp = csp_position(&doc);
        let body = doc.find("<p>hello</p>").expect("body text");
        assert!(csp < body, "CSP must be injected before body content");
    }

    #[test]
    fn inject_head_ignores_head_inside_a_script() {
        let doc = document(
            "<html><body><script>var s = \"<head>\";</script><img src=\"https://tracker.example/x\"></body></html>",
            false,
            &[],
        );
        let csp = csp_position(&doc);
        let script = doc.find("<script>").expect("script tag");
        assert!(csp < script, "CSP must be injected before the script");
    }

    #[test]
    fn inject_head_does_not_treat_header_as_head() {
        let doc = document(
            "<html><body><img src=\"https://tracker.example/p.png\"><header>Hi</header></body></html>",
            false,
            &[],
        );
        let csp = csp_position(&doc);
        let img = doc.find("<img").expect("img");
        assert!(
            csp < img,
            "CSP must be present before content with remote resources"
        );
    }

    #[test]
    fn inject_head_ignores_head_inside_attribute_values() {
        let doc = document(
            "<div data-x=\"<head>\"><img src=\"https://tracker.example/x\"></div>",
            false,
            &[],
        );
        let csp = csp_position(&doc);
        let img = doc.find("<img").expect("img");
        assert!(csp < img, "CSP must be injected before body content");
    }

    #[test]
    fn inject_head_ignores_head_inside_textarea_and_title() {
        let doc = document(
            "<html><body><textarea><head></textarea><title><head></title></body></html>",
            false,
            &[],
        );
        let csp = csp_position(&doc);
        let textarea = doc.find("<textarea>").expect("textarea");
        assert!(csp < textarea);
    }

    #[test]
    fn inject_head_still_injects_after_real_head_with_attributes() {
        let doc = document(
            "<html><head profile=\"https://x.test\"><title>t</title></head><body>hi</body></html>",
            false,
            &[],
        );
        let head_open = doc.find("<head profile=").expect("head open tag");
        let head_close = doc[head_open..].find('>').expect("head close") + head_open + 1;
        let csp = csp_position(&doc);
        assert!(csp >= head_close && csp < head_close + 100);
    }

    #[test]
    fn inject_head_wraps_bare_fragments_in_a_document() {
        let doc = inject_head("<p>hi</p>", "<meta charset=\"utf-8\">");
        assert!(doc.starts_with("<!DOCTYPE html><html><head>"));
        assert!(doc.contains("<meta charset=\"utf-8\">"));
    }

    // ------------------------- has_remote_content widening -------------------------

    #[test]
    fn remote_content_detection_allows_whitespace_around_equals() {
        assert!(has_remote_content(
            "<img src = \"https://tracker.example/p.png\">"
        ));
        assert!(has_remote_content("<img src=\"//tracker.example/p.png\">"));
        assert!(has_remote_content(
            "<link href = \"https://x.test/app.css\">"
        ));
        assert!(has_remote_content(
            "<object data=\"https://x.test/a.svg\"></object>"
        ));
        assert!(has_remote_content(
            "<video poster = \"https://x.test/p.jpg\"></video>"
        ));
        assert!(has_remote_content(
            "<form action = \"https://x.test/collect\"></form>"
        ));
        assert!(has_remote_content(
            "<formaction formaction=\"https://x.test/x\"></formaction>"
        ));
        assert!(has_remote_content(
            "<style>body { background: url( https://x.test/x.png ) }</style>"
        ));
        assert!(has_remote_content(
            "<img srcset = \"local.png 1x, https://x.test/r.png 2x\">"
        ));
        assert!(!has_remote_content("<a href = \"//example.com\">link</a>"));
        assert!(!has_remote_content("<img src = \"cid:logo@example.com\">"));
        assert!(!has_remote_content("<p>See https://example.com</p>"));
    }

    #[test]
    fn remote_content_detection_handles_quoted_and_unquoted_values() {
        assert!(has_remote_content(
            "<img src=https://tracker.example/p.png>"
        ));
        assert!(has_remote_content(
            "<img src='https://tracker.example/p.png'>"
        ));
        assert!(has_remote_content(
            "<object data='//x.test/a.svg'></object>"
        ));
    }

    // ------------------------- dangerous-URL NCR handling -------------------------

    #[test]
    fn sanitizer_strips_unterminated_numeric_ncr_javascript_url() {
        let sanitized = sanitize_html("<a href=\"javascript&#58alert(1)\">x</a>");
        assert!(
            !sanitized.contains("javascript"),
            "unterminated NCR must not hide a javascript: URL: {sanitized}"
        );
        let sanitized = sanitize_html("<a href=\"javascript&#x3a;alert(1)\">x</a>");
        assert!(
            !sanitized.contains("javascript"),
            "unterminated hex NCR must not hide a javascript: URL: {sanitized}"
        );
    }

    #[test]
    fn dangerous_url_detection_matches_browser_ncr_tokenization() {
        assert!(is_dangerous_url("javascript&#58alert(1)"));
        assert!(is_dangerous_url("javascript&#58;alert(1)"));
        assert!(is_dangerous_url("javascript&#x3a;alert(1)"));
        assert!(is_dangerous_url("java&#115;cript:alert(1)"));
        assert!(is_dangerous_url("&#106;avascript:alert(1)"));
    }
}
