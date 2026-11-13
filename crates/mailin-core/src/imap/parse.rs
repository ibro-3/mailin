//! Conversion between raw RFC 5322 bytes and Mailin models.
//!
//! Header blocks and complete messages both go through `mail-parser`, which
//! decodes RFC 2047 encoded words, folded headers and the usual MIME charsets.

use mail_parser::{Address, HeaderValue, Message, MessageParser, MimeHeaders};

use crate::model::{AttachmentData, AttachmentMeta, PgpMimeKind};

/// Fields extracted from a message (or a header block).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParsedHeader {
    pub subject: String,
    pub from_name: Option<String>,
    pub from_addr: Option<String>,
    pub to_addrs: String,
    pub cc_addrs: String,
    pub sent_at: Option<i64>,
    pub message_id: Option<String>,
    pub thread_key: Option<String>,
    pub references: Option<String>, // Full References header for thread depth
}

/// An OpenPGP/MIME payload found while parsing a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedPgp {
    pub kind: PgpMimeKind,
    pub payload: String,
    pub signature: Option<String>,
}

/// Body and attachment data of a complete message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParsedBody {
    pub text: Option<String>,
    pub html: Option<String>,
    pub snippet: Option<String>,
    pub attachments: Vec<AttachmentData>,
    pub pgp: Option<ParsedPgp>,
}

/// Parse a `BODY[HEADER]` block. Missing or malformed bytes degrade to an
/// empty header rather than failing the whole sync page.
pub(crate) fn parse_header(bytes: &[u8]) -> ParsedHeader {
    match MessageParser::default().parse(bytes) {
        Some(message) => header_from(&message),
        None => ParsedHeader::default(),
    }
}

/// Parse a complete message into header, body and attachment metadata.
pub(crate) fn parse_message(bytes: &[u8]) -> (ParsedHeader, ParsedBody) {
    let Some(message) = MessageParser::default().parse(bytes) else {
        return (ParsedHeader::default(), ParsedBody::default());
    };
    let header = header_from(&message);
    let mut body = body_from(&message);
    body.pgp = pgp_from_raw(bytes);
    (header, body)
}

/// Parse a standalone MIME entity (for example, decrypted OpenPGP content).
pub(crate) fn parse_entity(bytes: &[u8]) -> ParsedBody {
    match MessageParser::default().parse(bytes) {
        Some(message) => body_from(&message),
        None => ParsedBody::default(),
    }
}

fn body_from(message: &Message<'_>) -> ParsedBody {
    ParsedBody {
        text: message.body_text(0).map(|text| text.into_owned()),
        html: message.body_html(0).map(|html| html.into_owned()),
        snippet: message
            .body_preview(SNIPPET_LEN)
            .map(|snippet| snippet.into_owned()),
        attachments: attachments_from(message),
        pgp: None,
    }
}

/// Detect an OpenPGP/MIME wrapper and pull out the part the reader needs.
///
/// Encrypted messages keep the armored ciphertext; signed messages keep the
/// exact MIME entity plus the detached signature so it can be verified later.
fn pgp_from_raw(raw: &[u8]) -> Option<ParsedPgp> {
    let text = std::str::from_utf8(raw).ok()?;
    // Some paths produce LF-only messages, so accept both separators; the
    // original bytes are kept for payload extraction (a signature must verify
    // against the exact entity).
    let (headers, body) = split_headers_body(text)?;
    let lower = headers.to_ascii_lowercase();

    if lower.contains("multipart/encrypted") {
        let payload = armored_block(body, "MESSAGE")?;
        return Some(ParsedPgp {
            kind: PgpMimeKind::Encrypted,
            payload,
            signature: None,
        });
    }

    if lower.contains("multipart/signed") {
        let boundary = header_parameter(headers, "boundary")?;
        let marker = format!("--{boundary}");
        let mut parts = body.split(&marker);
        parts.next()?; // preamble
        let entity_part = parts.next()?;
        let entity = entity_part
            .strip_prefix("\r\n")
            .or_else(|| entity_part.strip_prefix('\n'))?
            .to_owned();
        let signature_part = parts.next()?;
        let signature = armored_block(signature_part, "SIGNATURE")?;
        return Some(ParsedPgp {
            kind: PgpMimeKind::Signed,
            payload: entity,
            signature: Some(signature),
        });
    }

    // PGP/MIME can also hide in a nested multipart part, for example when the
    // outer part carries the message and an inner part is multipart/encrypted.
    if lower.contains("multipart/")
        && let Some(boundary) = header_parameter(headers, "boundary")
    {
        let marker = format!("--{boundary}");
        for part in body.split(&marker) {
            let part = part.trim_start_matches(['\r', '\n']);
            let Some((part_headers, _)) = split_headers_body(part) else {
                continue;
            };
            let part_lower = part_headers.to_ascii_lowercase();
            if part_lower.contains("application/pgp-encrypted")
                || part_lower.contains("multipart/encrypted")
                || part_lower.contains("multipart/signed")
                || part_lower.contains("multipart/")
            {
                if let Some(pgp) = pgp_from_raw(part.as_bytes()) {
                    return Some(pgp);
                }
            }
        }
    }

    None
}

