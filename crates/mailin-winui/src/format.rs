//! Pure formatting helpers for the WinUI widgets.
//!
//! The GTK client formats dates through `glib::DateTime`; WinUI has no such
//! helper in the filtered projection, so the same visible strings are produced
//! from `SYSTEMTIME` through the Win32 conversion functions. Everything here is
//! free of widget state.

use std::time::{SystemTime, UNIX_EPOCH};

use mailin_core::MessageHeader;
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
use windows_sys::Win32::System::Time::FileTimeToSystemTime;

/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const WINDOWS_TO_UNIX_EPOCH: i64 = 11_644_473_600;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Current Unix timestamp in seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
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
    let Some(date) = local_system_time(timestamp) else {
        return String::new();
    };
    let this_year = local_system_time(now_unix())
        .map(|now| now.wYear)
        .unwrap_or_default();
    let month = month_name(date.wMonth);
    if date.wYear == this_year {
        format!("{month} {}", date.wDay)
    } else {
        format!("{month} {}, {}", date.wDay, date.wYear)
    }
}

/// Full timestamp for the reader header.
pub fn full_date(timestamp: i64) -> String {
    let Some(date) = local_system_time(timestamp) else {
        return String::new();
    };
    let weekday = WEEKDAYS
        .get(date.wDayOfWeek as usize)
        .copied()
        .unwrap_or_default();
    format!(
        "{weekday}, {} {}, {} at {:02}:{:02}",
        month_name(date.wMonth),
        date.wDay,
        date.wYear,
        date.wHour,
        date.wMinute
    )
}

fn month_name(month: u16) -> &'static str {
    MONTHS
        .get(month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or_default()
}

/// Convert a Unix timestamp to local `SYSTEMTIME`, or `None` when the value is
/// outside the representable range.
fn local_system_time(timestamp: i64) -> Option<SYSTEMTIME> {
    let intervals = timestamp
        .checked_add(WINDOWS_TO_UNIX_EPOCH)?
        .checked_mul(10_000_000)?;
    let utc = FILETIME {
        dwLowDateTime: intervals as u32,
        dwHighDateTime: (intervals >> 32) as u32,
    };
    let mut local = FILETIME::default();
    if unsafe { FileTimeToLocalFileTime(&utc, &mut local) } == 0 {
        return None;
    }
    let mut system_time = SYSTEMTIME::default();
    if unsafe { FileTimeToSystemTime(&local, &mut system_time) } == 0 {
        return None;
    }
    Some(system_time)
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
