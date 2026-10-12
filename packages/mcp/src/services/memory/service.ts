// Unified memory, the rules layer (.claude/tasks/unified-memory.md).
//
// Everything that protects a document lives here: versioning, atomic patches,
// conflict reporting, revert. The store underneath is dumb CRUD, so these
// rules are tested against an in-memory fake rather than a Postgres.

import { randomUUID } from "node:crypto";
import type { Indexer } from "./indexer";
import { appendToBody, applyEdits, invertEdits, type EditFailure } from "./patch";
import { DEFAULT_RANK, rankHits, type RankedHit } from "./projection";
import { JOURNAL_STREAM_MAX_LENGTH, assertDocName, assertProjectSlug, isJournalStream } from "./refs";
import type { AppendedEntry, MemoryStore } from "./store";
import type { Doc, DocPatch, DocSummary, Edit, Fact, JournalEntry, JournalStream, MemoryState, Project } from "./types";

// Expected failures are values, not stack traces: an agent that gets
// `{ error: "version_conflict", currentVersion: 9 }` knows what to do next,
// while a thrown string only tells it that something went wrong. The tool
// layer renders `code` + `details` straight into the JSON result.
export class MemoryError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly details: Record<string, unknown> = {},
  ) {
    super(message);
    this.name = "MemoryError";
  }
}

export interface WriteResult {
  project: string;
  doc: string;
  version: number;
  patchId: string;
  sizeBytes: number;
}

export interface HistoryEntry {
  patchId: string;
  kind: DocPatch["kind"];
  actor: string;
  rationale: string | null;
  versionBefore: number;
  versionAfter: number;
  editCount: number;
  createdAt: string;
}

export interface RecallHit {
  ref: string;
  text: string;
  score: number;
  distance: number;
  state: MemoryState;
  actor: string | null;
  ts: string;
}

export interface MemoryService {
  createProject(input: { slug: string; title: string }): Promise<Project>;
  listProjects(): Promise<Project[]>;
  listDocs(slug: string): Promise<{ project: Project; docs: DocSummary[] }>;
  readDoc(slug: string, name: string): Promise<Doc>;
  writeDoc(input: WriteDocInput): Promise<WriteResult>;
  appendDoc(input: AppendDocInput): Promise<WriteResult>;
  patchDoc(input: PatchDocInput): Promise<WriteResult>;
  history(slug: string, name: string, limit?: number): Promise<HistoryEntry[]>;
  revert(input: RevertInput): Promise<WriteResult>;
  remember(input: { body: string; tags?: string[]; source?: string | null }): Promise<Fact>;
  getFact(id: number): Promise<Fact>;
  updateFact(input: { id: number; body?: string; tags?: string[]; state?: MemoryState }): Promise<Fact>;
  recall(input: RecallInput): Promise<RecallHit[]>;
  journalAppend(input: JournalAppendInput): Promise<AppendedEntry>;
  journalTail(input: { stream: string; n?: number }): Promise<{ stream: string; entries: JournalEntry[] }>;
  listJournals(): Promise<JournalStream[]>;
}

export interface JournalAppendInput {
  stream: string;
  body: string;
  actor: string;
  // Stored on the stream; omitted keeps whatever the stream already has.
  capacity?: number;
}

// The journal's bounds. Capacity is per stream (stored with it); these are
// the defaults and the limits a caller may set within.
export const JOURNAL_DEFAULT_CAPACITY = 100;
export const JOURNAL_MAX_CAPACITY = 1000;
export const JOURNAL_TAIL_DEFAULT = 5;
export const JOURNAL_TAIL_MAX = 100;
// A digest is ~5k characters; the cap keeps one runaway write from making
// every later tail expensive.
export const JOURNAL_MAX_BODY_CHARS = 32_000;

export interface WriteDocInput {
  project: string;
  doc: string;
  body: string;
  summary?: string | null;
  // Required when the document exists; must be absent or 0 when creating it.
  expectedVersion?: number;
  actor: string;
  rationale?: string | null;
}

export interface AppendDocInput {
  project: string;
  doc: string;
  text: string;
  underHeading?: string | null;
  actor: string;
  rationale?: string | null;
}

export interface PatchDocInput {
  project: string;
  doc: string;
  expectedVersion: number;
  edits: Edit[];
  actor: string;
  rationale?: string | null;
}

