//! IMAP Modified UTF-7 (RFC 3501 §5.1.3) folder-name conversion.
//!
//! IMAP mailbox names use a modified form of UTF-7: runs of non-ASCII
//! characters are encoded as base64 of UTF-16BE code units wrapped in
//! `&...-` (the `-` shifts back to US-ASCII, so `&-` encodes a literal `&`).
//! The base64 alphabet replaces `/` with `,` and padding is omitted.

/// Decode an IMAP Modified-UTF-7 folder name to a Unicode string.
///
/// Sequences that cannot be decoded are left as-is (best-effort).
pub(crate) fn mutf7_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // Find the terminating `-` of the `&base64-` run; `&-` (literal `&`)
        // is an empty run.
        if bytes.get(i + 1) == Some(&b'-') {
            out.push('&');
            i += 2;
            continue;
        }
        let Some(end_rel) = bytes[i + 1..].iter().position(|&b| b == b'-') else {
            out.push('&');
            i += 1;
            continue;
        };
        let end = i + 1 + end_rel;
        let encoded = &s[i + 1..end];
        match decode_base64_utf16be(encoded) {
            Some(decoded) => out.push_str(&decoded),
            None => {
                // Not decodable: keep the raw run.
                out.push_str(&s[i..=end]);
            }
        }
        i = end + 1;
    }
    out
}

/// Encode a Unicode folder name to IMAP Modified-UTF-7.
pub(crate) fn mutf7_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run: Vec<u16> = Vec::new();
    for ch in s.chars() {
        let code = ch as u32;
        // Printable ASCII except `&` passes through; everything else (non-ASCII
        // or `&`) belongs to an encoded run.
        if (0x20..=0x7E).contains(&code) && ch != '&' {
            if !run.is_empty() {
                out.push_str(&encode_run(&run));
                run.clear();
            }
            out.push(ch);
        } else if ch == '&' {
            if !run.is_empty() {
                out.push_str(&encode_run(&run));
                run.clear();
            }
            out.push_str("&-");
        } else {
            let mut buf = [0u16; 2];
            for unit in ch.encode_utf16(&mut buf) {
                run.push(*unit);
            }
        }
    }
    if !run.is_empty() {
        out.push_str(&encode_run(&run));
    }
    out
}

/// Decode modified base64 (`,` instead of `/`, no padding) to UTF-16BE text.
fn decode_base64_utf16be(encoded: &str) -> Option<String> {
    if encoded.len() % 4 == 1 {
        return None; // impossible base64 length
    }
    let mut bytes = Vec::with_capacity(encoded.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for ch in encoded.bytes() {
        let value = match ch {
            b'A'..=b'Z' => (ch - b'A') as u32,
            b'a'..=b'z' => (ch - b'a' + 26) as u32,
            b'0'..=b'9' => (ch - b'0' + 52) as u32,
            b'+' => 62,
            b',' => 63,
            _ => return None,
        };
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
        }
    }
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = String::new();
    let mut units = bytes.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]]));
    while let Some(unit) = units.next() {
        if (0xD800..0xDC00).contains(&unit) {
            // High surrogate: must pair with the next low surrogate.
            let next = units.next()?;
            if !(0xDC00..0xE000).contains(&next) {
                return None;
            }
            let scalar = 0x10000
                + (((unit as u32) - 0xD800) << 10)
                + ((next as u32) - 0xDC00);
            out.push(char::from_u32(scalar)?);
        } else if (0xDC00..0xE000).contains(&unit) {
            return None; // unpaired low surrogate
        } else {
            out.push(char::from_u32(unit as u32)?);
        }
    }
    Some(out)
}

/// Encode UTF-16BE code units as a modified-base64 `&...;` run.
fn encode_run(units: &[u16]) -> String {
    let mut bytes = Vec::with_capacity(units.len() * 2);
    for unit in units {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";
    let mut out = String::from("&");
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out.push('-');
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passthrough() {
        assert_eq!(mutf7_decode("INBOX"), "INBOX");
        assert_eq!(mutf7_decode("Archive/2025"), "Archive/2025");
        assert_eq!(mutf7_encode("INBOX"), "INBOX");
    }

    #[test]
    fn ampersand_literal() {
        assert_eq!(mutf7_decode("&-"), "&");
        assert_eq!(mutf7_decode("Trash & Archive"), "Trash & Archive");
        assert_eq!(mutf7_encode("Trash & Archive"), "Trash &- Archive");
    }

    #[test]
    fn german_folder() {
        // "Entwürfe" with ü (U+00FC) encoded as UTF-16BE base64.
        assert_eq!(mutf7_decode("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(mutf7_encode("Entwürfe"), "Entw&APw-rfe");
    }

    #[test]
    fn non_bmp_round_trips() {
        // 🎉 is a non-BMP character (surrogate pair in UTF-16).
        let encoded = mutf7_encode("🎉");
        assert!(encoded.starts_with('&') && encoded.ends_with('-'));
        assert_eq!(mutf7_decode(&encoded), "🎉");
    }

    #[test]
    fn round_trip() {
        for name in [
            "INBOX",
            "Sent",
            "Entwürfe",
            "日本語",
            "Trash & Archive",
            "Ünicode & Stuff",
        ] {
            assert_eq!(
                mutf7_decode(&mutf7_encode(name)),
                name,
                "round-trip failed for {name}"
            );
        }
    }

    #[test]
    fn malformed_runs_are_left_as_is() {
        // No terminating `-`.
        assert_eq!(mutf7_decode("&APw"), "&APw");
        // Impossible base64 length (mod 4 == 1).
        assert_eq!(mutf7_decode("&ABCDE-"), "&ABCDE-");
    }
}
