import { describe, expect, it } from "vitest";
import { createSessionContext } from "./session-context";
import { storeToolResult, TOOL_RESULT_INLINE_MAX_BYTES, TOOL_RESULT_PREVIEW_MAX_BYTES } from "./tool-results";

function memory() {
  return createSessionContext({
    id: "test",
    env: { now: new Date(), timezone: "UTC", userEmail: null, newsLastReadAt: null },
  }).memory;
}

describe("tool result storage", () => {
  it.each(["", "plain text ${key}", "{\"answer\":42}", "null", "{broken JSON"])(
    "preserves the exact small result: %j", (value) => {
      const store = memory();
      const reply = storeToolResult(store, value);
      expect(reply).toMatchObject({ content: value, truncated: false, size_bytes: Buffer.byteLength(value) });
      expect(store.get(reply.memory_key)).toBe(value);
      expect(store.list()).toEqual([{ key: reply.memory_key, format: reply.format, sizeBytes: reply.size_bytes }]);
      expect(reply).not.toHaveProperty("preview");
    },
  );

  it("keeps the exact byte threshold inline and hides larger content", () => {
    const store = memory();
    expect(storeToolResult(store, "x".repeat(TOOL_RESULT_INLINE_MAX_BYTES)).truncated).toBe(false);
    const value = "x".repeat(TOOL_RESULT_INLINE_MAX_BYTES) + "hidden tail";
    const reply = storeToolResult(store, value);
    expect(reply.truncated).toBe(true);
    if (!reply.truncated) throw new Error("Expected a preview");
    expect(reply).not.toHaveProperty("content");
    expect(reply.preview).toBe("x".repeat(TOOL_RESULT_PREVIEW_MAX_BYTES));
    expect(store.get(reply.memory_key)).toBe(value);
  });

  it("measures UTF-8 bytes and never cuts a preview inside a character", () => {
    const store = memory();
    const value = "я🦊".repeat(2_000);
    const reply = storeToolResult(store, value);
    expect(reply.truncated).toBe(true);
    expect(reply.size_bytes).toBe(12_000);
    if (!reply.truncated) throw new Error("Expected a preview");
    expect(Buffer.byteLength(reply.preview)).toBeLessThanOrEqual(TOOL_RESULT_PREVIEW_MAX_BYTES);
    expect(reply.preview).not.toContain("�");
    expect(value.startsWith(reply.preview)).toBe(true);
    expect(store.get(reply.memory_key)).toBe(value);
  });

  it("allocates distinct keys for identical outputs", () => {
    const store = memory();
    const replies = Array.from({ length: 20 }, () => storeToolResult(store, "same"));
    expect(new Set(replies.map((r) => r.memory_key)).size).toBe(20);
    expect(store.list()).toHaveLength(20);
  });
});
