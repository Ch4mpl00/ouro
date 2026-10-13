// The Codex service around a stand-in `codex` binary put first on PATH: the
// real spawn, stdin, temp files, exit codes and signals, with the CLI's
// behaviour scripted by the prompt (answer, fail, or hang ignoring SIGTERM
// with a child of its own). And the HTTP front: what a caller may ask for.

import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import type http from "node:http";
import { tmpdir } from "node:os";
import path from "node:path";
import fc from "fast-check";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { runCodex, type CodexRunRequest } from "../src/runner";
import { createCodexServer } from "../src/server";

let bin: string;
const previousPath = process.env.PATH;

const FAKE_CODEX = `#!/usr/bin/env node
const fs = require("node:fs");
const { spawn } = require("node:child_process");
const args = process.argv.slice(2);
const prompt = args[args.length - 1];
let stdin = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (d) => (stdin += d));
process.stdin.on("end", () => {
  if (prompt.includes("MODE:hang")) {
    const pidFile = /PIDFILE:(\\S+)/.exec(prompt)[1];
    const child = spawn("sleep", ["60"], { stdio: "ignore" });
    fs.writeFileSync(pidFile, String(child.pid));
    process.on("SIGTERM", () => {});
    setInterval(() => {}, 1000);
    return;
  }
  if (prompt.includes("MODE:fail")) {
    console.log(JSON.stringify({ type: "error", message: "model exploded" }));
    process.exit(2);
  }
  const i = args.indexOf("--output-schema");
  const schema = i >= 0 ? fs.readFileSync(args[i + 1], "utf8") : null;
  const echo = JSON.stringify({ args, stdin, schema, schemaPath: i >= 0 ? args[i + 1] : null });
  console.log(JSON.stringify({ type: "thread.started", thread_id: "th-1" }));
  console.log("a line that is not JSON");
  console.log(JSON.stringify({ type: "item.completed", item: { type: "agent_message", text: echo } }));
  console.log(JSON.stringify({ type: "turn.completed", usage: { input_tokens: 1 } }));
});
`;

beforeAll(() => {
  bin = mkdtempSync(path.join(tmpdir(), "fake-codex-"));
  writeFileSync(path.join(bin, "codex"), FAKE_CODEX);
  chmodSync(path.join(bin, "codex"), 0o755);
  process.env.PATH = `${bin}${path.delimiter}${previousPath}`;
});
afterAll(() => {
  process.env.PATH = previousPath;
  rmSync(bin, { recursive: true, force: true });
});

interface Echo {
  args: string[];
  stdin: string;
  schema: string | null;
  schemaPath: string | null;
}

function echoOf(content: string): Echo {
  const v: unknown = JSON.parse(content);
  if (typeof v !== "object" || v === null) throw new Error(`not an echo: ${content}`);
  const args: unknown = Reflect.get(v, "args");
  const stdin: unknown = Reflect.get(v, "stdin");
  const schema: unknown = Reflect.get(v, "schema");
  const schemaPath: unknown = Reflect.get(v, "schemaPath");
  if (!Array.isArray(args) || !args.every((a): a is string => typeof a === "string") || typeof stdin !== "string") {
    throw new Error(`not an echo: ${content}`);
  }
  return {
    args,
    stdin,
    schema: typeof schema === "string" ? schema : null,
    schemaPath: typeof schemaPath === "string" ? schemaPath : null,
  };
}

const request: fc.Arbitrary<CodexRunRequest> = fc.record(
  {
    prompt: fc.string({ unit: "grapheme", minLength: 1, maxLength: 40 }).map((p) => `task: ${p}`),
    input: fc.oneof(fc.string({ maxLength: 30 }), fc.jsonValue({ maxDepth: 2 })),
    schema: fc.jsonValue({ maxDepth: 2 }),
    sandbox: fc.constantFrom("read-only" as const, "workspace-write" as const),
    model: fc.constantFrom("gpt-5.4", "o4-mini"),
  },
  { requiredKeys: ["prompt"] },
);

