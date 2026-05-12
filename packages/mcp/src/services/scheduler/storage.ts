import { getDb } from "../../db/client";

// Persistence layer for the agent-managed task scheduler. Cron expression
// is the only time spec; for one-shot reminders the agent encodes a
// specific minute (e.g. "15 14 12 5 *" = 14:15 on May 12) and we mark the
// task as fired after the first match. Cancel = DELETE.

export interface ScheduledTaskRow {
  id: number;
  cron_expr: string;
  recurring: number; // 0 | 1
  prompt: string;
  last_run_at: number | null;
  created_at: string;
}

export function insertScheduledTask(input: {
  cron_expr: string;
  recurring: boolean;
  prompt: string;
}): ScheduledTaskRow {
  const stmt = getDb().prepare(
    `INSERT INTO scheduled_tasks (cron_expr, recurring, prompt)
     VALUES (?, ?, ?)
     RETURNING id, cron_expr, recurring, prompt, last_run_at, created_at`,
  );
  return stmt.get(input.cron_expr, input.recurring ? 1 : 0, input.prompt) as ScheduledTaskRow;
}

// Tasks that may still fire — recurring (always) or one-shots that haven't
// yet been marked. Used by both the poller (to check what's due) and the
// list_scheduled_tasks tool (to show the user what's queued).
export function listActiveTasks(): ScheduledTaskRow[] {
  return getDb()
    .prepare(
      `SELECT id, cron_expr, recurring, prompt, last_run_at, created_at
         FROM scheduled_tasks
        WHERE recurring = 1 OR last_run_at IS NULL
        ORDER BY id ASC`,
    )
    .all() as ScheduledTaskRow[];
}

export function getScheduledTask(id: number): ScheduledTaskRow | null {
  const row = getDb()
    .prepare(
      `SELECT id, cron_expr, recurring, prompt, last_run_at, created_at
         FROM scheduled_tasks WHERE id = ?`,
    )
    .get(id) as ScheduledTaskRow | undefined;
  return row ?? null;
}

// Stamp the fire timestamp. For one-shot tasks this also takes them out
// of `listActiveTasks` (the WHERE clause filters them by `last_run_at IS
// NULL`). For recurring tasks it just records when they last fired so the
// poller knows what cron-slot to compute "next" from.
export function markTaskFired(id: number, firedAt: number): void {
  getDb()
    .prepare(`UPDATE scheduled_tasks SET last_run_at = ? WHERE id = ?`)
    .run(firedAt, id);
}

export function deleteScheduledTask(id: number): boolean {
  const info = getDb().prepare(`DELETE FROM scheduled_tasks WHERE id = ?`).run(id);
  return info.changes > 0;
}
