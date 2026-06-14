// Canonical READ shape of an agent run's trace — the structure the judge and
// the self-improvement loop consume. It mirrors the Langfuse public-API
// response so a local mirror and a Langfuse fetch are interchangeable behind
// the `TraceSource` interface. Pure types + one pure resolver, no I/O and no
// env reads, so the local recorder / store can import it without dragging in
// the Langfuse client (which throws at load when creds are missing).

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

export interface TraceSummary {
  id: string;
  name: string;
  timestamp: string;
  tags: string[];
}

// A judgeable generation tags itself with its node role under
// metadata[JUDGE_NODE_META], so the per-node judge classifies by an explicit
// producer→judge contract rather than by observation NAMES ("attempt-1",
// "llm_compose:digest") — those stay display-only and can be renamed freely.
// Kept separate from metadata.skill on purpose: `skill` means "which skill
// composed the output" (drives resolveSkill / the traces.skill column), and a
// planner generation must NOT claim that role. Agent nodes need no tag — an
// llm_agent step is already an AGENT-type span.
export const JUDGE_NODE_META = "judge_node";
export type JudgeNodeRole = "planner" | "compose";

// Which skill composed the output. Workflow path stamps it on the
// llm_compose / llm_agent step observation's metadata.skill; agent-loop path
// lists it on trace.metadata.skills. First non-empty wins. Used both at
// record time (to fill the traces.skill column) and by the judge.
export function resolveSkill(
  observations: Array<Pick<Observation, "metadata">>,
  traceMetadata: Record<string, unknown> | null,
): string | null {
  for (const o of observations) {
    const skill = o.metadata?.skill;
    if (typeof skill === "string" && skill.length > 0) return skill;
  }
  const skills = traceMetadata?.skills;
  if (Array.isArray(skills) && typeof skills[0] === "string") return skills[0];
  return null;
}
