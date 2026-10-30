//! Rules engine: pure evaluation of mail filtering rules.
//!
//! This module contains no I/O and is fully unit-testable. It evaluates
//! rule conditions against message headers and optional body text.

use crate::model::{MessageHeader, Rule, RuleAction, RuleCondition};
use regex::Regex;
use std::collections::HashMap;

/// Maximum number of rules per account (guard against runaway configs).
pub const MAX_RULES_PER_ACCOUNT: usize = 200;

/// Compiled regex cache for rule conditions.
#[derive(Default)]
pub struct RegexCache {
    cache: HashMap<String, Regex>,
}

impl RegexCache {
    pub fn get(&mut self, pattern: &str) -> Result<&Regex, regex::Error> {
        if self.cache.contains_key(pattern) {
            return Ok(self.cache.get(pattern).unwrap());
        }
        let re = Regex::new(pattern)?;
        self.cache.insert(pattern.to_owned(), re);
        Ok(self.cache.get(pattern).unwrap())
    }
}

/// Evaluate a single rule against a message.
/// Returns true if all conditions match.
pub fn eval_rule(
    rule: &Rule,
    header: &MessageHeader,
    body: Option<&str>,
    regex_cache: &mut RegexCache,
) -> bool {
    if !rule.enabled {
        return false;
    }

    let conditions: Vec<RuleCondition> = match serde_json::from_str(&rule.conditions_json) {
        Ok(c) => c,
        Err(_) => return false,
    };

    for cond in conditions {
        if !eval_condition(&cond, header, body, regex_cache) {
            return false;
        }
    }
    true
}

