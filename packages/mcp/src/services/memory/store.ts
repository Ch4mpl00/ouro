// The persistence port for unified memory.
//
// The service holds every rule (versioning, patch application, ranking); the
// store is deliberately dumb CRUD. That split is what lets the acceptance
// criteria in .claude/tasks/unified-memory.md be tested without a Postgres,
// and it keeps the PG implementation small enough to read in one sitting.

import type {
  Doc,
  DocPatch,
  DocSummary,
  Fact,
  IndexEntry,
  IndexHit,
  JournalEntry,
  JournalStream,
  MemoryState,
  Project,
} from "./types";

export interface CreateProjectInput {
  slug: string;
  title: string;
}

// A patch on its way in, written together with the body it describes.
export type NewPatch = Omit<DocPatch, "createdAt" | "docId">;

export interface CreateDocInput {
  projectId: number;
  name: string;
  summary: string | null;
  body: string;
  patch: NewPatch;
}

export interface UpdateDocInput {
  docId: number;
  // Compare-and-swap. The update must not apply when the document has moved
  // on — that is the whole concurrency story for multiple agents (D1).
  expectedVersion: number;
  body: string;
  summary?: string | null;
  patch: NewPatch;
}

// A document and the patch that produced its current version, committed in
// one transaction: history can never disagree with the body it records.
export interface CommittedDoc {
  doc: Doc;
  patch: DocPatch;
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

export interface AppendEntryInput {
  stream: string;
  body: string;
  actor: string;
  // Stored on the stream when given; omitted keeps the stream's capacity, or
  // `defaultCapacity` for a stream this append creates.
  capacity?: number;
  defaultCapacity: number;
}

// The entry and its stream after the trim.
export interface AppendedEntry {
  entry: JournalEntry;
  stream: JournalStream;
}

export type IndexBasis =
  | { kind: "fact"; id: number; body: string; tags: string[]; state: MemoryState }
  | { kind: "doc"; docId: number; version: number };

export interface MemoryStore {
  createProject(input: CreateProjectInput): Promise<Project>;
  getProject(slug: string): Promise<Project | null>;
  listProjects(): Promise<Project[]>;

  listDocs(projectId: number): Promise<DocSummary[]>;
  getDoc(projectId: number, name: string): Promise<Doc | null>;
  // Resolves to null when another writer created the name first.
  createDoc(input: CreateDocInput): Promise<CommittedDoc | null>;
  // Resolves to null when `expectedVersion` no longer matches. Callers turn
  // that into a conflict the agent can act on, never a silent overwrite.
  updateDoc(input: UpdateDocInput): Promise<CommittedDoc | null>;

  // Newest first, by version: the order the body went through them.
  listPatches(docId: number, limit: number): Promise<DocPatch[]>;
  getPatch(docId: number, pid: string): Promise<DocPatch | null>;

  createFact(input: CreateFactInput): Promise<Fact>;
  getFact(id: number): Promise<Fact | null>;
  // Lookup by provenance string, which is what makes the one-shot import of
  // knowledge_base_notes re-runnable without duplicating anything.
  getFactBySource(source: string): Promise<Fact | null>;
  updateFact(input: UpdateFactInput): Promise<Fact | null>;

  // Replaces every index row owned by `sourceRef` with `entries`, so a
  // re-index of a shrunken document cannot leave orphaned chunks behind —
  // but only while the source still looks like `basis` (what the entries
  // were built from). Two writes racing on one source finish embedding in
  // either order; the stale one finds the source moved on and writes
  // nothing, leaving the rows to the later write's re-index. Re-indexes of
  // one source run one at a time. Returns whether the rows were written.
  replaceIndex(sourceRef: string, entries: IndexUpsert[], basis: IndexBasis): Promise<boolean>;
  searchIndex(opts: SearchIndexOpts): Promise<IndexHit[]>;
  // Rows whose inline embed failed (provider down), past `afterId` in id
  // order. Drained by the backfill.
  listUnembedded(limit: number, afterId?: number): Promise<{ id: number; text: string }[]>;
  setEmbedding(id: number, embedding: number[]): Promise<void>;

  // The journal. One append creates the stream if needed, stores the entry
  // and drops the entries beyond the stream's capacity, oldest first — all in
  // one transaction, serialised per stream, so no reader ever sees a stream
  // over capacity and racing appends all land.
  appendEntry(input: AppendEntryInput): Promise<AppendedEntry>;
  // Newest first, by append order. Empty for a stream that does not exist.
  tailEntries(stream: string, limit: number): Promise<JournalEntry[]>;
  // By name.
  listStreams(): Promise<JournalStream[]>;
}
