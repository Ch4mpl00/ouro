import {
  and,
  arrayOverlaps,
  cosineDistance,
  eq,
  isNotNull,
  isNull,
} from "drizzle-orm";
import type { Database } from "../../db/pg/client";
import { knowledgeBaseNotes } from "../../db/pg/schema";
import type { EmbeddingService } from "../embeddings/service";

// The single facade over knowledge_base_notes. Notes go in (add) →
// stored → embedded → searchable (find). Mirrors NewsRepository but much
// smaller: one source, one row at a time, no poller and no cross-source
// dedup. Only `body` is embedded; `tags` are LLM-supplied metadata used
// for the optional overlap filter, never vectorised (see schema note).

export interface Note {
  id: number;
  body: string;
  tags: string[];
  source: string | null;
  createdAt: string;
  updatedAt: string;
}

export interface AddNoteInput {
  body: string;
  // Caller (the agent LLM) generates these; we store them verbatim after
  // trim/dedup. Omitted → empty array.
  tags?: string[];
  source?: string | null;
}

export interface AddNoteResult {
  id: number;
  // false when the inline embed failed (row persists with a NULL vector;
  // embedMissingBatch / embed:backfill will retry). The note is not
  // findable until embedded.
  embedded: boolean;
  // The tags actually stored (after trim/dedup) — so the caller echoes
  // what's in the row, not its raw input.
  tags: string[];
}

export interface FindNotesOpts {
  query: string;
  k?: number;
  // Optional structured filter: keep only notes sharing at least one of
  // these tags (array overlap). Applied on top of the vector search.
  tags?: string[];
}

export interface NoteSearchResult extends Note {
  distance: number;
}

export interface EmbedResult {
  embedded: number;
  failed: number;
}

export interface KnowledgeRepository {
  addNote(input: AddNoteInput): Promise<AddNoteResult>;
  findNotes(opts: FindNotesOpts): Promise<NoteSearchResult[]>;
  // Picks up notes with embedding=NULL (left by a failed inline embed)
  // and embeds one batch. Returns 0/0 when the backlog is empty, so
  // callers can loop until drained (embed:backfill does).
  embedMissingBatch(batchSize?: number): Promise<EmbedResult>;
}

export interface KnowledgeRepositoryDeps {
  db: Database;
  embeddings: EmbeddingService;
}

export function createKnowledgeRepository(
  deps: KnowledgeRepositoryDeps,
): KnowledgeRepository {
  const { db, embeddings } = deps;

  // Embed text == body only (the user's choice; tags stay out of the
  // vector). Shared by addNote and the backfill path so the contract is
  // one place. On provider failure the rows keep their NULL embedding.
  const embedRows = async (
    rows: { id: number; body: string }[],
  ): Promise<EmbedResult> => {
    if (rows.length === 0) return { embedded: 0, failed: 0 };
    let vectors: number[][];
    try {
      vectors = await embeddings.embedBatch(rows.map((r) => r.body.trim()));
    } catch (err) {
      console.error(
        `[knowledge] embed failed for ${rows.length} note(s):`,
        err instanceof Error ? err.message : err,
      );
      return { embedded: 0, failed: rows.length };
    }
    let embedded = 0;
    for (let i = 0; i < rows.length; i++) {
      const vector = vectors[i];
      if (!vector) continue;
      await db
        .update(knowledgeBaseNotes)
        .set({ embedding: vector, embeddedAt: new Date() })
        .where(eq(knowledgeBaseNotes.id, rows[i]!.id));
      embedded++;
    }
    return { embedded, failed: 0 };
  };

  return {
    addNote: async (input) => {
      const tags = normalizeTags(input.tags);
      const [inserted] = await db
        .insert(knowledgeBaseNotes)
        .values({ body: input.body, tags, source: input.source ?? null })
        .returning({
          id: knowledgeBaseNotes.id,
          body: knowledgeBaseNotes.body,
        });
      if (!inserted) throw new Error("addNote insert returned no row");
      const id = Number(inserted.id);
      const result = await embedRows([{ id, body: inserted.body }]);
      return { id, embedded: result.embedded > 0, tags };
    },

    findNotes: async (opts) => {
      const k = opts.k ?? 10;
      const [vector] = await embeddings.embedBatch([opts.query]);
      if (!vector) return [];

      const distance = cosineDistance(knowledgeBaseNotes.embedding, vector);
      const filters = [isNotNull(knowledgeBaseNotes.embedding)];
      const tags = normalizeTags(opts.tags);
      if (tags.length > 0) {
        filters.push(arrayOverlaps(knowledgeBaseNotes.tags, tags));
      }

      const rows = await db
        .select({
          id: knowledgeBaseNotes.id,
          body: knowledgeBaseNotes.body,
          tags: knowledgeBaseNotes.tags,
          source: knowledgeBaseNotes.source,
          createdAt: knowledgeBaseNotes.createdAt,
          updatedAt: knowledgeBaseNotes.updatedAt,
          distance,
        })
        .from(knowledgeBaseNotes)
        .where(and(...filters))
        .orderBy(distance)
        .limit(k);

      return rows.map((r) => ({
        id: Number(r.id),
        body: r.body,
        tags: r.tags ?? [],
        source: r.source,
        createdAt: r.createdAt.toISOString(),
        updatedAt: r.updatedAt.toISOString(),
        // pg returns the cosine distance as a numeric string; coerce.
        distance: Number(r.distance),
      }));
    },

    embedMissingBatch: async (batchSize = 100) => {
      const rows = await db
        .select({
          id: knowledgeBaseNotes.id,
          body: knowledgeBaseNotes.body,
        })
        .from(knowledgeBaseNotes)
        .where(isNull(knowledgeBaseNotes.embedding))
        .limit(batchSize);
      if (rows.length === 0) return { embedded: 0, failed: 0 };
      return embedRows(rows.map((r) => ({ id: Number(r.id), body: r.body })));
    },
  };
}

// Trim, drop empties, de-duplicate — but preserve the casing/wording the
// LLM chose. Tag normalisation policy lives at the call site (the tool
// asks the model for short lowercase tags) so both add and the find
// filter stay consistent without the repo imposing a scheme.
function normalizeTags(tags: string[] | undefined): string[] {
  if (!tags || tags.length === 0) return [];
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of tags) {
    const tag = raw.trim();
    if (!tag || seen.has(tag)) continue;
    seen.add(tag);
    out.push(tag);
  }
  return out;
}
