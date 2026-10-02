//! The history database, schema v3 (v3 added `calls.emergency` and `calls.first_voice_ms`; `target`
//! is `unit` for a unit-to-unit call).
//!
//! - `calls`: every finished call, followed or not, with its site; `transmissions`: the radios
//!   heard in it, the primary (the grant's radio, else the voice's) first.
//! - `tg_hour` / `radio_hour`: totals per hour (t: its start, UTC), kept in the call's
//!   transaction, so a month on a busy site reads a few thousand rows, not 300k calls.
//!   `voiced_grant_ms` is the grant time of the calls with decoded voice (for voice per grant).
//! - `talkgroups` / `radios`: first and last seen, per system (IDs are system-wide).
//! - `radio_events`: affiliations and registrations per (site, radio, talkgroup).
//! - `recordings`: the WAV files, linked to their call.
//!
//! Two kinds of time are never added as if they were one: voice (decoded, measured, followed
//! calls only) and grant (the grant to its last update on the control channel; the only time
//! known for a call that was not followed, credited to the radio granted the channel).

pub const VERSION: u32 = 3;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS systems (id TEXT PRIMARY KEY, protocol TEXT NOT NULL, label TEXT, identity TEXT);
CREATE TABLE IF NOT EXISTS sites (
    id TEXT PRIMARY KEY,
    system TEXT REFERENCES systems(id),
    label TEXT,
    identity TEXT,
    calls INTEGER NOT NULL DEFAULT 0,
    first_ms INTEGER,
    last_ms INTEGER
);
CREATE TABLE IF NOT EXISTS calls (
    id INTEGER PRIMARY KEY,
    site TEXT NOT NULL,
    call_id INTEGER NOT NULL,
    started_ms INTEGER NOT NULL,
    ended_ms INTEGER NOT NULL,
    target TEXT NOT NULL DEFAULT 'group',
    tg INTEGER NOT NULL,
    source INTEGER,
    freq_hz INTEGER,
    channel TEXT,
    timeslot INTEGER,
    lane INTEGER NOT NULL DEFAULT 0,
    encrypted INTEGER NOT NULL,
    enc_alg INTEGER,
    enc_key INTEGER,
    followed INTEGER NOT NULL,
    not_followed TEXT,
    voice_ms INTEGER NOT NULL,
    grant_ms INTEGER NOT NULL,
    codec TEXT,
    frames INTEGER NOT NULL DEFAULT 0,
    frame_errors INTEGER NOT NULL DEFAULT 0,
    close_reason TEXT,
    end_kind TEXT,
    emergency INTEGER NOT NULL DEFAULT 0,
    first_voice_ms INTEGER,
    UNIQUE(site, call_id, started_ms)
);
CREATE INDEX IF NOT EXISTS calls_site_time ON calls(site, started_ms);
CREATE INDEX IF NOT EXISTS calls_site_tg ON calls(site, tg, started_ms);
CREATE INDEX IF NOT EXISTS calls_time ON calls(started_ms);
CREATE INDEX IF NOT EXISTS calls_call_id ON calls(call_id);
CREATE TABLE IF NOT EXISTS transmissions (
    call INTEGER NOT NULL REFERENCES calls(id) ON DELETE CASCADE,
    site TEXT NOT NULL,
    unit INTEGER NOT NULL,
    started_ms INTEGER NOT NULL,
    ended_ms INTEGER,
    voice_ms INTEGER,
    primary_src INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS transmissions_site_unit ON transmissions(site, unit, started_ms);
CREATE INDEX IF NOT EXISTS transmissions_call ON transmissions(call);
CREATE TABLE IF NOT EXISTS talkgroups (
    system TEXT NOT NULL,
    tg INTEGER NOT NULL,
    first_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL,
    calls INTEGER NOT NULL,
    first_encrypted_ms INTEGER,
    last_encrypted_ms INTEGER,
    last_clear_ms INTEGER,
    PRIMARY KEY(system, tg)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS radios (
    system TEXT NOT NULL,
    unit INTEGER NOT NULL,
    first_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL,
    calls INTEGER NOT NULL,
    PRIMARY KEY(system, unit)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS radio_events (
    site TEXT NOT NULL,
    unit INTEGER NOT NULL,
    tg INTEGER NOT NULL,
    kind TEXT NOT NULL,
    first_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY(site, unit, tg, kind)
);
CREATE TABLE IF NOT EXISTS tg_hour (
    site TEXT NOT NULL,
    t INTEGER NOT NULL,
    tg INTEGER NOT NULL,
    calls INTEGER NOT NULL,
    followed INTEGER NOT NULL,
    encrypted INTEGER NOT NULL,
    voice_ms INTEGER NOT NULL,
    clear_grant_ms INTEGER NOT NULL,
    enc_grant_ms INTEGER NOT NULL,
    voiced_grant_ms INTEGER NOT NULL,
    first_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL,
    PRIMARY KEY(site, t, tg)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS radio_hour (
    site TEXT NOT NULL,
    t INTEGER NOT NULL,
    unit INTEGER NOT NULL,
    tg INTEGER NOT NULL,
    calls INTEGER NOT NULL,
    encrypted INTEGER NOT NULL,
    voice_ms INTEGER NOT NULL,
    clear_grant_ms INTEGER NOT NULL,
    enc_grant_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL,
    PRIMARY KEY(site, t, unit, tg)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS radio_hour_unit ON radio_hour(site, unit, t);
CREATE TABLE IF NOT EXISTS recordings (
    id INTEGER PRIMARY KEY,
    file TEXT NOT NULL UNIQUE,
    store TEXT NOT NULL,
    site TEXT,
    call_id INTEGER,
    started_ms INTEGER NOT NULL,
    tg INTEGER,
    source INTEGER,
    bytes INTEGER NOT NULL,
    duration_ms INTEGER,
    call INTEGER REFERENCES calls(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS recordings_call_id ON recordings(call_id);
";

/// Columns a later schema added to a table, for a database an earlier one created.
pub const ADDED: &[(&str, &str, &str)] = &[
    ("calls", "emergency", "INTEGER NOT NULL DEFAULT 0"),
    ("calls", "first_voice_ms", "INTEGER"),
];
