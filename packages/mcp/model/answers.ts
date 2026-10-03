// Both sides answer in one shape, holding only what the contract fixes: ok or
// the error code, version, size, body, history, and the version a conflict
// reports. Messages, patch ids, timestamps and near-match suggestions are
// left out — they are allowed to differ.

import type { Outcome } from "../specs/support/environment";
import type { Call } from "./calls";
import { step, type Reply, type World } from "./model";

export type Answer = Record<string, unknown>;

// A write that lost a compare-and-swap race to a concurrent one: a
// version_conflict without a current version. It changes nothing.
export const RACED: Answer = { ok: false, error: "raced" };

const failure = (error: string, extra: Answer = {}): Answer => ({ ok: false, error, ...extra });

export function render(reply: Reply): Answer {
  switch (reply.kind) {
    case "written":
      return { ok: true, version: reply.version, sizeBytes: reply.sizeBytes };
    case "read":
      return { ok: true, body: reply.body, version: reply.version };
    case "history":
      return { ok: true, history: reply.entries };
    case "docNotFound":
      return failure("doc_not_found");
    case "versionRequired":
      return failure("version_required", { currentVersion: reply.current });
    case "versionConflict":
      return failure("version_conflict", { currentVersion: reply.current });
    case "editFailed":
      return failure("edit_failed");
    case "patchNotFound":
      return failure("patch_not_found");
    case "revertConflict":
      return failure("revert_conflict", { rollbackToVersion: reply.rollbackTo });
    case "rejected":
      return { rejected: true };
  }
}

// The model's answer to a call, and the world after it.
export function modelAnswer(world: World, call: Call): [World, Answer] {
  const [next, reply] = step(world, call.cmd);
  return [next, render(reply)];
}

const get = (value: unknown, key: string): unknown =>
  value !== null && typeof value === "object" ? (value as Record<string, unknown>)[key] : undefined;

// The server's answer in the model's shape.
export function normalise(call: Call, outcome: Outcome): Answer {
  if (outcome.kind === "rejected") return { rejected: true };
  if (outcome.kind === "toolError") {
    // Invalid parameters: the TS SDK reports them as a tool error carrying
    // the JSON-RPC code.
    if (outcome.message.startsWith("MCP error -32602:")) return { rejected: true };
    // Not a memory answer at all (a database error): never valid.
    return { failed: outcome.message };
  }
  const value = outcome.value;
  if (get(value, "ok") !== true) {
    const error = String(get(value, "error") ?? "?");
    if (error === "version_conflict" && get(value, "currentVersion") === undefined) return RACED;
    if (error === "version_conflict" || error === "version_required") {
      return failure(error, { currentVersion: get(value, "currentVersion") });
    }
    if (error === "revert_conflict") return failure(error, { rollbackToVersion: get(value, "rollbackToVersion") });
    return failure(error);
  }
  switch (call.cmd.op) {
    case "read": {
      const doc = get(value, "doc");
      return { ok: true, body: get(doc, "body"), version: get(doc, "version") };
    }
    case "history": {
      const history = get(value, "history");
      return {
        ok: true,
        history: (Array.isArray(history) ? history : []).map((p) => ({
          kind: get(p, "kind"),
          versionBefore: get(p, "versionBefore"),
          versionAfter: get(p, "versionAfter"),
        })),
      };
    }
    default:
      return { ok: true, version: get(value, "version"), sizeBytes: get(value, "sizeBytes") };
  }
}

// Key order does not matter; everything else does.
export function sameAnswer(a: Answer, b: Answer): boolean {
  return canonical(a) === canonical(b);
}

export function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value !== null && typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>)
      .filter(([, v]) => v !== undefined)
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`).join(",")}}`;
  }
  return JSON.stringify(value);
}
