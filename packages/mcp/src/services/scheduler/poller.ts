import { CronExpressionParser } from "cron-parser";
import { recordSignal } from "../signals";
import { getTimezone } from "../settings";
import {
  listActiveTasks,
  markTaskFired,
  type ScheduledTaskRow,
} from "./storage";

// Scheduler tick. Every 30s scans active tasks, evaluates each cron in the
// configured timezone, and enqueues a signal for every task whose
// next-fire time has slipped past now. Robust against poller restarts:
// the cron-next is computed from `last_run_at` (or `created_at` for
// never-fired tasks), so a task can't double-fire for the same slot.
//
// A task's `source` column controls which signal source is emitted:
//   - NULL → 'scheduler' (user-created reminders; scheduler.md skill)
//   - 'news-digest' / 'dreaming' / 'tech-digest' → the respective skill
// This makes the scheduler the single mechanism for any time-triggered
// signal. The hardcoded daily pollers (news-digest, dreaming, tech-digest)
// that used to live in their own service dirs are now just seeded rows
// in `scheduled_tasks` (see db/client.ts seedSystemTasks).

const TICK_INTERVAL_MS = 30_000;
const DEFAULT_SIGNAL_SOURCE = "scheduler";

function logPrefix(): string {
  return `[${new Date().toISOString()}] [scheduler-poller]`;
}

function parsedAnchor(task: ScheduledTaskRow): Date {
  // Anchor: when we last fired (or, never-fired, when the task was created).
  // cron-parser computes `next(after=anchor)` — we then check if that next
  // slot is already in the past relative to wall clock, which means we owe
  // a fire.
  if (task.last_run_at != null) return new Date(task.last_run_at * 1000);
  // created_at is stored as "YYYY-MM-DD HH:MM:SS" UTC by SQLite datetime('now')
  return new Date(`${task.created_at.replace(" ", "T")}Z`);
}

function tick(): void {
  const tz = getTimezone();
  const now = Date.now();
  const tasks = listActiveTasks();

  for (const task of tasks) {
    let nextSlotMs: number;
    try {
      const it = CronExpressionParser.parse(task.cron_expr, {
        tz,
        currentDate: parsedAnchor(task),
      });
      nextSlotMs = it.next().toDate().getTime();
    } catch (err) {
      console.warn(
        `${logPrefix()} task #${task.id} has invalid cron '${task.cron_expr}': ${(err as Error).message}`,
      );
      continue;
    }
    if (nextSlotMs > now) continue;

    // Snapshot previous fire BEFORE we stamp the new slot, so source
    // skills (e.g. dreaming) can scope `since=<previous fire>` from the
    // signal header.
    const previousIso =
      task.last_run_at != null ? new Date(task.last_run_at * 1000).toISOString() : null;

    // The slot we're firing for is `nextSlotMs` — that goes into
    // last_run_at so the *following* cron lookup advances correctly even
    // if our wall clock is several minutes past the slot.
    const scheduledForSec = Math.floor(nextSlotMs / 1000);
    markTaskFired(task.id, scheduledForSec);

    const source = task.source ?? DEFAULT_SIGNAL_SOURCE;
    const content = renderContent(task, previousIso, nextSlotMs);
    recordSignal({ source, content });
    console.log(
      `${logPrefix()} fired task #${task.id} source=${source} (slot ${new Date(nextSlotMs).toISOString()}, ${task.recurring ? "recurring" : "one-shot"})`,
    );
  }
}

// Signal content. A short header carries the cron metadata + previous
// fire (for skills that scope `since=...` from it), then the task's own
// prompt body verbatim. Source-specific skills (news-digest, dreaming,
// tech-digest) parse the header lines they need; the scheduler skill
// (default source) treats the whole thing as a free-form trigger.
function renderContent(task: ScheduledTaskRow, previousIso: string | null, slotMs: number): string {
  const lines = [
    `Scheduled task #${task.id} fired.`,
    `Cron: ${task.cron_expr}`,
    `Slot: ${new Date(slotMs).toISOString()}`,
    `Now: ${new Date().toISOString()}`,
    `Previous fire: ${previousIso ?? "never (this is the first run)"}`,
    `Recurring: ${task.recurring === 1 ? "yes" : "no (one-shot)"}`,
    ``,
    task.prompt,
  ];
  return lines.join("\n");
}

export function startSchedulerPoller(): void {
  console.log(`${logPrefix()} started (tick every ${TICK_INTERVAL_MS / 1000}s, tz=${getTimezone()})`);
  tick();
  setInterval(tick, TICK_INTERVAL_MS);
}