export interface RevertInput {
  project: string;
  doc: string;
  patchId: string;
  // Opt-in escalation: restore the whole document to the state before that
  // patch, discarding everything written after it. The only destructive shape
  // of revert, so it is never the default.
  rollback?: boolean;
  actor: string;
}

export interface RecallInput {
  query: string;
  limit?: number;
  states?: MemoryState[];
  tags?: string[];
}

export interface MemoryServiceDeps {
  store: MemoryStore;
  indexer: Indexer;
  now?: () => Date;
  // Injectable so tests get stable ids. Short and random in production —
  // sequential ids invite a model to guess a neighbour's (D9, pitfall 3).
  newPatchId?: () => string;
}

// How many times append_doc re-reads and retries when another agent commits in
// between. Append takes no expected_version — it is the version-free safe op —
// so it absorbs the race instead of pushing it onto the caller.
const APPEND_CAS_ATTEMPTS = 3;

export function createMemoryService(deps: MemoryServiceDeps): MemoryService {
  const { store, indexer } = deps;
  const now = deps.now ?? (() => new Date());
  const newPatchId = deps.newPatchId ?? (() => `pa:${randomUUID().replace(/-/g, "").slice(0, 6)}`);

  async function requireProject(slug: string): Promise<Project> {
    assertProjectSlug(slug);
    const project = await store.getProject(slug);
    if (project) return project;
    const projects = await store.listProjects();
    throw new MemoryError("project_not_found", `No project "${slug}".`, {
      projects: projects.map((p) => p.slug),
    });
  }

  async function requireDoc(project: Project, name: string): Promise<Doc> {
    assertDocName(name);
    const doc = await store.getDoc(project.id, name);
    if (doc) return doc;
    const docs = await store.listDocs(project.id);
    // Listing what does exist is the cheapest defence against notes.md /
    // notes2.md / progress-new.md multiplying (D5).
    throw new MemoryError("doc_not_found", `No document "${name}" in project "${project.slug}".`, {
      project: project.slug,
      docs: docs.map((d) => d.name),
    });
  }

  // Re-index after every successful write. Failures here must never fail the
  // write: the document is the truth, searchability is allowed to lag (D4).
  async function reindexDoc(project: Project, doc: Doc): Promise<void> {
    try {
      await indexer.indexDoc(project, doc);
    } catch (err) {
      console.error(`[memory] indexing ${project.slug}/${doc.name} failed:`, err);
    }
  }

  const result = (project: Project, doc: Doc, patchId: string): WriteResult => ({
    project: project.slug,
    doc: doc.name,
    version: doc.version,
    patchId,
    sizeBytes: doc.sizeBytes,
  });

  // Shared tail of every mutating op: swap the body under a version check
  // together with its patch, re-index. Null when the compare-and-swap lost.
  async function commit(input: {
    project: Project;
    doc: Doc;
    body: string;
    summary?: string | null;
    kind: DocPatch["kind"];
    edits: Edit[];
    actor: string;
    rationale: string | null;
  }): Promise<WriteResult | null> {
    const committed = await store.updateDoc({
      docId: input.doc.id,
      expectedVersion: input.doc.version,
      body: input.body,
      summary: input.summary,
      patch: {
        pid: newPatchId(),
        kind: input.kind,
        edits: input.edits,
        bodyBefore: input.doc.body,
        versionBefore: input.doc.version,
        versionAfter: input.doc.version + 1,
        actor: input.actor,
        rationale: input.rationale,
      },
    });
    if (!committed) return null;
    await reindexDoc(input.project, committed.doc);
    return result(input.project, committed.doc, committed.patch.pid);
  }

  // The creating half of write_doc. `{ theirs }` when another writer created
  // the same name between our read and our insert.
  async function createDoc(
    project: Project,
    input: WriteDocInput,
  ): Promise<{ created: WriteResult } | { theirs: Doc }> {
    if (input.expectedVersion !== undefined && input.expectedVersion !== 0) {
      throw new MemoryError(
        "version_conflict",
        `Document "${input.doc}" does not exist yet; expected_version must be omitted or 0.`,
        { project: project.slug, doc: input.doc, currentVersion: 0 },
      );
    }
    const committed = await store.createDoc({
      projectId: project.id,
      name: input.doc,
      summary: input.summary ?? null,
      body: input.body,
      patch: {
        pid: newPatchId(),
        kind: "write",
        edits: [],
        bodyBefore: "",
        versionBefore: 0,
        versionAfter: 1,
        actor: input.actor,
        rationale: input.rationale ?? null,
      },
    });
    if (!committed) {
      const theirs = await store.getDoc(project.id, input.doc);
      if (!theirs) throw raced(project.slug, input.doc);
      return { theirs };
    }
    await reindexDoc(project, committed.doc);
    return { created: result(project, committed.doc, committed.patch.pid) };
  }

  function requireStream(stream: string): void {
    if (isJournalStream(stream)) return;
    throw new MemoryError(
      "invalid_stream",
      `Invalid stream "${stream}". Use lowercase letters, digits and hyphens, segments joined by "/", ` +
        `at most ${JOURNAL_STREAM_MAX_LENGTH} characters, e.g. "news-digest" or "digest/news".`,
      { stream },
    );
  }

  return {
    createProject: async ({ slug, title }) => {
      assertProjectSlug(slug);
      const existing = await store.getProject(slug);
      if (existing) {
        throw new MemoryError("project_exists", `Project "${slug}" already exists.`, {
          project: existing.slug,
          title: existing.title,
        });
      }
      return store.createProject({ slug, title: title.trim() || slug });
    },

    listProjects: () => store.listProjects(),

    listDocs: async (slug) => {
      const project = await requireProject(slug);
      return { project, docs: await store.listDocs(project.id) };
    },

    readDoc: async (slug, name) => {
      const project = await requireProject(slug);
      return requireDoc(project, name);
    },

    writeDoc: async (input) => {
      const project = await requireProject(input.project);
      assertDocName(input.doc);
      let existing = await store.getDoc(project.id, input.doc);
      if (!existing) {
        const outcome = await createDoc(project, input);
        if ("created" in outcome) return outcome.created;
        // Created by someone else in the meantime: this call is now an
        // overwrite of their document, judged as one.
        existing = outcome.theirs;
      }

      // write_doc is the only op that can lose content, so it never runs blind.
      if (input.expectedVersion === undefined) {
        throw new MemoryError(
          "version_required",
          `Document "${input.doc}" exists at version ${existing.version}; read it and pass expected_version.`,
          { project: project.slug, doc: input.doc, currentVersion: existing.version },
        );
      }
      if (input.expectedVersion !== existing.version) {
        throw new MemoryError(
          "version_conflict",
          `Document "${input.doc}" is at version ${existing.version}, not ${input.expectedVersion}. Re-read it and retry.`,
          { project: project.slug, doc: input.doc, currentVersion: existing.version },
        );
      }

      const committed = await commit({
        project,
        doc: existing,
        body: input.body,
        summary: input.summary,
        kind: "write",
        edits: [],
        actor: input.actor,
        rationale: input.rationale ?? null,
      });
      if (!committed) throw raced(project.slug, input.doc);
      return committed;
    },

    appendDoc: async (input) => {
      const project = await requireProject(input.project);
      for (let attempt = 0; attempt < APPEND_CAS_ATTEMPTS; attempt++) {
        const doc = await requireDoc(project, input.doc);
        const appended = appendToBody(doc.body, input.text, input.underHeading);
        if (!appended.ok) {
          throw new MemoryError(
            "heading_not_found",
            `No heading "${input.underHeading}" in "${input.doc}".`,
            { project: project.slug, doc: input.doc, headings: appended.headings },
          );
        }
        const committed = await commit({
          project,
          doc,
          body: appended.body,
          kind: "append",
          edits: [],
          actor: input.actor,
          rationale: input.rationale ?? null,
        });
        if (committed) return committed;
      }
      throw raced(project.slug, input.doc);
    },

    patchDoc: async (input) => {
      const project = await requireProject(input.project);
      const doc = await requireDoc(project, input.doc);
      if (doc.version !== input.expectedVersion) {
        throw new MemoryError(
          "version_conflict",
          `Document "${input.doc}" is at version ${doc.version}, not ${input.expectedVersion}. Re-read it and retry.`,
          { project: project.slug, doc: input.doc, currentVersion: doc.version },
        );
      }

      const applied = applyEdits(doc.body, input.edits);
      if (!applied.ok) throw editFailure(project.slug, input.doc, applied.failures);

      const committed = await commit({
        project,
        doc,
        body: applied.body,
        kind: "patch",
        edits: input.edits,
        actor: input.actor,
        rationale: input.rationale ?? null,
      });
      if (!committed) throw raced(project.slug, input.doc);
      return committed;
    },

    history: async (slug, name, limit = 20) => {
      const project = await requireProject(slug);
      const doc = await requireDoc(project, name);
      const patches = await store.listPatches(doc.id, limit);
      return patches.map((p) => ({
        patchId: p.pid,
        kind: p.kind,
        actor: p.actor,
        rationale: p.rationale,
        versionBefore: p.versionBefore,
        versionAfter: p.versionAfter,
        editCount: p.edits.length,
        createdAt: p.createdAt,
      }));
    },

    revert: async (input) => {
      const project = await requireProject(input.project);
      const doc = await requireDoc(project, input.doc);
      const patch = await store.getPatch(doc.id, input.patchId);
      if (!patch) {
        // An unknown id is a hard error listing near matches, never a silent
        // fuzzy match — the model will hallucinate ids (D9, pitfall 3).
        const recent = await store.listPatches(doc.id, 10);
        throw new MemoryError("patch_not_found", `No patch "${input.patchId}" on "${input.doc}".`, {
          project: project.slug,
          doc: input.doc,
          knownPatchIds: recent.map((p) => p.pid),
        });
      }

      // The patch that produced the current version. Decided by version, not
      // by position in history, so it holds however writers interleaved.
      const isNewest = patch.versionAfter === doc.version;

      let body: string;
      if (input.rollback === true || isNewest) {
        // Exact by construction: every patch stores the body it replaced.
        body = patch.bodyBefore;
      } else {
        const inverse = patch.edits.length > 0 ? invertEdits(patch.edits) : null;
        if (!inverse) throw revertConflict(project.slug, input.doc, patch, []);
        const applied = applyEdits(doc.body, inverse);
        if (!applied.ok) throw revertConflict(project.slug, input.doc, patch, applied.failures);
        body = applied.body;
      }

      const rationale =
        input.rollback === true && !isNewest
          ? `rollback to v${patch.versionBefore} (discards patches after ${patch.pid})`
          : `revert ${patch.pid}`;
      const committed = await commit({
        project,
        doc,
        body,
        kind: "revert",
        edits: [],
        actor: input.actor,
        rationale,
      });
      if (!committed) throw raced(project.slug, input.doc);
      return committed;
    },

    remember: async (input) => {
      const body = input.body.trim();
      if (body.length === 0) throw new MemoryError("empty_body", "A fact needs a body.");
      const fact = await store.createFact({
        body,
        tags: normalizeTags(input.tags),
        source: input.source ?? null,
      });
      await indexFactQuietly(indexer, fact);
      return fact;
    },

    getFact: async (id) => {
      const fact = await store.getFact(id);
      if (!fact) throw new MemoryError("fact_not_found", `No fact ${id}.`, { id });
      return fact;
    },

    updateFact: async (input) => {
      const updated = await store.updateFact({
        id: input.id,
        body: input.body?.trim(),
        tags: input.tags === undefined ? undefined : normalizeTags(input.tags),
        state: input.state,
      });
      if (!updated) throw new MemoryError("fact_not_found", `No fact ${input.id}.`, { id: input.id });
      await indexFactQuietly(indexer, updated);
      return updated;
    },

    recall: async (input) => {
      const limit = clamp(input.limit ?? 10, 1, 50);
      const embedding = await indexer.embedQuery(input.query);
      if (!embedding) {
        // "Search is down" and "we remember nothing about that" must not look
        // the same to the agent — one is worth retrying, the other isn't.
        throw new MemoryError(
          "search_unavailable",
          "Recall needs the embedding provider, which is currently unreachable. Documents still read and patch.",
        );
      }
      // Over-fetch so the recency boost has something to reorder.
      const hits = await store.searchIndex({
        embedding,
        limit: limit * 3,
        states: input.states ?? ["active"],
        tags: input.tags,
      });
      return rankHits(hits, { now: now(), ...DEFAULT_RANK })
        .slice(0, limit)
        .map(toRecallHit);
    },

    // Never indexed, never patched: the journal is records, not knowledge.
    journalAppend: async (input) => {
      requireStream(input.stream);
      if (input.body.trim().length === 0) throw new MemoryError("empty_body", "A journal entry needs a body.");
      if (input.body.length > JOURNAL_MAX_BODY_CHARS) {
        throw new MemoryError(
          "body_too_large",
          `A journal entry is at most ${JOURNAL_MAX_BODY_CHARS} characters; this one is ${input.body.length}.`,
          { max: JOURNAL_MAX_BODY_CHARS, length: input.body.length },
        );
      }
      const { capacity } = input;
      if (capacity !== undefined && !(Number.isInteger(capacity) && capacity >= 1 && capacity <= JOURNAL_MAX_CAPACITY)) {
        throw new MemoryError(
          "invalid_capacity",
          `Capacity is a whole number of entries from 1 to ${JOURNAL_MAX_CAPACITY}.`,
          { capacity, max: JOURNAL_MAX_CAPACITY },
        );
      }
      return store.appendEntry({
        stream: input.stream,
        body: input.body,
        actor: input.actor,
        capacity,
        defaultCapacity: JOURNAL_DEFAULT_CAPACITY,
      });
    },

    journalTail: async ({ stream, n = JOURNAL_TAIL_DEFAULT }) => {
      requireStream(stream);
      if (!(Number.isInteger(n) && n >= 1 && n <= JOURNAL_TAIL_MAX)) {
        throw new MemoryError("invalid_n", `n is a whole number of entries from 1 to ${JOURNAL_TAIL_MAX}.`, {
          n,
          max: JOURNAL_TAIL_MAX,
        });
      }
      return { stream, entries: await store.tailEntries(stream, n) };
    },

    listJournals: () => store.listStreams(),
  };
}

