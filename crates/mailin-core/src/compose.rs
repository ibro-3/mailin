//! Building outgoing RFC 5322 messages.
//!
//! The composer deals with what the user typed, not with MIME minutiae: it
//! parses address lists, encodes non-ASCII headers, wraps the body in base64
//! and turns attachments into a `multipart/mixed` message. The SMTP envelope is
//! returned separately because `Bcc` recipients must not appear in headers.

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::error::{Error, Result};
use crate::model::{MessageHeader, OutgoingEnvelope};

/// A file attached to an outgoing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeAttachment {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

impl ComposeAttachment {
    pub fn new(filename: impl Into<String>, mime_type: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            filename: filename.into(),
            mime_type: mime_type.into(),
            data,
        }
    }
}

/// Everything needed to build an outgoing message. Address lists accept the
/// usual `Name <address>, other@example.com` syntax; semicolons are accepted as
/// separators too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub from_name: String,
    pub from_addr: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    /// Message IDs of the conversation, used for threading by receiving
    /// clients. Values may be given with or without angle brackets.
    pub references: Vec<String>,
    /// Stable RFC 5322 Message-ID for a draft. Reusing it lets the server-side
    /// draft be replaced instead of duplicated on every auto-save. Empty for
    /// messages that are about to be sent.
    pub draft_key: Option<String>,
    pub attachments: Vec<ComposeAttachment>,
}

impl OutgoingMessage {
    /// Build the wire message and its SMTP envelope.
    ///
    /// Header values are stripped of line breaks, so a malicious or accidental
    /// newline in a subject cannot inject headers.
    pub fn build(&self) -> Result<(Vec<u8>, OutgoingEnvelope)> {
        self.build_inner(true)
    }

    /// Build a draft that may have no recipients yet. Drafts are appended to
    /// the Drafts folder rather than transmitted, so the envelope is not
    /// validated.
    pub fn build_draft(&self) -> Result<Vec<u8>> {
        self.build_inner(false).map(|(raw, _)| raw)
    }

