// Postgres implementation of MemoryStore. Deliberately thin: no rules live
// here, only the SQL that the service's rules are expressed in terms of. The
// one piece of real logic is the compare-and-swap in updateDoc, committed in
// one transaction with the patch it records.

import {
  and,
  arrayOverlaps,
  cosineDistance,
  count,
  desc,
  eq,
  inArray,
  isNotNull,
  isNull,
  max,
  notInArray,
  sql,
} from "drizzle-orm";
import type { Database } from "../../db/pg/client";
import {
  docPatches,
  facts,
  journalEntries,
  journalStreams,
  memoryIndex,
  projectDocs,
  projects,
} from "../../db/pg/schema";
import type {
  AppendEntryInput,
  CreateDocInput,
  CreateFactInput,
  CreateProjectInput,
  IndexUpsert,
  MemoryStore,
  NewPatch,
  SearchIndexOpts,
  UpdateDocInput,
  UpdateFactInput,
} from "./store";
import type { Doc, DocPatch, Edit, Fact, IndexHit, JournalEntry, MemoryState, Project } from "./types";

export interface PgMemoryStoreDeps {
  db: Database;
}

type Tx = Parameters<Parameters<Database["transaction"]>[0]>[0];

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

  const insertPatch = async (tx: Tx, docId: number, patch: NewPatch): Promise<DocPatch> => {
    const [row] = await tx
      .insert(docPatches)
      .values({
        docId,
        pid: patch.pid,
        kind: patch.kind,
        edits: patch.edits,
        bodyBefore: patch.bodyBefore,
        versionBefore: patch.versionBefore,
        versionAfter: patch.versionAfter,
        actor: patch.actor,
        rationale: patch.rationale,
      })
      .returning();
    if (!row) throw new Error("insertPatch inserted no row");
    return toPatch(row);
  };

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

    createDoc: async (input: CreateDocInput) =>
      db.transaction(async (tx) => {
        // ON CONFLICT: another writer created the name between our read and
        // this insert. That is a lost race, not a database error.
        const [row] = await tx
          .insert(projectDocs)
          .values({
            projectId: input.projectId,
            name: input.name,
            summary: input.summary,
            bodyMd: input.body,
          })
          .onConflictDoNothing()
          .returning();
        if (!row) return null;
        const doc = toDoc(row);
        return { doc, patch: await insertPatch(tx, doc.id, input.patch) };
      }),

    updateDoc: async (input: UpdateDocInput) =>
      db.transaction(async (tx) => {
        // The version in the WHERE clause is the whole concurrency story: the
        // UPDATE matches no row when another agent has already moved on, and
        // the caller turns that into a conflict instead of overwriting. The
        // patch commits in the same transaction, so history can never miss or
        // reorder a version.
        const [row] = await tx
          .update(projectDocs)
          .set({
            bodyMd: input.body,
            ...(input.summary === undefined ? {} : { summary: input.summary }),
            version: input.expectedVersion + 1,
            updatedAt: new Date(),
          })
          .where(and(eq(projectDocs.id, input.docId), eq(projectDocs.version, input.expectedVersion)))
          .returning();
        if (!row) return null;
        return { doc: toDoc(row), patch: await insertPatch(tx, input.docId, input.patch) };
      }),

    listPatches: async (docId, limit) => {
      const rows = await db
        .select()
        .from(docPatches)
        .where(eq(docPatches.docId, docId))
        .orderBy(desc(docPatches.versionAfter), desc(docPatches.id))
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

    replaceIndex: async (sourceRef, entries, basis) =>
      // Delete-then-insert rather than upsert: a re-indexed document can have
      // fewer chunks than before, and the leftovers would keep answering
      // recalls with text that is no longer in the document. The source row
      // is locked first, so re-indexes of one source can't interleave their
      // delete and insert (which doubled the rows), and the check sees the
      // source's latest committed state.
      db.transaction(async (tx) => {
        if (basis.kind === "fact") {
          const [row] = await tx
            .select({ body: facts.body, tags: facts.tags, state: facts.state })
            .from(facts)
            .where(eq(facts.id, basis.id))
            .for("update");
          const same =
            row !== undefined &&
            row.body === basis.body &&
            row.state === basis.state &&
            row.tags.length === basis.tags.length &&
            row.tags.every((t, i) => t === basis.tags[i]);
          if (!same) return false;
        } else {
          const [row] = await tx
            .select({ version: projectDocs.version })
            .from(projectDocs)
            .where(eq(projectDocs.id, basis.docId))
            .for("update");
          if (row?.version !== basis.version) return false;
        }
        await tx.delete(memoryIndex).where(eq(memoryIndex.sourceRef, sourceRef));
        if (entries.length > 0) await tx.insert(memoryIndex).values(entries.map(toIndexRow));
        return true;
      }),

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

    appendEntry: async (input: AppendEntryInput) =>
      db.transaction(async (tx) => {
        // The upsert always updates, even when the capacity stays, because
        // the row lock it takes is the whole concurrency story: a second
        // append to the stream waits here until the first commits, so its
        // entry id is drawn later and its trim sees the first one's entry.
        // A race to create the stream resolves the same way on the unique
        // name.
        const [stream] = await tx
          .insert(journalStreams)
          .values({ name: input.stream, capacity: input.capacity ?? input.defaultCapacity })
          .onConflictDoUpdate({
            target: journalStreams.name,
            set: { capacity: input.capacity ?? sql`${journalStreams.capacity}` },
          })
          .returning();
        if (!stream) throw new Error("appendEntry upserted no stream");

        const [row] = await tx
          .insert(journalEntries)
          .values({ streamId: stream.id, body: input.body, actor: input.actor, metadata: input.metadata })
          .returning();
        if (!row) throw new Error("appendEntry inserted no entry");

        const newest = tx
          .select({ id: journalEntries.id })
          .from(journalEntries)
          .where(eq(journalEntries.streamId, stream.id))
          .orderBy(desc(journalEntries.id))
          .limit(stream.capacity);
        await tx
          .delete(journalEntries)
          .where(and(eq(journalEntries.streamId, stream.id), notInArray(journalEntries.id, newest)));

        const [stats] = await tx
          .select({ size: count() })
          .from(journalEntries)
          .where(eq(journalEntries.streamId, stream.id));
        const entry = toEntry(row, stream.name);
        return {
          entry,
          stream: { name: stream.name, capacity: stream.capacity, size: stats?.size ?? 0, lastAt: entry.createdAt },
        };
      }),

    tailEntries: async (stream, limit) => {
      const rows = await db
        .select({ entry: journalEntries })
        .from(journalEntries)
        .innerJoin(journalStreams, eq(journalEntries.streamId, journalStreams.id))
        .where(eq(journalStreams.name, stream))
        .orderBy(desc(journalEntries.id))
        .limit(limit);
      return rows.map((r) => toEntry(r.entry, stream));
    },

    listStreams: async () => {
      const rows = await db
        .select({
          name: journalStreams.name,
          capacity: journalStreams.capacity,
          size: count(journalEntries.id),
          lastAt: max(journalEntries.createdAt),
        })
        .from(journalStreams)
        .leftJoin(journalEntries, eq(journalEntries.streamId, journalStreams.id))
        .groupBy(journalStreams.id)
        // Byte order, the same order the in-memory store's sort() gives —
        // a locale collation would sort "digest/news" around "digest-x".
        .orderBy(sql`${journalStreams.name} collate "C"`);
      return rows.map((r) => ({
        name: r.name,
        capacity: r.capacity,
        size: r.size,
        lastAt: r.lastAt?.toISOString() ?? null,
      }));
    },
  };
}

function toEntry(row: typeof journalEntries.$inferSelect, stream: string): JournalEntry {
  return {
    id: Number(row.id),
    stream,
    body: row.body,
    actor: row.actor,
    metadata: row.metadata ?? null,
    createdAt: row.createdAt.toISOString(),
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
