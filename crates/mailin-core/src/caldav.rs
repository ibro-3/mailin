//! CalDAV client for calendar synchronization.
//! Stub implementation - infrastructure ready, HTTP client to be implemented.

use crate::error::{Error, Result};
use crate::model::CalendarEvent;
use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};

/// CalDAV client for interacting with calendar servers.
pub struct CalDavClient {
    agent: ureq::Agent,
    base_url: String,
    username: String,
    password: String,
}

impl CalDavClient {
    /// Create a new CalDAV client.
    pub fn new(base_url: &str, username: &str, password: &str) -> Self {
        Self {
            agent: ureq::Agent::new_with_defaults(),
            base_url: base_url.trim_end_matches('/').to_string(),
            username: username.to_string(),
            password: password.to_string(),
        }
    }

    /// Discover calendar home set for the current user.
    pub fn discover_calendar_home(&self) -> Result<String> {
        let _url = format!("{}/.well-known/caldav", self.base_url);
        // TODO: Implement HTTP request
        // For now, return a default
        Ok(format!("{}/calendars/{}", self.base_url, self.username))
    }

    /// List all calendars for the current user.
    pub fn list_calendars(&self, _home_url: &str) -> Result<Vec<CalendarInfo>> {
        // Stub for now - full implementation requires XML parsing
        Ok(Vec::new())
    }

    /// Sync a calendar (get events with ETag support).
    pub fn sync_calendar(&self, _calendar_url: &str, _ctag: Option<&str>) -> Result<Vec<EventInfo>> {
        // Stub for now - full implementation requires XML parsing
        Ok(Vec::new())
    }

    /// Make a WebDAV/CalDAV request.
    /// TODO: Implement full HTTP client using ureq 3.x API.
    fn request(&self, _method: &str, _url: &str, _body: Option<&str>, _content_type: Option<&str>) -> Result<String> {
        Err(Error::Protocol("CalDAV HTTP client not yet implemented".into()))
    }
}

/// Calendar info from CalDAV discovery.
#[derive(Debug, Clone)]
pub struct CalendarInfo {
    pub href: String,
    pub display_name: String,
    pub color: Option<String>,
    pub ctag: Option<String>,
}

/// Parsed event info from CalDAV.
#[derive(Debug, Clone)]
pub struct EventInfo {
    pub href: String,
    pub etag: String,
    pub ical: String,
}

/// Parse iCalendar data into a CalendarEvent.
/// TODO: Implement full iCal parsing.
pub fn parse_ical_event(_ical_data: &str, _calendar_id: i64) -> Result<CalendarEvent> {
    Err(Error::Protocol("iCal parsing not yet implemented".into()))
}

// Parse datetime helper (shared with tests)
fn parse_datetime(value: &str, params: &[(String, Vec<String>)]) -> Option<i64> {
    let is_date = params.iter().any(|(k, v)| k == "VALUE" && v.iter().any(|val| val == "DATE"));
    
    if value.ends_with('Z') {
        NaiveDateTime::parse_from_str(&value[..value.len()-1], "%Y%m%dT%H%M%S")
            .ok()
            .map(|dt| Utc.from_utc_datetime(&dt).timestamp())
    } else if value.len() == 8 {
        // Date-only format (YYYYMMDD)
        NaiveDate::parse_from_str(value, "%Y%m%d")
            .ok()
            .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
    } else {
        NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
            .ok()
            .map(|dt| Utc.from_utc_datetime(&dt).timestamp())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_datetime_utc() {
        let dt = parse_datetime("20240115T103000Z", &[]).unwrap();
        assert!(dt > 0);
    }

    #[test]
    fn test_parse_datetime_date_only() {
        let dt = parse_datetime("20240115", &[("VALUE".into(), vec!["DATE".into()])]).unwrap();
        assert!(dt > 0);
    }
}