import { JUDGE_NODE_META, type Observation, type Trace } from "../trace-model";
import { createSkillStore } from "../skills";
import type { NodeKind } from "./schema";
import type { TraceSource } from "./trace-source";

const skillStore = createSkillStore();

function stringify(x: unknown): string {
  if (x === null || x === undefined) return "";
  if (typeof x === "string") return x;
  try {
    return JSON.stringify(x, null, 2);
  } catch {
    return String(x);
  }
}

// Render a node's recorded input/output for the judge. A generation's input is
// a chat-messages array (system + user) — flatten it to role-labelled blocks
// so the judge reads the contract/system and the candidates the way the model
// saw them. Everything else (a span's input, a plain output string) stringifies.
function renderIo(value: unknown): string {
  if (
    Array.isArray(value) &&
    value.length > 0 &&
    value.every((m) => m !== null && typeof m === "object" && "role" in m)
  ) {
    return (value as Array<{ role: unknown; content: unknown }>)
      .map((m) => `[${String(m.role)}]\n${stringify(m.content)}`)
      .join("\n\n");
  }
  return stringify(value);
}

// One generative node lifted out of the observation tree, ready to judge. The
// `skill` is the OWNER (a named skill, or "planner" for the planner node and
// for prompt-only composes the planner authored); the improver aggregates by
// it. `contract` is that owner's rubric text. `inputText`/`outputText` are the
// node's own recorded IO — the judge scores this node in isolation, never the
// whole run.
export interface NodeMaterial {
  observationId: string;
  kind: NodeKind;
  skill: string;
  // Display label for the CLI scorecard — the observation name
  // (e.g. "attempt-1", "llm_compose:digest", "step[2]:llm_agent").
  label: string;
  contract: string | null;
  inputText: string;
  outputText: string;
}

const ROOT_SKIP = Symbol("root");

// True when any ancestor of `obs` is an AGENT span — i.e. `obs` is a
// generation/span INSIDE a spawned sub-agent (its `iter-*` calls), which we
// judge only black-box at the agent-step boundary, never node by node.
function hasAgentAncestor(obs: Observation, byId: Map<string, Observation>): boolean {
  let parent = obs.parentObservationId ? byId.get(obs.parentObservationId) : undefined;
  while (parent) {
    if (parent.type === "AGENT") return true;
    parent = parent.parentObservationId ? byId.get(parent.parentObservationId) : undefined;
  }
  return false;
}

// Walk the observation tree and classify every JUDGEABLE generative node.
// Classification is by an explicit metadata contract, NEVER by observation
// name (names are display-only and can be renamed freely):
//   - generation tagged judge_node="planner"  → planner node (owner planner)
//   - generation tagged judge_node="compose"  → compose node; owner = the
//                                                tagged-along metadata.skill, or
//                                                planner when prompt-only (null)
//   - AGENT-type span (an llm_agent step)      → agent node, judged black-box
//                                                (input→output); its inner
//                                                generations are skipped
// Tool spans, embeddings, events, the trace root, untagged generations, and
// pure container spans (planner/runner/step/parallel) are skipped.
export async function assembleNodeMaterials(
  source: TraceSource,
  traceId: string,
): Promise<{ trace: Trace; nodes: NodeMaterial[] }> {
  const { trace, observations } = await source.getTrace(traceId);
  const byId = new Map(observations.map((o) => [o.id, o]));

  const plannerContract = await skillStore.readSkillRaw("planner");
  // Memoize composer-skill reads — a run can have several compose nodes on the
  // same skill (map stage), and the same skill across nodes.
  const contractCache = new Map<string, string | null>([["planner", plannerContract]]);
  const readContract = async (skill: string): Promise<string | null> => {
    if (!contractCache.has(skill)) contractCache.set(skill, await skillStore.readSkillRaw(skill));
    return contractCache.get(skill) ?? null;
  };

  // Sort by start time so the scorecard reads in execution order.
  const sorted = [...observations].sort(
    (a, b) => new Date(a.startTime).getTime() - new Date(b.startTime).getTime(),
  );

  const nodes: NodeMaterial[] = [];
  for (const o of sorted) {
    const classified = classify(o, trace, byId);
    if (classified === ROOT_SKIP || classified === null) continue;

    const skill = classified.skill;
    const contract = skill === "planner" ? plannerContract : await readContract(skill);
    nodes.push({
      observationId: o.id,
      kind: classified.kind,
      skill,
      label: o.name,
      contract,
      inputText: renderIo(o.input),
      outputText: renderIo(o.output),
    });
  }

  return { trace, nodes };
}

// Pure classification of one observation. Returns the node kind + owner skill,
// null to skip, or ROOT_SKIP for the trace root. Kept separate so tests can
// pin the rules without rendering or skill IO.
export function classify(
  o: Observation,
  trace: Pick<Trace, "name">,
  byId: Map<string, Observation>,
): { kind: NodeKind; skill: string } | null | typeof ROOT_SKIP {
  if (o.parentObservationId === null && o.name === trace.name) return ROOT_SKIP;

  // An llm_agent step IS an AGENT span — judged black-box. A nested agent (an
  // agent spawned inside another) is already covered by the outer black box.
  if (o.type === "AGENT") {
    if (hasAgentAncestor(o, byId)) return null;
    return { kind: "agent", skill: ownerSkill(o.metadata?.skill) };
  }

  if (o.type !== "GENERATION") return null;
  if (hasAgentAncestor(o, byId)) return null;

  const role = o.metadata?.[JUDGE_NODE_META];
  if (role === "planner") return { kind: "planner", skill: "planner" };
  // Compose node carries its own owner skill (null → prompt-only → planner).
  if (role === "compose") return { kind: "compose", skill: ownerSkill(o.metadata?.skill) };

  return null;
}

// A node's owner: the named skill if present, else the planner (prompt-only
// composes and any node whose skill is absent are the planner's responsibility).
function ownerSkill(skill: unknown): string {
  return typeof skill === "string" && skill.length > 0 ? skill : "planner";
}

export { ROOT_SKIP };
