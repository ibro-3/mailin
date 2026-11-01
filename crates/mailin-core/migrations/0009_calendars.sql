-- CalDAV calendar integration.
-- Calendars are synced from CalDAV servers (Nextcloud, Google, etc.).
-- Events are stored locally for offline browsing and RSVP.

CREATE TABLE calendars (
    id              INTEGER PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    href            TEXT NOT NULL,                    -- CalDAV calendar URL
    display_name    TEXT NOT NULL,
    color           TEXT,                             -- Hex color like #FF5733
    ctag            TEXT,                             -- Calendar sync token
    last_synced_at  INTEGER,
    UNIQUE (account_id, href)
) STRICT;

CREATE INDEX idx_calendars_account ON calendars(account_id);

CREATE TABLE events (
    id              INTEGER PRIMARY KEY,
    calendar_id     INTEGER NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    uid             TEXT NOT NULL,                    -- iCalendar UID
    summary         TEXT NOT NULL DEFAULT '',
    start_at        INTEGER NOT NULL,                 -- Unix timestamp (UTC)
    end_at          INTEGER NOT NULL,                 -- Unix timestamp (UTC)
    all_day         INTEGER NOT NULL DEFAULT 0,       -- Boolean
    location        TEXT,
    description     TEXT,
    attendees_json  TEXT NOT NULL DEFAULT '[]',       -- JSON array of attendee objects
    ics_raw         TEXT NOT NULL DEFAULT '',         -- Raw iCalendar for exact replies
    etag            TEXT,                             -- For incremental sync
    UNIQUE (calendar_id, uid)
) STRICT;

CREATE INDEX idx_events_calendar ON events(calendar_id);
CREATE INDEX idx_events_time ON events(start_at, end_at);
CREATE INDEX idx_events_uid ON events(uid);