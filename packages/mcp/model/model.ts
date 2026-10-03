// The reference model of memory documents: what `patch_doc`, `append_doc`,
// `write_doc`, `revert_patch`, `read_doc` and `doc_history` mean, with nothing
// else in the way — no database, no concurrency, no indexing, no async.
//
// This file is the specification. The real server (src/services/memory) is
// checked against it: server.check.ts runs random sequences of calls through
// both and requires the same answers, sequentially and under concurrency.
// properties.test.ts checks properties of this model, so the model itself is
// checked too. It is a line-by-line port of the Lean model on
// feat/rust-rewrite (crates/mcp/model/MemoryModel/Model.lean).
//
// One project; documents by name. Text is a list of characters (code
// points), so every rule below is ordinary list code. Nothing here mutates:
// every call takes a world and returns a new one.

export type Text = readonly string[];

export const chars = (s: string): Text => Array.from(s);
export const str = (t: Text): string => t.join("");

export type Kind = "write" | "append" | "patch" | "revert";

export interface Edit {
  old: string;
  new: string;
}

// Every change is recorded with the body it replaced.
export interface Patch {
  kind: Kind;
  edits: readonly Edit[];
  bodyBefore: string;
  versionBefore: number;
  versionAfter: number;
}

export interface Doc {
  body: string;
  version: number;
  // Oldest first. Never rewritten, only extended.
  history: readonly Patch[];
}

// A document that does not exist yet behaves as an empty one at version 0.
export const emptyDoc: Doc = { body: "", version: 0, history: [] };

export type World = ReadonlyMap<string, Doc>;

export const emptyWorld: World = new Map();

function set(w: World, name: string, d: Doc): World {
  return new Map(w).set(name, d);
}

// ── Calls and answers ────────────────────────────────────────────────────────

export type Cmd =
  // `expected`: undefined when creating; the current version when overwriting.
  | { op: "write"; doc: string; body: string; expected?: number }
  | { op: "append"; doc: string; text: string }
  | { op: "patch"; doc: string; expected: number; edits: readonly Edit[] }
  // `index`: position in the document's history, oldest = 0.
  | { op: "revert"; doc: string; index: number; rollback: boolean }
  | { op: "read"; doc: string }
  | { op: "history"; doc: string };

export type Reply =
  | { kind: "written"; version: number; sizeBytes: number }
  | { kind: "read"; body: string; version: number }
  // Newest first.
  | { kind: "history"; entries: { kind: Kind; versionBefore: number; versionAfter: number }[] }
  | { kind: "docNotFound" }
  // Overwriting an existing document without saying which version you read.
  | { kind: "versionRequired"; current: number }
  | { kind: "versionConflict"; current: number }
  // A quote matched zero times or more than once. Nothing was written.
  | { kind: "editFailed" }
  | { kind: "patchNotFound" }
  // Later patches touched this patch's text; a rollback would restore this version.
  | { kind: "revertConflict"; rollbackTo: number }
  // Bad parameters: rejected before memory is consulted.
  | { kind: "rejected" };

export function isError(reply: Reply): boolean {
  return reply.kind !== "written" && reply.kind !== "read" && reply.kind !== "history";
}

// ── Text rules ───────────────────────────────────────────────────────────────

function isPrefixAt(old: Text, s: Text, at: number): boolean {
  if (at + old.length > s.length) return false;
  return old.every((c, i) => s[at + i] === c);
}

// How many times `old` occurs in `s`. Overlapping matches count: "aaa"
// contains "aa" twice, because the quote could mean either.
export function occurrences(old: Text, s: Text): number {
  let n = 0;
  for (let at = 0; at < s.length; at++) if (isPrefixAt(old, s, at)) n++;
  return n;
}

// Replace the first occurrence of `old` by `new`.
export function replaceFirst(old: Text, replacement: Text, s: Text): Text {
  for (let at = 0; at < s.length; at++) {
    if (isPrefixAt(old, s, at)) return [...s.slice(0, at), ...replacement, ...s.slice(at + old.length)];
  }
  return s;
}

// All edits apply, in order, or none do. Each `old` must occur exactly once in
// the text as the earlier edits left it.
export function applyEdits(body: Text, edits: readonly Edit[]): Text | null {
  let text = body;
  for (const e of edits) {
    if (occurrences(chars(e.old), text) !== 1) return null;
    text = replaceFirst(chars(e.old), chars(e.new), text);
  }
  return text;
}

// The edits that undo `edits`. A deletion cannot be undone in place — there
// is nothing left to anchor the re-insertion to.
export function invert(edits: readonly Edit[]): Edit[] | null {
  const out: Edit[] = [];
  for (const e of [...edits].reverse()) {
    if (e.new === "") return null;
    out.push({ old: e.new, new: e.old });
  }
  return out;
}

