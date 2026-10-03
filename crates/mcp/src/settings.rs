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

    pub async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let row = self.db.client().await?.query_opt("SELECT value FROM settings WHERE key = $1", &[&key]).await?;
        Ok(row.map(|r| r.get(0)))
    }

    pub async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.db
            .client()
            .await?
            .execute(
                "INSERT INTO settings (key, value) VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
                &[&key, &value],
            )
            .await?;
        Ok(())
    }

    // ── 2. timezone ──────────────────────────────────────────────────────────

    // Never fails: an unset, unreadable or (hand-edited) unparseable value
    // falls back to UTC, so a poller tick always has a usable zone.
    pub async fn timezone(&self) -> Tz {
        match self.get(TIMEZONE_KEY).await {
            Ok(Some(name)) => name.parse().unwrap_or_else(|_| {
                tracing::warn!(%name, "stored timezone is not a valid IANA name, using UTC");
                Tz::UTC
            }),
            Ok(None) => Tz::UTC,
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "reading timezone failed, using UTC");
                Tz::UTC
            }
        }
    }

    pub async fn set_timezone(&self, tz: Tz) -> anyhow::Result<()> {
        self.set(TIMEZONE_KEY, tz.name()).await
    }

    pub async fn local_time(&self, at: DateTime<Utc>) -> LocalTime {
        local_time(self.timezone().await, at)
    }
}

// Validate before storing, so a bogus name never reaches the pollers.
pub fn parse_timezone(name: &str) -> Result<Tz, String> {
    name.parse().map_err(|_| format!("Invalid time zone specified: {name}"))
}

pub fn local_time(tz: Tz, at: DateTime<Utc>) -> LocalTime {
    let local = at.with_timezone(&tz);
    LocalTime { date: local.format("%Y-%m-%d").to_string(), hm: local.format("%H:%M").to_string(), tz }
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

    #[test]
    fn rejects_bogus_names_and_follows_the_zone() {
        assert!(parse_timezone("Mars/Olympus").is_err());
        let at = Utc.with_ymd_and_hms(2026, 10, 1, 22, 30, 0).unwrap();
        assert_eq!(local_time(parse_timezone("Europe/Kiev").unwrap(), at).display(), "2026-10-02 01:30");
    }

    #[tokio::test]
    async fn defaults_to_utc_and_round_trips() {
        let Some(db) = Db::test().await else { return };
        let s = Settings::new(db);
        assert_eq!(s.timezone().await, Tz::UTC);
        s.set_timezone(chrono_tz::Europe::Kiev).await.unwrap();
        assert_eq!(s.timezone().await, chrono_tz::Europe::Kiev);
    }
}
