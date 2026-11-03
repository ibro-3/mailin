//! Search query parser and types for Mailin.
//!
//! Supports structured queries like:
//! - `from:alice` / `to:bob` / `subject:report`
//! - `has:attachment` / `is:unread` / `is:flagged`
//! - `before:2024-01-15` / `after:2024-01-01`
//! - Quoted phrases: `"exact phrase"`
//! - Plain terms fall through to FTS

use crate::model::FolderId;
use chrono::NaiveDate;

/// Parsed search query with structured fields and free-text terms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    /// Free-text terms for FTS (subject, body, from, to)
    pub fts_terms: Vec<String>,
    /// Field-specific filters
    pub filters: SearchFilters,
}

/// Field-specific search filters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchFilters {
    /// Filter by sender address/name (substring match)
    pub from: Option<String>,
    /// Filter by recipient address/name (substring match)
    pub to: Option<String>,
    /// Filter by subject (substring match)
    pub subject: Option<String>,
    /// Filter by CC recipients
    pub cc: Option<String>,
    /// Has attachment
    pub has_attachment: Option<bool>,
    /// Is unread
    pub is_unread: Option<bool>,
    /// Is flagged/starred
    pub is_flagged: Option<bool>,
    /// Before date (exclusive, YYYY-MM-DD)
    pub before: Option<i64>,
    /// After date (inclusive, YYYY-MM-DD)
    pub after: Option<i64>,
    /// Folder scope (None = all folders in account)
    pub folder_id: Option<FolderId>,
}

/// Parse a search query string into structured SearchQuery.
///
/// Supported syntax:
/// - `from:alice` - messages from alice
/// - `to:bob` - messages to bob
/// - `subject:report` - subject contains "report"
/// - `has:attachment` - has attachments
/// - `is:unread` - unread messages
/// - `is:flagged` - flagged messages
/// - `before:2024-01-15` - before date
/// - `after:2024-01-01` - after date
/// - `"exact phrase"` - exact phrase (quoted)
/// - plain terms - free text search
pub fn parse_search_query(query: &str) -> SearchQuery {
    let mut result = SearchQuery::default();
    let tokens = tokenize(query);
    let mut tokens = tokens.into_iter().peekable();

    while let Some(token) = tokens.next() {
        if let Some((field, value)) = parse_field(&token, &mut tokens) {
            apply_field(&mut result, field, value);
        } else {
            // Plain term - add to FTS
            result.fts_terms.push(token);
        }
    }

    result
}

