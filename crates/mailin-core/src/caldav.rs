//! CalDAV client for calendar synchronization.

use crate::error::{Error, Result};
use crate::model::CalendarEvent;
use base64::Engine;
use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
use http::Method;
use std::collections::HashMap;
use ureq::Agent;
use xmltree::Element;

/// CalDAV client for interacting with calendar servers.
pub struct CalDavClient {
    agent: Agent,
    base_url: String,
    username: String,
    password: String,
}

impl CalDavClient {
    /// Create a new CalDAV client.
    pub fn new(base_url: &str, username: &str, password: &str) -> Self {
        Self {
            agent: Agent::new_with_defaults(),
            base_url: base_url.trim_end_matches('/').to_string(),
            username: username.to_string(),
            password: password.to_string(),
        }
    }

    /// Make a WebDAV/CalDAV request with basic auth.
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        content_type: Option<&str>,
    ) -> Result<String> {
        self.request_with_depth(method, url, body, content_type, None)
    }

    /// Make a WebDAV/CalDAV request with basic auth and a `Depth` header.
    /// WebDAV PROPFIND defaults to `Depth: 0`, which would hide the child
    /// collections a calendar listing needs.
    fn request_with_depth(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        content_type: Option<&str>,
        depth: Option<&str>,
    ) -> Result<String> {
        let method = method
            .parse::<Method>()
            .map_err(|e| Error::Config(format!("invalid CalDAV HTTP method {method:?}: {e}")))?;
        let mut req = http::Request::builder()
            .method(method)
            .uri(url)
            .header("Authorization", basic_auth(&self.username, &self.password));
        if let Some(ct) = content_type {
            req = req.header("Content-Type", ct);
        }
        if let Some(depth) = depth {
            req = req.header("Depth", depth);
        }
        let req = if let Some(b) = body {
            req.body(b.to_string())
                .map_err(|e| Error::Protocol(format!("could not build CalDAV request: {e}")))?
        } else {
            req.body(String::new())
                .map_err(|e| Error::Protocol(format!("could not build CalDAV request: {e}")))?
        };
        let resp = self
            .agent
            .run(req)
            .map_err(|e| Error::Protocol(format!("HTTP error: {e}")))?;
        let status = resp.status();
        let body = resp
            .into_body()
            .read_to_string()
            .map_err(|e| Error::Protocol(format!("Body read error: {e}")))?;
        if !(200..300).contains(&status.as_u16()) {
            return Err(Error::Protocol(format!(
                "CalDAV error {}: {}",
                status, body
            )));
        }
        Ok(body)
    }

    /// Discover calendar home set for the current user.
    ///
    /// RFC 6764 §5: the well-known URI answers with a redirect to the
    /// calendar home. The agent follows it, so the *final* URI of the
    /// response is the discovery result — not a `Location:` header inside
    /// the body.
    pub fn discover_calendar_home(&self) -> Result<String> {
        let url = format!("{}/.well-known/caldav", self.base_url);
        let method = "GET".parse::<Method>().ok();
        let Some(method) = method else {
            return Err(Error::Protocol("invalid CalDAV method GET".into()));
        };
        let req = http::Request::builder()
            .method(method)
            .uri(&url)
            .header("Authorization", basic_auth(&self.username, &self.password))
            .body(String::new())
            .map_err(|e| Error::Protocol(format!("could not build CalDAV request: {e}")))?;
        let resp = self
            .agent
            .run(req)
            .map_err(|e| Error::Protocol(format!("HTTP error: {e}")))?;
        let status = resp.status();
        if !(200..300).contains(&status.as_u16()) {
            return Err(Error::Protocol(format!("CalDAV discovery error {status}")));
        }
        let final_uri = ureq::ResponseExt::get_uri(&resp).to_string();
        if final_uri.is_empty() {
            return Err(Error::Protocol("could not discover CalDAV home".into()));
        }
        Ok(final_uri)
    }

    /// List all calendars for the current user.
    pub fn list_calendars(&self, home_url: &str) -> Result<Vec<CalendarInfo>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:displayname/>
    <D:resourcetype/>
    <C:calendar-color/>
    <C:calendar-timezone/>
    <D:getctag/>
  </D:prop>