/// Evaluate a single condition against a message.
fn eval_condition(
    cond: &RuleCondition,
    header: &MessageHeader,
    body: Option<&str>,
    regex_cache: &mut RegexCache,
) -> bool {
    let field_value = get_field_value(cond.field.as_str(), header, body);
    match cond.op.as_str() {
        "contains" => field_value
            .to_lowercase()
            .contains(&cond.value.to_lowercase()),
        "equals" => field_value.eq_ignore_ascii_case(&cond.value),
        "starts_with" => field_value
            .to_lowercase()
            .starts_with(&cond.value.to_lowercase()),
        "regex" => {
            if let Ok(re) = regex_cache.get(&cond.value) {
                re.is_match(&field_value)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Extract the field value from a message header/body.
fn get_field_value(field: &str, header: &MessageHeader, body: Option<&str>) -> String {
    match field {
        "from" => header.from_addr.clone().unwrap_or_default(),
        "to" => header.to_addrs.clone(),
        "cc" => header.cc_addrs.clone(),
        "subject" => header.subject.clone(),
        "body" => body.unwrap_or_default().to_owned(),
        "recipient_is_me" => {
            // This would need the account's email - for now return empty
            // The actual check is done in the rule application logic
            String::new()
        }
        "has_attachment" => {
            if header.has_attachments { "true".to_owned() } else { "false".to_owned() }
        }
        _ => String::new(),
    }
}

/// Parse rule actions from JSON.
pub fn parse_actions(rule: &Rule) -> Vec<RuleAction> {
    serde_json::from_str(&rule.actions_json).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FolderId, MessageId};

    fn sample_header() -> MessageHeader {
        MessageHeader {
            id: MessageId(1),
            folder_id: FolderId(1),
            uid: 1,
            message_id: Some("test@example.com".into()),
            thread_key: None,
            subject: "Test Subject".into(),
            from_name: Some("John Doe".into()),
            from_addr: Some("john@example.com".into()),
            to_addrs: "me@example.com".into(),
            cc_addrs: "cc@example.com".into(),
            sent_at: Some(1_700_000_000),
            received_at: 1_700_000_000,
            snippet: None,
            size: None,
            flags: vec![],
            has_attachments: false,
            body_fetched: false,
        }
    }

    fn make_rule(conditions_json: &str, actions_json: &str) -> Rule {
        Rule {
            id: 1,
            account_id: crate::account::AccountId::parse("00000000-0000-0000-0000-000000000001").unwrap(),
            name: "Test Rule".into(),
            enabled: true,
            priority: 0,
            conditions_json: conditions_json.into(),
            actions_json: actions_json.into(),
            stop: false,
            created_at: 0,
        }
    }

    #[test]
    fn eval_rule_from_contains() {
        let rule = make_rule(
            r#"[{"field": "from", "op": "contains", "value": "john"}]"#,
            "[]",
        );
        let mut cache = RegexCache::default();
        let header = sample_header();
        assert!(eval_rule(&rule, &header, None, &mut cache));
    }

    #[test]
    fn eval_rule_from_not_contains() {
        let rule = make_rule(
            r#"[{"field": "from", "op": "contains", "value": "jane"}]"#,
            "[]",
        );
        let mut cache = RegexCache::default();
        let header = sample_header();
        assert!(!eval_rule(&rule, &header, None, &mut cache));
    }

    #[test]
    fn eval_rule_subject_equals() {
        let rule = make_rule(
            r#"[{"field": "subject", "op": "equals", "value": "Test Subject"}]"#,
            "[]",
        );
        let mut cache = RegexCache::default();
        let header = sample_header();
        assert!(eval_rule(&rule, &header, None, &mut cache));
    }

    #[test]
    fn eval_rule_subject_starts_with() {
        let rule = make_rule(
            r#"[{"field": "subject", "op": "starts_with", "value": "Test"}]"#,
            "[]",
        );
        let mut cache = RegexCache::default();
        let header = sample_header();
        assert!(eval_rule(&rule, &header, None, &mut cache));
    }

    #[test]
    fn eval_rule_regex() {
        let rule = make_rule(
            r#"[{"field": "subject", "op": "regex", "value": "Test.*"}]"#,
            "[]",
        );
        let mut cache = RegexCache::default();
        let header = sample_header();
        assert!(eval_rule(&rule, &header, None, &mut cache));
    }

    #[test]
    fn eval_rule_multiple_conditions_all_match() {
        let rule = make_rule(
            r#"[
                {"field": "from", "op": "contains", "value": "john"},
                {"field": "subject", "op": "contains", "value": "Test"}
            ]"#,
            "[]",
        );
        let mut cache = RegexCache::default();
        let header = sample_header();
        assert!(eval_rule(&rule, &header, None, &mut cache));
    }

    #[test]
    fn eval_rule_multiple_conditions_one_fails() {
        let rule = make_rule(
            r#"[
                {"field": "from", "op": "contains", "value": "john"},
                {"field": "subject", "op": "contains", "value": "Wrong"}
            ]"#,
            "[]",
        );
        let mut cache = RegexCache::default();
        let header = sample_header();
        assert!(!eval_rule(&rule, &header, None, &mut cache));
    }

    #[test]
    fn eval_rule_disabled() {
        let mut rule = make_rule(r#"[{"field": "from", "op": "contains", "value": "john"}]"#, "[]");
        rule.enabled = false;
        let mut cache = RegexCache::default();
        let header = sample_header();
        assert!(!eval_rule(&rule, &header, None, &mut cache));
    }

    #[test]
    fn eval_rule_has_attachment() {
        let mut header = sample_header();
        header.has_attachments = true;
        let rule = make_rule(
            r#"[{"field": "has_attachment", "op": "equals", "value": "true"}]"#,
            "[]",
        );
        let mut cache = RegexCache::default();
        assert!(eval_rule(&rule, &header, None, &mut cache));
    }

    #[test]
    fn eval_rule_body_contains() {
        let rule = make_rule(
            r#"[{"field": "body", "op": "contains", "value": "hello"}]"#,
            "[]",
        );
        let mut cache = RegexCache::default();
        let header = sample_header();
        assert!(!eval_rule(&rule, &header, None, &mut cache));
        assert!(eval_rule(&rule, &header, Some("hello world"), &mut cache));
    }
}