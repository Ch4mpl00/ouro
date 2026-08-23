// The persistence port for unified memory.
//
// The service holds every rule (versioning, patch application, ranking); the
// store is deliberately dumb CRUD. That split is what lets the acceptance
// criteria in .claude/tasks/unified-memory.md be tested without a Postgres,
// and it keeps the PG implementation small enough to read in one sitting.

import type { Doc, DocPatch, DocSummary, Fact, IndexEntry, IndexHit, MemoryState, Project } from "./types";

export interface CreateProjectInput {
  slug: string;
  title: string;
}

export interface CreateDocInput {
  projectId: number;
  name: string;
  summary: string | null;
  body: string;
}

export interface UpdateDocInput {
  docId: number;
  // Compare-and-swap. The update must not apply when the document has moved
  // on — that is the whole concurrency story for multiple agents (D1).
  expectedVersion: number;
  body: string;
  summary?: string | null;
}

export interface CreateFactInput {
  body: string;
  tags: string[];
  source: string | null;
}

export interface UpdateFactInput {
  id: number;
  body?: string;
  tags?: string[];
  state?: MemoryState;
}

// A row on its way into the search projection. `sourceRef` identifies the
// owning object (`doc:slug/name`, `fact:88`) and is what a re-index deletes
// by — exact equality, never a LIKE prefix, so `fact:88` can't wipe `fact:880`.
export interface IndexUpsert extends IndexEntry {
  sourceRef: string;
  embedding: number[] | null;
}

export interface SearchIndexOpts {
  embedding: number[];
  limit: number;
  states: MemoryState[];
  tags?: string[];
}

export interface MemoryStore {
  createProject(input: CreateProjectInput): Promise<Project>;
  getProject(slug: string): Promise<Project | null>;
  listProjects(): Promise<Project[]>;

  listDocs(projectId: number): Promise<DocSummary[]>;
  getDoc(projectId: number, name: string): Promise<Doc | null>;
  createDoc(input: CreateDocInput): Promise<Doc>;
  // Resolves to null when `expectedVersion` no longer matches. Callers turn
  // that into a conflict the agent can act on, never a silent overwrite.
  updateDoc(input: UpdateDocInput): Promise<Doc | null>;

  insertPatch(input: Omit<DocPatch, "createdAt">): Promise<DocPatch>;
  listPatches(docId: number, limit: number): Promise<DocPatch[]>;
  getPatch(docId: number, pid: string): Promise<DocPatch | null>;

  createFact(input: CreateFactInput): Promise<Fact>;
  getFact(id: number): Promise<Fact | null>;
  // Lookup by provenance string, which is what makes the one-shot import of
  // knowledge_base_notes re-runnable without duplicating anything.
  getFactBySource(source: string): Promise<Fact | null>;
  updateFact(input: UpdateFactInput): Promise<Fact | null>;

  // Replaces every index row owned by `sourceRef` with `entries`, so a
  // re-index of a shrunken document cannot leave orphaned chunks behind.
  replaceIndex(sourceRef: string, entries: IndexUpsert[]): Promise<void>;
  searchIndex(opts: SearchIndexOpts): Promise<IndexHit[]>;
  // Rows whose inline embed failed (provider down). Drained by the backfill.
  listUnembedded(limit: number): Promise<{ id: number; text: string }[]>;
  setEmbedding(id: number, embedding: number[]): Promise<void>;
}
