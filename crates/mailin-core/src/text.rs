//! Small text helpers shared by the front-ends.
//!
//! The client is English-only for now, but the wording that is repeated
//! across the two UIs (counts, plural nouns) lives in one place so a later
//! localisation pass has a single place to change.

/// A count that can be pluralized.
///
/// Implemented for every integer type so call sites can pass the width they
/// already have without a cast.
pub trait Count {
    /// The count as a `u64`.
    fn count(self) -> u64;
}

macro_rules! impl_count {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl Count for $ty {
                fn count(self) -> u64 {
                    self as u64
                }
            }
        )+
    };
}

impl_count!(u8, u16, u32, u64, usize, i8, i16, i32, i64);

/// The plural form of an English noun for `count`.
///
/// English uses the singular for exactly one item and the plural for every
/// other count, including zero.
pub fn plural_noun<'a>(count: impl Count, singular: &'a str, plural: &'a str) -> &'a str {
    if count.count() == 1 { singular } else { plural }
}

/// The `"s"` suffix for `count`, for templates that interpolate the suffix
/// separately from the noun.
pub fn plural_suffix(count: impl Count) -> &'static str {
    if count.count() == 1 { "" } else { "s" }
}

/// `count` followed by the correctly pluralized noun: `plural(2, "message",
/// "messages")` → `"2 messages"`.
pub fn plural(count: impl Count, singular: &str, plural_form: &str) -> String {
    let count = count.count();
    format!("{count} {}", plural_noun(count, singular, plural_form))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singular_is_used_for_exactly_one() {
        assert_eq!(plural_noun(1, "message", "messages"), "message");
        assert_eq!(plural_noun(0, "message", "messages"), "messages");
        assert_eq!(plural_noun(2, "message", "messages"), "messages");
    }

    #[test]
    fn suffix_matches_the_noun() {
        assert_eq!(plural_suffix(1), "");
        assert_eq!(plural_suffix(0), "s");
        assert_eq!(plural_suffix(7), "s");
    }

    #[test]
    fn count_phrase_reads_naturally() {
        assert_eq!(plural(1, "message", "messages"), "1 message");
        assert_eq!(plural(4, "message", "messages"), "4 messages");
        assert_eq!(plural(0, "attachment", "attachments"), "0 attachments");
    }

    #[test]
    fn counts_of_different_integer_widths_work() {
        assert_eq!(plural(1u32, "message", "messages"), "1 message");
        assert_eq!(plural(3usize, "message", "messages"), "3 messages");
        assert_eq!(plural(2u64, "message", "messages"), "2 messages");
        assert_eq!(plural(5i32, "message", "messages"), "5 messages");
    }
}
