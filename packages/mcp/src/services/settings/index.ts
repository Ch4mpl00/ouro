import { getDb } from "../../db/client";

// MCP-side user settings KV. Currently:
//   - `timezone` — IANA name (e.g. "Europe/Kiev"). Drives cron evaluation
//     and "what day is today" calculations for the daily-digest pollers.
//
// Read on every tick (cheap: indexed PK lookup on a tiny table). Mutated
// rarely, usually via the `set_timezone` MCP tool when the user tells the
// agent to change it.

const TIMEZONE_KEY = "timezone";

export function getSetting(key: string): string | null {
  const row = getDb()
    .prepare(`SELECT value FROM settings WHERE key = ?`)
    .get(key) as { value: string } | undefined;
  return row?.value ?? null;
}

export function setSetting(key: string, value: string): void {
  getDb()
    .prepare(
      `INSERT INTO settings (key, value) VALUES (?, ?)
       ON CONFLICT(key) DO UPDATE SET
         value = excluded.value,
         updated_at = datetime('now')`,
    )
    .run(key, value);
}

export function deleteSetting(key: string): void {
  getDb().prepare(`DELETE FROM settings WHERE key = ?`).run(key);
}

// Configured timezone (IANA). Defaults to UTC when unset. Never throws —
// callers can rely on this returning a usable Intl.DateTimeFormat input.
export function getTimezone(): string {
  return getSetting(TIMEZONE_KEY) ?? "UTC";
}

export function setTimezone(tz: string): void {
  // Validate by attempting to format a Date with it; bogus strings throw
  // RangeError. Cheap and avoids storing unusable values.
  new Intl.DateTimeFormat("en-CA", { timeZone: tz }).format(new Date());
  setSetting(TIMEZONE_KEY, tz);
}

// "What time is it for the user" — returns YYYY-MM-DD, hour (0-23), minute
// in the configured timezone. Used by daily-digest pollers that need to
// know "is it past 9:00 in user's local time" / "is this a new day yet".
export function localTime(date: Date = new Date()): {
  date: string;
  hour: number;
  minute: number;
  tz: string;
} {
  const tz = getTimezone();
  const parts = new Intl.DateTimeFormat("en-CA", {
    timeZone: tz,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).formatToParts(date);
  const get = (type: string) => parts.find((p) => p.type === type)?.value ?? "";
  return {
    date: `${get("year")}-${get("month")}-${get("day")}`,
    // "24" can leak through for midnight in some runtimes; clamp.
    hour: Number(get("hour")) % 24,
    minute: Number(get("minute")),
    tz,
  };
}
