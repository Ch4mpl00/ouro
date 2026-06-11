import { fetchTraceById, type Observation, type Trace } from "../scripts/langfuse-api";
import { createSkillStore } from "../skills";

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

function findSkill(trace: Trace, observations: Observation[]): string | null {
  for (const o of observations) {
    const skill = o.metadata?.skill;
    if (typeof skill === "string" && skill.length > 0) return skill;
  }
  const skills = trace.metadata?.skills;
  if (Array.isArray(skills) && typeof skills[0] === "string") return skills[0];
  return null;
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
  lines.push(`\n# FINAL OUTPUT (trace.output)\n${stringify(trace.output)}`);
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

export async function assembleMaterials(traceId: string): Promise<JudgeMaterials> {
  const { trace, observations } = await fetchTraceById(traceId);
  const skillName = findSkill(trace, observations);
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
