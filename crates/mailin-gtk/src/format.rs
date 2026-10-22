//! Small pure formatting helpers used by the GTK widgets.
//!
//! Everything in this module is free of widget state so it can be unit tested
//! without a display server.

use std::time::{SystemTime, UNIX_EPOCH};

use gtk::glib;

use mailin_core::MessageHeader;

/// Current Unix timestamp in seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}

/// Compact timestamp for the message list: `now`, `12 min`, `3 h`, `5 d`,
/// then the calendar date.
pub fn list_time(now: i64, then: i64) -> String {
    if then <= 0 {
        return String::new();
    }
    let delta = now.saturating_sub(then);
    match delta {
        d if d < 60 => "now".to_owned(),
        d if d < 3_600 => format!("{} min", d / 60),
        d if d < 86_400 => format!("{} h", d / 3_600),
        d if d < 7 * 86_400 => format!("{} d", d / 86_400),
        _ => short_date(then),
    }
}

/// `Oct 12` for the current year, `Oct 12, 2024` otherwise.
fn short_date(timestamp: i64) -> String {
    let Some(date) = glib::DateTime::from_unix_local(timestamp).ok() else {
        return String::new();
    };
    let this_year = glib::DateTime::now_local()
        .map(|now| now.year())
        .unwrap_or_default();
    let pattern = if date.year() == this_year {
        "%b %-e"
    } else {
        "%b %-e, %Y"
    };
    date.format(pattern)
        .map(|value| value.to_string())
        .unwrap_or_default()
}

/// Full timestamp for the reader header.
pub fn full_date(timestamp: i64) -> String {
    glib::DateTime::from_unix_local(timestamp)
        .and_then(|date| date.format("%a, %b %-e, %Y at %H:%M"))
        .map(|value| value.to_string())
        .unwrap_or_default()
}

/// Name shown in the message list: the display name when there is one.
pub fn sender_label(message: &MessageHeader) -> String {
    match (&message.from_name, &message.from_addr) {
        (Some(name), _) if !name.trim().is_empty() => name.clone(),
        (_, Some(address)) => address.clone(),
        _ => "(unknown sender)".to_owned(),
    }
}

/// Subject fallback for messages without one.
pub fn subject_label(message: &MessageHeader) -> String {
    let subject = message.subject.trim();
    if subject.is_empty() {
        "(no subject)".to_owned()
    } else {
        subject.to_owned()
    }
}

/// Collapse a snippet to a single line of at most `max` characters.
pub fn one_line(text: &str, max: usize) -> String {
    let mut collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        collapsed = collapsed.chars().take(max.saturating_sub(1)).collect();
        collapsed.push('…');
    }
    collapsed
}

/// Human readable file size for attachment metadata.
pub fn file_size(bytes: u32) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let value = f64::from(bytes);
    if value < KIB {
        format!("{bytes} B")
    } else if value < MIB {
        format!("{:.0} KB", value / KIB)
    } else {
        format!("{:.1} MB", value / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_are_compact() {
        let now = 1_700_000_000;
        assert_eq!(list_time(now, now - 10), "now");
        assert_eq!(list_time(now, now - 120), "2 min");
        assert_eq!(list_time(now, now - 7_200), "2 h");
        assert_eq!(list_time(now, now - 2 * 86_400), "2 d");
        assert_eq!(list_time(now, 0), "");
        assert!(!list_time(now, now - 30 * 86_400).is_empty());
    }

    #[test]
    fn snippets_are_collapsed_and_capped() {
        assert_eq!(one_line("hello\n\n  world", 40), "hello world");
        assert_eq!(one_line("abcdef", 4), "abc…");
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(file_size(512), "512 B");
        assert_eq!(file_size(2_048), "2 KB");
        assert_eq!(file_size(5 * 1024 * 1024), "5.0 MB");
    }
}
