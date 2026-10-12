import { sqlTime, type StateDb } from "../../db/state";

// Persistence layer for the agent-managed task scheduler. Cron expression
// is the only time spec; for one-shot reminders the agent encodes a
// specific minute (e.g. "15 14 12 5 *" = 14:15 on May 12) and we mark the
// task as fired after the first match. Cancel = DELETE.

export interface ScheduledTaskRow {
  id: number;
  cron_expr: string;
  // 0 | 1 — schedule_task has always returned the raw sqlite row.
  recurring: number;
  prompt: string;
  source: string | null; // null → signal source 'scheduler' (user-created)
  // Unix seconds of the slot last fired for.
  last_run_at: number | null;
  // "YYYY-MM-DD HH:MM:SS", UTC.
  created_at: string;
}

export interface ScheduledTasks {
  insert(input: { cron_expr: string; recurring: boolean; prompt: string; source?: string | null }): Promise<ScheduledTaskRow>;
  // Tasks that may still fire — recurring (always) or one-shots that haven't
  // fired yet. Used by both the poller and list_scheduled_tasks.
  listActive(): Promise<ScheduledTaskRow[]>;
  // Compare-and-set of the slot last fired for: moves last_run_at from
  // `expected` to `next` only while it still is `expected` and the task still
  // exists. The poller claims a slot with it (previous → slot), so
  // overlapping ticks or a cancel in between can't fire it as well, and
  // gives a claim back (slot → previous) when the signal couldn't be
  // recorded. Stamping a slot on a one-shot also retires it.
  swapLastRun(id: number, expected: number | null, next: number | null): Promise<boolean>;
  delete(id: number): Promise<boolean>;
}

const COLUMNS = `id, cron_expr, recurring::int AS recurring, prompt, source, last_run_at,
                 ${sqlTime("created_at")} AS created_at`;

export function createScheduledTasks(db: StateDb): ScheduledTasks {
  return {
    insert: async ({ cron_expr, recurring, prompt, source }) => {
      const { rows } = await db.pool.query<ScheduledTaskRow>(
        `INSERT INTO scheduled_tasks (cron_expr, recurring, prompt, source)
         VALUES ($1, $2, $3, $4) RETURNING ${COLUMNS}`,
        [cron_expr, recurring, prompt, source ?? null],
      );
      return rows[0]!;
    },

    listActive: async () => {
      const { rows } = await db.pool.query<ScheduledTaskRow>(
        `SELECT ${COLUMNS} FROM scheduled_tasks
          WHERE recurring OR last_run_at IS NULL
          ORDER BY id ASC`,
      );
      return rows;
    },

    swapLastRun: async (id, expected, next) => {
      const { rowCount } = await db.pool.query(
        "UPDATE scheduled_tasks SET last_run_at = $1 WHERE id = $2 AND last_run_at IS NOT DISTINCT FROM $3",
        [next, id, expected],
      );
      return (rowCount ?? 0) > 0;
    },

    delete: async (id) => {
      const { rowCount } = await db.pool.query("DELETE FROM scheduled_tasks WHERE id = $1", [id]);
      return (rowCount ?? 0) > 0;
    },
  };
}