</D:propfind>"#;
        let xml = self.request_with_depth(
            "PROPFIND",
            home_url,
            Some(body),
            Some("application/xml; charset=utf-8"),
            Some("1"),
        )?;
        parse_calendar_multistatus(&xml, home_url)
    }

    /// Sync a calendar (get events with ETag support).
    ///
    /// A PROPFIND for the collection tag runs first: when the stored ctag
    /// still matches the server's, the calendar is unchanged and the full
    /// event download is skipped entirely.
    pub fn sync_calendar(&self, calendar_url: &str, ctag: Option<&str>) -> Result<Vec<EventInfo>> {
        let server_ctag = self.current_ctag(calendar_url)?;
        if let Some(stored) = ctag
            && let Some(server) = &server_ctag
            && stored == server
        {
            tracing::debug!(calendar = %calendar_url, "ctag unchanged; skipping event fetch");
            return Ok(Vec::new());
        }

        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
</D:propfind>"#;
        let xml = self.request(
            "REPORT",
            calendar_url,
            Some(body),
            Some("application/xml; charset=utf-8"),
        )?;
        parse_event_multistatus(&xml)
    }

    /// The calendar collection tag (`getctag`) currently reported by the
    /// server, or `None` when the server does not support it.
    fn current_ctag(&self, calendar_url: &str) -> Result<Option<String>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getctag/>
  </D:prop>
</D:propfind>"#;
        let xml = self.request_with_depth(
            "PROPFIND",
            calendar_url,
            Some(body),
            Some("application/xml; charset=utf-8"),
            Some("0"),
        )?;
        Ok(parse_first_prop(&xml, "getctag"))
    }
}

fn basic_auth(username: &str, password: &str) -> String {
    let creds = format!("{}:{}", username, password);
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(creds)
    )
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
    pub etag: String,
    pub ical: String,
}

/// Parse PROPFIND multistatus response for calendar listing.
fn parse_calendar_multistatus(xml: &str, base_url: &str) -> Result<Vec<CalendarInfo>> {
    let root = Element::parse(xml.as_bytes())
        .map_err(|e| Error::Protocol(format!("XML parse error: {e}")))?;
    let mut calendars = Vec::new();

    for response in root.children.iter().filter_map(|c| c.as_element()) {
        if response.name != "response" {
            continue;
        }
        let mut href = String::new();
        let mut display_name = String::new();
        let mut color = None;
        let mut ctag = None;
        let mut is_calendar = false;

        for propstat in response.children.iter().filter_map(|c| c.as_element()) {
            if propstat.name != "propstat" {
                continue;
            }
            let status_ok = propstat
                .get_child("status")
                .and_then(|s| s.get_text())
                .map(|s| s.contains("200"))
                .unwrap_or(false);
            if !status_ok {
                continue;
            }
            if let Some(prop) = propstat.get_child("prop") {
                for child in &prop.children {
                    if let Some(elem) = child.as_element() {
                        match elem.name.as_str() {
                            "href" => href = elem.get_text().unwrap_or_default().into_owned(),
                            "displayname" => {
                                display_name = elem.get_text().unwrap_or_default().into_owned()
                            }
                            "resourcetype" => {
                                is_calendar = elem.children.iter().any(|c| {
                                    if let Some(e) = c.as_element() {
                                        e.name == "calendar"
                                            && e.namespace.as_deref()
                                                == Some("urn:ietf:params:xml:ns:caldav")
                                    } else {
                                        false
                                    }
                                });
                            }
                            "calendar-color" => color = elem.get_text().map(|s| s.into_owned()),
                            "getctag" => ctag = elem.get_text().map(|s| s.into_owned()),
                            _ => {}
                        }
                    }
                }
            }
        }
        if is_calendar && !href.is_empty() {
            // Make href absolute
            if !href.starts_with("http") {
                let base = base_url.trim_end_matches('/');
                let h = href.trim_start_matches('/');
                href = format!("{}/{}", base, h);
            }
            calendars.push(CalendarInfo {
                href,
                display_name: if display_name.is_empty() {
                    "Unnamed".into()
                } else {
                    display_name
                },
                color,
                ctag,
            });
        }
    }
    Ok(calendars)
}

