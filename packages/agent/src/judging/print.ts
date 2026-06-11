import { JUDGE_MODEL, JUDGE_PROMPT_VERSION, type Faithfulness, type Scorecard } from "./schema";

export function printScorecard(traceId: string, skillName: string | null, card: Scorecard): void {
  console.log(
    `\n=== JUDGE ${JUDGE_MODEL} (prompt ${JUDGE_PROMPT_VERSION}) · trace ${traceId} · skill ${skillName ?? "—"} ===\n`,
  );
  for (const a of card.axes) {
    const score = a.applicable && a.score !== null ? a.score.toFixed(2) : "n/a";
    console.log(`● ${a.axis}: ${a.label} (${score})`);
    console.log(`  ${a.rationale}`);
    if (a.evidence) console.log(`  ↳ ${a.evidence}`);
    console.log();
  }
  console.log(`overall: ${card.overall_note}\n`);
}

export function printFaithfulness(f: Faithfulness): void {
  if (!f.applicable) {
    console.log(`● faithfulness: n/a — ${f.note}\n`);
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
  console.log();
}
