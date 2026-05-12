import type { Engine } from "./engine";
import { getMemory, MEMORY_KEYS } from "./db/memory";

// `Current context` block prepended to every session's system prompt.
// Saves the agent a few tool calls per session (no `get_timezone`,
// no `get_last_news_read_at`) and gives every skill a single canonical
// place to look for "what's the current state of the world".
//
// Kept deliberately small — only data that's cheap to gather, useful in
// most sessions, and stable enough that a ~minute of staleness is fine.
// Bigger / per-source state still goes through tool calls.

interface ContextInputs {
  now: Date;
  tz: string;
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

function render(inputs: ContextInputs): string {
  const lines = [
    "## Current context",
    `- Local time: ${formatLocalTime(inputs.now, inputs.tz)} (${inputs.tz})`,
  ];
  if (inputs.userEmail) lines.push(`- User email: ${inputs.userEmail}`);
  lines.push(
    `- News last read at: ${inputs.newsLastReadAt ?? "never (bootstrap with now - 24h)"}`,
  );
  return lines.join("\n");
}

// Reads the integration-owned timezone from MCP exactly once per call.
// Returns "UTC" if the MCP call fails — the block is best-effort, we'd
// rather inject a slightly-wrong tz than crash the session.
async function readTimezone(engine: Engine): Promise<string> {
  try {
    const raw = await engine.mcp.callTool("get_timezone", {});
    if (raw.startsWith("[tool error]")) return "UTC";
    const parsed = JSON.parse(raw) as { timezone?: string };
    return parsed.timezone ?? "UTC";
  } catch {
    return "UTC";
  }
}

export async function buildSessionContext(engine: Engine): Promise<string> {
  const [tz] = await Promise.all([readTimezone(engine)]);
  return render({
    now: new Date(),
    tz,
    userEmail: process.env.USER_EMAIL ?? null,
    newsLastReadAt: getMemory(MEMORY_KEYS.newsLastReadAt),
  });
}
