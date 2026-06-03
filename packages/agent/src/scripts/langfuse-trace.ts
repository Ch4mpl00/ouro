import "dotenv/config";

// Inspect a Langfuse trace from the terminal. Usage:
//   pnpm trace <sessionId>            — all traces in a session, summarised
//   pnpm trace <sessionId> --raw      — also dump full input/output JSON
//   pnpm trace <traceId> --by-id      — fetch a single trace by its trace id
//
// Auth comes from LANGFUSE_PUBLIC_KEY / LANGFUSE_SECRET_KEY / LANGFUSE_BASE_URL
// in .env (same vars the agent uses).

const baseUrl = process.env.LANGFUSE_BASE_URL ?? "https://cloud.langfuse.com";
const publicKey = process.env.LANGFUSE_PUBLIC_KEY;
const secretKey = process.env.LANGFUSE_SECRET_KEY;
if (!publicKey || !secretKey) {
  console.error("LANGFUSE_PUBLIC_KEY / LANGFUSE_SECRET_KEY missing in env");
  process.exit(1);
}
const authHeader = `Basic ${Buffer.from(`${publicKey}:${secretKey}`).toString("base64")}`;

async function api<T>(path: string): Promise<T> {
  const res = await fetch(`${baseUrl}/api/public${path}`, {
    headers: { Authorization: authHeader },
  });
  if (!res.ok) throw new Error(`langfuse ${res.status} ${res.statusText} on ${path}`);
  return (await res.json()) as T;
}

interface Trace {
  id: string;
  name: string;
  sessionId: string | null;
  timestamp: string;
  input: unknown;
  output: unknown;
  metadata: Record<string, unknown> | null;
  // /traces?sessionId=... returns IDs; /traces/<id> returns full
  // Observation objects inline. Handle both.
  observations: Array<string | Observation>;
  latency: number;
  totalCost: number;
  tags: string[];
}

interface Observation {
  id: string;
  name: string;
  type: "GENERATION" | "SPAN" | "EVENT";
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
  calculatedTotalCost: number | null;
  latency: number;
}

function truncate(s: string, n: number): string {
  if (s.length <= n) return s;
  return s.slice(0, n) + `… (+${s.length - n} chars)`;
}

function toText(x: unknown, max = 300): string {
  if (x === null || x === undefined) return "—";
  if (typeof x === "string") return truncate(x, max);
  try {
    return truncate(JSON.stringify(x, null, 2), max);
  } catch {
    return String(x);
  }
}

interface AssistantMessage {
  content?: string | null;
  tool_calls?: Array<{
    id?: string;
    function?: { name?: string; arguments?: string };
  }>;
}

function summariseGenerationOutput(out: unknown): {
  content: string;
  tools: Array<{ name: string; args: string }>;
} {
  if (!out || typeof out !== "object") return { content: "", tools: [] };
  const msg = out as AssistantMessage;
  return {
    content: (msg.content ?? "").trim(),
    tools: (msg.tool_calls ?? []).map((c) => ({
      name: c.function?.name ?? "?",
      args: c.function?.arguments ?? "",
    })),
  };
}

function fmtCost(n: number): string {
  if (!n) return "—";
  return n < 0.01 ? `$${n.toFixed(5)}` : `$${n.toFixed(4)}`;
}

function fmtSec(s: number): string {
  return `${s.toFixed(2)}s`;
}

interface PrintOpts {
  raw: boolean;
}

