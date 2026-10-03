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
  // Stamps the slot fired for. For a one-shot this also retires it.
  markFired(id: number, slotUnix: number): Promise<void>;
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

    markFired: async (id, slotUnix) => {
      await db.pool.query("UPDATE scheduled_tasks SET last_run_at = $1 WHERE id = $2", [slotUnix, id]);
    },

    delete: async (id) => {
      const { rowCount } = await db.pool.query("DELETE FROM scheduled_tasks WHERE id = $1", [id]);
      return (rowCount ?? 0) > 0;
    },
  };
}
