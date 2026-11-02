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
        let method = method.parse::<Method>().unwrap();
        let mut req = http::Request::builder()
            .method(method)
            .uri(url)
            .header("Authorization", basic_auth(&self.username, &self.password));
        if let Some(ct) = content_type {
            req = req.header("Content-Type", ct);
        }
        let req = if let Some(b) = body {
            req.body(b.to_string()).unwrap()
        } else {
            req.body(String::new()).unwrap()
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
    pub fn discover_calendar_home(&self) -> Result<String> {
        let url = format!("{}/.well-known/caldav", self.base_url);
        let body = self.request("GET", &url, None, None)?;
        // Follow redirect if present (simple handling)
        if body.contains("Location:") {
            for line in body.lines() {
                if line.starts_with("Location:") {
                    let loc = line["Location:".len()..].trim();
                    if loc.starts_with("http") {
                        return Ok(loc.to_string());
                    }
                    return Ok(format!(
                        "{}/{}",
                        self.base_url.trim_end_matches('/'),
                        loc.trim_start_matches('/')
                    ));
                }
            }
        }
        Ok(format!("{}/calendars/{}", self.base_url, self.username))
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
        let xml = self.request(
            "PROPFIND",
            home_url,
            Some(body),
            Some("application/xml; charset=utf-8"),
        )?;
        parse_calendar_multistatus(&xml, home_url)
    }

    /// Sync a calendar (get events with ETag support).
    pub fn sync_calendar(&self, calendar_url: &str, ctag: Option<&str>) -> Result<Vec<EventInfo>> {
        let body = if let Some(ctag_val) = ctag {
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
</D:propfind>"#
            )
        } else {
            // Full sync: request all events
            r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
</D:propfind>"#
                .to_string()
        };
        let xml = self.request(
            "REPORT",
            calendar_url,
            Some(&body),
            Some("application/xml; charset=utf-8"),
        )?;
        parse_event_multistatus(&xml)
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
    pub href: String,
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
            events.push(EventInfo { href, etag, ical });
        }
    }
    Ok(events)
}

/// Parse iCalendar data into a CalendarEvent.
pub fn parse_ical_event(ical_data: &str, calendar_id: i64) -> Result<CalendarEvent> {
    let lines: Vec<&str> = ical_data.lines().collect();
    let mut i = 0;
    let mut in_vevent = false;
    let mut uid = String::new();
    let mut summary = String::new();
    let mut description = String::new();
    let mut location = String::new();
    let mut start = 0i64;
    let mut end = 0i64;
    let mut all_day = false;
    let mut rrule = None;
    let mut recurrence_id = None;
    let mut dtstart_params: HashMap<String, Vec<String>> = HashMap::new();
    let mut params: HashMap<String, Vec<String>> = HashMap::new();

    while i < lines.len() {
        let line = lines[i];
        // Handle line folding
        let mut full_line = line.to_string();
        i += 1;
        while i < lines.len() && (lines[i].starts_with(' ') || lines[i].starts_with('\t')) {
            full_line.push_str(&lines[i][1..]);
            i += 1;
        }

        if full_line == "BEGIN:VEVENT" {
            in_vevent = true;
            continue;
        }
        if full_line == "END:VEVENT" {
            in_vevent = false;
            break;
        }
        if !in_vevent {
            continue;
        }

        // Parse property with parameters
        let (name, value, prop_params) = parse_ical_property(&full_line);
        params = prop_params;

        match name.as_str() {
            "UID" => uid = value,
            "SUMMARY" => summary = value,
            "DESCRIPTION" => description = value,
            "LOCATION" => location = value,
            "DTSTART" => {
                start = parse_datetime(&value, &params).unwrap_or(0);
                dtstart_params = params.clone();
            }
            "DTEND" => end = parse_datetime(&value, &params).unwrap_or(0),
            "RRULE" => rrule = Some(value),
            "RECURRENCE-ID" => recurrence_id = Some(value),
            _ => {}
        }
    }

    if uid.is_empty() {
        return Err(Error::Protocol("Event missing UID".into()));
    }
    if start == 0 {
        return Err(Error::Protocol("Event missing DTSTART".into()));
    }

    // Determine if all-day
    all_day = dtstart_params
        .get("VALUE")
        .is_some_and(|v| v.iter().any(|val| val == "DATE"));

    Ok(CalendarEvent {
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
    })
}

fn parse_ical_property(line: &str) -> (String, String, HashMap<String, Vec<String>>) {
    let mut params: HashMap<String, Vec<String>> = HashMap::new();
    let colon = line.find(':').unwrap_or(line.len());
    let (name_part, value) = line.split_at(colon);
    let value = &value[1..]; // skip ':'

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

    if value.ends_with('Z') {
        NaiveDateTime::parse_from_str(&value[..value.len() - 1], "%Y%m%dT%H%M%S")
            .ok()
            .map(|dt| Utc.from_utc_datetime(&dt).timestamp())
    } else if is_date || value.len() == 8 {
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
        let event = parse_ical_event(ical, 1).unwrap();
        assert_eq!(event.uid, "test@example.com");
        assert_eq!(event.summary, "Test Event");
        assert!(!event.all_day);
    }

    #[test]
    fn test_parse_ical_all_day() {
        let ical = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:test2@example.com\nDTSTART;VALUE=DATE:20240115\nDTEND;VALUE=DATE:20240116\nSUMMARY:All Day\nEND:VEVENT\nEND:VCALENDAR";
        let event = parse_ical_event(ical, 1).unwrap();
        assert!(event.all_day);
    }
}
