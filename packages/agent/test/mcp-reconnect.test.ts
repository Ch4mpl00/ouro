// The agent's MCP client (packages/agent/src/mcp-client.ts) against the real
// Streamable HTTP transport of packages/mcp, served by a process that dies
// the way a container does — SIGKILL — and comes back on the same port, or
// whose single session another client takes over. Every tool call has a side
// effect logged outside the server (packages/mcp/test/support/mcp-child.ts —
// run as a process, so the two packages still meet only over the protocol).
//
// Rules, from the timeline alone:
//   - nothing runs twice: a call the tool began is never replayed, even when
//     the server dies under it (a Telegram message must not go out twice);
//   - a call that returned a result ran exactly once, to completion;
//   - a call made while the server is up and stays up succeeds, however many
//     restarts and evictions came before — reconnecting is the client's job;
//   - a call made while the server is down fails, and starts nothing;
//   - concurrent callers that find the session gone open one session between
//     them, not one each.

import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import fc from "fast-check";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectMcp, type McpHandle } from "../src/mcp-client";

const REPO = path.resolve(import.meta.dirname, "../../..");
const CHILD = path.join(REPO, "packages/mcp/test/support/mcp-child.ts");
const RUNS = Number(process.env.PROPERTY_RUNS ?? 10);

type Step =
  | { kind: "calls"; parallel: number }
  | { kind: "restart" } // dies and comes back between calls
  | { kind: "killMidCall" } // dies while a call is running, comes back
  | { kind: "callWhileDown" }
  | { kind: "evict" }; // another client takes the single session

const step: fc.Arbitrary<Step> = fc.oneof(
  { weight: 4, arbitrary: fc.integer({ min: 1, max: 3 }).map((parallel): Step => ({ kind: "calls", parallel })) },
  { weight: 2, arbitrary: fc.constant<Step>({ kind: "restart" }) },
  { weight: 2, arbitrary: fc.constant<Step>({ kind: "killMidCall" }) },
  { weight: 1, arbitrary: fc.constant<Step>({ kind: "callWhileDown" }) },
  { weight: 1, arbitrary: fc.constant<Step>({ kind: "evict" }) },
);

async function freePort(): Promise<number> {
  const server = net.createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  await new Promise((resolve) => server.close(resolve));
  if (address === null || typeof address === "string") throw new Error("no TCP address");
  return address.port;
}

function lines(ledger: string): string[] {
  return existsSync(ledger) ? readFileSync(ledger, "utf8").split("\n").filter(Boolean) : [];
}