    fn build_inner(&self, require_recipients: bool) -> Result<(Vec<u8>, OutgoingEnvelope)> {
        let from = parse_single(&self.from_name, &self.from_addr)?;
        let to = parse_mailboxes(&self.to)?;
        let cc = parse_mailboxes(&self.cc)?;
        let bcc = parse_mailboxes(&self.bcc)?;

        let envelope = OutgoingEnvelope {
            from: from.address.clone(),
            to: to.iter().map(|mailbox| mailbox.address.clone()).collect(),
            cc: cc.iter().map(|mailbox| mailbox.address.clone()).collect(),
            bcc: bcc.iter().map(|mailbox| mailbox.address.clone()).collect(),
        };
        if require_recipients {
            envelope.validate()?;
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
            .unwrap_or_default();
        let date = format_rfc2822(timestamp);
        let domain = from
            .address
            .split_once('@')
            .map_or("mailin.local", |(_, domain)| domain);
        let message_id = self
            .draft_key
            .as_deref()
            .map(message_id_header)
            .filter(|value| !value.is_empty())
            .map(|value| value.trim_matches(['<', '>']).to_owned())
            .unwrap_or_else(|| format!("{}@{}", uuid::Uuid::new_v4(), domain));

        let mut message = String::with_capacity(1024 + self.body.len());
        write_header(&mut message, "Date", &date);
        write_header(&mut message, "From", &format_mailbox(&from));
        write_header(&mut message, "To", &format_mailboxes(&to));
        if !cc.is_empty() {
            write_header(&mut message, "Cc", &format_mailboxes(&cc));
        }
        // A Bcc header would leak the hidden recipients to everybody else; the
        // envelope above is the only place they belong.
        write_header(&mut message, "Subject", &encode_unstructured(&self.subject));
        write_header(&mut message, "Message-ID", &format!("<{message_id}>"));
        write_header(&mut message, "MIME-Version", "1.0");
        if let Some(parent) = self
            .in_reply_to
            .as_deref()
            .map(message_id_header)
            .filter(|value| !value.is_empty())
        {
            write_header(&mut message, "In-Reply-To", &parent);
        }
        let references = self
            .references
            .iter()
            .map(|id| message_id_header(id))
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>();
        if !references.is_empty() {
            write_header(&mut message, "References", &references.join(" "));
        }

        let body = normalize_crlf(&self.body);
        if self.attachments.is_empty() {
            write_header(&mut message, "Content-Type", "text/plain; charset=utf-8");
            write_header(&mut message, "Content-Transfer-Encoding", "base64");
            message.push_str("\r\n");
            write_base64(&mut message, body.as_bytes());
        } else {
            let boundary = format!("mailin-{}", uuid::Uuid::new_v4().simple());
            write_header(
                &mut message,
                "Content-Type",
                &format!("multipart/mixed; boundary=\"{boundary}\""),
            );
            message.push_str("\r\n");
            message.push_str("This is a multi-part message in MIME format.\r\n");

            message.push_str(&format!("--{boundary}\r\n"));
            message.push_str("Content-Type: text/plain; charset=utf-8\r\n");
            message.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
            write_base64(&mut message, body.as_bytes());

            for attachment in &self.attachments {
                message.push_str(&format!("--{boundary}\r\n"));
                let mime_type = sanitize_mime_type(&attachment.mime_type);
                message.push_str(&format!("Content-Type: {mime_type}\r\n"));
                message.push_str("Content-Transfer-Encoding: base64\r\n");
                message.push_str(&format!(
                    "Content-Disposition: attachment; {}\r\n\r\n",
                    filename_parameter(&attachment.filename)
                ));
                write_base64(&mut message, &attachment.data);
            }
            message.push_str(&format!("--{boundary}--\r\n"));
        }

        Ok((message.into_bytes(), envelope))
    }
}

/// RFC 3676 signature separator line.
pub const SIGNATURE_SEPARATOR: &str = "-- ";

/// The account signature as an isolated block, or an empty string when the
/// signature is blank. The `-- ` separator is part of the block.
pub fn signature_block(signature: &str) -> String {
    let signature = signature.trim();
    if signature.is_empty() {
        String::new()
    } else {
        format!("{SIGNATURE_SEPARATOR}\n{signature}")
    }
}

/// Place the signature above a prefilled body (reply quotation or forwarded
/// headers), separated by one blank line. A blank signature leaves `body`
/// untouched.
pub fn with_signature_prefix(signature: &str, body: &str) -> String {
    let block = signature_block(signature);
    if block.is_empty() {
        return body.to_owned();
    }
    format!("{block}\n\n{}", body.trim_start_matches('\n'))
}

/// Rebuild a composer draft from a stored Drafts-folder message. The Message-ID
/// becomes the draft key so the next save replaces the same server message.
pub fn draft_from_header(header: &MessageHeader, body: Option<&str>) -> OutgoingMessage {
    OutgoingMessage {
        from_name: header.from_name.clone().unwrap_or_default(),
        from_addr: header.from_addr.clone().unwrap_or_default(),
        to: header.to_addrs.clone(),
        cc: header.cc_addrs.clone(),
        bcc: String::new(),
        subject: header.subject.clone(),
        body: body.unwrap_or_default().to_owned(),
        in_reply_to: None,
        references: Vec::new(),
        draft_key: header.message_id.clone(),
        attachments: Vec::new(),
    }
}

/// A fresh Message-ID used to replace a draft on later auto-saves.
pub fn new_draft_key(from_addr: &str) -> String {
    let domain = from_addr
        .split_once('@')
        .map_or("mailin.local", |(_, domain)| domain);
    format!("{}@{}", uuid::Uuid::new_v4(), domain)
}

/// Substitute `{{variable}}` placeholders in a template string.
/// Variables are provided as a HashMap. Unknown variables are left as-is.
pub fn apply_template(template: &str, vars: &std::collections::HashMap<&str, &str>) -> String {
    let mut result = template.to_owned();
    for (key, value) in vars {
        let placeholder = format!("{{{{{key}}}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

/// Create an OutgoingMessage from a Template with variable substitution.
pub fn template_draft(template: &crate::model::Template, vars: &std::collections::HashMap<&str, &str>) -> OutgoingMessage {
    OutgoingMessage {
        from_name: String::new(),
        from_addr: String::new(),
        to: String::new(),
        cc: String::new(),
        bcc: String::new(),
        subject: apply_template(&template.subject, vars),
        body: apply_template(&template.body, vars),
        in_reply_to: None,
        references: Vec::new(),
        draft_key: Some(new_draft_key("template")),
        attachments: Vec::new(),
    }
}

/// Parse an address list into bare addresses. Front-ends use this to build
/// replies without duplicating the address syntax rules.
pub fn recipient_addresses(input: &str) -> Result<Vec<String>> {
    Ok(parse_mailboxes(input)?
        .into_iter()
        .map(|mailbox| mailbox.address)
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mailbox {
    name: Option<String>,
    address: String,
}

fn parse_single(name: &str, address: &str) -> Result<Mailbox> {
    let address = address.trim();
    let parsed = address
        .parse::<lettre::address::Address>()
        .map_err(|error| Error::Protocol(format!("invalid sender address {address:?}: {error}")))?;
    let name = name.trim();
    Ok(Mailbox {
        name: (!name.is_empty()).then(|| name.to_owned()),
        address: parsed.to_string(),
    })
}

/// Split on commas or semicolons that are not inside quotes or angle brackets.
fn split_list(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut in_angle = false;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            '<' if !in_quotes => in_angle = true,
            '>' if !in_quotes => in_angle = false,
            ',' | ';' if !in_quotes && !in_angle => {
                parts.push(&input[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&input[start..]);
    parts
}

fn parse_mailboxes(input: &str) -> Result<Vec<Mailbox>> {
    let mut mailboxes = Vec::new();
    for token in split_list(input) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let (name, address) = match token.rsplit_once('<') {
            Some((name, rest)) if rest.trim_end().ends_with('>') => {
                let address = rest.trim_end().trim_end_matches('>').trim();
                (Some(unquote(name.trim())), address)
            }
            _ => (None, token),
        };
        let address = address.trim();
        if address.contains(char::is_whitespace) {
            return Err(Error::Protocol(format!(
                "invalid recipient address {address:?}: contains whitespace"
            )));
        }
        let parsed = address
            .parse::<lettre::address::Address>()
            .map_err(|error| {
                Error::Protocol(format!("invalid recipient address {address:?}: {error}"))
            })?;
        mailboxes.push(Mailbox {
            name: name.filter(|name| !name.is_empty()),
            address: parsed.to_string(),
        });
    }
    Ok(mailboxes)
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        value.to_owned()
    }
}

fn format_mailbox(mailbox: &Mailbox) -> String {
    match &mailbox.name {
        Some(name) if !name.trim().is_empty() => {
            format!("{} <{}>", encode_phrase(name), mailbox.address)
        }
        _ => mailbox.address.clone(),
    }
}

fn format_mailboxes(mailboxes: &[Mailbox]) -> String {
    mailboxes
        .iter()
        .map(format_mailbox)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Encode a display name as a quoted phrase or an RFC 2047 encoded word.
fn encode_phrase(name: &str) -> String {
    let clean = strip_line_breaks(name);
    if !clean.is_ascii() {
        return encode_word(&clean);
    }
    format!("\"{}\"", clean.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Encode an unstructured header such as `Subject`.
fn encode_unstructured(value: &str) -> String {
    let clean = strip_line_breaks(value);
    if clean.is_ascii() {
        return clean;
    }
    // RFC 2047 encoded words are limited to 75 characters. Splitting the input
    // on character boundaries keeps every resulting word below that limit for
    // the 4/3 base64 expansion of UTF-8 text.
    let mut words = Vec::new();
    let mut chunk = String::new();
    for character in clean.chars() {
        if chunk.len() + character.len_utf8() > 45 {
            words.push(encode_word(&chunk));
            chunk.clear();
        }
        chunk.push(character);
    }
    if !chunk.is_empty() {
        words.push(encode_word(&chunk));
    }
    words.join(" ")
}

fn encode_word(value: &str) -> String {
    format!("=?UTF-8?B?{}?=", STANDARD.encode(value.as_bytes()))
}

fn strip_line_breaks(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character == '\r' || character == '\n' || character == '\0' {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn message_id_header(value: &str) -> String {
    let clean = strip_line_breaks(value);
    let clean = clean.trim().trim_start_matches('<').trim_end_matches('>');
    if clean.is_empty() {
        String::new()
    } else {
        format!("<{clean}>")
    }
}

fn write_header(output: &mut String, name: &str, value: &str) {
    let _ = write!(output, "{name}: {value}\r\n");
}

fn sanitize_mime_type(value: &str) -> String {
    let clean = value.trim();
    let valid = !clean.is_empty()
        && clean
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/+-.".contains(character));
    if valid {
        clean.to_owned()
    } else {
        "application/octet-stream".to_owned()
    }
}

fn filename_parameter(filename: &str) -> String {
    let clean = strip_line_breaks(filename);
    let clean = if clean.trim().is_empty() {
        "attachment"
    } else {
        clean.trim()
    };
    let ascii = clean
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "._- ".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let mut encoded = String::new();
    for byte in clean.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-' | b'~') {
            encoded.push(*byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    format!("filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

fn write_base64(output: &mut String, data: &[u8]) {
    let encoded = STANDARD.encode(data);
    for chunk in encoded.as_bytes().chunks(76) {
        // Base64 output is always ASCII.
        output.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        output.push_str("\r\n");
    }
}

fn normalize_crlf(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() != Some(&'\n') {
                    output.push_str("\r\n");
                }
            }
            '\n' => output.push_str("\r\n"),
            _ => output.push(character),
        }
    }
    output
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// RFC 2822 date in UTC, e.g. `Tue, 14 Nov 2023 22:13:20 +0000`.
pub(crate) fn format_rfc2822(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let weekday = WEEKDAYS[days.rem_euclid(7) as usize];
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    format!(
        "{weekday}, {day:02} {} {year:04} {hour:02}:{minute:02}:{second:02} +0000",
        MONTHS[(month - 1) as usize]
    )
}

/// Howard Hinnant's civil-from-days algorithm, valid for the whole i64 range
/// that matters here.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> OutgoingMessage {
        OutgoingMessage {
            from_name: "Ada Lovelace".into(),
            from_addr: "ada@example.com".into(),
            to: "Grace Hopper <grace@example.com>".into(),
            subject: "Hello".into(),
            body: "Line one\nLine two".into(),
            ..OutgoingMessage::default()
        }
    }

    fn text(raw: &[u8]) -> String {
        String::from_utf8(raw.to_vec()).expect("utf-8")
    }

    #[test]
    fn address_lists_accept_names_quotes_and_separators() {
        let parsed = parse_mailboxes(
            "\"Hopper, Grace\" <grace@example.com>; bob@example.com, Dave <dave@example.com>",
        )
        .expect("addresses");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].name.as_deref(), Some("Hopper, Grace"));
        assert_eq!(parsed[0].address, "grace@example.com");
        assert_eq!(parsed[1].name, None);
        assert_eq!(parsed[2].name.as_deref(), Some("Dave"));
    }

    #[test]
    fn invalid_addresses_are_rejected() {
        let mut message = draft();
        message.to = "not an address".into();
        assert!(message.build().is_err());
        message.to = "   ".into();
        assert!(
            message.build().is_err(),
            "at least one recipient is required"
        );
    }

    #[test]
    fn simple_message_has_crlf_headers_and_a_base64_body() {
        let (raw, envelope) = draft().build().expect("build");
        let raw = text(&raw);
        assert!(raw.starts_with("Date: "));
        assert!(raw.contains("\r\nFrom: \"Ada Lovelace\" <ada@example.com>\r\n"));
        assert!(raw.contains("\r\nTo: \"Grace Hopper\" <grace@example.com>\r\n"));
        assert!(raw.contains("\r\nSubject: Hello\r\n"));
        assert!(raw.contains("\r\nContent-Type: text/plain; charset=utf-8\r\n"));
        assert_eq!(envelope.to, vec!["grace@example.com"]);
        assert!(envelope.bcc.is_empty());

        let (_, body) = raw.split_once("\r\n\r\n").expect("body separator");
        let decoded = STANDARD.decode(body.trim()).expect("base64 body");
        assert_eq!(decoded, b"Line one\r\nLine two");
    }

    #[test]
    fn bcc_recipients_are_in_the_envelope_but_not_in_headers() {
        let mut message = draft();
        message.bcc = "Hidden <hidden@example.com>".into();
        let (raw, envelope) = message.build().expect("build");
        assert_eq!(envelope.bcc, vec!["hidden@example.com"]);
        assert!(!text(&raw).to_ascii_lowercase().contains("bcc:"));
    }

    #[test]
    fn non_ascii_headers_use_rfc2047() {
        let mut message = draft();
        message.subject = "Héllo wörld".into();
        message.from_name = "Adá".into();
        let raw = text(&message.build().expect("build").0);
        assert!(raw.contains("Subject: =?UTF-8?B?"));
        assert!(raw.contains("From: =?UTF-8?B?"));
        assert!(!raw.contains("Héllo"));
    }

    #[test]
    fn header_injection_is_neutralized() {
        let mut message = draft();
        message.subject = "Hello\r\nBcc: evil@example.com".into();
        message.to = "grace@example.com\r\nX-Evil: yes".into();
        // The newline turns the second address line into an invalid address.
        assert!(message.build().is_err());

        message.to = "grace@example.com".into();
        let raw = text(&message.build().expect("build").0);
        assert!(!raw.contains("\r\nBcc: evil@example.com"));
        assert!(raw.contains("Subject: Hello  Bcc: evil@example.com"));
    }

    #[test]
    fn attachments_produce_a_multipart_message() {
        let mut message = draft();
        message.attachments.push(ComposeAttachment::new(
            "résumé.pdf",
            "application/pdf",
            b"%PDF-1.4".to_vec(),
        ));
        let (raw, envelope) = message.build().expect("build");
        let raw = text(&raw);
        assert!(raw.contains("Content-Type: multipart/mixed; boundary=\"mailin-"));
        assert!(raw.contains("Content-Type: application/pdf"));
        assert!(raw.contains("filename=\"r_sum_.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"));
        assert_eq!(envelope.to, vec!["grace@example.com"]);
    }

    #[test]
    fn dates_are_formatted_in_utc() {
        assert_eq!(format_rfc2822(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(
            format_rfc2822(1_700_000_000),
            "Tue, 14 Nov 2023 22:13:20 +0000"
        );
        assert_eq!(
            format_rfc2822(2_000_000_000),
            "Wed, 18 May 2033 03:33:20 +0000"
        );
    }

    #[test]
    fn replies_keep_the_thread_headers() {
        let mut message = draft();
        message.subject = "Re: Hello".into();
        message.in_reply_to = Some("parent@example.com".into());
        message.references = vec!["root@example.com".into(), "<parent@example.com>".into()];
        let raw = text(&message.build().expect("build").0);
        assert!(raw.contains("In-Reply-To: <parent@example.com>"));
        assert!(raw.contains("References: <root@example.com> <parent@example.com>"));
    }

    #[test]
    fn mime_types_fall_back_to_octet_stream() {
        assert_eq!(sanitize_mime_type("text/plain"), "text/plain");
        assert_eq!(
            sanitize_mime_type("bad\r\nheader"),
            "application/octet-stream"
        );
    }

    #[test]
    fn drafts_may_have_no_recipients_and_reuse_the_key() {
        let mut message = draft();
        message.to.clear();
        message.subject.clear();
        message.draft_key = Some("draft-key@example.com".into());
        // Sending this must still fail, but saving a draft is allowed.
        assert!(message.build().is_err());
        let raw = text(&message.build_draft().expect("draft build"));
        assert!(raw.contains("Message-ID: <draft-key@example.com>"));
        assert!(raw.contains("To: \r\n"));
    }

    #[test]
    fn a_stored_draft_can_be_resumed() {
        let header = MessageHeader {
            id: crate::model::MessageId(7),
            folder_id: crate::model::FolderId(3),
            uid: 42,
            message_id: Some("draft-key@example.com".into()),
            thread_key: None,
            subject: "Unfinished".into(),
            from_name: Some("Ada".into()),
            from_addr: Some("ada@example.com".into()),
            to_addrs: "grace@example.com".into(),
            cc_addrs: String::new(),
            sent_at: None,
            received_at: 0,
            snippet: None,
            size: None,
            flags: vec!["\\Draft".into()],
            has_attachments: false,
            body_fetched: true,
        };
        let draft = draft_from_header(&header, Some("Half written"));
        assert_eq!(draft.to, "grace@example.com");
        assert_eq!(draft.subject, "Unfinished");
        assert_eq!(draft.body, "Half written");
        assert_eq!(draft.draft_key.as_deref(), Some("draft-key@example.com"));
    }

    #[test]
    fn blank_signatures_change_nothing() {
        assert_eq!(signature_block(""), "");
        assert_eq!(signature_block("  \n\n"), "");
        let quote = "\n\nOn Monday, Grace wrote:\n> hi";
        assert_eq!(with_signature_prefix("", quote), quote);
    }

    #[test]
    fn signature_block_starts_with_the_rfc_separator() {
        assert_eq!(
            signature_block("Ada\nhttps://example.com\n"),
            "-- \nAda\nhttps://example.com"
        );
    }

    #[test]
    fn signature_sits_above_the_quotation() {
        let reply = with_signature_prefix("Ada", "\n\nOn Monday, Grace wrote:\n> hi");
        assert_eq!(reply, "-- \nAda\n\nOn Monday, Grace wrote:\n> hi");
        let forwarded = with_signature_prefix(
            "Ada",
            "\n\n---------- Forwarded message ----------\nFrom: Grace",
        );
        assert!(forwarded.starts_with("-- \nAda\n\n---------- Forwarded message ----------"));
    }

    #[test]
    fn apply_template_substitutes_variables() {
        let template = "Hello {{name}}, your order {{order_id}} is ready.";
        let mut vars = std::collections::HashMap::new();
        vars.insert("name", "Ada");
        vars.insert("order_id", "12345");
        let result = apply_template(template, &vars);
        assert_eq!(result, "Hello Ada, your order 12345 is ready.");
    }

    #[test]
    fn apply_template_leaves_unknown_variables() {
        let template = "Hello {{name}}, your order {{order_id}} is ready.";
        let mut vars = std::collections::HashMap::new();
        vars.insert("name", "Ada");
        let result = apply_template(template, &vars);
        assert_eq!(result, "Hello Ada, your order {{order_id}} is ready.");
    }

    #[test]
    fn apply_template_handles_empty_vars() {
        let template = "Hello {{name}}!";
        let vars = std::collections::HashMap::new();
        let result = apply_template(template, &vars);
        assert_eq!(result, "Hello {{name}}!");
    }

    #[test]
    fn template_draft_creates_outgoing_message() {
        use crate::account::AccountId;
        use crate::model::Template;
        let template = Template {
            id: 1,
            account_id: AccountId::parse("00000000-0000-0000-0000-000000000001").unwrap(),
            name: "Test Template".into(),
            subject: "Hello {{name}}".into(),
            body: "Dear {{name}},\n\nBody text.".into(),
            created_at: 0,
            updated_at: 0,
        };
        let mut vars = std::collections::HashMap::new();
        vars.insert("name", "World");
        let draft = template_draft(&template, &vars);
        assert_eq!(draft.subject, "Hello World");
        assert_eq!(draft.body, "Dear World,\n\nBody text.");
        assert!(draft.draft_key.is_some());
    }
}
