import { recordSignal } from "../signals";
import { getKv, setKv, getLastDreamingAt } from "./storage";

// Daily dreaming poller. Once per local-time day, after the configured
// hour, fires a `dreaming` signal so the agent reviews recent activity
// and updates its skill files. Same watermark pattern as news-poller —
// at most once per day across restarts.
//
// The signal carries the previous `last_dreaming_at` ISO timestamp; the
// dreaming skill uses it to scope `list_signals(since=<ts>)` and decide
// what to reflect on. The watermark is advanced AFTER the signal fires
// (not after the LLM finishes) so a crash mid-dream doesn't loop forever.

const TICK_INTERVAL_MS = 60_000;
const LAST_FIRE_DATE_KEY = "last_fire_date";
const SIGNAL_SOURCE = "dreaming";

function logPrefix(): string {
  return `[${new Date().toISOString()}] [dreaming-poller]`;
}

function targetHour(): number {
  const raw = process.env.DREAMING_HOUR;
  if (!raw) return 4;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0 || n > 23) return 4;
  return Math.floor(n);
}

function todayLocalDate(): string {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

function tick(): void {
  const today = todayLocalDate();
  if (getKv(LAST_FIRE_DATE_KEY) === today) return;
  if (new Date().getHours() < targetHour()) return;

  const since = getLastDreamingAt();
  const nowIso = new Date().toISOString();

  setKv(LAST_FIRE_DATE_KEY, today);

  const content = [
    `Dreaming tick for ${today}.`,
    `Last dreaming was at: ${since ?? "never (this is the first reflection)"}.`,
    `Now is: ${nowIso}.`,
    ``,
    `Review the signals processed since the last dreaming and consider`,
    `whether any skill files (skills/<source>.md) deserve an edit based on`,
    `patterns, recurring user feedback, or failure modes you observed.`,
    `Use list_signals(since=<above timestamp>) to scope the review.`,
    `Edit skills directly via write_skill when an improvement is warranted.`,
    `When done, call set_last_dreaming_at(now) so the next dream knows the cutoff.`,
  ].join("\n");

  recordSignal({ source: SIGNAL_SOURCE, content });
  console.log(`${logPrefix()} emitted dreaming signal for ${today} (since=${since ?? "n/a"})`);
}

export function startDreamingPoller(): void {
  console.log(`${logPrefix()} started (dream at ${targetHour()}:00 local time, daily)`);
  if (getKv(LAST_FIRE_DATE_KEY) === null) {
    setKv(LAST_FIRE_DATE_KEY, todayLocalDate());
    console.log(`${logPrefix()} bootstrapped watermark to today, no emit until tomorrow`);
  }
  tick();
  setInterval(tick, TICK_INTERVAL_MS);
}