// Lean's Char.isWhitespace: space, tab, newline, carriage return.
const isWhitespace = (c: string): boolean => c === " " || c === "\t" || c === "\n" || c === "\r";

export function trimEnd(t: Text): Text {
  let end = t.length;
  while (end > 0 && isWhitespace(t[end - 1]!)) end--;
  return t.slice(0, end);
}

export function trim(t: Text): Text {
  let start = 0;
  while (start < t.length && isWhitespace(t[start]!)) start++;
  return trimEnd(t.slice(start));
}

// Appending adds a paragraph: one blank line before it, exactly one newline
// at the end.
export function joinParagraphs(body: Text, addition: Text): Text {
  const base = trimEnd(body);
  if (base.length === 0 && addition.length === 0) return [];
  if (addition.length === 0) return [...base, "\n"];
  if (base.length === 0) return [...addition, "\n"];
  return [...base, "\n", "\n", ...addition, "\n"];
}

export const sizeBytes = (body: string): number => Buffer.byteLength(body, "utf8");

// ── The operations ───────────────────────────────────────────────────────────

// Every successful write: bump the version, record what was replaced.
function commit(w: World, name: string, d: Doc, kind: Kind, edits: readonly Edit[], body: string): [World, Reply] {
  const p: Patch = { kind, edits, bodyBefore: d.body, versionBefore: d.version, versionAfter: d.version + 1 };
  return [
    set(w, name, { body, version: d.version + 1, history: [...d.history, p] }),
    { kind: "written", version: d.version + 1, sizeBytes: sizeBytes(body) },
  ];
}

function write(w: World, name: string, body: string, expected: number | undefined): [World, Reply] {
  const d = w.get(name);
  if (!d) {
    if (expected === undefined || expected === 0) return commit(w, name, emptyDoc, "write", [], body);
    return [w, { kind: "versionConflict", current: 0 }];
  }
  if (expected === undefined) return [w, { kind: "versionRequired", current: d.version }];
  if (expected !== d.version) return [w, { kind: "versionConflict", current: d.version }];
  return commit(w, name, d, "write", [], body);
}

function append(w: World, name: string, text: string): [World, Reply] {
  if (text === "") return [w, { kind: "rejected" }];
  const d = w.get(name);
  if (!d) return [w, { kind: "docNotFound" }];
  return commit(w, name, d, "append", [], str(joinParagraphs(chars(d.body), trim(chars(text)))));
}

function patch(w: World, name: string, expected: number, edits: readonly Edit[]): [World, Reply] {
  if (expected === 0 || edits.length === 0 || edits.some((e) => e.old === "")) return [w, { kind: "rejected" }];
  const d = w.get(name);
  if (!d) return [w, { kind: "docNotFound" }];
  if (expected !== d.version) return [w, { kind: "versionConflict", current: d.version }];
  const body = applyEdits(chars(d.body), edits);
  if (body === null) return [w, { kind: "editFailed" }];
  return commit(w, name, d, "patch", edits, str(body));
}

// The newest patch, or any patch with `rollback`, restores the body it
// replaced. An older one is undone in place by inverting its edits, which
// works only while later patches left its text alone.
function revert(w: World, name: string, index: number, rollback: boolean): [World, Reply] {
  const d = w.get(name);
  if (!d) return [w, { kind: "docNotFound" }];
  const p = d.history[index];
  if (!p) return [w, { kind: "patchNotFound" }];
  if (rollback || index + 1 === d.history.length) return commit(w, name, d, "revert", [], p.bodyBefore);
  const inverse = p.edits.length === 0 ? null : invert(p.edits);
  const body = inverse === null ? null : applyEdits(chars(d.body), inverse);
  if (body === null) return [w, { kind: "revertConflict", rollbackTo: p.versionBefore }];
  return commit(w, name, d, "revert", [], str(body));
}

export function step(w: World, cmd: Cmd): [World, Reply] {
  switch (cmd.op) {
    case "write":
      return write(w, cmd.doc, cmd.body, cmd.expected);
    case "append":
      return append(w, cmd.doc, cmd.text);
    case "patch":
      return patch(w, cmd.doc, cmd.expected, cmd.edits);
    case "revert":
      return revert(w, cmd.doc, cmd.index, cmd.rollback);
    case "read": {
      const d = w.get(cmd.doc);
      return [w, d ? { kind: "read", body: d.body, version: d.version } : { kind: "docNotFound" }];
    }
    case "history": {
      const d = w.get(cmd.doc);
      if (!d) return [w, { kind: "docNotFound" }];
      const entries = [...d.history]
        .reverse()
        .map((p) => ({ kind: p.kind, versionBefore: p.versionBefore, versionAfter: p.versionAfter }));
      return [w, { kind: "history", entries }];
    }
  }
}
