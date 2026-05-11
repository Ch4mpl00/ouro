import { getDb } from "../../db/client";
import { recordSignal } from "../signals";

// Daily news-digest poller. Fires the curated topical news signal that
// pulls posts from the user's subscribed Telegram channels (via gramjs
// userbot) and filters them down to the topics defined in
// `skills/news-digest.md` (Odessa / PMR / RU-UA conflict / World).
//
// Distinct from `channel-digest` (raw per-channel summary, no topic
// filtering) and `tech-digest` (HN/Habr IT news).

const TICK_INTERVAL_MS = 60_000;
const LAST_FIRE_DATE_KEY = "last_fire_date";
const SIGNAL_SOURCE = "news-digest";

function logPrefix(): string {
  return `[${new Date().toISOString()}] [news-digest-poller]`;
}

function targetHour(): number {
  const raw = process.env.NEWS_DIGEST_HOUR;
  if (!raw) return 9;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0 || n > 23) return 9;
  return Math.floor(n);
}

function todayLocalDate(): string {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

function getKv(key: string): string | null {
  const row = getDb()
    .prepare(`SELECT value FROM news_digest_kv WHERE key = ?`)
    .get(key) as { value: string } | undefined;
  return row?.value ?? null;
}

function setKv(key: string, value: string): void {
  getDb()
    .prepare(
      `INSERT INTO news_digest_kv (key, value) VALUES (?, ?)
       ON CONFLICT(key) DO UPDATE SET value = excluded.value`,
    )
    .run(key, value);
}

function tick(): void {
  const today = todayLocalDate();
  if (getKv(LAST_FIRE_DATE_KEY) === today) return;
  if (new Date().getHours() < targetHour()) return;

  setKv(LAST_FIRE_DATE_KEY, today);
  recordSignal({
    source: SIGNAL_SOURCE,
    content: [
      `Daily news-digest tick for ${today}.`,
      `Read posts from the user's subscribed Telegram channels over the`,
      `last 24h, filter to the user's interests (see skill), and post a`,
      `topical digest grouped by region/theme to Telegram.`,
    ].join("\n"),
  });
  console.log(`${logPrefix()} emitted news-digest signal for ${today}`);
}

export function startNewsDigestPoller(): void {
  console.log(`${logPrefix()} started (digest at ${targetHour()}:00 local time, daily)`);
  if (getKv(LAST_FIRE_DATE_KEY) === null) {
    setKv(LAST_FIRE_DATE_KEY, todayLocalDate());
    console.log(`${logPrefix()} bootstrapped watermark to today, no emit until tomorrow`);
  }
  tick();
  setInterval(tick, TICK_INTERVAL_MS);
}
