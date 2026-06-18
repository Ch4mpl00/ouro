import { and, eq } from "drizzle-orm";
import type { AgentDatabase } from "./client";
import { improverState, type ImproverStateRow } from "./schema";

// Persistent state for the closed-loop improver (Phase 3, п3): one row per
// (skill, axis), holding the last cycle's outcome and — when the last attempt
// shipped — the live-monitor bookkeeping (pre-ship baseline + shipped lesson +
// status). Kept in its own tiny store, not bolted onto TraceStore: the improver
// owns this state, the trace mirror does not. See schema.ts `improverState`.

export type ImproverState = ImproverStateRow;

// The mutable fields a cycle writes back. Identity (skill, axis) is separate.
export interface ImproverStateUpdate {
  lastOutcome: string;
  shippedAt: string | null;
  shippedLesson: string | null;
  baselineMean: number | null;
  baselineN: number | null;
  monitorStatus: string | null;
}

export interface ImproverStore {
  get(skill: string, axis: string): ImproverState | null;
  upsert(skill: string, axis: string, lastAttemptAt: string, patch: ImproverStateUpdate): void;
}

export function createImproverStore(db: AgentDatabase): ImproverStore {
  return {
    get(skill, axis) {
      return (
        db
          .select()
          .from(improverState)
          .where(and(eq(improverState.skill, skill), eq(improverState.axis, axis)))
          .get() ?? null
      );
    },

    upsert(skill, axis, lastAttemptAt, patch) {
      const row = { skill, axis, lastAttemptAt, ...patch };
      db.insert(improverState)
        .values(row)
        .onConflictDoUpdate({
          target: [improverState.skill, improverState.axis],
          set: {
            lastAttemptAt: row.lastAttemptAt,
            lastOutcome: row.lastOutcome,
            shippedAt: row.shippedAt,
            shippedLesson: row.shippedLesson,
            baselineMean: row.baselineMean,
            baselineN: row.baselineN,
            monitorStatus: row.monitorStatus,
          },
        })
        .run();
    },
  };
}