describe("runCodex against a stand-in codex binary", () => {
  it("passes the request through to the CLI and reads its answer back", async () => {
    await fc.assert(
      fc.asyncProperty(request, async (req) => {
        const result = await runCodex(req);
        if (!result.ok) throw new Error(`run failed: ${result.error}`);
        const echo = echoOf(result.content);
        expect(echo.args.at(-1), "the prompt is the last argument, verbatim").toBe(req.prompt);
        const flag = (name: string) => {
          const i = echo.args.indexOf(name);
          return i >= 0 ? echo.args[i + 1] : undefined;
        };
        expect(flag("--sandbox"), "sandbox").toBe(req.sandbox ?? "read-only");
        expect(flag("--model"), "model only when asked").toBe(req.model);
        expect(echo.args, "never the user's codex config").toContain("--ignore-user-config");
        const wantStdin = req.input === undefined ? "" : typeof req.input === "string" ? req.input : JSON.stringify(req.input, null, 2);
        expect(echo.stdin, "input arrives on stdin").toBe(wantStdin);
        if (req.schema === undefined) expect(echo.schemaPath, "no schema file without a schema").toBeNull();
        else {
          expect(JSON.parse(echo.schema ?? "null"), "the schema file holds the schema").toEqual(JSON.parse(JSON.stringify(req.schema)));
          expect(existsSync(echo.schemaPath ?? ""), "the schema file is gone afterwards").toBe(false);
        }
        expect(result.threadId).toBe("th-1");
        expect(result.usage).toEqual({ input_tokens: 1 });
        expect(result.parsed, "JSON content comes back parsed").toEqual(echo);
      }),
      { numRuns: 25 },
    );
  }, 120_000);

  it("reports a failed run with the CLI's own error", async () => {
    const result = await runCodex({ prompt: "MODE:fail" });
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.code).toBe(2);
      expect(result.error).toContain("model exploded");
    }
  });

  // A run past its deadline ends — even when the CLI ignores SIGTERM — and
  // takes the processes it started with it.
  it("ends a run past its deadline, children included", async () => {
    const pidFile = path.join(bin, `pid-${Date.now()}`);
    const started = Date.now();
    const result = await runCodex({ prompt: `MODE:hang PIDFILE:${pidFile}`, timeoutMs: 500 });
    expect(Date.now() - started, "returns soon after the deadline").toBeLessThan(5_000);
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatch(/timed out/);
    const grandchild = Number(readFileSync(pidFile, "utf8"));
    await new Promise((r) => setTimeout(r, 200));
    expect(() => process.kill(grandchild, 0), "the CLI's own child is gone too").toThrow();
  }, 30_000);
});

describe("the HTTP front", () => {
  let server: http.Server;
  let base: string;

  beforeAll(async () => {
    server = createCodexServer({ maxBodyBytes: 4096 });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (address === null || typeof address === "string") throw new Error("no TCP address");
    base = `http://127.0.0.1:${address.port}`;
  });
  afterAll(() => new Promise<void>((resolve) => server.close(() => resolve())));

  const post = (body: string) => fetch(`${base}/run`, { method: "POST", body, headers: { "content-type": "application/json" } });

  it("answers health and 404s anything else", async () => {
    expect((await fetch(`${base}/health`)).status).toBe(200);
    expect((await fetch(`${base}/elsewhere`)).status).toBe(404);
  });

  it("runs a well-formed request", async () => {
    const res = await post(JSON.stringify({ prompt: "hello", sandbox: "workspace-write" }));
    expect(res.status).toBe(200);
  });

  it.each([
    ["no prompt", JSON.stringify({})],
    ["empty prompt", JSON.stringify({ prompt: "" })],
    ["not JSON", "{oops"],
    ["an array", "[]"],
    ["full access asked for", JSON.stringify({ prompt: "x", sandbox: "danger-full-access" })],
    ["an unknown sandbox", JSON.stringify({ prompt: "x", sandbox: "yolo" })],
    ["an unknown approval policy", JSON.stringify({ prompt: "x", approvalPolicy: "always" })],
    ["a timeout that is not a number", JSON.stringify({ prompt: "x", timeoutMs: "soon" })],
    ["a body over the limit", JSON.stringify({ prompt: "x".repeat(10_000) })],
  ])("refuses %s", async (_label, body) => {
    const res = await post(body);
    expect(res.status).toBeGreaterThanOrEqual(400);
    expect(res.status).toBeLessThan(500);
  });
});
