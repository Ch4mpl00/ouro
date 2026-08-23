// Domain types for unified memory (.claude/tasks/unified-memory.md).
//
// Two read models sit behind one search projection (D8): projects made of
// markdown documents, and flat facts. Both feed `memory_index`, which is the
// only thing that holds embeddings.

// D7 — decay is an explicit state plus a recency boost at ranking time.
// Nothing is ever deleted.
export const MEMORY_STATES = ["active", "done", "archived"] as const;
export type MemoryState = (typeof MEMORY_STATES)[number];

export function isMemoryState(value: string): value is MemoryState {
  return (MEMORY_STATES as readonly string[]).includes(value);
}

export interface Project {
  id: number;
  slug: string;
  title: string;
  createdAt: string;
  updatedAt: string;
}

// What `list_memory(project)` returns per document: enough to choose one
// without loading it. The `summary` is what keeps a project from growing
// notes.md / notes2.md / progress-new.md (D5).
export interface DocSummary {
  name: string;
  summary: string | null;
  version: number;
  sizeBytes: number;
  updatedAt: string;
}

export interface Doc extends DocSummary {
  id: number;
  projectId: number;
  body: string;
}

export interface Fact {
  id: number;
  body: string;
  tags: string[];
  source: string | null;
  state: MemoryState;
  createdAt: string;
  updatedAt: string;
}

// One row per write, whatever its shape. `bodyBefore` is the whole previous
// document: it is what makes "roll roadmap.md back to v7" answerable and what
// a failed mid-stack revert falls back to (D9, pitfall 1).
export type PatchKind = "write" | "append" | "patch" | "revert";

export interface Edit {
  old: string;
  new: string;
}

export interface DocPatch {
  // Short and random, never sequential — sequential ids invite a model to
  // guess a neighbour's (D9, pitfall 3).
  pid: string;
  docId: number;
  kind: PatchKind;
  edits: Edit[];
  bodyBefore: string;
  versionBefore: number;
  versionAfter: number;
  actor: string;
  rationale: string | null;
  createdAt: string;
}

// A row in the flat search projection. `ref` points back at whatever produced
// it — `doc:<project>/<name>#<chunk>` or `fact:<id>`.
export interface IndexEntry {
  ref: string;
  text: string;
  tags: string[];
  actor: string | null;
  state: MemoryState;
  ts: string;
}

export interface IndexHit extends IndexEntry {
  id: number;
  distance: number;
}
