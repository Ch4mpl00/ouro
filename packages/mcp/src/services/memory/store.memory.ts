// In-memory MemoryStore. Used by the service tests — the rules that protect a
// document (versioning, atomicity, conflicts) are worth testing directly, and
// a fake store is the only way to do that without a Postgres in CI.
//
// Kept deliberately literal: no shortcuts the PG implementation can't also
// take, so a test passing here means the same thing there.

import type { Doc, DocPatch, DocSummary, Fact, IndexHit, Project } from "./types";
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

export interface InMemoryStoreOpts {
  // Fires just before an updateDoc compare-and-swap. The tests use it to
  // simulate another agent committing in the window between read and write.
  onBeforeUpdateDoc?: (docId: number) => Promise<void> | void;
  now?: () => Date;
}

interface DocRow extends Doc {}

export function createInMemoryStore(opts: InMemoryStoreOpts = {}): MemoryStore {
  const now = opts.now ?? (() => new Date());
  const projects: Project[] = [];
  const docs: DocRow[] = [];
  const patches: DocPatch[] = [];
  const facts: Fact[] = [];
  const index: (IndexUpsert & { id: number })[] = [];
  let seq = 0;
  const nextId = (): number => ++seq;
  const stamp = (): string => now().toISOString();

  const summarise = (doc: DocRow): DocSummary => ({
    name: doc.name,
    summary: doc.summary,
    version: doc.version,
    sizeBytes: Buffer.byteLength(doc.body, "utf-8"),
    updatedAt: doc.updatedAt,
  });

  return {
    createProject: async (input: CreateProjectInput) => {
      if (projects.some((p) => p.slug === input.slug)) {
        throw new Error(`project ${input.slug} already exists`);
      }
      const project: Project = {
        id: nextId(),
        slug: input.slug,
        title: input.title,
        createdAt: stamp(),
        updatedAt: stamp(),
      };
      projects.push(project);
      return project;
    },

    getProject: async (slug) => projects.find((p) => p.slug === slug) ?? null,

    listProjects: async () => [...projects].sort((a, b) => a.slug.localeCompare(b.slug)),

    listDocs: async (projectId) =>
      docs
        .filter((d) => d.projectId === projectId)
        .map(summarise)
        .sort((a, b) => a.name.localeCompare(b.name)),

    getDoc: async (projectId, name) => {
      const doc = docs.find((d) => d.projectId === projectId && d.name === name);
      return doc ? { ...doc, sizeBytes: Buffer.byteLength(doc.body, "utf-8") } : null;
    },

    createDoc: async (input: CreateDocInput) => {
      if (docs.some((d) => d.projectId === input.projectId && d.name === input.name)) {
        throw new Error(`document ${input.name} already exists`);
      }
      const doc: DocRow = {
        id: nextId(),
        projectId: input.projectId,
        name: input.name,
        summary: input.summary,
        body: input.body,
        version: 1,
        sizeBytes: Buffer.byteLength(input.body, "utf-8"),
        updatedAt: stamp(),
      };
      docs.push(doc);
      return { ...doc };
    },

    updateDoc: async (input: UpdateDocInput) => {
      await opts.onBeforeUpdateDoc?.(input.docId);
      const doc = docs.find((d) => d.id === input.docId);
      if (!doc) return null;
      if (doc.version !== input.expectedVersion) return null;
      doc.body = input.body;
      if (input.summary !== undefined) doc.summary = input.summary;
      doc.version += 1;
      doc.sizeBytes = Buffer.byteLength(doc.body, "utf-8");
      doc.updatedAt = stamp();
      return { ...doc };
    },

    insertPatch: async (input) => {
      const patch: DocPatch = { ...input, createdAt: stamp() };
      patches.push(patch);
      return patch;
    },

    listPatches: async (docId, limit) =>
      patches
        .filter((p) => p.docId === docId)
        .slice()
        .reverse()
        .slice(0, limit),

    getPatch: async (docId, pid) => patches.find((p) => p.docId === docId && p.pid === pid) ?? null,

    createFact: async (input: CreateFactInput) => {
      const fact: Fact = {
        id: nextId(),
        body: input.body,
        tags: input.tags,
        source: input.source,
        state: "active",
        createdAt: stamp(),
        updatedAt: stamp(),
      };
      facts.push(fact);
      return { ...fact };
    },

    getFact: async (id) => {
      const fact = facts.find((f) => f.id === id);
      return fact ? { ...fact } : null;
    },

    getFactBySource: async (source) => {
      const fact = facts.find((f) => f.source === source);
      return fact ? { ...fact } : null;
    },

    updateFact: async (input: UpdateFactInput) => {
      const fact = facts.find((f) => f.id === input.id);
      if (!fact) return null;
      if (input.body !== undefined) fact.body = input.body;
      if (input.tags !== undefined) fact.tags = input.tags;
      if (input.state !== undefined) fact.state = input.state;
      fact.updatedAt = stamp();
      return { ...fact };
    },

    replaceIndex: async (sourceRef, entries) => {
      for (let i = index.length - 1; i >= 0; i--) {
        if (index[i]!.sourceRef === sourceRef) index.splice(i, 1);
      }
      for (const entry of entries) index.push({ ...entry, id: nextId() });
    },

    searchIndex: async (opts_: SearchIndexOpts): Promise<IndexHit[]> => {
      const states = new Set(opts_.states);
      const wanted = opts_.tags?.filter((t) => t.length > 0) ?? [];
      return index
        .filter((row) => row.embedding !== null)
        .filter((row) => states.has(row.state))
        .filter((row) => wanted.length === 0 || row.tags.some((t) => wanted.includes(t)))
        .map((row) => ({
          id: row.id,
          ref: row.ref,
          text: row.text,
          tags: row.tags,
          actor: row.actor,
          state: row.state,
          ts: row.ts,
          distance: cosineDistance(row.embedding!, opts_.embedding),
        }))
        .sort((a, b) => a.distance - b.distance)
        .slice(0, opts_.limit);
    },

    listUnembedded: async (limit) =>
      index
        .filter((row) => row.embedding === null)
        .slice(0, limit)
        .map((row) => ({ id: row.id, text: row.text })),

    setEmbedding: async (id, embedding) => {
      const row = index.find((r) => r.id === id);
      if (row) row.embedding = embedding;
    },
  };
}

function cosineDistance(a: number[], b: number[]): number {
  let dot = 0;
  let normA = 0;
  let normB = 0;
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const x = a[i] ?? 0;
    const y = b[i] ?? 0;
    dot += x * y;
    normA += x * x;
    normB += y * y;
  }
  if (normA === 0 || normB === 0) return 1;
  return 1 - dot / (Math.sqrt(normA) * Math.sqrt(normB));
}
