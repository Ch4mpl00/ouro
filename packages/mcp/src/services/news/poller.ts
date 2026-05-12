import { recordSignal } from "../signals";
import { localTime } from "../settings";
import { getKv, setKv } from "./storage";

// Daily digest poller. Once per local-time day, after the configured hour,
// emits a `tech-digest` signal so the agent assembles and posts the daily
// IT digest. We check every minute and use a YYYY-MM-DD watermark so each
// day fires at most once even across restarts.

const TICK_INTERVAL_MS = 60_000;
const LAST_DIGEST_KEY = "last_digest_date";
const SIGNAL_SOURCE = "tech-digest";

function logPrefix(): string {
  return `[${new Date().toISOString()}] [news-poller]`;
}

function targetHour(): number {
  const raw = process.env.TECH_DIGEST_HOUR;
  if (!raw) return 9;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0 || n > 23) return 9;
  return Math.floor(n);
}

function tick(): void {
  const { date: today, hour } = localTime();
  if (getKv(LAST_DIGEST_KEY) === today) return;
  if (hour < targetHour()) return;

  setKv(LAST_DIGEST_KEY, today);
  recordSignal({
    source: SIGNAL_SOURCE,
    content: [
      `Daily tech digest tick for ${today}.`,
      `Compose a personalized IT news digest for the user and post to Telegram.`,
      `Use list_news_headlines first (titles only), pick the items matching the`,
      `interests in the system prompt, then call fetch_article(url) for each pick`,
      `to read the body before summarizing.`,
    ].join("\n"),
  });
  console.log(`${logPrefix()} emitted tech-digest signal for ${today}`);
}

export function startNewsPoller(): void {
  console.log(`${logPrefix()} started (digest at ${targetHour()}:00 local time, daily)`);
  // Bootstrap on first ever run: stamp today's date without emitting, so a
  // restart in the middle of the day doesn't surprise the user with an
  // unscheduled digest. The first real digest fires at NEWS_DIGEST_HOUR
  // tomorrow.
  if (getKv(LAST_DIGEST_KEY) === null) {
    setKv(LAST_DIGEST_KEY, localTime().date);
    console.log(`${logPrefix()} bootstrapped watermark to today, no emit until tomorrow`);
  }
  tick();
  setInterval(tick, TICK_INTERVAL_MS);
}
