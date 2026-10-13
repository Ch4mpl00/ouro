import { CronExpressionParser } from "cron-parser";
import type { Signals } from "../signals";
import type { Settings } from "../settings";
import type { ScheduledTaskRow, ScheduledTasks } from "./storage";

// Scheduler tick. Every 30s scans active tasks, evaluates each cron in the
// configured timezone, and enqueues a signal for every task whose
// next-fire time has slipped past now. Robust against poller restarts:
// the cron-next is computed from `last_run_at` (or `created_at` for
// never-fired tasks), so a task can't double-fire for the same slot.
//
// A task's `source` column controls which signal source is emitted:
//   - NULL → 'scheduler' (user-created reminders; scheduler.md skill)
//   - 'dreaming' → the respective skill
// This makes the scheduler the single mechanism for any time-triggered
// signal; the system digests and dreaming are seeded rows in
// `scheduled_tasks` (see db/state.ts).

const TICK_INTERVAL_MS = 30_000;
const DEFAULT_SIGNAL_SOURCE = "scheduler";

export interface SchedulerDeps {
  tasks: ScheduledTasks;
  settings: Settings;
  signals: Signals;
}

function logPrefix(): string {
  return `[${new Date().toISOString()}] [scheduler-poller]`;
}

// The slot a task's next fire is computed from: the slot it last fired for,
// or — never fired — its creation time.
function anchor(task: ScheduledTaskRow): Date {
  if (task.last_run_at != null) return new Date(task.last_run_at * 1000);
  return new Date(`${task.created_at.replace(" ", "T")}Z`);
}

// Fires every task whose next slot (after its anchor) has passed, one slot
// per task per tick: a late tick still advances to the following slot,
// never skips or repeats one. Returns how many fired.
export async function tick(deps: SchedulerDeps, now: Date = new Date()): Promise<number> {
  const tz = await deps.settings.timezone();
  let fired = 0;
  for (const task of await deps.tasks.listActive()) {
    let slot: Date;
    try {
      slot = CronExpressionParser.parse(task.cron_expr, { tz, currentDate: anchor(task) }).next().toDate();
    } catch (err) {
      console.warn(`${logPrefix()} task #${task.id} has invalid cron '${task.cron_expr}': ${(err as Error).message}`);
      continue;
    }
    if (slot.getTime() > now.getTime()) continue;

    // Snapshot the previous fire BEFORE stamping the new slot, so source
    // skills (e.g. dreaming) can scope `since=<previous fire>` from the
    // signal header.
    const previousIso = task.last_run_at != null ? new Date(task.last_run_at * 1000).toISOString() : null;

    // The slot (not the wall clock) goes into last_run_at, so the following
    // cron lookup advances correctly even when this tick is late. Claimed
    // with a compare-and-set: an overlapping tick that got there first, or a
    // cancel since listActive, means this tick doesn't fire it.
    const slotUnix = Math.floor(slot.getTime() / 1000);
    if (!(await deps.tasks.swapLastRun(task.id, task.last_run_at, slotUnix))) continue;

    const source = task.source ?? DEFAULT_SIGNAL_SOURCE;
    try {
      await deps.signals.record({ source, content: renderContent(task, previousIso, slot, now) });
    } catch (err) {
      // Give the slot back so the next tick fires it, rather than leaving it
      // marked as done with no signal.
      await deps.tasks.swapLastRun(task.id, slotUnix, task.last_run_at);
      throw err;
    }
    console.log(
      `${logPrefix()} fired task #${task.id} source=${source} (slot ${slot.toISOString()}, ${task.recurring ? "recurring" : "one-shot"})`,
    );
    fired++;
  }
  return fired;
}

// Signal content. A short header carries the cron metadata + previous
// fire (for skills that scope `since=...` from it), then the task's own
// prompt body verbatim.
function renderContent(task: ScheduledTaskRow, previousIso: string | null, slot: Date, now: Date): string {
  const lines = [
    `Scheduled task #${task.id} fired.`,
    `Cron: ${task.cron_expr}`,
    `Slot: ${slot.toISOString()}`,
    `Now: ${now.toISOString()}`,
    `Previous fire: ${previousIso ?? "never (this is the first run)"}`,
    `Recurring: ${task.recurring === 1 ? "yes" : "no (one-shot)"}`,
    ``,
    task.prompt,
  ];
  return lines.join("\n");
}

export function startSchedulerPoller(deps: SchedulerDeps): void {
  console.log(`${logPrefix()} started (tick every ${TICK_INTERVAL_MS / 1000}s)`);
  const run = (): void => {
    tick(deps).catch((err) => console.error(`${logPrefix()} tick failed:`, err instanceof Error ? err.message : err));
  };
  run();
  setInterval(run, TICK_INTERVAL_MS);
}
