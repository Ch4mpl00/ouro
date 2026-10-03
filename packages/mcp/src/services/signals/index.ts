import { sqlTime, type StateDb } from "../../db/state";

// In-MCP signal queue. Pollers (Telegram, Gmail, cron, webhooks) enqueue
// rows; the agent's loop consumes them one at a time via get_next_signal.
//
// Agent never polls external systems directly — every signal it sees has
// been built and queued by some poller running inside the MCP process.

// Timestamps go out as "YYYY-MM-DD HH:MM:SS" UTC, the shape the agent has
// always been handed.
export interface PendingSignal {
  id: number;
  source: string;
  content: string;
  created_at: string;
}

export interface SignalRow extends PendingSignal {
  consumed_at: string | null;
}

export interface ListSignalsFilter {
  // Exclusive lower bound on created_at.
  since?: Date;
  source?: string;
  limit?: number;
}

export interface Signals {
  record(input: { source: string; content: string }): Promise<number>;
  // Atomically pops the oldest pending signal; null when the queue is empty.
  popNext(): Promise<PendingSignal | null>;
  countPending(): Promise<number>;
  // Read-only view of past signals — never pops. The dreaming session
  // reviews what happened since its previous fire with this.
  list(filter: ListSignalsFilter): Promise<SignalRow[]>;
}

const DEFAULT_LIST_LIMIT = 200;

export function createSignals(db: StateDb): Signals {
  return {
    record: async ({ source, content }) => {
      const { rows } = await db.pool.query<{ id: number }>(
        "INSERT INTO signals (source, content) VALUES ($1, $2) RETURNING id",
        [source, content],
      );
      return rows[0]!.id;
    },

    // SKIP LOCKED: a concurrent popper takes the next row instead of waiting,
    // never the same one.
    popNext: async () => {
      const { rows } = await db.pool.query<PendingSignal>(
        `UPDATE signals SET consumed_at = now()
          WHERE id = (SELECT id FROM signals WHERE consumed_at IS NULL
                       ORDER BY id ASC LIMIT 1 FOR UPDATE SKIP LOCKED)
          RETURNING id, source, content, ${sqlTime("created_at")} AS created_at`,
      );
      return rows[0] ?? null;
    },

    countPending: async () => {
      const { rows } = await db.pool.query<{ n: number }>(
        "SELECT count(*) AS n FROM signals WHERE consumed_at IS NULL",
      );
      return rows[0]!.n;
    },

    list: async ({ since, source, limit }) => {
      const params: unknown[] = [];
      const clauses: string[] = [];
      if (since) {
        params.push(since);
        clauses.push(`created_at > $${params.length}`);
      }
      if (source) {
        params.push(source);
        clauses.push(`source = $${params.length}`);
      }
      params.push(limit ?? DEFAULT_LIST_LIMIT);
      const where = clauses.length > 0 ? `WHERE ${clauses.join(" AND ")}` : "";
      const { rows } = await db.pool.query<SignalRow>(
        `SELECT id, source, content, ${sqlTime("created_at")} AS created_at,
                ${sqlTime("consumed_at")} AS consumed_at
           FROM signals ${where}
          ORDER BY id ASC
          LIMIT $${params.length}`,
        params,
      );
      return rows;
    },
  };
}
