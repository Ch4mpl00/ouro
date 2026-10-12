import { z } from "zod";
import type { McpHandle } from "../mcp-client";
import type { SupervisorModule } from "./module";

// The supervisor's poll loop: take one signal at a time from MCP and run it
// to completion before asking for the next. An empty queue waits a poll
// interval; a failed signal (already reported by recovery) or a failed poll
// is logged, waits the same interval, and the loop carries on.

export const POLL_INTERVAL_MS = 2_000;

const nextSignal = z.object({
  signal: z
    .object({
      id: z.number().int(),
      source: z.string(),
      content: z.string(),
      envContext: z.string().nullable(),
      created_at: z.string(),
    })
    .nullable(),
  pendingAfter: z.number().int(),
});

export interface SupervisorLoopDeps {
  mcp: Pick<McpHandle, "callTool">;
  supervisor: Pick<SupervisorModule, "runSignal">;
  // Checked before every poll; the loop returns once it is true.
  stopping: () => boolean;
  sleep?: (ms: number) => Promise<void>;
  pollIntervalMs?: number;
}

export async function runSupervisorLoop(deps: SupervisorLoopDeps): Promise<void> {
  const sleep = deps.sleep ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
  const interval = deps.pollIntervalMs ?? POLL_INTERVAL_MS;
  while (!deps.stopping()) {
    try {
      const result = nextSignal.parse(JSON.parse(await deps.mcp.callTool("get_next_signal", {})));
      if (!result.signal) {
        await sleep(interval);
        continue;
      }
      console.log(
        `[supervisor] signal #${result.signal.id} source=${result.signal.source} (${result.pendingAfter} pending after)`,
      );
      await deps.supervisor.runSignal(result.signal);
    } catch (err) {
      if (deps.stopping()) break;
      console.error("[supervisor] loop error:", err);
      await sleep(interval);
    }
  }
}
