// The patch engine (D10). Search/replace on quoted literals, never line
// numbers: an off-by-one line number is a *silent* corruption that reports
// success, while a quoted string that doesn't match fails loudly and changes
// nothing. Line numbers also shift under a concurrent edit above them.
//
// Everything here is pure — body in, body out — so the rules that protect the
// document are testable without a database.

import type { Edit } from "./types";

export type EditFailureReason = "empty" | "not_found" | "ambiguous";

export interface EditFailure {
  // Position in the caller's `edits` array, so the model can fix one edit
  // rather than resend the whole call blind.
  index: number;
  old: string;
  reason: EditFailureReason;
  occurrences: number;
  // Literal text from the document that *nearly* matched. Suggested, never
  // applied: fuzzy-applying a near match is how an agent silently edits the
  // wrong sentence.
  suggestions: string[];
}

export type ApplyEditsResult =
  | { ok: true; body: string }
  | { ok: false; failures: EditFailure[] };

// Atomic: every edit applies or none do. Edits are applied in order against a
// working copy, so a later edit may legitimately target text an earlier one
// produced; nothing is written unless the whole list succeeds.
export function applyEdits(body: string, edits: Edit[]): ApplyEditsResult {
  if (edits.length === 0) {
    return { ok: false, failures: [{ index: 0, old: "", reason: "empty", occurrences: 0, suggestions: [] }] };
  }

  let working = body;
  const failures: EditFailure[] = [];

  for (const [index, edit] of edits.entries()) {
    // An empty `old` would match at every position; there is no sane
    // interpretation, and "insert at the start" is what append_doc is for.
    if (edit.old.length === 0) {
      failures.push({ index, old: edit.old, reason: "empty", occurrences: 0, suggestions: [] });
      continue;
    }

    const occurrences = countOccurrences(working, edit.old);
    if (occurrences === 1) {
      working = working.replace(edit.old, () => edit.new);
      continue;
    }
    if (occurrences > 1) {
      // Never "take the first match" — the agent must re-quote with enough
      // surrounding context to be unique.
      failures.push({ index, old: edit.old, reason: "ambiguous", occurrences, suggestions: [] });
      continue;
    }
    failures.push({
      index,
      old: edit.old,
      reason: "not_found",
      occurrences: 0,
      suggestions: findNearMatches(working, edit.old),
    });
  }

  if (failures.length > 0) return { ok: false, failures };
  return { ok: true, body: working };
}

// The inverse of a patch, for reverting one that is no longer the newest
// (D9). Reversed order because edits were applied in sequence.
//
// A deletion (`new: ""`) has no inverse: re-inserting text needs a position,
// and the anchor it was attached to is gone. Callers get `null` and fall back
// to a whole-document rollback, which is exact because every patch stores the
// body it replaced.
export function invertEdits(edits: Edit[]): Edit[] | null {
  const inverted: Edit[] = [];
  for (const edit of [...edits].reverse()) {
    if (edit.new.length === 0) return null;
    inverted.push({ old: edit.new, new: edit.old });
  }
  return inverted;
}

function countOccurrences(haystack: string, needle: string): number {
  let count = 0;
  let from = 0;
  for (;;) {
    const at = haystack.indexOf(needle, from);
    if (at === -1) return count;
    count++;
    from = at + needle.length;
  }
}

// ---------------------------------------------------------------------------
// Near matches
//
// The usual miss is not a wrong quote but a *normalised* one: the model
// retyped an em-dash as a hyphen, straightened a «quote», wrote е for ё, or
// reflowed whitespace. Normalising both sides and mapping the hit back to the
// original characters hands the agent the exact literal to retry with.
// ---------------------------------------------------------------------------

const DASHES = /[‐-―−]/g;
const QUOTES = /[«»“”„‘’′″]/g;

interface Normalised {
  text: string;
  // originIndex[i] = index in the source string of normalised char i.
  originIndex: number[];
}

function normalise(source: string): Normalised {
  const text: string[] = [];
  const originIndex: number[] = [];
  let pendingSpace = false;

  for (let i = 0; i < source.length; i++) {
    const raw = source[i]!;
    if (/\s/.test(raw)) {
      // Collapse whitespace runs, and never let one start the string.
      pendingSpace = text.length > 0;
      continue;
    }
    if (pendingSpace) {
      text.push(" ");
      originIndex.push(i);
      pendingSpace = false;
    }
    const mapped = raw
      .normalize("NFC")
      .toLowerCase()
      .replace(DASHES, "-")
      .replace(QUOTES, '"')
      .replace(/ё/g, "е");
    for (const ch of mapped) {
      text.push(ch);
      originIndex.push(i);
    }
  }

  return { text: text.join(""), originIndex };
}

