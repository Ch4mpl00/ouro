import type { RetrievedItem } from "./types";

// OpenAI text-embedding-3 returns unit-normalized vectors when the
// `dimensions` parameter is set, so cosine similarity = dot product
// and cosine distance = 1 - dot product. We don't re-normalize.
export function cosineDistance(a: number[], b: number[]): number {
  if (a.length !== b.length) {
    throw new Error(`vector length mismatch: ${a.length} vs ${b.length}`);
  }
  let dot = 0;
  for (let i = 0; i < a.length; i++) {
    const av = a[i];
    const bv = b[i];
    if (av === undefined || bv === undefined) continue;
    dot += av * bv;
  }
  return 1 - dot;
}

export function cosineTopK(
  queryVec: number[],
  corpus: Array<{ id: number; embedding: number[] }>,
  k: number,
): RetrievedItem[] {
  const scored = corpus.map((row) => ({
    id: row.id,
    distance: cosineDistance(queryVec, row.embedding),
  }));
  scored.sort((a, b) => a.distance - b.distance);
  return scored.slice(0, k);
}
