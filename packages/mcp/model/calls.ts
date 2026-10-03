// What a generated call is, and how it is made concrete.
//
// fast-check generates ABSTRACT steps: "a quote from the current text", "the
// current version", "the newest patch". Each is made concrete against the
// model's view of the document just before it is sent, so quotes actually
// match, versions are usually right and reverts name patches that exist —
// otherwise almost every random call would bounce off a validation error.

import fc from "fast-check";
import { chars, type Cmd, type World } from "./model";

export const DOCS = ["a.md", "b.md"] as const;
export const PROJECT = "p";

// An index the model never reaches, for a patch the server does not have.
export const NO_PATCH = 1_000_000;
// Never a real id: real ones are "pa:" + 6 hex digits.
export const MISSING_PID = "pa:zzzzzz";

// Few distinct characters, so quotes collide, overlap and nearly match
// ("ё"/"е", "—"/"-") as often as possible.
export const ALPHABET = ["a", "b", "c", "x", " ", "\n", "#", "ё", "е", "—", "-"];

export type Version = "current" | "stale" | "ahead" | "omitted";
// A slice of the current text (start and length, wrapped to fit), or a literal.
export type Quote = { slice: [number, number] } | { literal: string };
export type PatchPick = "newest" | { index: number } | "missing";

export type Step =
  | { op: "write"; doc: number; body: string; version: Version }
  | { op: "append"; doc: number; text: string }
  | { op: "patch"; doc: number; version: Version; edits: [Quote, string][] }
  | { op: "revert"; doc: number; pick: PatchPick; rollback: boolean }
  | { op: "read"; doc: number }
  | { op: "history"; doc: number };

// Mostly random characters, sometimes a run of one character: repeats are
// where quotes overlap ("ее" occurs twice in "еее"), and uniformly random
// text almost never has them.
export const text = (maxLength: number): fc.Arbitrary<string> =>
  fc.oneof(
    { weight: 3, arbitrary: fc.array(fc.constantFrom(...ALPHABET), { maxLength }).map((cs) => cs.join("")) },
    {
      weight: 2,
      arbitrary: fc
        .tuple(fc.constantFrom(...ALPHABET), fc.nat(maxLength))
        .map(([c, n]) => c.repeat(n)),
    },
  );

const version: fc.Arbitrary<Version> = fc.oneof(
  { weight: 6, arbitrary: fc.constant("current" as const) },
  { weight: 2, arbitrary: fc.constant("stale" as const) },
  { weight: 1, arbitrary: fc.constant("ahead" as const) },
  { weight: 1, arbitrary: fc.constant("omitted" as const) },
);

const quote: fc.Arbitrary<Quote> = fc.oneof(
  { weight: 4, arbitrary: fc.tuple(fc.nat(63), fc.integer({ min: 1, max: 5 })).map((slice) => ({ slice })) },
  { weight: 1, arbitrary: text(3).map((literal) => ({ literal })) },
);

// Rollback in roughly three reverts out of ten.
const rollback: fc.Arbitrary<boolean> = fc.oneof(
  { weight: 3, arbitrary: fc.constant(true) },
  { weight: 7, arbitrary: fc.constant(false) },
);

const pick: fc.Arbitrary<PatchPick> = fc.oneof(
  { weight: 3, arbitrary: fc.constant("newest" as const) },
  { weight: 3, arbitrary: fc.nat(7).map((index) => ({ index })) },
  { weight: 1, arbitrary: fc.constant("missing" as const) },
);

