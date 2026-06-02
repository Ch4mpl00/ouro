import { cosineDistance } from "../services/retrieval";
import type { RetrievedItem } from "./types";

export { cosineDistance };

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