/// Parse REPORT multistatus response for events.
fn parse_event_multistatus(xml: &str) -> Result<Vec<EventInfo>> {
    let root = Element::parse(xml.as_bytes())
        .map_err(|e| Error::Protocol(format!("XML parse error: {e}")))?;
    let mut events = Vec::new();

    for response in root.children.iter().filter_map(|c| c.as_element()) {
        if response.name != "response" {
            continue;
        }
        let mut href = String::new();
        let mut etag = String::new();
        let mut ical = String::new();

        for propstat in response.children.iter().filter_map(|c| c.as_element()) {
            if propstat.name != "propstat" {
                continue;
            }
            let status_ok = propstat
                .get_child("status")
                .and_then(|s| s.get_text())
                .map(|s| s.contains("200"))
                .unwrap_or(false);
            if !status_ok {
                continue;
            }
            if let Some(prop) = propstat.get_child("prop") {
                for child in &prop.children {
                    if let Some(elem) = child.as_element() {
                        match elem.name.as_str() {
                            "href" => href = elem.get_text().unwrap_or_default().into_owned(),
                            "getetag" => {
                                etag = elem
                                    .get_text()
                                    .unwrap_or_default()
                                    .trim_matches('"')
                                    .to_string()
                            }
                            "calendar-data" => {
                                ical = elem.get_text().unwrap_or_default().into_owned()
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if !href.is_empty() && !ical.is_empty() {
            events.push(EventInfo { etag, ical });
        }
    }
    Ok(events)
}

/// Parse iCalendar data into one `CalendarEvent` per VEVENT.
///
/// An ICS entity may contain several events (for example recurring instances
/// exported as a series); malformed events are skipped with a warning.
pub fn parse_ical_events(ical_data: &str, calendar_id: i64) -> Result<Vec<CalendarEvent>> {
    let lines: Vec<&str> = ical_data.lines().collect();
    let mut events = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        // Fast-forward to the next VEVENT.
        while i < lines.len() && lines[i].trim() != "BEGIN:VEVENT" {
            i += 1;
        }
        if i >= lines.len() {
            break;
        }
        i += 1; // consume BEGIN:VEVENT

        let mut uid = String::new();
        let mut summary = String::new();
        let mut description = String::new();
        let mut location = String::new();
        let mut start = 0i64;
        let mut end = 0i64;
        let mut dtstart_params: HashMap<String, Vec<String>> = HashMap::new();

        while i < lines.len() {
            // Handle line folding (RFC 5545 §3.1)
            let mut full_line = lines[i].to_string();
            i += 1;
            while i < lines.len() && (lines[i].starts_with(' ') || lines[i].starts_with('\t')) {
                full_line.push_str(&lines[i][1..]);
                i += 1;
            }

            if full_line.trim_end() == "END:VEVENT" {
                break;
            }

            // Parse property with parameters
            let (name, value, params) = parse_ical_property(&full_line);

            match name.as_str() {
                "UID" => uid = value,
                "SUMMARY" => summary = value,
                "DESCRIPTION" => description = value,
                "LOCATION" => location = value,
                "DTSTART" => {
                    start = parse_datetime(&value, &params).unwrap_or(0);
                    dtstart_params = params;
                }
                "DTEND" => end = parse_datetime(&value, &params).unwrap_or(0),
                _ => {}
            }
        }

        if uid.is_empty() || start == 0 {
            tracing::debug!("skipping VEVENT without UID or DTSTART");
            continue;
        }

        // Determine if all-day
        let all_day = dtstart_params
            .get("VALUE")
            .is_some_and(|v| v.iter().any(|val| val == "DATE"));

        events.push(CalendarEvent {
            id: 0, // Will be assigned by store
            calendar_id,
            uid,
            summary: if summary.is_empty() {
                String::new()
            } else {
                summary
            },
            start_at: start,
            end_at: end,
            all_day,
            location: location.into(),
            description: description.into(),
            attendees_json: "[]".to_string(),
            ics_raw: ical_data.to_string(),
            etag: None,
        });
    }

    if events.is_empty() {
        return Err(Error::Protocol(
            "no parsable VEVENT in iCalendar data".into(),
        ));
    }
    Ok(events)
}

fn parse_ical_property(line: &str) -> (String, String, HashMap<String, Vec<String>>) {
    let mut params: HashMap<String, Vec<String>> = HashMap::new();
    let colon = line.find(':').unwrap_or(line.len());
    let (name_part, value) = line.split_at(colon);
    // Skip the leading ':' if the line was split at one.
    let value = value.strip_prefix(':').unwrap_or(value);

    let parts: Vec<&str> = name_part.split(';').collect();
    let name = parts[0].to_uppercase();
    for param in &parts[1..] {
        if let Some(eq) = param.find('=') {
            let (k, v) = param.split_at(eq);
            params
                .entry(k.to_uppercase())
                .or_default()
                .push(v[1..].to_string());
        }
    }
    (name, value.to_string(), params)
}

/// Parse datetime helper
fn parse_datetime(value: &str, params: &HashMap<String, Vec<String>>) -> Option<i64> {
    let is_date = params
        .get("VALUE")
        .is_some_and(|v| v.iter().any(|val| val == "DATE"));

    if let Some(stripped) = value.strip_suffix('Z') {
        NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
            .ok()
            .map(|dt| Utc.from_utc_datetime(&dt).timestamp())
    } else if is_date || value.len() == 8 {
        NaiveDate::parse_from_str(value, "%Y%m%d").ok().map(|d| {
            d.and_hms_opt(0, 0, 0)
                .expect("midnight is a valid time")
                .and_utc()
                .timestamp()
        })
    } else if let Some(tzid) = params.get("TZID").and_then(|v| v.first()) {
        // A TZID-qualified datetime is wall-clock time in that zone (RFC 5545
        // §3.3.5); convert it to UTC instead of reading it as UTC.
        match tzid.parse::<chrono_tz::Tz>() {
            Ok(tz) => {
                let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
                let local = tz.from_local_datetime(&naive).single()?;
                Some(local.with_timezone(&Utc).timestamp())
            }
            Err(_) => {
                tracing::warn!(tzid = %tzid, "unknown TZID; falling back to UTC");
                NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
                    .ok()
                    .map(|dt| Utc.from_utc_datetime(&dt).timestamp())
            }
        }
    } else {
        NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
            .ok()
            .map(|dt| Utc.from_utc_datetime(&dt).timestamp())
    }
}

/// The text of the first XML element with the given name in a PROPFIND
/// multistatus response.
fn parse_first_prop(xml: &str, prop_name: &str) -> Option<String> {
    let root = Element::parse(xml.as_bytes()).ok()?;
    for response in root.children.iter().filter_map(|c| c.as_element()) {
        if response.name != "response" {
            continue;
        }
        for propstat in response.children.iter().filter_map(|c| c.as_element()) {
            if propstat.name != "propstat" {
                continue;
            }
            let status_ok = propstat
                .get_child("status")
                .and_then(|s| s.get_text())
                .map(|s| s.contains("200"))
                .unwrap_or(false);
            if !status_ok {
                continue;
            }
            if let Some(prop) = propstat.get_child("prop")
                && let Some(elem) = prop.get_child(prop_name)
                && let Some(text) = elem.get_text()
            {
                return Some(text.into_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_datetime_utc() {
        let dt = parse_datetime("20240115T103000Z", &HashMap::new()).unwrap();
        assert!(dt > 0);
    }

    #[test]
    fn test_parse_datetime_date_only() {
        let mut params: HashMap<String, Vec<String>> = HashMap::new();
        params.insert("VALUE".into(), vec!["DATE".into()]);
        let dt = parse_datetime("20240115", &params).unwrap();
        assert!(dt > 0);
    }

    #[test]
    fn test_parse_ical_basic() {
        let ical = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:test@example.com\nDTSTART:20240115T100000Z\nDTEND:20240115T110000Z\nSUMMARY:Test Event\nEND:VEVENT\nEND:VCALENDAR";
        let event = &parse_ical_events(ical, 1).unwrap()[0];
        assert_eq!(event.uid, "test@example.com");
        assert_eq!(event.summary, "Test Event");
        assert!(!event.all_day);
    }

    #[test]
    fn test_parse_ical_all_day() {
        let ical = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:test2@example.com\nDTSTART;VALUE=DATE:20240115\nDTEND;VALUE=DATE:20240116\nSUMMARY:All Day\nEND:VEVENT\nEND:VCALENDAR";
        let event = &parse_ical_events(ical, 1).unwrap()[0];
        assert!(event.all_day);
    }

    #[test]
    fn multiple_vevents_parsed() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a\r\nDTSTART:20240101T100000Z\r\nDTEND:20240101T110000Z\r\nSUMMARY:A\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:b\r\nDTSTART:20240102T100000Z\r\nDTEND:20240102T110000Z\r\nSUMMARY:B\r\nEND:VEVENT\r\nEND:VCALENDAR";
        let events = parse_ical_events(ics, 1).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].uid, "a");
        assert_eq!(events[1].uid, "b");
    }

    #[test]
    fn malformed_vevents_are_skipped_but_valid_ones_survive() {
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:no uid here\nEND:VEVENT\nBEGIN:VEVENT\nUID:good@example.com\nDTSTART:20240101T100000Z\nSUMMARY:Good\nEND:VEVENT\nEND:VCALENDAR";
        let events = parse_ical_events(ics, 1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "good@example.com");
    }

    #[test]
    fn tzid_datetime_is_converted_to_utc() {
        let mut params: HashMap<String, Vec<String>> = HashMap::new();
        params.insert("TZID".into(), vec!["America/New_York".into()]);
        // 2024-01-15 10:00 in New York (EST, UTC-5) = 15:00 UTC.
        let dt = parse_datetime("20240115T100000", &params).unwrap();
        let expected = parse_datetime("20240115T150000Z", &HashMap::new()).unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn unknown_tzid_falls_back_to_utc() {
        let mut params: HashMap<String, Vec<String>> = HashMap::new();
        params.insert("TZID".into(), vec!["Mars/Olympus_Mons".into()]);
        let dt = parse_datetime("20240115T100000", &params).unwrap();
        let expected = parse_datetime("20240115T100000Z", &HashMap::new()).unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn test_parse_ical_line_without_colon_does_not_panic() {
        // A content line with no ':' separator must not panic the parser.
        let ical = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:malformed@example.com\nDTSTART:20240115T100000Z\nA LINE WITHOUT ANY COLON AT ALL\nEND:VEVENT\nEND:VCALENDAR";
        let event = &parse_ical_events(ical, 1).expect("malformed line is tolerated")[0];
        assert_eq!(event.uid, "malformed@example.com");
    }

    #[test]
    fn test_parse_ical_trailing_colon_line_does_not_panic() {
        // "SUMMARY:" carries an empty value and must not panic the parser.
        let ical = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:empty@example.com\nDTSTART:20240115T100000Z\nSUMMARY:\nEND:VEVENT\nEND:VCALENDAR";
        let event = &parse_ical_events(ical, 1).expect("trailing colon is tolerated")[0];
        assert_eq!(event.uid, "empty@example.com");
        assert_eq!(event.summary, "");
    }

    #[test]
    fn test_parse_ical_folded_lines_unfold() {
        // RFC 5545 §3.1: a line break followed by a space/tab is a fold.
        let ical = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:folded@example.com\nDTSTART:20240115T100000Z\nSUMMARY:Weekly sync\n  meeting\nEND:VEVENT\nEND:VCALENDAR";
        let event = &parse_ical_events(ical, 1).expect("folded lines are unfolded")[0];
        assert_eq!(event.summary, "Weekly sync meeting");
    }
}