function renderTrace(t: Trace, observations: Observation[], opts: PrintOpts): void {
  const meta = t.metadata ?? {};
  const preset = meta.preset ?? "—";
  const model = meta.model ?? "—";
  const effort = meta.reasoning_effort ?? "—";
  const skills = Array.isArray(meta.skills) ? (meta.skills as string[]).join(",") : "—";

  console.log(`\n=== ${t.name}  (${preset} / ${model} · ${effort})  ===`);
  console.log(
    `id=${t.id}  session=${t.sessionId ?? "—"}  skills=[${skills}]  tags=[${t.tags.join(",")}]`,
  );
  console.log(
    `latency=${fmtSec(t.latency)}  cost=${fmtCost(t.totalCost)}  observations=${observations.length}  @ ${t.timestamp}`,
  );

  console.log(`\nINPUT`);
  console.log(toText(t.input, opts.raw ? 100_000 : 600));

  // Sort observations chronologically.
  const sorted = [...observations].sort(
    (a, b) => new Date(a.startTime).getTime() - new Date(b.startTime).getTime(),
  );

  // Top-level span (matches trace name) is implicit; skip it visually but still
  // walk children.
  const children = sorted.filter(
    (o) => o.parentObservationId === null || o.name !== t.name,
  );

  console.log(`\nFLOW`);
  for (const o of children) {
    if (o.name === t.name && o.type === "SPAN") continue; // root span placeholder
    if (o.type === "GENERATION") {
      const { content, tools } = summariseGenerationOutput(o.output);
      const usage = o.usage ? `${o.usage.input}→${o.usage.output}` : "—";
      const cost = o.calculatedTotalCost != null ? fmtCost(o.calculatedTotalCost) : "—";
      const lvl = o.level !== "DEFAULT" ? ` [${o.level}]` : "";
      console.log(`\n  • ${o.name}  (${o.model ?? "—"}, ${usage} tok, ${cost}, ${fmtSec(o.latency)})${lvl}`);
      if (o.statusMessage) console.log(`    status: ${o.statusMessage}`);
      if (content) console.log(`    content: ${truncate(content, opts.raw ? 100_000 : 400)}`);
      for (const t of tools) {
        console.log(`    → ${t.name}(${truncate(t.args, opts.raw ? 100_000 : 200)})`);
      }
    } else if (o.type === "SPAN") {
      const lvl = o.level !== "DEFAULT" ? ` [${o.level}]` : "";
      console.log(`\n  · ${o.name}  (${fmtSec(o.latency)})${lvl}`);
      if (o.statusMessage) console.log(`    status: ${o.statusMessage}`);
      const inText = toText(o.input, opts.raw ? 100_000 : 200);
      const outText = toText(o.output, opts.raw ? 100_000 : 300);
      if (inText !== "—") console.log(`    in:  ${inText.replace(/\n/g, "\n         ")}`);
      if (outText !== "—") console.log(`    out: ${outText.replace(/\n/g, "\n         ")}`);
    }
  }

  console.log(`\nOUTPUT`);
  console.log(toText(t.output, opts.raw ? 100_000 : 800));
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const id = args.find((a) => !a.startsWith("--"));
  if (!id) {
    console.error("usage: pnpm trace <sessionId|traceId> [--by-id] [--raw]");
    process.exit(1);
  }
  const byId = args.includes("--by-id");
  const raw = args.includes("--raw");
  const opts: PrintOpts = { raw };

  let traces: Trace[];
  if (byId) {
    const t = await api<Trace>(`/traces/${encodeURIComponent(id)}`);
    traces = [t];
  } else {
    const list = await api<{ data: Trace[] }>(
      `/traces?sessionId=${encodeURIComponent(id)}`,
    );
    traces = list.data;
    if (traces.length === 0) {
      console.error(`no traces found for session ${id}`);
      process.exit(1);
    }
    // Show oldest first — easier to follow how a session unfolded.
    traces.sort((a, b) => new Date(a.timestamp).getTime() - new Date(b.timestamp).getTime());
  }

  for (const t of traces) {
    // /traces?sessionId returns plain IDs (need a follow-up fetch per
    // observation), /traces/<id> inlines full Observation objects.
    // Fan out in parallel — strings get resolved, objects pass through.
    const observations = await Promise.all(
      t.observations.map((entry) =>
        typeof entry === "string"
          ? api<Observation>(`/observations/${entry}`)
          : Promise.resolve(entry),
      ),
    );
    renderTrace(t, observations, opts);
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
