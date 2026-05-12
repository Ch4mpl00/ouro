import { CronExpressionParser } from "cron-parser";
import { recordSignal } from "../signals";
import { getTimezone } from "../settings";
import {
  listActiveTasks,
  markTaskFired,
  type ScheduledTaskRow,
} from "./storage";

// Scheduler tick. Every 30s scans active tasks, evaluates each cron in the
// configured timezone, and enqueues a `scheduler` signal for every task
// whose next-fire time has slipped past now. Robust against poller
// restarts: the cron-next is computed from `last_run_at` (or `created_at`
// for never-fired tasks), so a task can't double-fire for the same slot.

const TICK_INTERVAL_MS = 30_000;
const SIGNAL_SOURCE = "scheduler";

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

    // The slot we're firing for is `nextSlotMs` — that goes into
    // last_run_at so the *following* cron lookup advances correctly even
    // if our wall clock is several minutes past the slot.
    const scheduledForSec = Math.floor(nextSlotMs / 1000);
    markTaskFired(task.id, scheduledForSec);

    const content = JSON.stringify({
      task_id: task.id,
      prompt: task.prompt,
      scheduled_for: new Date(nextSlotMs).toISOString(),
      cron_expr: task.cron_expr,
      recurring: task.recurring === 1,
    });
    recordSignal({ source: SIGNAL_SOURCE, content });
    console.log(
      `${logPrefix()} fired task #${task.id} (slot ${new Date(nextSlotMs).toISOString()}, ${task.recurring ? "recurring" : "one-shot"})`,
    );
  }
}

export function startSchedulerPoller(): void {
  console.log(`${logPrefix()} started (tick every ${TICK_INTERVAL_MS / 1000}s, tz=${getTimezone()})`);
  tick();
  setInterval(tick, TICK_INTERVAL_MS);
}