async function indexFactQuietly(indexer: Indexer, fact: Fact): Promise<void> {
  try {
    await indexer.indexFact(fact);
  } catch (err) {
    console.error(`[memory] indexing fact ${fact.id} failed:`, err);
  }
}

function toRecallHit(hit: RankedHit): RecallHit {
  return {
    ref: hit.ref,
    text: hit.text,
    score: hit.score,
    distance: hit.distance,
    state: hit.state,
    actor: hit.actor,
    ts: hit.ts,
  };
}

function raced(project: string, doc: string): MemoryError {
  return new MemoryError(
    "version_conflict",
    `Document "${doc}" changed while this write was in flight. Re-read it and retry.`,
    { project, doc },
  );
}

function editFailure(project: string, doc: string, failures: EditFailure[]): MemoryError {
  return new MemoryError("edit_failed", summariseFailures(failures), {
    project,
    doc,
    // Nothing was written: the document is exactly as it was.
    applied: false,
    failures,
  });
}

function summariseFailures(failures: EditFailure[]): string {
  return failures
    .map((f) => {
      if (f.reason === "empty") return `edit ${f.index}: \`old\` is empty; use append_doc to add text`;
      if (f.reason === "ambiguous") {
        return `edit ${f.index}: \`old\` matches ${f.occurrences} times; quote more context`;
      }
      const hint = f.suggestions.length > 0 ? `; did you mean: ${JSON.stringify(f.suggestions[0])}` : "";
      return `edit ${f.index}: \`old\` not found${hint}`;
    })
    .join("; ");
}

function revertConflict(
  project: string,
  doc: string,
  patch: { pid: string; versionBefore: number },
  failures: EditFailure[],
): MemoryError {
  return new MemoryError(
    "revert_conflict",
    `Patch ${patch.pid} cannot be undone in place because later patches touched the same text. ` +
      `Retry with rollback=true to restore version ${patch.versionBefore}, discarding everything written after it.`,
    { project, doc, patchId: patch.pid, rollbackToVersion: patch.versionBefore, failures },
  );
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, Math.trunc(value)));
}

function normalizeTags(tags: string[] | undefined): string[] {
  if (!tags) return [];
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of tags) {
    const tag = raw.trim();
    if (tag.length === 0 || seen.has(tag)) continue;
    seen.add(tag);
    out.push(tag);
  }
  return out;
}
