//! `mailto:` URI parsing shared by both front-ends.
//!
//! Both shells register Mailin as a default handler for `mailto:` links;
//! this module turns one into an [`OutgoingMessage`] draft so the composer
//! can be pre-filled regardless of how the link was activated.

use crate::compose::OutgoingMessage;

/// Parse a `mailto:` URI into a message draft.
///
/// Supports `mailto:addr?subject=...&body=...&cc=...&bcc=...`. A malformed
/// query component is skipped without discarding the parts that already
/// parsed.
pub fn parse_mailto_uri(uri: &str) -> Option<OutgoingMessage> {
    let uri = uri.strip_prefix("mailto:")?;
    let (addr_part, query_part) = uri.split_once('?').unwrap_or((uri, ""));
    let to = percent_decode(addr_part)?;
    let mut draft = OutgoingMessage {
        to,
        ..OutgoingMessage::default()
    };
    for pair in query_part.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let Some(value) = percent_decode(value) else {
            continue;
        };
        match key {
            "subject" => draft.subject = value,
            "body" => draft.body = value,
            "cc" => draft.cc = value,
            "bcc" => draft.bcc = value,
            _ => {}
        }
    }
    Some(draft)
}

/// Percent-decode a URI component, assembling bytes before interpreting them
/// as UTF-8 so multi-byte characters survive intact.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                let byte = u8::from_str_radix(hex, 16).ok()?;
                out.push(byte);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailto_parses_recipient_and_fields() {
        let draft = parse_mailto_uri("mailto:ada@example.com?subject=Hi&body=Hello+there&cc=a@b.c")
            .expect("parsed");
        assert_eq!(draft.to, "ada@example.com");
        assert_eq!(draft.subject, "Hi");
        assert_eq!(draft.body, "Hello there");
        assert_eq!(draft.cc, "a@b.c");
    }

    #[test]
    fn mailto_decodes_multibyte_utf8() {
        // Percent-encoded "Grüße" = Gr%C3%BC%C3%9Fe
        let draft =
            parse_mailto_uri("mailto:ada@example.com?subject=Gr%C3%BC%C3%9Fe").expect("parsed");
        assert_eq!(draft.subject, "Grüße");
    }

    #[test]
    fn mailto_keeps_fields_when_a_later_component_is_malformed() {
        // The trailing pair has no '='; the earlier fields must survive.
        let draft = parse_mailto_uri("mailto:ada@example.com?subject=Hi&broken").expect("parsed");
        assert_eq!(draft.subject, "Hi");
        assert_eq!(draft.to, "ada@example.com");
    }

    #[test]
    fn percent_decode_handles_plus_and_plain_text() {
        assert_eq!(percent_decode("a+b%20c").as_deref(), Some("a b c"));
    }
}
