// Append-only skill patches: the improver's unit of change. A patch is extra
// instruction text appended AFTER a skill's body (never an edit of the body —
// don't break what works, and keep the change token-cheap and trivially
// revertible: delete the .patch.md).
//
// `appendPatch` is the ONE injection point that MUST be identical in both places
// or the gate is measuring fiction:
//   - prod runtime: append the live patch after the freshly-loaded skill body
//     (compose) / after the <tools>/<skills> block (planner) — Phase 3 wiring.
//   - gate replay: append the CANDIDATE patch to the node's RECORDED system
//     message before re-running the generator.
// Appending at the END preserves the planner's prompt-cache prefix (the stable
// <tools>/<skills> block compile.ts builds up front).

export const PATCH_MARKER = "<!-- improver-patch -->";

// Append a patch to a skill's effective system text. Empty/whitespace patch is a
// no-op (so an absent .patch.md changes nothing).
export function appendPatch(system: string, patch: string): string {
  const trimmed = patch.trim();
  if (trimmed.length === 0) return system;
  return `${system.replace(/\s+$/, "")}\n\n${PATCH_MARKER}\n${trimmed}\n`;
}

export interface ChatMessage {
  role: string;
  content: unknown;
}

function asText(content: unknown): string {
  return typeof content === "string" ? content : JSON.stringify(content);
}

// Apply a patch to a recorded chat-message array by appending it to the FIRST
// system message (the skill body for a compose node; the body+tools+skills
// prompt for a planner node). Returns a NEW array; throws when there is no
// system message to attach to (a malformed recording the gate can't replay).
export function patchMessages(messages: ChatMessage[], patch: string): ChatMessage[] {
  const idx = messages.findIndex((m) => m.role === "system");
  if (idx < 0) throw new Error("recorded input has no system message to patch");
  const sys = messages[idx]!;
  const patched: ChatMessage = { ...sys, content: appendPatch(asText(sys.content), patch) };
  return messages.map((m, i) => (i === idx ? patched : m));
}
