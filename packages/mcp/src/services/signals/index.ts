import { getDb } from "../../db/client";

// In-MCP signal queue. Pollers (Telegram, Gmail, cron, webhooks) enqueue
// rows; the agent's loop consumes them one at a time via get_next_signal.
//
// Agent never polls external systems directly — every signal it sees has
// been built and queued by some poller running inside the MCP process.

export interface PendingSignal {
  id: number;
  source: string;
  content: string;
  created_at: string;
}

export function recordSignal(input: { source: string; content: string }): number {
  const stmt = getDb().prepare(
    `INSERT INTO signals (source, content) VALUES (?, ?)`,
  );
  const info = stmt.run(input.source, input.content);
  return Number(info.lastInsertRowid);
}

// Atomically pops the oldest pending signal. Returns null if none.
// Uses SQLite's RETURNING + a subselect to keep it one statement; safe
// under concurrent callers (better-sqlite3 is single-writer per process).
export function popNextSignal(): PendingSignal | null {
  const row = getDb()
    .prepare(
      `UPDATE signals
         SET consumed_at = datetime('now')
       WHERE id = (
         SELECT id FROM signals
          WHERE consumed_at IS NULL
          ORDER BY id ASC
          LIMIT 1
       )
       RETURNING id, source, content, created_at`,
    )
    .get() as PendingSignal | undefined;
  return row ?? null;
}

export function countPendingSignals(): number {
  const row = getDb()
    .prepare(`SELECT COUNT(*) AS n FROM signals WHERE consumed_at IS NULL`)
    .get() as { n: number };
  return row.n;
}

export interface SignalRow {
  id: number;
  source: string;
  content: string;
  created_at: string;
  consumed_at: string | null;
}

// Read-only view of past signals — does not pop or mutate. Used by the
// dreaming session to review what happened since the last reflection.
export function listSignals(opts: {
  since?: string;
  source?: string;
  limit?: number;
}): SignalRow[] {
  const limit = opts.limit ?? 200;
  const clauses: string[] = [];
  const params: unknown[] = [];
  if (opts.since) {
    clauses.push("created_at > ?");
    params.push(opts.since);
  }
  if (opts.source) {
    clauses.push("source = ?");
    params.push(opts.source);
  }
  const where = clauses.length > 0 ? `WHERE ${clauses.join(" AND ")}` : "";
  const sql = `SELECT id, source, content, created_at, consumed_at
                 FROM signals ${where}
                 ORDER BY id ASC
                 LIMIT ?`;
  params.push(limit);
  return getDb().prepare(sql).all(...params) as SignalRow[];
}
