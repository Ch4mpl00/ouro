import { config as loadEnv } from "dotenv";
loadEnv({ path: ".env" });
loadEnv({ path: ".env.mcp" });

import { mkdir, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { truncateChunker } from "../services/embeddings/chunker";
import { createOpenAIProvider } from "../services/embeddings/provider";
import { createEmbeddingService } from "../services/embeddings/service";
import { loadConfig } from "../eval/config";
import { renderMarkdown } from "../eval/report";
import { createEvalRun } from "../eval/run";

// Cyrillic text uses ~6 bytes/token vs ~4 for English, so the prod
// limit of 8000 chars (~8000 tokens English / ~10k tokens Cyrillic)
// exceeds the model's 8192 hard cap on some posts. 6000 chars stays
// within the cap for the channels we ingest.
const EVAL_MAX_CHARS = 6000;

function parseArg(name: string): string | undefined {
  const idx = process.argv.indexOf(name);
  return idx !== -1 ? process.argv[idx + 1] : undefined;
}

function timestamp(): string {
  const d = new Date();
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getUTCFullYear()}${pad(d.getUTCMonth() + 1)}${pad(d.getUTCDate())}-${pad(d.getUTCHours())}${pad(d.getUTCMinutes())}`;
}

async function main(): Promise<void> {
  const evalDir = resolve(dirname(fileURLToPath(import.meta.url)), "../eval");
  const configArg = parseArg("--config");
  const configPath = resolve(configArg ?? `${evalDir}/configs/baseline.json`);

  const apiKey = process.env.OPENAI_API_KEY;
  if (!apiKey) {
    throw new Error("OPENAI_API_KEY is required (set in .env.mcp)");
  }

  const config = await loadConfig(configPath);
  console.log(`[eval-rag] config: ${config.name} (${configPath})`);

  const evalRun = createEvalRun({
    embedFactory: (model, dimensions) =>
      createEmbeddingService({
        provider: createOpenAIProvider({ apiKey, model, dimensions }),
        chunker: truncateChunker(EVAL_MAX_CHARS),
      }),
    corpusPath: `${evalDir}/fixtures/corpus.jsonl`,
    queriesPath: `${evalDir}/fixtures/queries.jsonl`,
    cacheDir: `${evalDir}/cache`,
  });

  const startedAt = Date.now();
  const result = await evalRun.run(config);
  const durationMs = Date.now() - startedAt;

  const reportDir = `${evalDir}/reports`;
  await mkdir(reportDir, { recursive: true });
  const reportPath = `${reportDir}/${config.name}-${timestamp()}.md`;
  await writeFile(reportPath, renderMarkdown(result), "utf-8");

  console.log("");
  console.log(`[eval-rag] ${result.cacheHit ? "cache hit" : "fresh embed"} · ${durationMs}ms`);
  console.log(`[eval-rag] scored queries: ${result.aggregate.scoredQueries}`);
  console.log(`[eval-rag] P@5   ${result.aggregate.precisionAt5.toFixed(3)}   R@5   ${result.aggregate.recallAt5.toFixed(3)}`);
  console.log(`[eval-rag] P@10  ${result.aggregate.precisionAt10.toFixed(3)}   R@10  ${result.aggregate.recallAt10.toFixed(3)}   R@30  ${result.aggregate.recallAt30.toFixed(3)}`);
  console.log(`[eval-rag] MRR   ${result.aggregate.mrr.toFixed(3)}`);
  console.log(`[eval-rag] uniq sources @5/@10  ${result.aggregate.meanUniqueSourcesAt5.toFixed(2)} / ${result.aggregate.meanUniqueSourcesAt10.toFixed(2)}`);
  console.log(`[eval-rag] report → ${reportPath}`);
}

main().catch((err) => {
  console.error("[eval-rag] crashed:", err);
  process.exitCode = 1;
});
