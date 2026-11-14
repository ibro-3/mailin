//! Protocol-correctness integration tests: IMAP flag validation, RFC 5322
//! header folding and OAuth2 page escaping.
//!
//! These exercise the public API the way a front-end would, complementing the
//! unit tests inside the crate.

use mail_parser::MessageParser;
use mailin_core::{OutgoingMessage, validate_flag};

// --------------------------------------------------------------------- flags

#[test]
fn system_flags_and_keywords_are_accepted() {
    for flag in [
        "\\Seen",
        "\\Answered",
        "\\Flagged",
        "\\Deleted",
        "$Junk",
        "NonJunk",
    ] {
        assert!(validate_flag(flag).is_ok(), "valid flag rejected: {flag}");
    }
}

#[test]
fn flag_injection_attempts_are_rejected() {
    // Each of these would change the meaning of a `UID STORE` command if it
    // reached the wire, because IMAP flags are sent as bare atoms.
    for flag in [
        "",                    // empty
        "seen deleted",        // space splits into two flags
        "seen\r\nBcc: evil@x", // CRLF injection
        "bad)flag",            // closes a list
        "bad{12}",             // literal syntax
        "bad*flag",            // list wildcard
        "bad%flag",            // list wildcard
        "bad\"flag",           // quoted string
        "bad\\flag",           // stray backslash
        "bad]flag",            // section marker
        "\\",                  // lone backslash
        "\\1nvalid",           // system flag with non-letter
        "nönascii",            // non-ASCII byte
        "ctrl\u{1}",           // control character
    ] {
        assert!(
            validate_flag(flag).is_err(),
            "invalid flag accepted: {flag:?}"
        );
    }
}

// ------------------------------------------------------------ header folding

fn outgoing_to_fifteen_recipients() -> OutgoingMessage {
    // Plain display names (no spaces) keep the header unquoted, so the fold
    // points can land between recipients rather than being suppressed inside
    // quoted strings.
    let recipients: Vec<String> = (0..15)
        .map(|i| format!("User{i} <user{i}@example{i}.example.com>"))
        .collect();
    OutgoingMessage {
        from_name: "Ada Lovelace".into(),
        from_addr: "ada@example.com".into(),
        to: recipients.join(", "),
        subject: "Protocol test".into(),
        body: "Hello everyone.".into(),
        ..OutgoingMessage::default()
    }
}

#[test]
fn long_to_header_stays_within_rfc_5322_line_limits() {
    let message = outgoing_to_fifteen_recipients();
    let (raw, _) = message.build().expect("build");
    let text = String::from_utf8(raw).expect("utf-8");

    // Every line, including the folded To header, must respect the
    // recommended 78-character limit (RFC 5322 §2.1.1).
    for line in text.split("\r\n") {
        assert!(line.len() <= 78, "line exceeds 78 chars: {line:?}");
    }
    // The To header must actually have been folded.
    assert!(text.contains("\r\n\t"), "the long To header was not folded");
}

#[test]
fn folded_recipients_survive_a_round_trip_through_a_parser() {
    let message = outgoing_to_fifteen_recipients();
    let (raw, _) = message.build().expect("build");

    let parsed = MessageParser::default().parse(&raw).expect("parse");
    let to = parsed.to().expect("To header");
    let addresses: Vec<String> = to
        .iter()
        .filter_map(|addr| addr.address().map(str::to_owned))
        .collect();

    // Folding must not lose or corrupt any of the fifteen recipients.
    assert_eq!(
        addresses.len(),
        15,
        "all recipients must survive: {addresses:?}"
    );
    for i in 0..15 {
        let expected = format!("user{i}@example{i}.example.com");
        assert!(
            addresses.iter().any(|a| a == &expected),
            "recipient {expected} missing after round-trip: {addresses:?}"
        );
    }
}

#[test]
fn short_headers_are_not_folded() {
    let message = OutgoingMessage {
        from_name: "Ada Lovelace".into(),
        from_addr: "ada@example.com".into(),
        to: "grace@example.com".into(),
        subject: "Hello".into(),
        body: "Line one\nLine two".into(),
        ..OutgoingMessage::default()
    };
    let (raw, _) = message.build().expect("build");
    let text = String::from_utf8(raw).expect("utf-8");
    assert!(text.contains("To: grace@example.com\r\n"));
    assert!(!text.contains("\r\n\t"), "no fold in a short message");
}

// ---------------------------------------------------------------- oauth html

#[test]
fn html_escaping_neutralizes_markup_in_error_pages() {
    let escapes = |s: &str| mailin_core::html::escape(s);

    let script = escapes("<script>alert('xss')</script>");
    assert!(!script.contains('<'), "angle bracket survived: {script:?}");
    assert!(!script.contains('>'), "angle bracket survived: {script:?}");

    let attribute = escapes("\"><img src=x onerror=alert(1)>");
    assert!(!attribute.contains('"'), "quote survived: {attribute:?}");
    assert!(!attribute.contains('<'));

    // An error_description from a hostile provider round-trips as inert text.
    let description = "<b>Access denied</b> & <i>please retry</i>";
    let escaped = escapes(description);
    assert!(escaped.contains("&lt;b&gt;"));
    assert!(escaped.contains("&amp;"));

    // Plain text is untouched.
    assert_eq!(
        escapes("authorization failed: timeout"),
        "authorization failed: timeout"
    );
}
