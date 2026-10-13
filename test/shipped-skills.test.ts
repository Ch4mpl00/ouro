// The agent validates every skill against the MCP registry at startup and
// refuses to start on a mismatch (supervisor/main.ts) — on the droplet that
// is a crash loop. This checks the same thing before a deploy: the real MCP
// server process, booted the way prod's `mcp` instance is (every toolset),
// listed through the agent's own MCP client, and the agent's own
// `validateAll` over the skills that ship in skills.default.
//
// Gateway tools (`prefix__tool`) can't be listed offline; a skill may declare
// one only under the prefix of an upstream the shipped gateway.config.json
// enables. Whether that upstream is reachable at boot is F15.

import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectMcp, type McpHandle } from "../packages/agent/src/mcp-client";
import { createSkillStore } from "../packages/agent/src/skills";
import { databaseAvailable, testDatabase, type TestDatabase } from "../packages/mcp/test/support/database";

const REPO = path.resolve(import.meta.dirname, "..");

async function freePort(): Promise<number> {
  const server = net.createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  await new Promise((resolve) => server.close(resolve));
  if (address === null || typeof address === "string") throw new Error("no TCP address");
  return address.port;
}

const available = await databaseAvailable();

describe.skipIf(!available)("the skills that ship", () => {
  let db: TestDatabase;
  let dir: string;
  let child: ChildProcess;
  let mcp: McpHandle;

  beforeAll(async () => {
    db = await testDatabase();
    dir = mkdtempSync(path.join(tmpdir(), "shipped-skills-"));
    // Own tools only: the gateway's upstreams are checked by prefix below.
    writeFileSync(path.join(dir, "gateway.config.json"), JSON.stringify({ upstreams: [] }));
    const port = await freePort();
    child = spawn(path.join(REPO, "node_modules/.bin/tsx"), [path.join(REPO, "packages/mcp/src/server.ts")], {
      cwd: dir,
      env: {
        PATH: process.env.PATH ?? "",
        HOME: process.env.HOME ?? "",
        MCP_TRANSPORT: "http",
        MCP_PORT: String(port),
        MCP_NO_POLLERS: "1",
        DATABASE_URL: db.url,
        ...(process.env.STATE_DATABASE_URL ? { STATE_DATABASE_URL: process.env.STATE_DATABASE_URL } : {}),
        GATEWAY_CONFIG: path.join(dir, "gateway.config.json"),
        // Required at boot; nothing here embeds.
        OPENAI_API_KEY: "sk-test-unused",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let out = "";
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`MCP did not start:\n${out}`)), 60_000);
      const onData = (c: Buffer) => {
        out += c.toString();
        if (out.includes("[mcp-http] listening")) {
          clearTimeout(timer);
          resolve();
        }
      };
      child.stdout?.on("data", onData);
      child.stderr?.on("data", onData);
      child.once("exit", (code) => {
        clearTimeout(timer);
        reject(new Error(`MCP exited (${code}):\n${out}`));
      });
    });
    // Connected the way the agent container is: transport from env.
    process.env.MCP_TRANSPORT = "http";
    process.env.MCP_URL = `http://127.0.0.1:${port}/mcp`;
    mcp = await connectMcp();
  }, 180_000);

  afterAll(async () => {
    await mcp?.close().catch(() => {});
    if (child && child.exitCode === null) {
      const exited = new Promise((r) => child.once("exit", r));
      child.kill("SIGTERM");
      await exited;
    }
    await db?.drop();
    if (dir) rmSync(dir, { recursive: true, force: true });
  });

  it("all pass the agent's startup validation against prod's MCP surface", async () => {
    const own = mcp.tools.map((t) => t.function.name);
    expect(own, "the full instance serves the signal queue").toContain("get_next_signal");
    const gateway: unknown = JSON.parse(readFileSync(path.join(REPO, "packages/mcp/gateway.config.json"), "utf8"));
    const upstreams: unknown = Reflect.get(Object(gateway), "upstreams");
    const prefixes = (Array.isArray(upstreams) ? upstreams : [])
      .filter((u) => Reflect.get(Object(u), "enabled") !== false)
      .map((u) => String(Reflect.get(Object(u), "prefix") ?? Reflect.get(Object(u), "name")));

    const store = createSkillStore({ liveDir: path.join(dir, "no-live-skills"), defaultsDir: path.join(REPO, "skills.default") });
    const skills = await store.listSkills();
    expect(skills.length, "skills.default is found").toBeGreaterThan(5);
    // What the gateway would add: every declared prefix__tool whose prefix
    // is an enabled upstream.
    const declared = new Set<string>();
    for (const s of skills) {
      const file = await store.readSkill(s.name);
      if (file && file.tools !== "*") for (const t of file.tools) declared.add(t);
    }
    const viaGateway = [...declared].filter((t) => prefixes.some((p) => t.startsWith(`${p}__`)));
    await expect(store.validateAll([...own, ...viaGateway])).resolves.toBeUndefined();
  }, 120_000);
});