// `docs`: how many documents the steps spread over (1 for a concurrent
// session, so the sessions collide on one document).
export function step(docs: number): fc.Arbitrary<Step> {
  const doc = fc.nat(docs - 1);
  return fc.oneof(
    { weight: 3, arbitrary: fc.record({ op: fc.constant("write" as const), doc, body: text(12), version }) },
    { weight: 3, arbitrary: fc.record({ op: fc.constant("append" as const), doc, text: text(6) }) },
    {
      weight: 5,
      arbitrary: fc.record({
        op: fc.constant("patch" as const),
        doc,
        version,
        edits: fc.array(fc.tuple(quote, text(4)), { minLength: 1, maxLength: 2 }),
      }),
    },
    {
      weight: 2,
      arbitrary: fc.record({ op: fc.constant("revert" as const), doc, pick, rollback }),
    },
    { weight: 1, arbitrary: fc.record({ op: fc.constant("read" as const), doc }) },
    { weight: 1, arbitrary: fc.record({ op: fc.constant("history" as const), doc }) },
  );
}

// A concrete call, as both sides receive it: the model's command, plus — for
// a revert — the same patch's id on the server.
export interface Call {
  cmd: Cmd;
  pid?: string;
}

// How the server names patches: ids oldest first, so index i is the model's
// patch i. Absent for model-only checks, where the index is enough.
export type PatchIds = (doc: string) => Promise<string[]>;

export async function concretise(step: Step, world: World, patchIds?: PatchIds): Promise<Call> {
  const doc = DOCS[step.doc]!;
  const view = world.get(doc) ?? { body: "", version: 0, history: [] };
  const expected = (v: Version): number | undefined =>
    v === "current" ? view.version : v === "stale" ? Math.max(view.version - 1, 0) : v === "ahead" ? view.version + 1 : undefined;

  switch (step.op) {
    case "write":
      return { cmd: { op: "write", doc, body: step.body, expected: expected(step.version) } };
    case "append":
      return { cmd: { op: "append", doc, text: step.text } };
    case "patch": {
      const body = chars(view.body);
      const edits = step.edits.map(([q, replacement]) => {
        let old: string;
        if ("literal" in q) old = q.literal;
        else if (body.length === 0) old = "zz";
        else {
          const start = q.slice[0] % body.length;
          old = body.slice(start, Math.min(start + q.slice[1], body.length)).join("");
        }
        return { old, new: replacement };
      });
      return { cmd: { op: "patch", doc, expected: expected(step.version) ?? 0, edits } };
    }
    case "revert": {
      const patches = view.history.length;
      const index = step.pick === "newest" ? Math.max(patches - 1, 0) : step.pick === "missing" ? patches + 5 : step.pick.index;
      if (!patchIds) return { cmd: { op: "revert", doc, index, rollback: step.rollback } };
      const pid = (await patchIds(doc))[index];
      // Unknown on the server: an id that never exists, and an index the
      // model will never reach, even if patches arrive meanwhile.
      if (pid === undefined) return { cmd: { op: "revert", doc, index: NO_PATCH, rollback: step.rollback }, pid: MISSING_PID };
      return { cmd: { op: "revert", doc, index, rollback: step.rollback }, pid };
    }
    case "read":
      return { cmd: { op: "read", doc } };
    case "history":
      return { cmd: { op: "history", doc } };
  }
}

// The same call as an MCP tool call.
export function toTool(call: Call): { tool: string; args: Record<string, unknown> } {
  const { cmd } = call;
  switch (cmd.op) {
    case "write":
      return {
        tool: "write_doc",
        args: { project: PROJECT, doc: cmd.doc, body: cmd.body, ...(cmd.expected === undefined ? {} : { expected_version: cmd.expected }) },
      };
    case "append":
      return { tool: "append_doc", args: { project: PROJECT, doc: cmd.doc, text: cmd.text } };
    case "patch":
      return { tool: "patch_doc", args: { project: PROJECT, doc: cmd.doc, expected_version: cmd.expected, edits: cmd.edits } };
    case "revert":
      return { tool: "revert_patch", args: { project: PROJECT, doc: cmd.doc, patch_id: call.pid, rollback: cmd.rollback } };
    case "read":
      return { tool: "read_doc", args: { project: PROJECT, doc: cmd.doc } };
    case "history":
      return { tool: "doc_history", args: { project: PROJECT, doc: cmd.doc, limit: 100 } };
  }
}

export const describeCall = (call: Call): string => JSON.stringify(call.cmd);
