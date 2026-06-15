import { eq, sql } from "drizzle-orm";
import type { AgentDatabase } from "./client";
import { memory } from "./schema";

// Agent-side memory KV. Lives in the `memory` table of `agent.db`. This is the
// freeform store for anything the agent wants to remember between sessions
// that doesn't fit a typed table — watermarks, last-seen markers, small
// notes. Distinct from MCP-side `tokens.db`, which holds integration
// state (OAuth tokens, queues, caches) the MCP process owns.

export interface MemoryStore {
  get(key: string): string | null;
  set(key: string, value: string): void;
}

export function createMemoryStore(db: AgentDatabase): MemoryStore {
  return {
    get(key) {
      const row = db
        .select({ value: memory.value })
        .from(memory)
        .where(eq(memory.key, key))
        .get();
      return row?.value ?? null;
    },
    set(key, value) {
      db.insert(memory)
        .values({ key, value })
        .onConflictDoUpdate({
          target: memory.key,
          set: { value, updatedAt: sql`(datetime('now'))` },
        })
        .run();
    },
  };
}

// Well-known keys injected into the session context block. Keep them here
// so writers and the supervisor agree on naming.
export const MEMORY_KEYS = {
  newsLastReadAt: "news_digest.last_read_at",
} as const;
