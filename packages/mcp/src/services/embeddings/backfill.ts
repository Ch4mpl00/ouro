import type { EmbeddingService } from "./service";

// Draining the NULL-vector backlog a store keeps after a failed inline
// embed (`pnpm embed:backfill`). Store-agnostic: each store supplies its own
// `embedMissingBatch` over its rows, in id order, past a cursor.

export interface BackfillBatch {
  embedded: number;
  failed: number;
  // The last id this batch looked at; null when nothing is left past the
  // cursor.
  lastId: number | null;
  // The embedder itself is down: a batch failed and so did a known-good
  // probe. Nothing later in the backlog would fare better.
  unavailable: boolean;
}

export type EmbedMissingBatch = (batchSize?: number, afterId?: number) => Promise<BackfillBatch>;

// A text any embedder accepts, to tell "the embedder is down" from "it
// rejects one of these inputs".
const PROBE = "probe";

export class EmbedderUnavailableError extends Error {}

// One request for the batch. When it fails: if a probe fails too, the
// embedder is down (EmbedderUnavailableError); otherwise some input is
// rejected — one bad input fails a whole request — so each text is tried on
// its own and only the rejected ones come back null.
export async function embedIsolating(embeddings: EmbeddingService, texts: string[]): Promise<(number[] | null)[]> {
  if (texts.length === 0) return [];
  try {
    const vectors = await embeddings.embedBatch(texts);
    return texts.map((_, i) => vectors[i] ?? null);
  } catch (batchError) {
    try {
      await embeddings.embed(PROBE);
    } catch {
      throw new EmbedderUnavailableError(batchError instanceof Error ? batchError.message : String(batchError));
    }
    const out: (number[] | null)[] = [];
    for (const text of texts) out.push(await embeddings.embed(text).catch(() => null));
    return out;
  }
}

// Embeds one store's rows and counts the result — shared by the stores'
// `embedMissingBatch`, which only select and write.
export async function backfillRows<R extends { id: number }>(
  embeddings: EmbeddingService,
  rows: R[],
  text: (row: R) => string,
  write: (row: R, vector: number[]) => Promise<void>,
): Promise<BackfillBatch> {
  const lastId = rows.length === 0 ? null : Math.max(...rows.map((r) => r.id));
  if (rows.length === 0) return { embedded: 0, failed: 0, lastId, unavailable: false };
  let vectors: (number[] | null)[];
  try {
    vectors = await embedIsolating(embeddings, rows.map(text));
  } catch (err) {
    if (err instanceof EmbedderUnavailableError) return { embedded: 0, failed: rows.length, lastId, unavailable: true };
    throw err;
  }
  let embedded = 0;
  for (const [i, row] of rows.entries()) {
    const vector = vectors[i];
    if (!vector) continue;
    await write(row, vector);
    embedded++;
  }
  return { embedded, failed: rows.length - embedded, lastId, unavailable: false };
}

export interface DrainOptions {
  batchSize?: number;
  log?: (line: string) => void;
}

// One pass over the backlog in id order: every row is tried once, so a row
// the embedder rejects for good can't hold up the rows behind it. Stops early
// only when the embedder is down.
export async function drainMissing(
  label: string,
  embedMissingBatch: EmbedMissingBatch,
  options: DrainOptions = {},
): Promise<{ embedded: number; failed: number }> {
  const batchSize = options.batchSize ?? 100;
  const log = options.log ?? ((line: string) => console.log(line));
  let embedded = 0;
  let failed = 0;
  let afterId = 0;
  for (;;) {
    const batch = await embedMissingBatch(batchSize, afterId);
    if (batch.lastId === null) break;
    embedded += batch.embedded;
    failed += batch.failed;
    log(`[embed-backfill:${label}] batch: embedded=${batch.embedded}, failed=${batch.failed} (running totals: ${embedded}/${failed})`);
    if (batch.unavailable) {
      log(`[embed-backfill:${label}] the embedder is unavailable; giving up`);
      break;
    }
    afterId = batch.lastId;
  }
  log(`[embed-backfill:${label}] done: embedded=${embedded}, failed=${failed}`);
  return { embedded, failed };
}
