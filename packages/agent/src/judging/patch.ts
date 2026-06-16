// Gate-side patch injection. The core primitive `appendPatch` lives in
// ../skills (the skill-overlay home) and is the SAME function prod runtime uses
// (compile.ts/execute.ts) — re-exported here so the gate and prod can't drift.
// This module adds the chat-message variant the replay needs.
import { appendPatch } from "../skills";

export { appendPatch, PATCH_MARKER } from "../skills";

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
