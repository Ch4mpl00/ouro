import { describe, expect, it } from "vitest";
import {
  createStore,
  DuplicateBindingError,
  MissingBindingError,
  substitute,
} from "./substitute";

describe("createStore", () => {
  it("returns primitive top-level values", () => {
    const s = createStore({ count: 7, name: "alice" });
    expect(s.get("count")).toBe(7);
    expect(s.get("name")).toBe("alice");
  });

  it("walks dot-notation paths into nested objects", () => {
    const s = createStore({ env: { chatId: 42, tz: "Europe/Kiev" } });
    expect(s.get("env.chatId")).toBe(42);
    expect(s.get("env.tz")).toBe("Europe/Kiev");
  });

  it("throws MissingBindingError on missing top-level key", () => {
    const s = createStore({ env: {} });
    expect(() => s.get("posts")).toThrow(MissingBindingError);
  });

  it("throws MissingBindingError on missing nested key", () => {
    const s = createStore({ env: { tz: "UTC" } });
    expect(() => s.get("env.chatId")).toThrow(MissingBindingError);
  });

  it("throws MissingBindingError when traversing through non-object", () => {
    const s = createStore({ env: "not-an-object" });
    expect(() => s.get("env.foo")).toThrow(MissingBindingError);
  });

  it("preserves null vs undefined: explicit null is found, undefined missing key isn't", () => {
    const s = createStore({ env: { chatId: null } });
    expect(s.has("env.chatId")).toBe(true);
    expect(s.get("env.chatId")).toBe(null);
    expect(s.has("env.tz")).toBe(false);
  });

  it("has() returns true for existing paths, false for missing", () => {
    const s = createStore({ a: { b: 1 } });
    expect(s.has("a")).toBe(true);
    expect(s.has("a.b")).toBe(true);
    expect(s.has("a.c")).toBe(false);
    expect(s.has("x")).toBe(false);
  });

  it("set() adds a new binding", () => {
    const s = createStore({});
    s.set("posts", [1, 2, 3]);
    expect(s.get("posts")).toEqual([1, 2, 3]);
  });

  it("set() throws DuplicateBindingError on second write to same name", () => {
    const s = createStore({ posts: [] });
    expect(() => s.set("posts", [1])).toThrow(DuplicateBindingError);
  });

  it("snapshot() returns a plain object copy of the store", () => {
    const s = createStore({ a: 1 });
    s.set("b", 2);
    expect(s.snapshot()).toEqual({ a: 1, b: 2 });
  });
});

describe("substitute — string mode", () => {
  it("whole-string placeholder returns the raw value (preserves type)", () => {
    const s = createStore({ posts: [{ id: 1 }, { id: 2 }] });
    expect(substitute("${posts}", s)).toEqual([{ id: 1 }, { id: 2 }]);
  });

  it("whole-string placeholder returns number as number, not stringified", () => {
    const s = createStore({ count: 7 });
    expect(substitute("${count}", s)).toBe(7);
  });

  it("whole-string placeholder returns null as null", () => {
    const s = createStore({ x: null });
    expect(substitute("${x}", s)).toBe(null);
  });

  it("interpolation stringifies non-string values", () => {
    const s = createStore({ name: "Bob", count: 7 });
    expect(substitute("Hello ${name}, count=${count}", s)).toBe(
      "Hello Bob, count=7",
    );
  });

  it("interpolation JSON-stringifies object values", () => {
    const s = createStore({ x: { a: 1 } });
    expect(substitute("payload=${x}", s)).toBe('payload={"a":1}');
  });

  it("interpolation walks nested paths", () => {
    const s = createStore({ env: { chatId: 42, tz: "Europe/Kiev" } });
    expect(substitute("chat=${env.chatId} tz=${env.tz}", s)).toBe(
      "chat=42 tz=Europe/Kiev",
    );
  });

  it("missing binding in whole-string mode throws MissingBindingError", () => {
    const s = createStore({});
    expect(() => substitute("${posts}", s)).toThrow(MissingBindingError);
  });

  it("missing binding in interpolation mode throws MissingBindingError", () => {
    const s = createStore({});
    expect(() => substitute("hi ${name}", s)).toThrow(MissingBindingError);
  });

  it("strings without placeholders pass through unchanged", () => {
    const s = createStore({});
    expect(substitute("plain text", s)).toBe("plain text");
  });
});

describe("substitute — recursive walks", () => {
  it("walks object values recursively", () => {
    const s = createStore({ env: { chatId: 42 }, text: "hello" });
    expect(
      substitute(
        { chatId: "${env.chatId}", text: "${text}" },
        s,
      ),
    ).toEqual({ chatId: 42, text: "hello" });
  });

  it("walks array values recursively", () => {
    const s = createStore({ a: 1, b: 2 });
    expect(substitute(["${a}", "${b}", "literal"], s)).toEqual([1, 2, "literal"]);
  });

  it("does not mutate the input", () => {
    const input = { chatId: "${env.chatId}" };
    const s = createStore({ env: { chatId: 42 } });
    const out = substitute(input, s) as Record<string, unknown>;
    expect(input.chatId).toBe("${env.chatId}");
    expect(out.chatId).toBe(42);
  });

  it("passes non-string primitives through unchanged", () => {
    const s = createStore({});
    expect(substitute(42, s)).toBe(42);
    expect(substitute(true, s)).toBe(true);
    expect(substitute(null, s)).toBe(null);
  });

  it("handles deeply nested args without losing types", () => {
    const s = createStore({
      env: { chatId: 285083560 },
      digest: "📰 Новости",
    });
    const args = {
      chatId: "${env.chatId}",
      text: "${digest}",
      meta: { recipient: "${env.chatId}" },
    };
    expect(substitute(args, s)).toEqual({
      chatId: 285083560,
      text: "📰 Новости",
      meta: { recipient: 285083560 },
    });
  });
});
