import { Buffer } from "node:buffer";
import { MEMORY_KEYS, type MemoryStore } from "./db/memory";

// A session owns its environment and working memory. Prompt rendering is a
// separate operation: renderContext includes the small environment block,
// while stored tool results are passed to consumers explicitly.

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

// A caller-supplied label. The store neither parses nor validates JSON.
export type WorkingMemoryFormat = "text" | "json";

export interface WorkingMemoryEntry {
  key: string;
  format: WorkingMemoryFormat;
  sizeBytes: number;
}

export interface WorkingMemory {
  // Insert only: keys are exact, non-empty strings. An occupied key throws.
  put(key: string, value: string, format?: WorkingMemoryFormat): void;
  // Returns the original string, including empty strings. Missing keys throw.
  get(key: string): string;
  // Detached metadata in insertion order; payloads stay out of the listing.
  list(): WorkingMemoryEntry[];
  // True if removed, false if absent. The deleted key may then be reused.
  delete(key: string): boolean;
}

export interface SessionContext {
  readonly id: string;
  readonly env: EnvData;
  readonly memory: WorkingMemory;
}

interface StoredValue {
  value: string;
  format: WorkingMemoryFormat;
  sizeBytes: number;
}

// Each call creates independent memory. Callers share the context explicitly
// with the components working on the same task and control its lifetime.
export function createSessionContext({ id, env }: { id: string; env: EnvData }): SessionContext {
  const entries = new Map<string, StoredValue>();

  return {
    id,
    env,
    memory: {
      put(key, value, format = "text") {
        if (key.length === 0) throw new Error("Key must not be empty");
        if (entries.has(key)) throw new Error(`Key ${JSON.stringify(key)} already exists`);

        entries.set(key, { value, format, sizeBytes: Buffer.byteLength(value, "utf8") });
      },

      get(key) {
        const entry = entries.get(key);
        if (entry === undefined) throw new Error(`Key ${JSON.stringify(key)} not found`);
        return entry.value;
      },

      list() {
        return Array.from(entries, ([key, { format, sizeBytes }]) => ({
          key,
          format,
          sizeBytes,
        }));
      },

      delete(key) {
        return entries.delete(key);
      },
    },
  };
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
export function renderContext(env: EnvData): string {
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