/// Tokenize the query string, respecting quoted phrases.
fn tokenize(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let chars = query.chars().peekable();

    for c in chars {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                if !in_quotes && !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(c),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

/// Try to parse a field:value pair. Returns (field, value) if successful.
fn parse_field(
    token: &str,
    tokens: &mut std::iter::Peekable<std::vec::IntoIter<String>>,
) -> Option<(String, String)> {
    let (field, value) = match token.split_once(':') {
        Some((f, v)) => (f.to_lowercase(), v.to_owned()),
        None => return None,
    };

    // Handle fields that might have a space after colon (e.g., "from: alice")
    if value.is_empty() {
        // Look ahead at next token
        if let Some(next) = tokens.peek() {
            // Don't consume if it looks like another field (contains :)
            if !next.contains(':') {
                let value = tokens.next().unwrap();
                return Some((field, value));
            }
        }
        return None;
    }

    Some((field, value))
}

/// Apply a parsed field to the search query.
fn apply_field(query: &mut SearchQuery, field: String, value: String) {
    match field.as_str() {
        "from" => query.filters.from = Some(value),
        "to" => query.filters.to = Some(value),
        "subject" => query.filters.subject = Some(value),
        "cc" => query.filters.cc = Some(value),
        "has" => match value.as_str() {
            "attachment" | "attachments" => query.filters.has_attachment = Some(true),
            _ => query.fts_terms.push(format!("has:{}", value)),
        },
        "is" => match value.as_str() {
            "unread" => query.filters.is_unread = Some(true),
            "read" => query.filters.is_unread = Some(false),
            "flagged" | "starred" => query.filters.is_flagged = Some(true),
            "unflagged" | "unstarred" => query.filters.is_flagged = Some(false),
            _ => query.fts_terms.push(format!("is:{}", value)),
        },
        "before" => {
            if let Ok(date) = parse_date(&value) {
                query.filters.before = Some(date);
            } else {
                query.fts_terms.push(format!("before:{}", value));
            }
        }
        "after" | "since" => {
            if let Ok(date) = parse_date(&value) {
                query.filters.after = Some(date);
            } else {
                query.fts_terms.push(format!("after:{}", value));
            }
        }
        "folder" => {
            // Folder filtering would require folder name resolution
            query.fts_terms.push(format!("folder:{}", value));
        }
        _ => {
            // Unknown field - treat as plain text
            query.fts_terms.push(format!("{}:{}", field, value));
        }
    }
}

/// Parse YYYY-MM-DD date to Unix timestamp (start of day UTC).
fn parse_date(date_str: &str) -> Result<i64, chrono::ParseError> {
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")?;
    Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
}

/// Build FTS query string from free-text terms.
/// Uses FTS5 syntax: terms are AND'd by default, OR with |, phrase with ".
pub fn build_fts_query(terms: &[String]) -> Option<String> {
    if terms.is_empty() {
        return None;
    }

    // Escape special FTS5 characters
    let escaped: Vec<String> = terms
        .iter()
        .map(|t| t.replace('"', "\"\"").replace(['*', '?'], ""))
        .collect();

    Some(escaped.join(" "))
}

/// Build SQL WHERE clause for structured filters.
/// Returns (sql_fragment, params).
pub fn build_filter_where(filters: &SearchFilters) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut clauses = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(ref from) = filters.from {
        clauses.push("(m.from_addr LIKE ? OR m.from_name LIKE ?)");
        params.push(Box::new(format!("%{}%", from)));
        params.push(Box::new(format!("%{}%", from)));
    }

    if let Some(ref to) = filters.to {
        clauses.push("m.to_addrs LIKE ?");
        params.push(Box::new(format!("%{}%", to)));
    }

    if let Some(ref subject) = filters.subject {
        clauses.push("m.subject LIKE ?");
        params.push(Box::new(format!("%{}%", subject)));
    }

    if let Some(ref cc) = filters.cc {
        clauses.push("m.cc_addrs LIKE ?");
        params.push(Box::new(format!("%{}%", cc)));
    }

    if let Some(has_att) = filters.has_attachment {
        clauses.push("m.has_attachments = ?");
        params.push(Box::new(if has_att { 1i64 } else { 0i64 }));
    }

    if let Some(is_unread) = filters.is_unread {
        if is_unread {
            clauses.push("instr(' ' || m.flags || ' ', ' \\Seen ') = 0");
        } else {
            clauses.push("instr(' ' || m.flags || ' ', ' \\Seen ') > 0");
        }
    }

    if let Some(is_flagged) = filters.is_flagged {
        if is_flagged {
            clauses.push("instr(' ' || m.flags || ' ', ' \\Flagged ') > 0");
        } else {
            clauses.push("instr(' ' || m.flags || ' ', ' \\Flagged ') = 0");
        }
    }

    if let Some(before) = filters.before {
        clauses.push("m.received_at < ?");
        params.push(Box::new(before));
    }

    if let Some(after) = filters.after {
        clauses.push("m.received_at >= ?");
        params.push(Box::new(after));
    }

    if let Some(folder_id) = filters.folder_id {
        clauses.push("m.folder_id = ?");
        params.push(Box::new(folder_id.0));
    }

    let sql = if clauses.is_empty() {
        "1=1".to_string()
    } else {
        clauses.join(" AND ")
    };

    (sql, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_plain_terms() {
        let q = parse_search_query("hello world");
        assert_eq!(q.fts_terms, vec!["hello", "world"]);
        assert!(q.filters.from.is_none());
    }

    #[test]
    fn test_parse_from_field() {
        let q = parse_search_query("from:alice@example.com");
        assert_eq!(q.filters.from, Some("alice@example.com".into()));
        assert!(q.fts_terms.is_empty());
    }

    #[test]
    fn test_parse_to_field() {
        let q = parse_search_query("to:bob");
        assert_eq!(q.filters.to, Some("bob".into()));
    }

    #[test]
    fn test_parse_subject_field() {
        let q = parse_search_query("subject:quarterly report");
        assert_eq!(q.filters.subject, Some("quarterly".into()));
        assert_eq!(q.fts_terms, vec!["report"]);
    }

    #[test]
    fn test_parse_has_attachment() {
        let q = parse_search_query("has:attachment");
        assert_eq!(q.filters.has_attachment, Some(true));
    }

    #[test]
    fn test_parse_is_unread() {
        let q = parse_search_query("is:unread");
        assert_eq!(q.filters.is_unread, Some(true));
    }

    #[test]
    fn test_parse_is_read() {
        let q = parse_search_query("is:read");
        assert_eq!(q.filters.is_unread, Some(false));
    }

    #[test]
    fn test_parse_is_flagged() {
        let q = parse_search_query("is:flagged");
        assert_eq!(q.filters.is_flagged, Some(true));
    }

    #[test]
    fn test_parse_before_date() {
        let q = parse_search_query("before:2024-01-15");
        assert!(q.filters.before.is_some());
    }

    #[test]
    fn test_parse_after_date() {
        let q = parse_search_query("after:2024-01-01");
        assert!(q.filters.after.is_some());
    }

    #[test]
    fn test_parse_quoted_phrase() {
        let q = parse_search_query("\"exact phrase\" test");
        assert_eq!(q.fts_terms, vec!["exact phrase", "test"]);
    }

    #[test]
    fn test_parse_mixed() {
        let q = parse_search_query("from:alice subject:report has:attachment before:2024-01-15");
        assert_eq!(q.filters.from, Some("alice".into()));
        assert_eq!(q.filters.subject, Some("report".into()));
        assert_eq!(q.filters.has_attachment, Some(true));
        assert!(q.filters.before.is_some());
        assert!(q.fts_terms.is_empty());
    }

    #[test]
    fn test_build_fts_query() {
        let terms = vec!["hello".into(), "world".into()];
        assert_eq!(build_fts_query(&terms), Some("hello world".into()));
    }

    #[test]
    fn test_build_fts_query_empty() {
        assert_eq!(build_fts_query(&[]), None);
    }

    #[test]
    fn test_build_filter_where() {
        let filters = SearchFilters {
            from: Some("alice".into()),
            is_unread: Some(true),
            ..Default::default()
        };
        let (sql, params) = build_filter_where(&filters);
        assert!(sql.contains("from_addr LIKE"));
        assert!(sql.contains("instr"));
        // from_addr and from_name only (is_unread uses SQL expression without params)
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn test_tokenize_quoted() {
        let tokens = tokenize(r#"from:alice "exact phrase" test"#);
        assert_eq!(tokens, vec!["from:alice", "exact phrase", "test"]);
    }
}
