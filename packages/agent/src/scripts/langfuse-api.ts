import "dotenv/config";

// Thin Langfuse public-API client shared by terminal scripts (the trace
// inspector and the eval judge). Auth comes from LANGFUSE_PUBLIC_KEY /
// LANGFUSE_SECRET_KEY / LANGFUSE_BASE_URL — the same vars the agent uses to
// WRITE traces; here we READ them back. Langfuse stores full payloads (no
// server-side truncation), so a reader gets the agent's complete retrieval
// results, not the UI-clipped view.

const baseUrl = process.env.LANGFUSE_BASE_URL ?? "https://cloud.langfuse.com";
const publicKey = process.env.LANGFUSE_PUBLIC_KEY;
const secretKey = process.env.LANGFUSE_SECRET_KEY;
if (!publicKey || !secretKey) {
  throw new Error("LANGFUSE_PUBLIC_KEY / LANGFUSE_SECRET_KEY missing in env");
}
const authHeader = `Basic ${Buffer.from(`${publicKey}:${secretKey}`).toString("base64")}`;

// Retry transient failures (5xx, network errors) with linear backoff; 4xx are
// permanent and thrown immediately. Langfuse Cloud occasionally 502s, and a
// large trace can time out the gateway — both are worth a couple of retries,
// especially for the online judge worker that polls continuously.
export async function api<T>(path: string, retries = 3): Promise<T> {
  let lastErr: Error | null = null;
  for (let attempt = 0; attempt < retries; attempt++) {
    if (attempt > 0) await new Promise((r) => setTimeout(r, 600 * attempt));
    let res: Response;
    try {
      res = await fetch(`${baseUrl}/api/public${path}`, {
        headers: { Authorization: authHeader },
      });
    } catch (err) {
      lastErr = err instanceof Error ? err : new Error(String(err));
      continue; // network error — retry
    }
    if (res.ok) return (await res.json()) as T;
    if (res.status >= 500) {
      lastErr = new Error(`langfuse ${res.status} ${res.statusText} on ${path}`);
      continue; // transient server error — retry
    }
    throw new Error(`langfuse ${res.status} ${res.statusText} on ${path}`);
  }
  throw lastErr ?? new Error(`langfuse request failed on ${path}`);
}

// Langfuse observation types. Beyond the original GENERATION/SPAN/EVENT, v5
// adds typed spans (AGENT/TOOL/CHAIN/RETRIEVER/…) the agent emits via `kind`.
// Only GENERATION/EMBEDDING carry model + token usage.
export type ObservationType =
  | "GENERATION"
  | "SPAN"
  | "EVENT"
  | "AGENT"
  | "TOOL"
  | "CHAIN"
  | "RETRIEVER"
  | "EVALUATOR"
  | "GUARDRAIL"
  | "EMBEDDING";

export interface Observation {
  id: string;
  name: string;
  type: ObservationType;
  parentObservationId: string | null;
  startTime: string;
  endTime: string;
  level: string;
  statusMessage: string | null;
  input: unknown;
  output: unknown;
  metadata: Record<string, unknown> | null;
  model: string | null;
  modelParameters: Record<string, unknown> | null;
  usage: { input: number; output: number; total: number } | null;
  usageDetails: Record<string, number> | null;
  calculatedTotalCost: number | null;
  latency: number;
}

export interface Trace {
  id: string;
  name: string;
  sessionId: string | null;
  timestamp: string;
  input: unknown;
  output: unknown;
  metadata: Record<string, unknown> | null;
  // /traces?sessionId=... returns observation IDs; /traces/<id> inlines full
  // Observation objects. Both shapes are handled by fetchTraceById.
  observations: Array<string | Observation>;
  latency: number;
  totalCost: number;
  tags: string[];
}

// Fetch one trace by id with every observation resolved to a full object.
// `/traces/<id>` inlines Observation objects; the string-id form (returned by
// `/traces?sessionId`) is resolved with a per-id follow-up, fanned out.
export async function fetchTraceById(
  id: string,
): Promise<{ trace: Trace; observations: Observation[] }> {
  try {
    const trace = await api<Trace>(`/traces/${encodeURIComponent(id)}`);
    const observations = await Promise.all(
      trace.observations.map((entry) =>
        typeof entry === "string"
          ? api<Observation>(`/observations/${entry}`)
          : Promise.resolve(entry),
      ),
    );
    return { trace, observations };
  } catch {
    // `/traces/<id>` inlines every observation and gateway-times-out (502) on
    // large traces (big digests with 50+ snippets). Fall back to fetching the
    // observations on their own lighter endpoint and synthesize a trace stub
    // from the root observation — judge/replay need observations + the root's
    // input/output/metadata, not the trace-list fields (tags, cost).
    const observations = await fetchObservationsByTrace(id);
    const root =
      observations.find((o) => o.parentObservationId === null) ?? observations[0];
    const trace: Trace = {
      id,
      name: root?.name ?? id,
      sessionId: null,
      timestamp: root?.startTime ?? "",
      input: root?.input ?? null,
      output: root?.output ?? null,
      metadata: root?.metadata ?? null,
      observations,
      latency: 0,
      totalCost: 0,
      tags: [],
    };
    return { trace, observations };
  }
}

// All observations for a trace via the dedicated endpoint, paged. Lighter than
// inlining them in `/traces/<id>` — each page is a bounded response.
async function fetchObservationsByTrace(traceId: string): Promise<Observation[]> {
  const all: Observation[] = [];
  for (let page = 1; ; page++) {
    const res = await api<{ data: Observation[]; meta: { totalPages: number } }>(
      `/observations?traceId=${encodeURIComponent(traceId)}&limit=100&page=${page}`,
    );
    all.push(...res.data);
    if (res.data.length === 0 || page >= (res.meta?.totalPages ?? 1)) break;
  }
  return all;
}

export interface TraceSummary {
  id: string;
  name: string;
  timestamp: string;
  tags: string[];
}

// Most recent traces first (Langfuse lists newest-first by default). Summary
// only — observations are NOT inlined; fetch them per-id with fetchTraceById.
export async function fetchRecentTraces(limit: number): Promise<TraceSummary[]> {
  const res = await api<{ data: TraceSummary[] }>(`/traces?limit=${limit}`);
  return res.data;
}
