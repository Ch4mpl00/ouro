// User-facing settings KV (`settings` table). Today it holds `timezone`, the
// IANA name every cron evaluation and "what day is it" decision reads. Read
// on every tick — a PK lookup on a tiny table — so a `set_timezone` takes
// effect on the next scheduler tick without a restart.
//
// Sections:
//   1. store    — raw get/set
//   2. timezone — validated IANA tz + the user's local wall clock

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use rusqlite::{OptionalExtension, params};

use crate::db::Db;

// ── 1. store ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Settings {
    db: Db,
}

const TIMEZONE_KEY: &str = "timezone";

impl Settings {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn get(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.db.conn().query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    pub fn set(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.db.conn().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
            params![key, value],
        )?;
        Ok(())
    }

    // ── 2. timezone ──────────────────────────────────────────────────────────

    // Never fails: an unset, unreadable or (hand-edited) unparseable value
    // falls back to UTC, so a poller tick always has a usable zone.
    pub fn timezone(&self) -> Tz {
        match self.get(TIMEZONE_KEY) {
            Ok(Some(name)) => name.parse().unwrap_or_else(|_| {
                tracing::warn!(%name, "stored timezone is not a valid IANA name, using UTC");
                Tz::UTC
            }),
            Ok(None) => Tz::UTC,
            Err(err) => {
                tracing::warn!(%err, "reading timezone failed, using UTC");
                Tz::UTC
            }
        }
    }

    // Validates before storing, so a bogus name never reaches the pollers.
    pub fn set_timezone(&self, name: &str) -> Result<Tz, SetTimezoneError> {
        let tz: Tz = name.parse().map_err(|_| SetTimezoneError::Invalid(name.to_owned()))?;
        self.set(TIMEZONE_KEY, tz.name())?;
        Ok(tz)
    }

    pub fn local_time(&self, at: DateTime<Utc>) -> LocalTime {
        let tz = self.timezone();
        let local = at.with_timezone(&tz);
        LocalTime { date: local.format("%Y-%m-%d").to_string(), hm: local.format("%H:%M").to_string(), tz }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SetTimezoneError {
    #[error("Invalid time zone specified: {0}")]
    Invalid(String),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

pub struct LocalTime {
    // YYYY-MM-DD in the user's zone.
    pub date: String,
    // HH:MM, 24h.
    pub hm: String,
    pub tz: Tz,
}

impl LocalTime {
    // The `local_now` shape the timezone tools return: "2026-10-02 09:05".
    pub fn display(&self) -> String {
        format!("{} {}", self.date, self.hm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn settings() -> Settings {
        Settings::new(Db::open_in_memory().unwrap())
    }

    #[test]
    fn defaults_to_utc_and_rejects_bogus_names() {
        let s = settings();
        assert_eq!(s.timezone(), Tz::UTC);
        assert!(matches!(s.set_timezone("Mars/Olympus"), Err(SetTimezoneError::Invalid(_))));
        assert_eq!(s.get(TIMEZONE_KEY).unwrap(), None);
    }

    #[test]
    fn local_time_follows_the_configured_zone() {
        let s = settings();
        s.set_timezone("Europe/Kiev").unwrap();
        let at = Utc.with_ymd_and_hms(2026, 10, 1, 22, 30, 0).unwrap();
        let local = s.local_time(at);
        assert_eq!(local.display(), "2026-10-02 01:30");
    }
}
