import type { Observation, Trace } from "../trace-model";
import { resolveSkill } from "../trace-model";
import { createSkillStore } from "../skills";
import type { TraceSource } from "./trace-source";

const skillStore = createSkillStore();

function stringify(x: unknown): string {
  if (x === null || x === undefined) return "null";
  if (typeof x === "string") return x;
  try {
    return JSON.stringify(x, null, 2);
  } catch {
    return String(x);
  }
}

export function buildTranscript(trace: Trace, observations: Observation[]): string {
  const lines: string[] = [];
  const intent = stringify(trace.input);
  lines.push(
    `# INTENT (trace.input)\n${
      intent === "null"
        ? "(empty — workflow path; the intent for this run is the skill contract below)"
        : intent
    }`,
  );
  lines.push(`# tags: ${trace.tags.join(", ")}`);

  const sorted = [...observations].sort(
    (a, b) => new Date(a.startTime).getTime() - new Date(b.startTime).getTime(),
  );
  lines.push(`\n# FLOW (${sorted.length} observations)`);
  for (const o of sorted) {
    if (o.parentObservationId === null && o.name === trace.name) continue;
    lines.push(`\n## ${o.name}  {${o.type.toLowerCase()}}`);
    if (o.statusMessage) lines.push(`status: ${o.statusMessage}`);
    if (o.type === "GENERATION" || o.type === "EMBEDDING") {
      lines.push(`model: ${o.model ?? "—"}`);
      lines.push(`output: ${stringify(o.output)}`);
    } else {
      if (o.input !== null && o.input !== undefined) lines.push(`input: ${stringify(o.input)}`);
      if (o.output !== null && o.output !== undefined) lines.push(`output: ${stringify(o.output)}`);
    }
  }
  const finalOutput = stringify(trace.output);
  lines.push(
    `\n# FINAL OUTPUT (trace.output)\n${
      finalOutput === "null"
        ? "(empty — workflow path; F is the text delivered by send/edit tool calls and the last compose output in the FLOW above)"
        : finalOutput
    }`,
  );
  return lines.join("\n");
}

export interface JudgeMaterials {
  trace: Trace;
  skillName: string | null;
  composerContract: string | null;
  orchestratorContract: string | null;
  transcript: string;
  obsCount: number;
}

export async function assembleMaterials(
  source: TraceSource,
  traceId: string,
): Promise<JudgeMaterials> {
  const { trace, observations } = await source.getTrace(traceId);
  const skillName = resolveSkill(observations, trace.metadata);
  const composerContract = skillName ? await skillStore.readSkillRaw(skillName) : null;
  const orchestratorContract = await skillStore.readSkillRaw("planner");
  const transcript = buildTranscript(trace, observations);
  return {
    trace,
    skillName,
    composerContract,
    orchestratorContract,
    transcript,
    obsCount: observations.length,
  };
}