export function findNearMatches(body: string, needle: string, limit = 3): string[] {
  const nBody = normalise(body);
  const nNeedle = normalise(needle);
  if (nNeedle.text.length === 0) return [];

  const spans: string[] = [];
  let from = 0;
  while (spans.length < limit) {
    const at = nBody.text.indexOf(nNeedle.text, from);
    if (at === -1) break;
    const start = nBody.originIndex[at];
    const lastNorm = at + nNeedle.text.length - 1;
    const end = nBody.originIndex[lastNorm];
    if (start !== undefined && end !== undefined) spans.push(body.slice(start, end + 1));
    from = at + Math.max(1, nNeedle.text.length);
  }
  if (spans.length > 0) return spans;

  // Nothing matched even loosely: fall back to the most similar lines, which
  // at least tells the agent where in the document to look.
  return similarLines(body, nNeedle.text, limit);
}

function similarLines(body: string, normalisedNeedle: string, limit: number): string[] {
  const target = normalisedNeedle.slice(0, 200);
  const scored = body
    .split("\n")
    .filter((line) => line.trim().length > 0)
    .map((line) => ({ line, score: diceCoefficient(normalise(line).text, target) }))
    .filter((entry) => entry.score >= 0.4)
    .sort((a, b) => b.score - a.score);
  return scored.slice(0, limit).map((entry) => entry.line);
}

// Bigram Dice coefficient: cheap, no dependency, and good enough to rank
// "which line did they mean" without pretending to be a diff algorithm.
function diceCoefficient(a: string, b: string): number {
  if (a === b) return 1;
  if (a.length < 2 || b.length < 2) return 0;
  const bigrams = new Map<string, number>();
  for (let i = 0; i < a.length - 1; i++) {
    const gram = a.slice(i, i + 2);
    bigrams.set(gram, (bigrams.get(gram) ?? 0) + 1);
  }
  let hits = 0;
  for (let i = 0; i < b.length - 1; i++) {
    const gram = b.slice(i, i + 2);
    const left = bigrams.get(gram) ?? 0;
    if (left > 0) {
      bigrams.set(gram, left - 1);
      hits++;
    }
  }
  return (2 * hits) / (a.length - 1 + b.length - 1);
}

// ---------------------------------------------------------------------------
// Append
//
// The safe default (D10, op 1): it physically cannot destroy existing text and
// needs no prior read. Markdown headings are the anchors because they survive
// edits elsewhere in the document.
// ---------------------------------------------------------------------------

export interface Heading {
  level: number;
  text: string;
  line: number;
}

export function listHeadings(body: string): Heading[] {
  const headings: Heading[] = [];
  body.split("\n").forEach((line, index) => {
    const match = /^(#{1,6})\s+(.+?)\s*$/.exec(line);
    if (match) headings.push({ level: match[1]!.length, text: match[2]!, line: index });
  });
  return headings;
}

export type AppendResult =
  | { ok: true; body: string }
  | { ok: false; reason: "heading_not_found"; headings: string[] };

export function appendToBody(body: string, text: string, underHeading?: string | null): AppendResult {
  const addition = text.trim();
  if (underHeading === undefined || underHeading === null || underHeading.trim() === "") {
    return { ok: true, body: joinBlocks(body, addition) };
  }

  const headings = listHeadings(body);
  // Accept "Progress" as readily as "## Progress" — the model has seen the
  // document and will quote it either way.
  const wanted = normalise(underHeading.replace(/^#+\s*/, "")).text;
  const target = headings.find((h) => normalise(h.text).text === wanted);
  if (!target) {
    return { ok: false, reason: "heading_not_found", headings: headings.map((h) => `${"#".repeat(h.level)} ${h.text}`) };
  }

  const lines = body.split("\n");
  // The section runs until the next heading of the same or higher rank; a
  // deeper sub-heading is still part of it.
  let end = lines.length;
  for (const heading of headings) {
    if (heading.line > target.line && heading.level <= target.level) {
      end = heading.line;
      break;
    }
  }

  const head = lines.slice(0, end).join("\n");
  const tail = lines.slice(end).join("\n");
  const merged = joinBlocks(head, addition);
  return { ok: true, body: tail.length > 0 ? `${merged}\n${tail}` : merged };
}

// One blank line between blocks, exactly one trailing newline. Markdown
// treats the blank line as significant, and stable formatting keeps later
// patches quoting text that still looks the way the agent last saw it.
function joinBlocks(existing: string, addition: string): string {
  const base = existing.replace(/\s+$/, "");
  if (addition.length === 0) return base.length > 0 ? `${base}\n` : "";
  if (base.length === 0) return `${addition}\n`;
  return `${base}\n\n${addition}\n`;
}
