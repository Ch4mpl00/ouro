// Postgres implementation of MemoryStore. Deliberately thin: no rules live
// here, only the SQL that the service's rules are expressed in terms of. The
// one piece of real logic is the compare-and-swap in updateDoc.

import { and, arrayOverlaps, cosineDistance, desc, eq, inArray, isNotNull, isNull } from "drizzle-orm";
import type { Database } from "../../db/pg/client";
import { docPatches, facts, memoryIndex, projectDocs, projects } from "../../db/pg/schema";
import type {
  CreateDocInput,
  CreateFactInput,
  CreateProjectInput,
  IndexUpsert,
  MemoryStore,
  SearchIndexOpts,
  UpdateDocInput,
  UpdateFactInput,
} from "./store";
import type { Doc, DocPatch, Edit, Fact, IndexHit, MemoryState, Project } from "./types";

export interface PgMemoryStoreDeps {
  db: Database;
}

export function createPgMemoryStore(deps: PgMemoryStoreDeps): MemoryStore {
  const { db } = deps;

  const toProject = (row: typeof projects.$inferSelect): Project => ({
    id: Number(row.id),
    slug: row.slug,
    title: row.title,
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
  });

  const toDoc = (row: typeof projectDocs.$inferSelect): Doc => ({
    id: Number(row.id),
    projectId: Number(row.projectId),
    name: row.name,
    summary: row.summary,
    body: row.bodyMd,
    version: row.version,
    sizeBytes: Buffer.byteLength(row.bodyMd, "utf-8"),
    updatedAt: row.updatedAt.toISOString(),
  });

  const toPatch = (row: typeof docPatches.$inferSelect): DocPatch => ({
    pid: row.pid,
    docId: Number(row.docId),
    kind: row.kind as DocPatch["kind"],
    edits: (row.edits ?? []) as Edit[],
    bodyBefore: row.bodyBefore,
    versionBefore: row.versionBefore,
    versionAfter: row.versionAfter,
    actor: row.actor,
    rationale: row.rationale,
    createdAt: row.createdAt.toISOString(),
  });

  const toFact = (row: typeof facts.$inferSelect): Fact => ({
    id: Number(row.id),
    body: row.body,
    tags: row.tags ?? [],
    source: row.source,
    state: row.state as MemoryState,
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
  });

  return {
    createProject: async (input: CreateProjectInput) => {
      const [row] = await db.insert(projects).values(input).returning();
      if (!row) throw new Error("createProject inserted no row");
      return toProject(row);
    },

    getProject: async (slug) => {
      const [row] = await db.select().from(projects).where(eq(projects.slug, slug)).limit(1);
      return row ? toProject(row) : null;
    },

    listProjects: async () => (await db.select().from(projects).orderBy(projects.slug)).map(toProject),

    listDocs: async (projectId) => {
      const rows = await db
        .select()
        .from(projectDocs)
        .where(eq(projectDocs.projectId, projectId))
        .orderBy(projectDocs.name);
      return rows.map(toDoc).map(({ name, summary, version, sizeBytes, updatedAt }) => ({
        name,
        summary,
        version,
        sizeBytes,
        updatedAt,
      }));
    },

    getDoc: async (projectId, name) => {
      const [row] = await db
        .select()
        .from(projectDocs)
        .where(and(eq(projectDocs.projectId, projectId), eq(projectDocs.name, name)))
        .limit(1);
      return row ? toDoc(row) : null;
    },

    createDoc: async (input: CreateDocInput) => {
      const [row] = await db
        .insert(projectDocs)
        .values({
          projectId: input.projectId,
          name: input.name,
          summary: input.summary,
          bodyMd: input.body,
        })
        .returning();
      if (!row) throw new Error("createDoc inserted no row");
      return toDoc(row);
    },

    updateDoc: async (input: UpdateDocInput) => {
      // The version in the WHERE clause is the whole concurrency story: the
      // UPDATE matches no row when another agent has already moved on, and
      // the caller turns that into a conflict instead of overwriting.
      const [row] = await db
        .update(projectDocs)
        .set({
          bodyMd: input.body,
          ...(input.summary === undefined ? {} : { summary: input.summary }),
          version: input.expectedVersion + 1,
          updatedAt: new Date(),
        })
        .where(and(eq(projectDocs.id, input.docId), eq(projectDocs.version, input.expectedVersion)))
        .returning();
      return row ? toDoc(row) : null;
    },

    insertPatch: async (input) => {
      const [row] = await db
        .insert(docPatches)
        .values({
          docId: input.docId,
          pid: input.pid,
          kind: input.kind,
          edits: input.edits,
          bodyBefore: input.bodyBefore,
          versionBefore: input.versionBefore,
          versionAfter: input.versionAfter,
          actor: input.actor,
          rationale: input.rationale,
        })
        .returning();
      if (!row) throw new Error("insertPatch inserted no row");
      return toPatch(row);
    },

    listPatches: async (docId, limit) => {
      const rows = await db
        .select()
        .from(docPatches)
        .where(eq(docPatches.docId, docId))
        .orderBy(desc(docPatches.createdAt), desc(docPatches.id))
        .limit(limit);
      return rows.map(toPatch);
    },

    getPatch: async (docId, pid) => {
      const [row] = await db
        .select()
        .from(docPatches)
        .where(and(eq(docPatches.docId, docId), eq(docPatches.pid, pid)))
        .limit(1);
      return row ? toPatch(row) : null;
    },

    createFact: async (input: CreateFactInput) => {
      const [row] = await db.insert(facts).values(input).returning();
      if (!row) throw new Error("createFact inserted no row");
      return toFact(row);
    },

    getFact: async (id) => {
      const [row] = await db.select().from(facts).where(eq(facts.id, id)).limit(1);
      return row ? toFact(row) : null;
    },

    getFactBySource: async (source) => {
      const [row] = await db.select().from(facts).where(eq(facts.source, source)).limit(1);
      return row ? toFact(row) : null;
    },

    updateFact: async (input: UpdateFactInput) => {
      const [row] = await db
        .update(facts)
        .set({
          ...(input.body === undefined ? {} : { body: input.body }),
          ...(input.tags === undefined ? {} : { tags: input.tags }),
          ...(input.state === undefined ? {} : { state: input.state }),
          updatedAt: new Date(),
        })
        .where(eq(facts.id, input.id))
        .returning();
      return row ? toFact(row) : null;
    },

    replaceIndex: async (sourceRef, entries) => {
      // Delete-then-insert rather than upsert: a re-indexed document can have
      // fewer chunks than before, and the leftovers would keep answering
      // recalls with text that is no longer in the document.
      await db.transaction(async (tx) => {
        await tx.delete(memoryIndex).where(eq(memoryIndex.sourceRef, sourceRef));
        if (entries.length === 0) return;
        await tx.insert(memoryIndex).values(entries.map(toIndexRow));
      });
    },

    searchIndex: async (opts: SearchIndexOpts): Promise<IndexHit[]> => {
      const distance = cosineDistance(memoryIndex.embedding, opts.embedding);
      const filters = [isNotNull(memoryIndex.embedding), inArray(memoryIndex.state, opts.states)];
      const tags = (opts.tags ?? []).filter((t) => t.length > 0);
      if (tags.length > 0) filters.push(arrayOverlaps(memoryIndex.tags, tags));

      const rows = await db
        .select({
          id: memoryIndex.id,
          ref: memoryIndex.ref,
          text: memoryIndex.text,
          tags: memoryIndex.tags,
          actor: memoryIndex.actor,
          state: memoryIndex.state,
          ts: memoryIndex.ts,
          distance,
        })
        .from(memoryIndex)
        .where(and(...filters))
        .orderBy(distance)
        .limit(opts.limit);

      return rows.map((row) => ({
        id: Number(row.id),
        ref: row.ref,
        text: row.text,
        tags: row.tags ?? [],
        actor: row.actor,
        state: row.state as MemoryState,
        ts: row.ts.toISOString(),
        // pg returns cosine distance as a numeric string.
        distance: Number(row.distance),
      }));
    },

    listUnembedded: async (limit) => {
      const rows = await db
        .select({ id: memoryIndex.id, text: memoryIndex.text })
        .from(memoryIndex)
        .where(isNull(memoryIndex.embedding))
        .limit(limit);
      return rows.map((row) => ({ id: Number(row.id), text: row.text }));
    },

    setEmbedding: async (id, embedding) => {
      await db
        .update(memoryIndex)
        .set({ embedding, embeddedAt: new Date() })
        .where(eq(memoryIndex.id, id));
    },
  };
}

function toIndexRow(entry: IndexUpsert): typeof memoryIndex.$inferInsert {
  return {
    sourceRef: entry.sourceRef,
    ref: entry.ref,
    text: entry.text,
    tags: entry.tags,
    actor: entry.actor,
    state: entry.state,
    ts: new Date(entry.ts),
    embedding: entry.embedding,
    embeddedAt: entry.embedding ? new Date() : null,
  };
}