/// Split a message or part into its header block and body, accepting both
/// CRLF and LF line endings. Returns slices of the original text.
fn split_headers_body(text: &str) -> Option<(&str, &str)> {
    let crlf = text.find("\r\n\r\n").map(|index| (index, 4));
    let lf = text.find("\n\n").map(|index| (index, 2));
    match (crlf, lf) {
        (Some((index, separator)), None) => Some((&text[..index], &text[index + separator..])),
        (None, Some((index, separator))) => Some((&text[..index], &text[index + separator..])),
        (Some((a, _)), Some((b, _))) => {
            // Take whichever blank line comes first.
            if a <= b {
                let (index, separator) = crlf.unwrap();
                Some((&text[..index], &text[index + separator..]))
            } else {
                let (index, separator) = lf.unwrap();
                Some((&text[..index], &text[index + separator..]))
            }
        }
        (None, None) => None,
    }
}

/// Value of a `; name="value"` (or `; name=value`) MIME parameter.
fn header_parameter(headers: &str, name: &str) -> Option<String> {
    let lower = headers.to_ascii_lowercase();
    let start = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &headers[start..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_owned())
    } else {
        let end = rest.find([';', '\r', '\n']).unwrap_or(rest.len());
        Some(rest[..end].trim().to_owned())
    }
}

/// The armored block with the given `BEGIN PGP <label>` marker.
fn armored_block(text: &str, label: &str) -> Option<String> {
    let begin = format!("-----BEGIN PGP {label}-----");
    let end = format!("-----END PGP {label}-----");
    let start = text.find(&begin)?;
    let end_at = text[start..].find(&end)? + start + end.len();
    Some(text[start..end_at].to_owned())
}

const SNIPPET_LEN: usize = 160;

fn header_from(message: &Message<'_>) -> ParsedHeader {
    let (from_name, from_addr) = message
        .from()
        .and_then(Address::first)
        .map(|addr| {
            (
                addr.name().map(str::to_owned),
                addr.address().map(str::to_owned),
            )
        })
        .unwrap_or_default();

    // Extract full References header for thread depth computation
    let references = message.references().as_text_list().map(|list| {
        list.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    });

    ParsedHeader {
        subject: message.subject().unwrap_or_default().to_owned(),
        from_name,
        from_addr,
        to_addrs: format_addresses(message.to()),
        cc_addrs: format_addresses(message.cc()),
        sent_at: message.date().map(|date| date.to_timestamp()),
        message_id: message.message_id().map(str::to_owned),
        thread_key: thread_key(message),
        references,
    }
}

