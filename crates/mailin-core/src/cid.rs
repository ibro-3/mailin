//! Accurate detection and pruning of `cid:` image references.
//!
//! Inline images are referenced from the HTML body as `cid:<content-id>`.
//! When the referenced part is missing, the reader has nothing to show and
//! the composer must not ship a broken reference. Both front-ends share the
//! scanner in this module so the two stay in step.

use crate::html::{attr_value_span, find_bytes, scan_tag_end, structural_mask};

/// The `cid:` scheme prefix, compared case-insensitively.
const CID_PREFIX: &str = "cid:";

/// The byte ranges of every `<img …>` element in `html`.
///
/// The range covers the whole element including the angle brackets.
/// Comments and raw-text elements (`<script>`, `<style>`, …) are skipped,
/// so markup that only looks like an image is not mistaken for one.
fn img_elements(html: &str) -> Vec<(usize, usize)> {
    let bytes = html.as_bytes();
    let mask = structural_mask(bytes);
    let mut elements = Vec::new();
    let mut cursor = 0;
    while let Some(start) = find_tag_from(&mask, cursor, b"<img") {
        // `find_tag_from` already checks the boundary after `<img`.
        let end = scan_tag_end(bytes, start);
        let end = (end + 1).min(bytes.len());
        elements.push((start, end));
        cursor = end.max(start + 1);
    }
    elements
}

/// Like [`crate::html::find_tag`] but resumes at `from`.
fn find_tag_from(mask: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let mut i = from;
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

/// The values of the `attr` attribute on `tag`, in document order.
///
/// Attribute names are matched case-insensitively and must not be a suffix
/// of a longer name (`data-src` does not match `src`).
fn attr_values(tag: &str, attr: &str) -> Vec<String> {
    let bytes = tag.as_bytes();
    let attr_bytes = attr.as_bytes();
    // Attribute names are case-insensitive; the lowercase copy has the same
    // byte offsets as the original, so values can be read from `bytes`.
    let lower = tag.to_ascii_lowercase();
    let mut values = Vec::new();
    let mut i = 0;
    while let Some(rel) = find_bytes(lower.as_bytes(), i, attr_bytes) {
        let left_ok = rel == 0
            || !(bytes[rel - 1].is_ascii_alphanumeric()
                || bytes[rel - 1] == b'-'
                || bytes[rel - 1] == b'_');
        let after = rel + attr_bytes.len();
        if left_ok {
            // The attribute name must not be followed by more name characters.
            let right_ok = !bytes
                .get(after)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_');
            if right_ok {
                if let Some((start, end)) = attr_value_span(bytes, after) {
                    values.push(String::from_utf8_lossy(&bytes[start..end]).into_owned());
                }
            }
        }
        i = rel + attr_bytes.len();
    }
    values
}

/// The content id of a `cid:` URL, if the value is one.
///
/// The prefix is matched case-insensitively and the id may be wrapped in
/// angle brackets (`cid:<id>`) as some mailers write it.
fn cid_content_id(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.len() < CID_PREFIX.len()
        || !trimmed[..CID_PREFIX.len()].eq_ignore_ascii_case(CID_PREFIX)
    {
        return None;
    }
    let id = trimmed[CID_PREFIX.len()..].trim();
    let id = id.trim_start_matches('<').trim_end_matches('>').trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_owned())
    }
}

/// The `cid:` references used by the `<img>` elements in `html`.
///
/// Duplicates are reported once, in document order.
pub fn references(html: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (start, end) in img_elements(html) {
        for id in element_cids(&html[start..end]) {
            if !found.contains(&id) {
                found.push(id);
            }
        }
    }
    found
}

