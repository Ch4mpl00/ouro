import { z } from "zod";
import { CronExpressionParser } from "cron-parser";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { ScheduledTasks } from "../services/scheduler";
import { localTime, type Settings } from "../services/settings";
import { jsonResult } from "../result";

// cron-parser also reads 4- and 6-field expressions — as ones with a seconds
// field — so a malformed or "with seconds" cron would silently fire every
// second (one slot per tick, forever behind). Only the documented 5 fields.
function fieldCountError(cronExpr: string): string | null {
  const fields = cronExpr.trim().split(/\s+/).length;
  return fields === 5
    ? null
    : `expected 5 fields (minute hour day-of-month month day-of-week), got ${fields}`;
}

function previewNextFires(cronExpr: string, tz: string, count: number): string[] {
  const it = CronExpressionParser.parse(cronExpr, { tz });
  const out: string[] = [];
  for (let i = 0; i < count; i++) out.push(it.next().toDate().toISOString());
  return out;
}

export function registerSchedulerTools(
  server: McpServer,
  deps: { tasks: ScheduledTasks; settings: Settings },
): void {
  const { tasks, settings } = deps;

  server.registerTool(
    "schedule_task",
    {
      title: "Schedule an agent task",
      description:
        "Register a cron-driven task. When the cron matches, MCP enqueues a " +
        "`scheduler` signal with the given prompt and the agent acts on it " +
        "(send a Telegram message, run a check, etc). Cron is standard 5-field " +
        "(minute hour day-of-month month day-of-week) evaluated in the " +
        "user's configured timezone. For one-shot reminders set `recurring: " +
        "false` and use a specific cron like '30 14 12 5 *' (14:30 on May " +
        "12); the task auto-deactivates after the first fire. For repeating " +
        "tasks use a generic cron like '0 9 * * *' (every day 9:00). The " +
        "agent is responsible for converting natural-language times into " +
        "cron syntax before calling this tool.",
      inputSchema: {
        cron_expr: z
          .string()
          .min(1)
          .describe("5-field cron expression (e.g. '30 9 * * *' = 09:30 daily)."),
        recurring: z
          .boolean()
          .describe("true = keep firing on every cron match; false = fire once then deactivate."),
        prompt: z
          .string()
          .min(1)
          .describe(
            "Free-text instruction delivered to the agent when the task fires. The agent " +
              "interprets it under the `scheduler` skill (e.g. 'remind me to take pills').",
          ),
      },
    },
    async ({ cron_expr, recurring, prompt }) => {
      const fieldError = fieldCountError(cron_expr);
      if (fieldError) {
        return jsonResult({ ok: false, error: `Invalid cron expression: ${fieldError}` });
      }
      const tz = await settings.timezone();
      let upcoming: string[];
      try {
        upcoming = previewNextFires(cron_expr, tz, recurring ? 3 : 1);
      } catch (err) {
        return jsonResult({
          ok: false,
          error: `Invalid cron expression: ${(err as Error).message}`,
        });
      }
      const row = await tasks.insert({ cron_expr, recurring, prompt });
      return jsonResult({
        ok: true,
        task: row,
        timezone: tz,
        upcoming_fires: upcoming,
      });
    },
  );

  server.registerTool(
    "list_scheduled_tasks",
    {
      title: "List scheduled tasks",
      description:
        "Show every task that may still fire — recurring tasks (always) and " +
        "one-shots that haven't been triggered yet. Each row includes the " +
        "cron expression, prompt, last fire time, and the next 1-3 upcoming " +
        "fire timestamps in the user's timezone for sanity-checking.",
      inputSchema: {},
    },
    async () => {
      const tz = await settings.timezone();
      const active = (await tasks.listActive()).map((t) => {
        let upcoming: string[] = [];
        try {
          upcoming = previewNextFires(t.cron_expr, tz, t.recurring ? 3 : 1);
        } catch {
          /* invalid cron — surface via empty upcoming */
        }
        return {
          ...t,
          recurring: t.recurring === 1,
          last_run_at_iso:
            t.last_run_at != null ? new Date(t.last_run_at * 1000).toISOString() : null,
          upcoming_fires: upcoming,
        };
      });
      return jsonResult({ timezone: tz, count: active.length, tasks: active });
    },
  );

  server.registerTool(
    "cancel_scheduled_task",
    {
      title: "Cancel a scheduled task",
      description:
        "Permanently remove a task by id. Use this when the user says " +
        "'forget about that reminder' or 'stop the daily X'. Returns " +
        "{ ok: true, removed: <id> } on success, { ok: false } if no such task.",
      inputSchema: {
        id: z.number().int().positive().describe("Task id from list_scheduled_tasks."),
      },
    },
    async ({ id }) => {
      const removed = await tasks.delete(id);
      return jsonResult({ ok: removed, id });
    },
  );

  server.registerTool(
    "get_timezone",
    {
      title: "Get configured timezone",
      description:
        "Return the IANA timezone driving cron evaluation and digest " +
        "schedule decisions. Defaults to UTC when unset.",
      inputSchema: {},
    },
    async () => {
      const tz = await settings.timezone();
      const now = localTime(tz);
      return jsonResult({
        timezone: tz,
        local_now: `${now.date} ${String(now.hour).padStart(2, "0")}:${String(now.minute).padStart(2, "0")}`,
      });
    },
  );

  server.registerTool(
    "set_timezone",
    {
      title: "Set the configured timezone",
      description:
        "Update the IANA timezone (e.g. 'Europe/Kiev', 'America/New_York', " +
        "'UTC'). Takes effect immediately — the next scheduler tick, daily " +
        "digest check, and any new schedule_task call all use the new value. " +
        "Existing tasks keep their cron string as-is, so their next-fire " +
        "wall-clock time shifts. Invalid IANA names are rejected.",
      inputSchema: {
        tz: z
          .string()
          .min(1)
          .describe("IANA timezone name, e.g. 'Europe/Kiev'."),
      },
    },
    async ({ tz }) => {
      try {
        await settings.setTimezone(tz);
      } catch (err) {
        return jsonResult({
          ok: false,
          error: `Invalid timezone '${tz}': ${(err as Error).message}`,
        });
      }
      const now = localTime(tz);
      return jsonResult({
        ok: true,
        timezone: tz,
        local_now: `${now.date} ${String(now.hour).padStart(2, "0")}:${String(now.minute).padStart(2, "0")}`,
      });
    },
  );
}