fn format_addresses(address: Option<&Address<'_>>) -> String {
    let Some(address) = address else {
        return String::new();
    };
    address
        .iter()
        .map(|addr| match (addr.name(), addr.address()) {
            (Some(name), Some(mail)) if !name.is_empty() => format!("{name} <{mail}>"),
            (Some(name), None) => name.to_owned(),
            (_, Some(mail)) => mail.to_owned(),
            (None, None) => String::new(),
        })
        .filter(|formatted| !formatted.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Root message id of a reply chain, falling back to the direct parent. This is
/// a cheap subject-independent conversation key.
fn thread_key(message: &Message<'_>) -> Option<String> {
    first_message_id(message.references()).or_else(|| first_message_id(message.in_reply_to()))
}

fn first_message_id(value: &HeaderValue<'_>) -> Option<String> {
    for item in value.as_text_list()? {
        for token in item.split_whitespace() {
            let trimmed = token.trim_matches(|c| c == '<' || c == '>');
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

fn attachments_from(message: &Message<'_>) -> Vec<AttachmentData> {
    message
        .attachments()
        .enumerate()
        .map(|(index, part)| {
            let mime_type = part
                .content_type()
                .map(|content_type| match &content_type.c_subtype {
                    Some(subtype) => format!("{}/{}", content_type.c_type, subtype),
                    None => content_type.c_type.to_string(),
                })
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            let meta = AttachmentMeta {
                part_id: (index + 1).to_string(),
                filename: part.attachment_name().map(str::to_owned),
                mime_type,
                size: u32::try_from(part.len()).ok(),
                content_id: part.content_id().map(strip_angle_brackets),
                is_inline: part
                    .content_disposition()
                    .is_some_and(|disposition| disposition.is_inline()),
            };
            let data = (!part.is_multipart()).then(|| part.contents().to_vec());
            AttachmentData { meta, data }
        })
        .collect()
}

fn strip_angle_brackets(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &[u8] = b"From: Ada Lovelace <ada@example.com>\r\n\
        To: Grace Hopper <grace@example.com>, bob@example.com\r\n\
        Cc: carol@example.com\r\n\
        Subject: =?UTF-8?B?SMOpbGxv?=\r\n\
        Date: Tue, 1 Jul 2025 10:30:00 +0200\r\n\
        Message-ID: <m1@example.com>\r\n\
        In-Reply-To: <root@example.com>\r\n\
        References: <root@example.com> <parent@example.com>\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        \r\n\
        Hello world, this is the body.\r\n";

    const MULTIPART: &[u8] = b"From: Ada <ada@example.com>\r\n\
        To: Grace <grace@example.com>\r\n\
        Subject: Report\r\n\
        Date: Tue, 1 Jul 2025 10:30:00 +0000\r\n\
        Message-ID: <m2@example.com>\r\n\
        MIME-Version: 1.0\r\n\
        Content-Type: multipart/mixed; boundary=\"outer\"\r\n\
        \r\n\
        --outer\r\n\
        Content-Type: multipart/alternative; boundary=\"inner\"\r\n\
        \r\n\
        --inner\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        \r\n\
        Plain text body\r\n\
        --inner\r\n\
        Content-Type: text/html; charset=utf-8\r\n\
        \r\n\
        <p>HTML body</p>\r\n\
        --inner--\r\n\
        --outer\r\n\
        Content-Type: application/pdf; name=\"report.pdf\"\r\n\
        Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
        Content-Transfer-Encoding: base64\r\n\
        \r\n\
        JVBERi0xLjQK\r\n\
        --outer--\r\n";

    #[test]
    fn parses_decoded_headers_and_thread_root() {
        let header = parse_header(PLAIN);
        assert_eq!(header.subject, "Héllo");
        assert_eq!(header.from_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(header.from_addr.as_deref(), Some("ada@example.com"));
        assert_eq!(
            header.to_addrs,
            "Grace Hopper <grace@example.com>, bob@example.com"
        );
        assert_eq!(header.cc_addrs, "carol@example.com");
        assert_eq!(header.message_id.as_deref(), Some("m1@example.com"));
        assert_eq!(header.thread_key.as_deref(), Some("root@example.com"));
        assert!(header.sent_at.is_some());
    }

    fn sample_message(body: &str) -> crate::compose::OutgoingMessage {
        crate::compose::OutgoingMessage {
            from_name: "Alice".into(),
            from_addr: "alice@example.com".into(),
            to: "Bob <bob@example.com>".into(),
            subject: "PGP".into(),
            body: body.into(),
            ..crate::compose::OutgoingMessage::default()
        }
    }

    #[test]
    fn extracts_encrypted_payload() {
        let (raw, _) = sample_message("secret").build().expect("build");
        let wrapped = crate::compose::wrap_encrypted(
            &raw,
            "-----BEGIN PGP MESSAGE-----\r\nAAAA\r\n-----END PGP MESSAGE-----",
        )
        .expect("wrap");
        let (_, body) = parse_message(&wrapped);
        let pgp = body.pgp.expect("pgp payload");
        assert_eq!(pgp.kind, PgpMimeKind::Encrypted);
        assert!(pgp.payload.contains("BEGIN PGP MESSAGE"));
        assert!(pgp.signature.is_none());
    }

    #[test]
    fn extracts_signed_entity_and_signature() {
        let (raw, _) = sample_message("signed body").build().expect("build");
        let wrapped = crate::compose::wrap_signed(
            &raw,
            "-----BEGIN PGP SIGNATURE-----\r\nBBBB\r\n-----END PGP SIGNATURE-----",
        )
        .expect("wrap");
        let (_, body) = parse_message(&wrapped);
        let pgp = body.pgp.expect("pgp payload");
        assert_eq!(pgp.kind, PgpMimeKind::Signed);
        assert!(pgp.payload.contains("Content-Type: text/plain"));
        assert!(!pgp.payload.contains("Subject:"));
        assert!(
            pgp.signature
                .expect("signature")
                .contains("BEGIN PGP SIGNATURE")
        );
    }

    #[test]
    fn extracts_encrypted_payload_with_lf_only_endings() {
        let wrapped = concat!(
            "Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"abc\"\n",
            "\n",
            "--abc\n",
            "Content-Type: application/pgp-encrypted\n",
            "\n",
            "Version: 1\n",
            "--abc\n",
            "Content-Type: application/octet-stream; name=\"encrypted.asc\"\n",
            "\n",
            "-----BEGIN PGP MESSAGE-----\n",
            "AAAA\n",
            "-----END PGP MESSAGE-----\n",
            "--abc--"
        );
        let (_, body) = parse_message(wrapped.as_bytes());
        let pgp = body.pgp.expect("pgp payload");
        assert_eq!(pgp.kind, PgpMimeKind::Encrypted);
        assert!(pgp.payload.contains("BEGIN PGP MESSAGE"));
    }

    #[test]
    fn pgp_inside_a_nested_part_is_detected() {
        // The encrypted entity hides one level below the outer multipart.
        let inner = concat!(
            "Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"pgp\"\r\n",
            "\r\n",
            "--pgp\r\n",
            "Content-Type: application/pgp-encrypted\r\n",
            "\r\n",
            "Version: 1\r\n",
            "--pgp\r\n",
            "Content-Type: application/octet-stream\r\n",
            "\r\n",
            "-----BEGIN PGP MESSAGE-----\r\n",
            "BBBB\r\n",
            "-----END PGP MESSAGE-----\r\n",
            "--pgp--"
        );
        let wrapped = format!(
            "Content-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n--outer\r\n\
             Content-Type: multipart/mixed; boundary=\"pgp-inner\"\r\n\r\n{inner}\r\n--outer--"
        );
        // Rebind the inner boundary so the nested scan can find it.
        let wrapped = wrapped.replace("boundary=\"pgp\"", "boundary=\"pgp-inner\"");
        let (_, body) = parse_message(wrapped.as_bytes());
        let pgp = body.pgp.expect("nested pgp payload");
        assert_eq!(pgp.kind, PgpMimeKind::Encrypted);
        assert!(pgp.payload.contains("BEGIN PGP MESSAGE"));
    }

    #[test]
    fn parses_bodies_and_attachments() {
        let (header, body) = parse_message(MULTIPART);
        assert_eq!(header.subject, "Report");
        assert_eq!(body.text.as_deref(), Some("Plain text body"));
        assert!(body.html.as_deref().unwrap_or("").contains("HTML body"));
        assert_eq!(body.attachments.len(), 1);
        let attachment = &body.attachments[0];
        assert_eq!(attachment.meta.filename.as_deref(), Some("report.pdf"));
        assert_eq!(attachment.meta.mime_type, "application/pdf");
        assert!(!attachment.meta.is_inline);
        assert!(attachment.meta.size.is_some());
        assert_eq!(attachment.data.as_deref(), Some(b"%PDF-1.4\n".as_slice()));
        assert!(body.snippet.as_deref().unwrap_or("").contains("Plain text"));
    }

    #[test]
    fn inline_images_keep_their_content_id_and_bytes() {
        let raw = b"From: a@example.com\r\n\
            Subject: Inline\r\n\
            MIME-Version: 1.0\r\n\
            Content-Type: multipart/related; boundary=\"b\"\r\n\
            \r\n\
            --b\r\n\
            Content-Type: text/html; charset=utf-8\r\n\
            \r\n\
            <p><img src=\"cid:logo@example.com\"></p>\r\n\
            --b\r\n\
            Content-Type: image/png\r\n\
            Content-ID: <logo@example.com>\r\n\
            Content-Disposition: inline\r\n\
            Content-Transfer-Encoding: base64\r\n\
            \r\n\
            iVBORw0KGgo=\r\n\
            --b--\r\n";
        let (_, body) = parse_message(raw);
        assert_eq!(body.attachments.len(), 1);
        let image = &body.attachments[0];
        assert_eq!(image.meta.content_id.as_deref(), Some("logo@example.com"));
        assert!(image.meta.is_inline);
        assert_eq!(image.data.as_deref(), Some(b"\x89PNG\r\n\x1a\n".as_slice()));
    }

    #[test]
    fn malformed_input_degrades_to_empty_header() {
        let header = parse_header(&[0xff, 0xfe, 0x00, 0x00]);
        assert_eq!(header, ParsedHeader::default());
    }

    #[test]
    fn html_only_message_gets_a_text_preview() {
        let raw = b"From: a@example.com\r\n\
            Subject: HTML only\r\n\
            Content-Type: text/html; charset=utf-8\r\n\
            \r\n\
            <html><body><p>Hello <b>there</b></p></body></html>\r\n";
        let (_, body) = parse_message(raw);
        assert!(body.text.as_deref().unwrap_or("").contains("Hello"));
        assert!(body.snippet.as_deref().unwrap_or("").contains("Hello"));
    }
}
