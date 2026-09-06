import { describe, expect, expectTypeOf, it } from "vitest";
import { createSessionContext } from "./session-context";

function createTestContext(id = "session") {
  return createSessionContext({
    id,
    env: {
      now: new Date("2026-09-06T12:00:00Z"),
      timezone: "Europe/Chisinau",
      userEmail: null,
      newsLastReadAt: null,
    },
  });
}

describe("session context", () => {
  it("owns its identity, environment and a stable memory instance", () => {
    const context = createTestContext("telegram:42");
    expect(context.id).toBe("telegram:42");
    expect(context.env).toEqual({
      now: new Date("2026-09-06T12:00:00Z"),
      timezone: "Europe/Chisinau",
      userEmail: null,
      newsLastReadAt: null,
    });

    const memory = context.memory;
    memory.put("news", "shared result");
    expect(context.memory).toBe(memory);
    expect(context.memory.get("news")).toBe("shared result");
  });
});

describe("session context memory invariants", () => {
  it("accepts and returns strings in its public contract", () => {
    const memory = createTestContext().memory;
    expectTypeOf(memory.put).parameter(1).toEqualTypeOf<string>();
    expectTypeOf(memory.get).returns.toEqualTypeOf<string>();
  });

  it("starts empty without shared state between instances", () => {
    const first = createTestContext().memory;
    const second = createTestContext().memory;

    expect(first.list()).toEqual([]);
    expect(second.list()).toEqual([]);
    first.put("news", "first run");
    expect(second.list()).toEqual([]);
    expect(() => second.get("news")).toThrow('Key "news" not found');

    second.put("news", "second run");
    first.delete("news");
    expect(second.get("news")).toBe("second run");
  });

  it.each([
    { label: "empty text", value: "" },
    { label: "whitespace and line endings", value: "  first\r\nsecond\n\t" },
    { label: "unicode", value: "Новини 📰 e\u0301" },
    { label: "JSON-looking text", value: '{ "items": [1, null, false] }' },
    { label: "placeholder-looking text", value: '${news.raw} {"ref":"news"}' },
    { label: "long text", value: "новость\n".repeat(10_000) },
  ])("preserves $label exactly on repeated reads", ({ value }) => {
    const memory = createTestContext().memory;
    memory.put("data", value);

    expect(memory.get("data")).toBe(value);
    expect(memory.get("data")).toBe(value);
    expect(memory.list()).toHaveLength(1);
  });

  it("uses exact flat keys without interpreting paths or object properties", () => {
    const memory = createTestContext().memory;
    const keys = ["news", "news.raw", "news[0]", "__proto__", "constructor", " news "];

    for (const key of keys) memory.put(key, `value of ${key}`);
    for (const key of keys) expect(memory.get(key)).toBe(`value of ${key}`);
    expect(() => memory.get("news.raw.title")).toThrow('Key "news.raw.title" not found');
    expect(() => memory.get("News")).toThrow('Key "News" not found');
    expect(memory.list()).toHaveLength(keys.length);
  });

  it("rejects an empty key without changing stored data", () => {
    const memory = createTestContext().memory;
    memory.put("existing", "keep");

    expect(() => memory.put("", "value")).toThrow("Key must not be empty");
    expect(memory.list()).toEqual([{ key: "existing", format: "text", sizeBytes: 4 }]);
    expect(memory.get("existing")).toBe("keep");
  });

  it("rejects duplicate keys without replacing their value or format", () => {
    const memory = createTestContext().memory;
    memory.put("result", "original");
    const before = memory.list();

    expect(() => memory.put("result", "original")).toThrow('Key "result" already exists');
    expect(() => memory.put("result", '{"new":true}', "json"))
      .toThrow('Key "result" already exists');
    expect(memory.get("result")).toBe("original");
    expect(memory.list()).toEqual(before);
  });

  it("reports a missing key while preserving an existing empty string", () => {
    const memory = createTestContext().memory;
    memory.put("empty", "");

    expect(memory.get("empty")).toBe("");
    expect(() => memory.get("missing")).toThrow('Key "missing" not found');
    expect(memory.list()).toEqual([{ key: "empty", format: "text", sizeBytes: 0 }]);
  });

  it("defaults to text and keeps an explicit format without parsing the value", () => {
    const memory = createTestContext().memory;
    memory.put("text", "null");
    memory.put("json", ' { "a": [1, true] }\n', "json");
    memory.put("incomplete-json", '{"a":', "json");

    expect(memory.get("text")).toBe("null");
    expect(memory.get("json")).toBe(' { "a": [1, true] }\n');
    expect(memory.get("incomplete-json")).toBe('{"a":');
    expect(memory.list().map(({ key, format }) => ({ key, format }))).toEqual([
      { key: "text", format: "text" },
      { key: "json", format: "json" },
      { key: "incomplete-json", format: "json" },
    ]);
  });

  it("lists only metadata with UTF-8 byte sizes, without exposing payloads", () => {
    const memory = createTestContext().memory;
    memory.put("ascii", "abc");
    memory.put("unicode", "Я📰");
    memory.put("json", "[1,2]", "json");

    expect(memory.list()).toEqual([
      { key: "ascii", format: "text", sizeBytes: 3 },
      { key: "unicode", format: "text", sizeBytes: 6 },
      { key: "json", format: "json", sizeBytes: 5 },
    ]);
  });

  it("returns detached metadata so callers cannot modify the store", () => {
    const memory = createTestContext().memory;
    memory.put("news", "data", "json");
    const entries = memory.list();
    const [entry] = entries;
    if (!entry) throw new Error("Expected metadata for news");

    entry.key = "renamed";
    entry.format = "text";
    entry.sizeBytes = 999;
    entries.splice(0);

    expect(memory.get("news")).toBe("data");
    expect(() => memory.get("renamed")).toThrow('Key "renamed" not found');
    expect(memory.list()).toEqual([{ key: "news", format: "json", sizeBytes: 4 }]);
  });

  it("deletes only the exact key and reports whether it existed", () => {
    const memory = createTestContext().memory;
    memory.put("news", "");
    memory.put("news.raw", "keep");

    expect(memory.delete("missing")).toBe(false);
    expect(memory.delete("news")).toBe(true);
    expect(memory.delete("news")).toBe(false);
    expect(() => memory.get("news")).toThrow('Key "news" not found');
    expect(memory.get("news.raw")).toBe("keep");
    expect(memory.list()).toEqual([{ key: "news.raw", format: "text", sizeBytes: 4 }]);
  });

  it("allows a deleted key to be explicitly reused", () => {
    const memory = createTestContext().memory;
    memory.put("result", "old");
    memory.delete("result");
    memory.put("result", "[1]", "json");

    expect(memory.get("result")).toBe("[1]");
    expect(memory.list()).toEqual([{ key: "result", format: "json", sizeBytes: 3 }]);
  });
});
