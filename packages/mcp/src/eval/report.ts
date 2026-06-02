import type { EvalResult, PerQueryResult } from "./types";

export function renderMarkdown(result: EvalResult): string {
  const lines: string[] = [];
  const { config, configHash, aggregate, perQuery, negativeTests, cacheHit } = result;

  lines.push(`# Eval: ${config.name}`);
  lines.push("");
  lines.push(`Config hash: \`${configHash}\` ${cacheHit ? "(corpus cache hit)" : "(fresh embed)"}`);
  lines.push("");
  lines.push("## Config");
  lines.push("```json");
  lines.push(JSON.stringify(config, null, 2));
  lines.push("```");
  lines.push("");

  lines.push("## Aggregate");
  lines.push("");
  lines.push("| Metric | Value |");
  lines.push("|---|---|");
  lines.push(`| Scored queries (gold > 0) | ${aggregate.scoredQueries} |`);
  lines.push(`| Precision@5 | ${fmt(aggregate.precisionAt5)} |`);
  lines.push(`| Precision@10 | ${fmt(aggregate.precisionAt10)} |`);
  lines.push(`| Recall@5 | ${fmt(aggregate.recallAt5)} |`);
  lines.push(`| Recall@10 | ${fmt(aggregate.recallAt10)} |`);
  lines.push(`| MRR | ${fmt(aggregate.mrr)} |`);
  lines.push(
    `| Mean distance to first gold | ${aggregate.meanDistToFirstGold === null ? "—" : fmt(aggregate.meanDistToFirstGold)} |`,
  );
  lines.push("");

  lines.push("## Per-query");
  lines.push("");
  lines.push("| qid | query | gold | hit@5 | hit@10 | P@5 | P@10 | first-gold rank | dist to gold |");
  lines.push("|---|---|---|---|---|---|---|---|---|");
  for (const q of perQuery) {
    lines.push(perQueryRow(q));
  }
  lines.push("");

  if (negativeTests.length > 0) {
    lines.push("## Negative tests (gold = 0)");
    lines.push("");
    lines.push("Queries where the corpus has nothing relevant. Min distance is the closest");
    lines.push("result the retriever surfaced — if it's low (< ~0.5), the retriever is");
    lines.push("confidently wrong; high distance is correct \"I don't know\" behaviour.");
    lines.push("");
    lines.push("| qid | query | min distance | top-1 id |");
    lines.push("|---|---|---|---|");
    for (const n of negativeTests) {
      lines.push(`| ${n.qid} | ${escapePipe(n.query)} | ${fmt(n.minDistance)} | ${n.top1Id} |`);
    }
    lines.push("");
  }

  return lines.join("\n");
}

function perQueryRow(q: PerQueryResult): string {
  const recall5 = q.goldCount > 0 ? `${q.hitAt5}/${q.goldCount}` : "—";
  const recall10 = q.goldCount > 0 ? `${q.hitAt10}/${q.goldCount}` : "—";
  const rank = q.firstGoldRank === null ? "—" : String(q.firstGoldRank);
  const dist = q.distToFirstGold === null ? "—" : fmt(q.distToFirstGold);
  return `| ${q.qid} | ${escapePipe(q.query)} | ${q.goldCount} | ${recall5} | ${recall10} | ${fmt(q.precisionAt5)} | ${fmt(q.precisionAt10)} | ${rank} | ${dist} |`;
}

function fmt(n: number): string {
  if (Number.isNaN(n)) return "—";
  return n.toFixed(3);
}

function escapePipe(s: string): string {
  return s.replace(/\|/g, "\\|");
}
