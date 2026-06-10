import { MEMORY_KEYS, type MemoryStore } from "./db/memory";

// `Current context` block prepended to every session's system prompt.
// Saves the agent a few tool calls per session (no `get_timezone`,
// no `get_last_news_read_at`) and gives every skill a single canonical
// place to look for "what's the current state of the world".
//
// Kept deliberately small — only data that's cheap to gather, useful in
// most sessions, and stable enough that a ~minute of staleness is fine.
// Bigger / per-source state still goes through tool calls.

// Narrow dependency surface: gathering env data needs one MCP call, one
// memory read and the user's email — not the whole Engine. `userEmail` is
// read from env ONCE in the composition root and injected here, so this
// per-signal business path touches no process.env.
export interface EnvDataDeps {
  mcp: { callTool(name: string, args: Record<string, unknown>): Promise<string> };
  memory: Pick<MemoryStore, "get">;
  userEmail: string | null;
}

// Structured env data — single source of truth for both the supervisor
// (markdown context block) and the workflow runner (variable store
// initial value under the `env` key). When this shape changes, both
// consumers update at once.
export interface EnvData {
  now: Date;
  timezone: string;
  userEmail: string | null;
  newsLastReadAt: string | null;
}

function formatLocalTime(now: Date, tz: string): string {
  const parts = new Intl.DateTimeFormat("en-CA", {
    timeZone: tz,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).formatToParts(now);
  const get = (type: string) => parts.find((p) => p.type === type)?.value ?? "";
  return `${get("year")}-${get("month")}-${get("day")} ${get("hour")}:${get("minute")}`;
}

// Reads the integration-owned timezone from MCP exactly once per call.
// Returns "UTC" if the MCP call fails — the block is best-effort, we'd
// rather inject a slightly-wrong tz than crash the session.
async function readTimezone(deps: EnvDataDeps): Promise<string> {
  try {
    const raw = await deps.mcp.callTool("get_timezone", {});
    if (raw.startsWith("[tool error]")) return "UTC";
    const parsed = JSON.parse(raw) as { timezone?: string };
    return parsed.timezone ?? "UTC";
  } catch {
    return "UTC";
  }
}

export async function gatherEnvData(deps: EnvDataDeps): Promise<EnvData> {
  const tz = await readTimezone(deps);
  return {
    now: new Date(),
    timezone: tz,
    userEmail: deps.userEmail,
    newsLastReadAt: deps.memory.get(MEMORY_KEYS.newsLastReadAt),
  };
}

// Pure render of the already-gathered EnvData into the markdown block.
export function buildSessionContext(env: EnvData): string {
  const lines = [
    "## Current context",
    `- Local time: ${formatLocalTime(env.now, env.timezone)} (${env.timezone})`,
  ];
  if (env.userEmail) lines.push(`- User email: ${env.userEmail}`);
  lines.push(
    `- News last read at: ${env.newsLastReadAt ?? "never (bootstrap with now - 24h)"}`,
  );
  return lines.join("\n");
}
