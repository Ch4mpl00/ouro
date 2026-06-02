import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import type { EvalConfig } from "./types";

export async function loadConfig(path: string): Promise<EvalConfig> {
  const raw = await readFile(path, "utf-8");
  const parsed = JSON.parse(raw);
  return validateConfig(parsed);
}

function validateConfig(obj: unknown): EvalConfig {
  if (!obj || typeof obj !== "object") {
    throw new Error("config must be an object");
  }
  const c = obj as Record<string, unknown>;
  const name = c.name;
  if (typeof name !== "string" || name.length === 0) {
    throw new Error("config.name must be a non-empty string");
  }
  const retrieval = c.retrieval as Record<string, unknown> | undefined;
  if (!retrieval) throw new Error("config.retrieval is required");
  const embed = retrieval.embed as Record<string, unknown> | undefined;
  if (!embed || typeof embed.model !== "string" || typeof embed.dimensions !== "number") {
    throw new Error("config.retrieval.embed.{model,dimensions} required");
  }
  if (retrieval.buildText !== "title+body" && retrieval.buildText !== "body-only") {
    throw new Error("config.retrieval.buildText must be 'title+body' or 'body-only'");
  }
  if (typeof retrieval.topK !== "number" || retrieval.topK <= 0) {
    throw new Error("config.retrieval.topK must be a positive number");
  }
  const query = c.query as Record<string, unknown> | undefined;
  if (!query || (query.field !== "query" && query.field !== "reformulation")) {
    throw new Error("config.query.field must be 'query' or 'reformulation'");
  }
  const scoring = c.scoring as Record<string, unknown> | undefined;
  if (!scoring || scoring.mode !== "binary") {
    throw new Error("config.scoring.mode must be 'binary' (only mode supported today)");
  }
  return {
    name,
    retrieval: {
      embed: { model: embed.model, dimensions: embed.dimensions },
      buildText: retrieval.buildText,
      topK: retrieval.topK,
      rerank: null,
    },
    query: { field: query.field },
    scoring: { mode: "binary" },
  };
}

// Hash only the fields that affect corpus embeddings — model, dimensions,
// buildText. Query field and scoring mode don't change corpus vectors, so
// they don't invalidate the cache.
export function hashCorpusInputs(config: EvalConfig): string {
  const key = JSON.stringify({
    model: config.retrieval.embed.model,
    dimensions: config.retrieval.embed.dimensions,
    buildText: config.retrieval.buildText,
  });
  return createHash("sha256").update(key).digest("hex").slice(0, 16);
}