async function until(what: string, check: () => boolean, ms = 20_000): Promise<void> {
  const deadline = Date.now() + ms;
  while (!check()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}

describe("the agent's MCP client across MCP restarts", () => {
  let dir: string;
  beforeAll(() => {
    dir = mkdtempSync(path.join(tmpdir(), "mcp-reconnect-"));
  });
  afterAll(() => rmSync(dir, { recursive: true, force: true }));

  it("never runs a call twice, and serves every call made while MCP is up", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(step, { minLength: 1, maxLength: 6 }), fc.boolean(), async (steps, multi) => {
        const port = await freePort();
        const ledger = path.join(dir, `ledger-${port}-${Date.now()}`);
        const url = `http://127.0.0.1:${port}/mcp`;
        let gen = 0;
        let child: ChildProcess | null = null;
        const start = async () => {
          gen++;
          let out = "";
          // node itself, with tsx as a loader: the tsx CLI runs the script in
          // a child process that a SIGKILL of the wrapper would orphan.
          const proc = spawn(process.execPath, ["--import", "tsx", CHILD], {
            cwd: REPO,
            env: { PATH: process.env.PATH ?? "", HOME: process.env.HOME ?? "", PORT: String(port), LEDGER: ledger, GEN: String(gen), MULTI: multi ? "1" : "0" },
            stdio: ["ignore", "pipe", "pipe"],
          });
          proc.stdout.on("data", (c: Buffer) => void (out += c.toString()));
          proc.stderr.on("data", (c: Buffer) => void (out += c.toString()));
          await until(`MCP generation ${gen} to listen (${out})`, () => out.includes("[mcp-http] listening") || proc.exitCode !== null);
          if (proc.exitCode !== null) throw new Error(`MCP exited: ${out}`);
          child = proc;
        };
        const kill = async () => {
          const proc = child;
          child = null;
          if (!proc || proc.exitCode !== null) return;
          const exited = new Promise((r) => proc.once("exit", r));
          proc.kill("SIGKILL");
          await exited;
        };

        await start();
        const mcp: McpHandle = await connectMcp({ createTransport: () => new StreamableHTTPClientTransport(new URL(url)) });
        let n = 0;
        // id → what the caller saw, and whether MCP was up for the whole call.
        const calls = new Map<string, { ok: boolean; up: boolean; text: string }>();
        const effect = async (up: boolean, delayMs?: number, signal?: AbortSignal) => {
          const id = `c${++n}`;
          const outcome = await mcp.callTool("effect", { id, ...(delayMs ? { delayMs } : {}) }, { signal }).then(
            (text) => ({ ok: !text.startsWith("[tool error]"), text }),
            (err: unknown) => ({ ok: false, text: err instanceof Error ? err.message : String(err) }),
          );
          calls.set(id, { ...outcome, up });
          return id;
        };
        const where = `${JSON.stringify(steps)} multi=${multi}`;
        try {
          for (const s of steps) {
            if (s.kind === "calls") {
              await Promise.all(Array.from({ length: s.parallel }, () => effect(true)));
            } else if (s.kind === "restart") {
              await kill();
              await start();
              witnessed.add("restart");
            } else if (s.kind === "killMidCall") {
              const id = `c${n + 1}`;
              // A call whose server dies under it is only failed by the SDK's
              // 60 s request timeout (F19); cancel it sooner, as the
              // supervisor's own cancellation would.
              const running = effect(false, 5_000, AbortSignal.timeout(2_000));
              await until(`call ${id} to start`, () => lines(ledger).includes(`start ${id}`));
              await kill();
              await running;
              await start();
              witnessed.add("killed mid-call");
            } else if (s.kind === "callWhileDown") {
              await kill();
              await effect(false);
              await start();
              witnessed.add("a call while down");
            } else if (!multi) {
              const intruder = new Client({ name: "intruder", version: "0" });
              await intruder.connect(new StreamableHTTPClientTransport(new URL(url)));
              await intruder.close();
              witnessed.add("evicted");
            }
          }
          // After everything, MCP is up: calls work again, several at once.
          await Promise.all([effect(true), effect(true)]);
        } finally {
          await mcp.close().catch(() => {});
          await kill();
        }

        const log = lines(ledger);
        const count = (line: string) => log.filter((l) => l === line).length;
        for (const [id, c] of calls) {
          expect(count(`start ${id}`), `${where}: ${id} ran at most once (${c.text})`).toBeLessThanOrEqual(1);
          if (c.ok) expect(count(`done ${id}`), `${where}: ${id} returned, so it ran to completion once`).toBe(1);
          if (c.up) expect(c.ok, `${where}: ${id} was made while MCP was up: ${c.text}`).toBe(true);
        }
        for (const [id, c] of calls) {
          if (!c.up && !c.ok && count(`start ${id}`) === 0) witnessed.add("a call failed and started nothing");
        }
        if (multi) {
          // One session per generation the client used: a reconnect shared by
          // concurrent callers, not one session each.
          for (let g = 1; g <= gen; g++) {
            expect(count(`session ${g}`), `${where}: sessions opened on generation ${g}`).toBeLessThanOrEqual(1);
          }
        }
      }),
      { numRuns: RUNS },
    );
    for (const w of ["restart", "killed mid-call", "a call while down", "a call failed and started nothing"]) expect(witnessed, w).toContain(w);
  }, 900_000);
});
