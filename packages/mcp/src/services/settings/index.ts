import type { StateDb } from "../../db/state";

// MCP-side user settings KV. Currently:
//   - `timezone` — IANA name (e.g. "Europe/Kiev"). Drives cron evaluation
//     and "what day is today" calculations for the daily-digest pollers.
//
// Read on every tick (cheap: indexed PK lookup on a tiny table). Mutated
// rarely, usually via the `set_timezone` MCP tool when the user tells the
// agent to change it.

const TIMEZONE_KEY = "timezone";

export interface Settings {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<void>;
  // Configured timezone (IANA). Defaults to UTC when unset. Never throws —
  // callers can rely on a usable Intl.DateTimeFormat input.
  timezone(): Promise<string>;
  // Rejects names Intl does not know, so an unusable value is never stored.
  setTimezone(tz: string): Promise<void>;
}

export function createSettings(db: StateDb): Settings {
  const get = async (key: string): Promise<string | null> => {
    const { rows } = await db.pool.query<{ value: string }>("SELECT value FROM settings WHERE key = $1", [key]);
    return rows[0]?.value ?? null;
  };

  const set = async (key: string, value: string): Promise<void> => {
    await db.pool.query(
      `INSERT INTO settings (key, value) VALUES ($1, $2)
       ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()`,
      [key, value],
    );
  };

  return {
    get,
    set,
    timezone: async () => (await get(TIMEZONE_KEY)) ?? "UTC",
    setTimezone: async (tz) => {
      // Bogus names throw RangeError here.
      new Intl.DateTimeFormat("en-CA", { timeZone: tz }).format(new Date());
      await set(TIMEZONE_KEY, tz);
    },
  };
}

// "What time is it for the user" — YYYY-MM-DD, hour (0-23) and minute on the
// wall clock of `tz`.
export function localTime(
  tz: string,
  date: Date = new Date(),
): { date: string; hour: number; minute: number; tz: string } {
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
