import { config as loadEnv } from "dotenv";
loadEnv({ path: ".env" });
loadEnv({ path: ".env.mcp" });

import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { truncateChunker } from "../services/embeddings/chunker";
import { createOpenAIProvider } from "../services/embeddings/provider";
import { createEmbeddingService } from "../services/embeddings/service";
import { loadConfig, hashCorpusInputs } from "../eval/config";
import { dedupByPairwiseCosine } from "../services/retrieval";
import { readCorpusCache, writeCorpusCache } from "../eval/corpus-cache";
import { buildText } from "../eval/embed";
import { cosineTopK } from "../eval/search";
import { sourceBucket, type CorpusRow, type QueryRow } from "../eval/types";

const EVAL_MAX_CHARS = 6000;
const DEFAULT_K = 15;

function parseArg(name: string): string | undefined {
  const idx = process.argv.indexOf(name);
  return idx !== -1 ? process.argv[idx + 1] : undefined;
}

async function loadJsonl<T>(path: string): Promise<T[]> {
  const raw = await readFile(path, "utf-8");
  const rows: T[] = [];
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue;
    rows.push(JSON.parse(line) as T);
  }
  return rows;
}

function truncTitle(row: CorpusRow, max = 120): string {
  const t = row.title ?? row.body.slice(0, 200);
  const oneLine = t.replace(/\s+/g, " ").trim();
  return oneLine.length > max ? oneLine.slice(0, max - 1) + "…" : oneLine;
}