/// The content ids referenced by one `<img>` element.
fn element_cids(tag: &str) -> Vec<String> {
    let mut values = attr_values(tag, "src");
    values.extend(attr_values(tag, "srcset"));
    let mut ids = Vec::new();
    for value in values {
        // `srcset` holds comma-separated candidates; each may carry its own
        // URL.
        for candidate in value.split(',') {
            let url = candidate.split_whitespace().next().unwrap_or_default();
            if let Some(id) = cid_content_id(url) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Whether one `<img>` element references a `cid:` that is not in `known`.
fn element_is_unresolved(tag: &str, known: &[String]) -> bool {
    element_cids(tag)
        .iter()
        .any(|id| !known.iter().any(|entry| entry == id))
}

/// The number of `<img>` elements whose `cid:` reference is not in `known`.
pub fn unresolved_count(html: &str, known: &[String]) -> usize {
    img_elements(html)
        .iter()
        .filter(|(start, end)| element_is_unresolved(&html[*start..*end], known))
        .count()
}

/// Remove the `<img>` elements whose `cid:` reference is not in `known`.
///
/// Elements that do not use the `cid:` scheme are left untouched.
pub fn prune_unresolved_images(html: &str, known: &[String]) -> String {
    let elements = img_elements(html);
    if elements.is_empty() {
        return html.to_owned();
    }
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0;
    for (start, end) in elements {
        let tag = &html[start..end];
        if element_is_unresolved(tag, known) {
            out.push_str(&html[cursor..start]);
        } else {
            out.push_str(&html[cursor..end]);
        }
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn references_are_collected_from_src_and_srcset() {
        let html = r#"<p><img src="cid:one@mailin.local"></p><img srcset="cid:two@mailin.local 1x, cid:three@mailin.local 2x">"#;
        assert_eq!(
            references(html),
            ids(&["one@mailin.local", "two@mailin.local", "three@mailin.local"])
        );
    }

    #[test]
    fn the_scheme_is_case_insensitive_and_brackets_are_ignored() {
        let html = r#"<img SRC="CID:<four@mailin.local>">"#;
        assert_eq!(references(html), ids(&["four@mailin.local"]));
    }

    #[test]
    fn unrelated_images_are_ignored() {
        let html =
            r#"<img src="https://example.test/a.png"><img src="data:image/png;base64,AA"><img>"#;
        assert!(references(html).is_empty());
    }

    #[test]
    fn data_src_attributes_are_not_mistaken_for_src() {
        let html = r#"<img data-src="cid:five@mailin.local" src="cid:six@mailin.local">"#;
        assert_eq!(references(html), ids(&["six@mailin.local"]));
    }

    #[test]
    fn comments_and_text_do_not_create_references() {
        let html = r#"<!-- <img src="cid:commented@mailin.local"> --><p>cid:text@mailin.local</p>"#;
        assert!(references(html).is_empty());
    }

    #[test]
    fn script_contents_do_not_create_references() {
        let html = r#"<script>var x = '<img src="cid:scripted@mailin.local">';</script>"#;
        assert!(references(html).is_empty());
    }

    #[test]
    fn unresolved_images_are_pruned_and_others_kept() {
        let html = r#"<p>a</p><img src="cid:known@mailin.local"><img src="cid:missing@mailin.local"><img src="https://example.test/x.png">"#;
        let pruned = prune_unresolved_images(html, &ids(&["known@mailin.local"]));
        assert!(pruned.contains("cid:known@mailin.local"));
        assert!(!pruned.contains("cid:missing@mailin.local"));
        assert!(pruned.contains("https://example.test/x.png"));
        assert!(pruned.contains("<p>a</p>"));
    }

    #[test]
    fn pruning_keeps_the_document_intact_when_everything_resolves() {
        let html = r#"<img src="cid:known@mailin.local">"#;
        assert_eq!(
            prune_unresolved_images(html, &ids(&["known@mailin.local"])),
            html
        );
    }

    #[test]
    fn unresolved_images_are_counted() {
        let html = r#"<img src="cid:known@mailin.local"><img src="cid:missing@mailin.local">"#;
        assert_eq!(unresolved_count(html, &ids(&["known@mailin.local"])), 1);
        assert_eq!(
            unresolved_count(html, &ids(&["known@mailin.local", "missing@mailin.local"])),
            0
        );
    }
}
