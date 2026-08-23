// Refs are the join between the search projection and the read models (D8):
// `recall` returns refs, the agent follows one with `read_doc` / `get_fact`.
//
//   doc:leetcode-graphs/roadmap.md#2
//   fact:88
//
// They travel through an LLM, so they are parsed strictly: a ref that doesn't
// round-trip is a bug we want to see, not something to guess at.

export const PROJECT_SLUG_RE = /^[a-z0-9][a-z0-9-]*$/;
// Document names are markdown filenames. No slashes — a project is a flat
// folder, and allowing separators would make refs ambiguous.
export const DOC_NAME_RE = /^[a-z0-9][a-z0-9._-]*\.md$/;

export interface DocRef {
  kind: "doc";
  project: string;
  doc: string;
  // Which chunk of the document the index row covers. Absent means the ref
  // addresses the document as a whole.
  chunk: number | null;
}

export interface FactRef {
  kind: "fact";
  id: number;
}

export type MemoryRef = DocRef | FactRef;

export function assertProjectSlug(slug: string): string {
  if (!PROJECT_SLUG_RE.test(slug)) {
    throw new Error(
      `Invalid project slug "${slug}". Use lowercase letters, digits and hyphens, e.g. "leetcode-graphs".`,
    );
  }
  return slug;
}

export function assertDocName(name: string): string {
  if (!DOC_NAME_RE.test(name)) {
    throw new Error(
      `Invalid document name "${name}". Use a lowercase markdown filename with no directories, e.g. "roadmap.md".`,
    );
  }
  return name;
}

export function docRef(project: string, doc: string, chunk?: number): string {
  const base = `doc:${project}/${doc}`;
  return chunk === undefined ? base : `${base}#${chunk}`;
}

// Every chunk of one document shares this prefix, which is how re-indexing
// drops the old chunks of exactly one document and nothing else.
export function docRefPrefix(project: string, doc: string): string {
  return `doc:${project}/${doc}#`;
}

export function factRef(id: number): string {
  return `fact:${id}`;
}

export function parseRef(raw: string): MemoryRef | null {
  const fact = /^fact:(\d+)$/.exec(raw);
  if (fact) return { kind: "fact", id: Number(fact[1]) };

  const doc = /^doc:([^/]+)\/([^#]+)(?:#(\d+))?$/.exec(raw);
  if (!doc) return null;
  const [, project = "", name = "", chunk] = doc;
  if (!PROJECT_SLUG_RE.test(project) || !DOC_NAME_RE.test(name)) return null;
  return {
    kind: "doc",
    project,
    doc: name,
    chunk: chunk === undefined ? null : Number(chunk),
  };
}