async function main(): Promise<void> {
  const evalDir = resolve(dirname(fileURLToPath(import.meta.url)), "../eval");
  const configArg = parseArg("--config");
  const configPath = resolve(configArg ?? `${evalDir}/configs/baseline.json`);
  const qidsArg = parseArg("--qids");
  if (!qidsArg) throw new Error("--qids q-014,q-017,... required");
  const requestedQids = qidsArg.split(",").map((s) => s.trim()).filter(Boolean);
  const kArg = parseArg("--k");
  const k = kArg ? Number.parseInt(kArg, 10) : DEFAULT_K;
  if (!Number.isFinite(k) || k <= 0) throw new Error("--k must be a positive integer");

  const apiKey = process.env.OPENAI_API_KEY;
  if (!apiKey) throw new Error("OPENAI_API_KEY is required (set in .env.mcp)");

  const config = await loadConfig(configPath);
  console.log(`[inspect] config: ${config.name} (${configPath})`);
  console.log(`[inspect] qids: ${requestedQids.join(", ")} · k=${k}\n`);

  const corpus = await loadJsonl<CorpusRow>(`${evalDir}/fixtures/corpus.jsonl`);
  const allQueries = await loadJsonl<QueryRow>(`${evalDir}/fixtures/queries.jsonl`);
  const corpusById = new Map(corpus.map((row) => [row.id, row]));
  const queries = requestedQids.map((qid) => {
    const q = allQueries.find((x) => x.id === qid);
    if (!q) throw new Error(`unknown qid: ${qid}`);
    return q;
  });

  const provider = createEmbeddingService({
    provider: createOpenAIProvider({
      apiKey,
      model: config.retrieval.embed.model,
      dimensions: config.retrieval.embed.dimensions,
    }),
    chunker: truncateChunker(EVAL_MAX_CHARS),
  });

  const configHash = hashCorpusInputs(config);
  const cacheDir = `${evalDir}/cache`;
  let corpusVecs = await readCorpusCache(cacheDir, configHash);
  if (!corpusVecs || corpusVecs.size !== corpus.length) {
    console.log(`[inspect] cache miss, embedding ${corpus.length} corpus rows…`);
    const texts = corpus.map((row) => buildText(row, config.retrieval.buildText));
    const embeddings = await provider.embedBatch(texts);
    corpusVecs = new Map<number, number[]>();
    corpus.forEach((row, i) => {
      const vec = embeddings[i];
      if (!vec) throw new Error(`missing embedding for corpus row ${row.id}`);
      corpusVecs!.set(row.id, vec);
    });
    await writeCorpusCache(cacheDir, configHash, corpusVecs);
  } else {
    console.log(`[inspect] corpus cache hit (${corpusVecs.size} rows)\n`);
  }

  const corpusForSearch = corpus
    .map((row) => {
      const embedding = corpusVecs!.get(row.id);
      return embedding ? { id: row.id, embedding } : null;
    })
    .filter((x): x is { id: number; embedding: number[] } => x !== null);

  const queryTexts = queries.map((q) =>
    config.query.field === "reformulation" ? q.reformulation : q.query,
  );
  const queryVecs = await provider.embedBatch(queryTexts);

  for (let i = 0; i < queries.length; i++) {
    const q = queries[i]!;
    const vec = queryVecs[i];
    if (!vec) throw new Error(`missing query vector for ${q.id}`);
    const dedupCfg = config.retrieval.dedup;
    const poolSize = dedupCfg ? Math.max(k * 2, 30) : k;
    const pool = cosineTopK(vec, corpusForSearch, poolSize);
    const afterDedup = dedupCfg
      ? dedupByPairwiseCosine(
          pool,
          (item) => corpusVecs!.get(item.id) ?? null,
          dedupCfg.threshold,
        )
      : pool;
    const topK = afterDedup.slice(0, k);
    const goldSet = new Set(q.gold);
    const acceptableSet = new Set(q.acceptable);

    console.log(`━━━ ${q.id} ━━━`);
    console.log(`query:         ${q.query}`);
    console.log(`reformulation: ${q.reformulation}`);
    console.log(
      `gold: ${q.gold.length} · acceptable: ${q.acceptable.length}`,
    );
    console.log("");

    const hits5 = topK.slice(0, 5).filter((x) => goldSet.has(x.id)).length;
    const hits10 = topK.slice(0, 10).filter((x) => goldSet.has(x.id)).length;
    const hitsK = topK.filter((x) => goldSet.has(x.id)).length;
    console.log(
      `gold hits  top-5: ${hits5}/${q.gold.length}   top-10: ${hits10}/${q.gold.length}   top-${k}: ${hitsK}/${q.gold.length}`,
    );
    console.log("");

    console.log(`rank  dist   mark  source                          title`);
    console.log(`────  ─────  ────  ──────                          ─────`);
    for (let r = 0; r < topK.length; r++) {
      const item = topK[r]!;
      const row = corpusById.get(item.id);
      const mark = goldSet.has(item.id)
        ? "GOLD"
        : acceptableSet.has(item.id)
          ? "okay"
          : "    ";
      const src = row ? sourceBucket(row).slice(0, 30).padEnd(30) : "?".padEnd(30);
      const title = row ? truncTitle(row) : "<row not found>";
      console.log(
        `${String(r + 1).padStart(4)}  ${item.distance.toFixed(3)}  ${mark}  ${src}  ${title}`,
      );
    }
    console.log("");

    // List gold that wasn't retrieved at all in top-K
    const retrievedIds = new Set(topK.map((x) => x.id));
    const missedGold = q.gold.filter((id) => !retrievedIds.has(id));
    if (missedGold.length > 0) {
      console.log(`missed gold (not in top-${k}): ${missedGold.length}`);
      for (const id of missedGold.slice(0, 8)) {
        const row = corpusById.get(id);
        const src = row ? sourceBucket(row).slice(0, 30).padEnd(30) : "?".padEnd(30);
        const title = row ? truncTitle(row) : "<not in corpus>";
        console.log(`        id=${String(id).padStart(4)}              ${src}  ${title}`);
      }
      if (missedGold.length > 8) {
        console.log(`        …and ${missedGold.length - 8} more`);
      }
    }
    console.log("");
  }
}

main().catch((err) => {
  console.error("[inspect] crashed:", err);
  process.exitCode = 1;
});
