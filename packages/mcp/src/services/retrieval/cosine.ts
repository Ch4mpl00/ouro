// OpenAI text-embedding-3 and pgvector both return unit-normalized
// vectors when used with the `dimensions` parameter, so cosine similarity
// = dot product and cosine distance = 1 - dot product. We don't
// re-normalize.
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
