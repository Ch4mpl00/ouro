// read_file / read_pdf over MCP on a generated maze of paths: only files
// whose real location lies under a readable root come back; everything else
// — absolute paths elsewhere, `..` escapes, symlinks pointing out, a sibling
// directory whose name merely starts like a root — is refused before it is
// opened.

import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import fc from "fast-check";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createFileAccess, defaultReadRoots } from "../src/services/files";
import { registerFsTools } from "../src/tools/fs";
import { registerPdfTools } from "../src/tools/pdf";
import { call } from "../specs/support/environment";

// base/
//   allowed/       a.txt  sub/b.txt  link-in → allowed/a.txt  link-out → outside/secret.txt  link-dir-out → outside/
//   allowed-evil/  c.txt               (shares a prefix with "allowed")
//   second/        d.txt               (a second root)
//   root-link → allowed                (a root given through a symlink)
//   outside/       secret.txt
let base: string;
let client: Client;
let real: (p: string) => string;
const FILES: Record<string, string> = {
  "allowed/a.txt": "alpha",
  "allowed/sub/b.txt": "beta",
  "allowed-evil/c.txt": "evil",
  "second/d.txt": "delta",
  "outside/secret.txt": "TOKEN=hunter2",
};

beforeAll(async () => {
  base = realpathSync(mkdtempSync(path.join(tmpdir(), "files-")));
  for (const [rel, content] of Object.entries(FILES)) {
    mkdirSync(path.dirname(path.join(base, rel)), { recursive: true });
    writeFileSync(path.join(base, rel), content);
  }
  symlinkSync(path.join(base, "allowed/a.txt"), path.join(base, "allowed/link-in"));
  symlinkSync(path.join(base, "outside/secret.txt"), path.join(base, "allowed/link-out"));
  symlinkSync(path.join(base, "outside"), path.join(base, "allowed/link-dir-out"));
  symlinkSync(path.join(base, "allowed"), path.join(base, "root-link"));
  real = (p) => realpathSync(p);

  const files = createFileAccess({ roots: ["root-link", path.join(base, "second")], cwd: base });
  const server = new McpServer({ name: "files", version: "0" });
  registerFsTools(server, files);
  registerPdfTools(server, files);
  const [ct, st] = InMemoryTransport.createLinkedPair();
  await server.connect(st);
  client = new Client({ name: "t", version: "0" });
  await client.connect(ct);
});

afterAll(async () => {
  await client?.close();
  rmSync(base, { recursive: true, force: true });
});

// Readable iff the real path is one of the roots' real paths or below one,
// by plain string comparison on whole path segments.
function readable(target: string): boolean | "missing" {
  let r: string;
  try {
    r = real(target);
  } catch {
    return "missing";
  }
  const roots = [real(path.join(base, "allowed")), real(path.join(base, "second"))];
  return roots.some((root) => r === root || r.startsWith(root + path.sep));
}

const segment = fc.constantFrom("allowed", "allowed-evil", "second", "root-link", "outside", "sub", "..", ".", "a.txt", "b.txt", "c.txt", "d.txt", "secret.txt", "link-in", "link-out", "link-dir-out", "nope");
// Random walks through the maze, plus walks known to end on a readable file
// (random ones rarely do).
const spelled = fc.record({
  absolute: fc.boolean(),
  segments: fc.oneof(
    { weight: 3, arbitrary: fc.array(segment, { minLength: 1, maxLength: 5 }) },
    {
      weight: 1,
      arbitrary: fc.constantFrom(
        ["allowed", "a.txt"],
        ["root-link", "sub", "b.txt"],
        ["second", "d.txt"],
        ["allowed", "link-in"],
        ["allowed", "sub", "..", "a.txt"],
      ),
    },
  ),
  doubleSlash: fc.boolean(),
});

describe("read_file and read_pdf stay inside the readable roots", () => {
  it("open exactly the files whose real location is under a root", async () => {
    const seen = new Set<string>();
    await fc.assert(
      fc.asyncProperty(spelled, async ({ absolute, segments, doubleSlash }) => {
        const rel = segments.join(doubleSlash ? "//" : "/");
        const input = absolute ? `${base}/${rel}` : rel;
        const target = path.resolve(base, input);
        const want = readable(target);
        const outcome = await call(client, "read_file", { path: input });
        if (want === true) {
          let isFile = true;
          try {
            readFileSyncText(target);
          } catch {
            isFile = false; // a directory: refused by the read itself
          }
          if (!isFile) return;
          seen.add("readable");
          expect(outcome.kind, `${input} is readable`).toBe("ok");
          if (outcome.kind === "ok" && typeof outcome.value === "object" && outcome.value !== null && "content" in outcome.value) {
            expect(outcome.value.content, input).toBe(readFileSyncText(target));
          }
        } else {
          seen.add(want === "missing" ? "missing" : "refused");
          expect(outcome.kind, `${input} must not be read`).toBe("toolError");
          if (outcome.kind === "toolError" && want === false) {
            expect(outcome.message, `${input}: refused, not opened`).toMatch(/refusing to read/);
            expect(outcome.message).not.toContain("hunter2");
          }
        }
      }),
      { numRuns: 600 },
    );
    expect([...seen].sort()).toEqual(["missing", "readable", "refused"]);
  });

  it.each([
    ["/etc/passwd"],
    ["/proc/self/environ"],
    ["../outside/secret.txt"],
    ["allowed/link-out"],
    ["allowed/link-dir-out/secret.txt"],
    ["allowed-evil/c.txt"],
    ["allowed/../outside/secret.txt"],
  ])("refuses %s", async (input) => {
    for (const [tool, arg] of [["read_file", "path"], ["read_pdf", "filePath"]] as const) {
      const outcome = await call(client, tool, { [arg]: input });
      expect(outcome.kind, `${tool}(${input})`).toBe("toolError");
      if (outcome.kind === "toolError") expect(outcome.message).not.toContain("hunter2");
    }
  });

  it("reads through a symlink that stays inside, and through a root given as a symlink", async () => {
    for (const input of ["allowed/link-in", "root-link/sub/b.txt", `${base}/second/d.txt`]) {
      expect((await call(client, "read_file", { path: input })).kind, input).toBe("ok");
    }
  });
});

describe("defaultReadRoots", () => {
  it("is MCP_READ_ROOTS when set, resolved against cwd", () => {
    expect(defaultReadRoots({ MCP_READ_ROOTS: ["a", "/b"].join(path.delimiter) }, "/srv")).toEqual(["/srv/a", "/b"]);
  });
  it("is storage, the skill directories and the eval fixtures otherwise", () => {
    expect(defaultReadRoots({ STORAGE_DIR: "/data/storage" }, "/app")).toEqual([
      "/data/storage",
      "/app/skills",
      "/app/skills.default",
      "/app/packages/agent/src/eval/fixtures",
    ]);
  });
});

function readFileSyncText(p: string): string {
  return readFileSync(p, "utf-8");
}
