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
    if (res.ok) {
      const text = await res.text();
      return (text ? JSON.parse(text) : undefined) as T;
    }
    if (res.status >= 500) {
      lastErr = new Error(`langfuse ${res.status} ${res.statusText} on ${path}`);
      continue; // transient server error — retry
    }
    throw new Error(`langfuse ${res.status} ${res.statusText} on ${path}`);
  }
  throw lastErr ?? new Error(`langfuse request failed on ${path}`);
}

export async function apiPost<T>(
  path: string,
  body: unknown,
  retries = 3,
): Promise<T> {
  let lastErr: Error | null = null;
  for (let attempt = 0; attempt < retries; attempt++) {
    if (attempt > 0) await new Promise((r) => setTimeout(r, 600 * attempt));
    let res: Response;
    try {
      res = await fetch(`${baseUrl}/api/public${path}`, {
        method: "POST",
        headers: {
          Authorization: authHeader,
          "content-type": "application/json",
        },
        body: JSON.stringify(body),
      });
    } catch (err) {
      lastErr = err instanceof Error ? err : new Error(String(err));
      continue;
    }
    if (res.ok) return (await res.json()) as T;
    if (res.status >= 500) {
      lastErr = new Error(`langfuse ${res.status} ${res.statusText} on POST ${path}`);
      continue;
    }
    const text = await res.text().catch(() => "");
    throw new Error(`langfuse ${res.status} ${res.statusText} on POST ${path}: ${text}`);
  }
  throw lastErr ?? new Error(`langfuse POST failed on ${path}`);
}

// Read-shape types now live in ../trace-model (shared with the local mirror).
// Re-export so existing `from "../scripts/langfuse-api"` imports keep working.
export type { ObservationType, Observation, Trace, TraceSummary } from "../trace-model";
import type { Observation, Trace, TraceSummary } from "../trace-model";

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

// Most recent traces first (Langfuse lists newest-first by default). Summary
// only — observations are NOT inlined; fetch them per-id with fetchTraceById.
export async function fetchRecentTraces(limit: number): Promise<TraceSummary[]> {
  const res = await api<{ data: TraceSummary[] }>(`/traces?limit=${limit}`);
  return res.data;
}
