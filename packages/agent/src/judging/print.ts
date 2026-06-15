import { JUDGE_MODEL, JUDGE_PROMPT_VERSION, type Faithfulness, type NodeJudgement } from "./schema";

// Header line for a judged node: its display label, kind, and owner skill.
export interface NodeHeader {
  label: string;
  kind: string;
  skill: string;
}

export function printTraceHeader(traceId: string): void {
  console.log(
    `\n=== JUDGE ${JUDGE_MODEL} (prompt ${JUDGE_PROMPT_VERSION}) · trace ${traceId} ===`,
  );
}

// One block per node: the axis scorecard (planner: query_formulation/process;
// composer/agent: coverage/composition) and, for compose/agent, faithfulness.
export function printNodeJudgement(node: NodeHeader, j: NodeJudgement): void {
  console.log(`\n── node ${node.label} · ${node.kind} · skill ${node.skill} ──`);
  for (const a of j.scorecard.axes) {
    const score = a.applicable && a.score !== null ? a.score.toFixed(2) : "n/a";
    console.log(`● ${a.axis}: ${a.label} (${score})`);
    console.log(`  ${a.rationale}`);
    if (a.evidence) console.log(`  ↳ ${a.evidence}`);
  }
  console.log(`  overall: ${j.scorecard.overall_note}`);
  if (j.faithfulness) printFaithfulness(j.faithfulness);
}

function printFaithfulness(f: Faithfulness): void {
  if (!f.applicable) {
    console.log(`● faithfulness: n/a — ${f.note}`);
    return;
  }
  const score = f.score !== null ? f.score.toFixed(2) : "—";
  const bad = f.claims.filter((c) => c.verdict !== "supported").length;
  console.log(`● faithfulness: ${score}  (${f.claims.length} claims, ${bad} not fully supported)`);
  for (const c of f.claims) {
    const mark = c.verdict === "supported" ? "✓" : c.verdict === "partial" ? "~" : "✗";
    console.log(`  ${mark} ${c.claim}`);
    if (c.verdict !== "supported") console.log(`      ↳ ${c.evidence}`);
  }
  if (f.note) console.log(`  ${f.note}`);
}

// Compact one-liner per node for a multi-node / multi-trace summary table:
//   node llm_compose:digest · compose · skill news-digest · coverage 0.80 composition 0.68 faithfulness 0.91
export function nodeSummaryLine(node: NodeHeader, j: NodeJudgement): string {
  const parts: string[] = [];
  for (const a of j.scorecard.axes) {
    if (a.applicable && a.score !== null) parts.push(`${a.axis} ${a.score.toFixed(2)}`);
  }
  if (j.faithfulness?.applicable && j.faithfulness.score !== null) {
    parts.push(`faithfulness ${j.faithfulness.score.toFixed(2)}`);
  }
  return `node ${node.label} · ${node.kind} · skill ${node.skill} · ${parts.join(" ") || "n/a"}`;
}
