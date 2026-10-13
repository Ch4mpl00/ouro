// The SSRF guard and fetch_url, judged from the outside.
//
// 1. Address classification, over every spelling a URL can carry: an
//    independent 128-bit oracle (below — not net.BlockList, not the guard's
//    own code) says which addresses must be refused, which must be allowed,
//    and which may go either way; the guard's URL check must agree.
// 2. fetch_url over MCP with the production guard: no spelling of a loopback
//    address ever reaches a local server.
// 3. fetch_url over MCP with a guard that lets a local server play the
//    public internet: byte caps, truncation and binary detection.

import http from "node:http";
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import fc from "fast-check";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { z } from "zod";
import { assertPublicUrl, createGuardedFetch } from "../src/services/fetch";
import { call, connect, startServer, type Server } from "../specs/support/environment";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 400);

// ── the oracle: 128-bit addresses, IPv4 as ::ffff:a.b.c.d ─────────────────────

const V4_MAPPED = 0xffffn << 32n;
const NAT64 = 0x64ff9bn << 96n;

function v4Bits(text: string): bigint {
  const parts = text.split(".").map(Number);
  if (parts.length !== 4 || parts.some((p) => !Number.isInteger(p) || p < 0 || p > 255)) throw new Error(`not v4: ${text}`);
  return parts.reduce((acc, p) => (acc << 8n) | BigInt(p), 0n);
}

interface Range {
  base: bigint;
  prefix: number; // of 128
  label: string;
}

const v4 = (cidr: string, label: string): Range[] => {
  const [addr, len] = cidr.split("/") as [string, string];
  const bits = v4Bits(addr);
  // The plain (mapped) form and the NAT64 form reach the same IPv4 host.
  return [
    { base: V4_MAPPED | bits, prefix: 96 + Number(len), label },
    { base: NAT64 | bits, prefix: 96 + Number(len), label: `${label} via NAT64` },
  ];
};
const v6 = (hex: string, prefix: number, label: string): Range => ({ base: BigInt(`0x${hex}`), prefix, label });

// Must never be fetched: the box itself, its private networks, cloud
// metadata, carrier-grade NAT (Tailscale lives there), and IPv6 equivalents.
const MUST_REFUSE: Range[] = [
  ...v4("0.0.0.0/8", "this network"),
  ...v4("10.0.0.0/8", "RFC1918"),
  ...v4("100.64.0.0/10", "CGNAT"),
  ...v4("127.0.0.0/8", "loopback"),
  ...v4("169.254.0.0/16", "link-local / metadata"),
  ...v4("172.16.0.0/12", "RFC1918"),
  ...v4("192.168.0.0/16", "RFC1918"),
  v6("0", 96, "unspecified / loopback / IPv4-compatible"),
  v6("fc000000000000000000000000000000", 7, "unique-local"),
  v6("fe800000000000000000000000000000", 10, "link-local"),
  v6("fec00000000000000000000000000000", 10, "site-local"),
  v6("0064ff9b000100000000000000000000", 48, "local-use NAT64"),
];

// Not reachable on the public internet either way; refusing is fine,
// allowing is harmless.
const EITHER: Range[] = [
  ...v4("192.0.0.0/24", "IETF"),
  ...v4("192.0.2.0/24", "documentation"),
  ...v4("198.18.0.0/15", "benchmarking"),
  ...v4("198.51.100.0/24", "documentation"),
  ...v4("203.0.113.0/24", "documentation"),
  ...v4("224.0.0.0/4", "multicast"),
  ...v4("240.0.0.0/4", "reserved / broadcast"),
  v6("0064ff9b000000000000000000000000", 96, "NAT64 of a public address"),
  v6("ff000000000000000000000000000000", 8, "multicast"),
  v6("20010db8000000000000000000000000", 32, "documentation"),
  v6("20020000000000000000000000000000", 16, "6to4"),
  v6("20010000000000000000000000000000", 32, "Teredo"),
  v6("01000000000000000000000000000000", 64, "discard"),
];

const inRange = (addr: bigint, r: Range) => addr >> BigInt(128 - r.prefix) === r.base >> BigInt(128 - r.prefix);
const verdict = (addr: bigint): "refuse" | "either" | "allow" =>
  MUST_REFUSE.some((r) => inRange(addr, r)) ? "refuse" : EITHER.some((r) => inRange(addr, r)) ? "either" : "allow";

// ── spellings ────────────────────────────────────────────────────────────────

const hextets = (addr: bigint) => Array.from({ length: 8 }, (_, i) => Number((addr >> BigInt(112 - 16 * i)) & 0xffffn).toString(16));
const dotted = (bits: bigint) => [24n, 16n, 8n, 0n].map((s) => Number((bits >> s) & 0xffn)).join(".");

// Every way a URL host can name the address. The WHATWG parser normalises
// most of them (decimal, hex and short IPv4; IPv4-mapped IPv6 to hex), which
// is exactly why each must be tried.
function spellings(addr: bigint): string[] {
  const h = hextets(addr);
  const out = [`[${h.join(":")}]`, `[${h.join(":").toUpperCase()}]`, `[${h.slice(0, 6).join(":")}:${dotted(addr & 0xffffffffn)}]`];
  if (addr >> 32n === 0xffffn) {
    const bits = addr & 0xffffffffn;
    out.push(dotted(bits), String(bits), `0x${bits.toString(16)}`, `[::ffff:${dotted(bits)}]`, `[::ffff:${h[6]}:${h[7]}]`);
    const [a, b, c, d] = dotted(bits).split(".");
    out.push(`${a}.${(Number(b) << 16) | (Number(c) << 8) | Number(d)}`, `0${Number(a).toString(8)}.${b}.${c}.${d}`);
  }
  return out;
}

const inside = (r: Range) =>
  fc.bigInt({ min: 0n, max: (1n << BigInt(128 - r.prefix)) - 1n }).map((offset) => (r.base & ~((1n << BigInt(128 - r.prefix)) - 1n)) | offset);

const address = fc.oneof(
  { weight: 4, arbitrary: fc.constantFrom(...MUST_REFUSE).chain(inside) },
  { weight: 2, arbitrary: fc.constantFrom(...EITHER).chain(inside) },
  { weight: 2, arbitrary: fc.bigInt({ min: 0n, max: 0xffffffffn }).map((bits) => V4_MAPPED | bits) },
  { weight: 2, arbitrary: fc.bigInt({ min: 0x2n << 124n, max: (0x4n << 124n) - 1n }) }, // 2000::/3, global unicast
  { weight: 1, arbitrary: fc.bigInt({ min: 0n, max: (1n << 128n) - 1n }) },
);

const PRIVATE = /private\/loopback/;

describe("SSRF guard: address classification", () => {
  it("refuses every spelling of a non-public address and allows public ones", async () => {
    const seen = new Set<string>();
    await fc.assert(
      fc.asyncProperty(address, async (addr) => {
        const want = verdict(addr);
        seen.add(want);
        for (const host of spellings(addr)) {
          const url = `http://${host}:8080/x`;
          const outcome = await assertPublicUrl(url).then(
            () => "allowed" as const,
            (err: unknown) => (err instanceof Error && PRIVATE.test(err.message) ? ("refused" as const) : err),
          );
          if (outcome !== "allowed" && outcome !== "refused") throw new Error(`${url}: unexpected failure ${String(outcome)}`);
          const label = [...MUST_REFUSE, ...EITHER].find((r) => inRange(addr, r))?.label ?? "public";
          if (want === "refuse" && outcome !== "refused") throw new Error(`${url} (${label}) must be refused, was allowed`);
          if (want === "allow" && outcome !== "allowed") throw new Error(`${url} (${label}) must be allowed, was refused`);
        }
      }),
      { numRuns: RUNS },
    );
    expect([...seen].sort()).toEqual(["allow", "either", "refuse"]);
  });

  it("refuses names that are not http(s)", async () => {
    for (const url of ["file:///etc/passwd", "gopher://127.0.0.1/", "ftp://example.com/", "data:text/plain,hi", "javascript:alert(1)"]) {
      await expect(assertPublicUrl(url), url).rejects.toThrow(/scheme|invalid URL/);
    }
  });
});

// ── over MCP ─────────────────────────────────────────────────────────────────

const fetchReply = z.union([
  z
    .object({
      url: z.string(),
      status: z.number().int(),
      contentType: z.string(),
      bytes: z.number().int(),
      truncated: z.boolean(),
      content: z.string(),
    })
    .strict(),
  z
    .object({ url: z.string(), status: z.number().int(), contentType: z.string(), binary: z.literal(true), note: z.string() })
    .strict(),
]);

// `cuts`: byte offsets where the body is split into separately flushed
// chunks (chunked encoding), so a chunk can end exactly at a cap.
interface Route {
  contentType: string;
  body: Buffer;
  cuts?: readonly number[];
}

interface Local {
  port: number;
  hits: string[];
  routes: Map<string, Route>;
  close(): Promise<void>;
}

// One server on the dual-stack wildcard, so a request through 127.0.0.1,
// ::1 or ::ffff:127.0.0.1 all land here and are counted.
async function localServer(): Promise<Local> {
  const hits: string[] = [];
  const routes = new Map<string, Route>();
  const server = http.createServer(async (req, res) => {
    hits.push(`${req.socket.localAddress} ${req.url}`);
    const route = routes.get(req.url ?? "");
    if (!route) return void res.writeHead(404).end();
    if (!route.cuts) {
      return void res.writeHead(200, { "content-type": route.contentType, "content-length": route.body.length }).end(route.body);
    }
    res.writeHead(200, { "content-type": route.contentType });
    let from = 0;
    for (const cut of [...route.cuts, route.body.length]) {
      if (cut <= from || cut > route.body.length) continue;
      res.write(route.body.subarray(from, cut));
      from = cut;
      await new Promise((resolve) => setTimeout(resolve, 2));
    }
    res.end();
  });
  await new Promise<void>((resolve) => server.listen(0, "::", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("local server: no TCP address");
  return { port: address.port, hits, routes, close: () => new Promise((resolve) => server.close(() => resolve())) };
}

const available = await databaseAvailable();

describe.skipIf(!available)("fetch_url over MCP", () => {
  let db: TestDatabase;
  let local: Local;
  let guarded: Server;
  let open: Server;
  let guardedClient: Client;
  let openClient: Client;

  beforeAll(async () => {
    db = await testDatabase();
    local = await localServer();
    guarded = await startServer(db.url, ["fetch"]);
    // Only 127.0.0.1 counts as public: the local server plays the internet.
    open = await startServer(db.url, ["fetch"], createGuardedFetch({ isPrivate: (ip) => ip !== "127.0.0.1" }));
    guardedClient = await connect(guarded.url);
    openClient = await connect(open.url);
  }, 180_000);

  afterAll(async () => {
    await guardedClient?.close();
    await openClient?.close();
    await guarded?.stop();
    await open?.stop();
    await local?.close();
    await db?.drop();
  });

  it("no spelling of the box's own address reaches it", async () => {
    local.routes.set("/secret", { contentType: "text/plain", body: Buffer.from("internal") });
    const loopback = fc.oneof(
      fc.bigInt({ min: 0x7f000000n, max: 0x7fffffffn }).map((bits) => V4_MAPPED | bits),
      fc.constantFrom(1n, 0n, V4_MAPPED | 0n),
    );
    await fc.assert(
      fc.asyncProperty(loopback, async (addr) => {
        for (const host of spellings(addr)) {
          const outcome = await call(guardedClient, "fetch_url", { url: `http://${host}:${local.port}/secret` });
          if (outcome.kind !== "toolError" || !PRIVATE.test(outcome.message)) {
            throw new Error(`${host}: expected a private-address refusal, got ${JSON.stringify(outcome).slice(0, 300)}`);
          }
        }
      }),
      { numRuns: 60 },
    );
    expect(local.hits).toEqual([]);
  }, 300_000);

  // The cap is on bytes; the content is the longest whole-character prefix
  // of the body within it, and `truncated` says whether anything was cut.
  it("returns a body whole, or its longest prefix within maxBytes", async () => {
    await fc.assert(
      fc.asyncProperty(fc.string({ unit: "grapheme", maxLength: 300 }), fc.integer({ min: 1, max: 1500 }), fc.boolean(), async (text, cap, chunked) => {
        const body = Buffer.from(text, "utf8");
        // Chunked: one chunk boundary exactly at the cap, one just before.
        local.routes.set("/page", { contentType: "text/plain; charset=utf-8", body, cuts: chunked ? [cap - 1, cap] : undefined });
        const outcome = await call(openClient, "fetch_url", { url: `http://127.0.0.1:${local.port}/page`, maxBytes: cap });
        if (outcome.kind !== "ok") throw new Error(`fetch_url failed: ${JSON.stringify(outcome)}`);
        const reply = fetchReply.parse(outcome.value);
        if ("binary" in reply) throw new Error("text came back as binary");
        const got = Buffer.byteLength(reply.content, "utf8");
        if (body.length <= cap) {
          expect(reply, "fits: whole body, not truncated").toMatchObject({ content: text, truncated: false, bytes: body.length, status: 200 });
        } else {
          expect(reply.truncated, "longer than the cap: truncated").toBe(true);
          expect(text.startsWith(reply.content), `a prefix of the body: ${JSON.stringify(reply.content.slice(-5))}`).toBe(true);
          expect(got, "within the cap").toBeLessThanOrEqual(cap);
          expect(got, "the longest such prefix").toBeGreaterThan(cap - 4);
        }
      }),
      { numRuns: 150 },
    );
  });

  it("returns text types as text and binary types as a note", async () => {
    const TEXT = ["text/html; charset=utf-8", "text/csv", "application/json", "application/xml", "application/ld+json", "application/javascript"];
    const BINARY = ["image/png", "audio/mpeg", "video/mp4", "application/pdf", "application/zip", "application/octet-stream", "application/x-gzip"];
    for (const contentType of [...TEXT, ...BINARY]) {
      local.routes.set("/typed", { contentType, body: Buffer.from("payload") });
      const outcome = await call(openClient, "fetch_url", { url: `http://127.0.0.1:${local.port}/typed` });
      if (outcome.kind !== "ok") throw new Error(`${contentType}: ${JSON.stringify(outcome)}`);
      const reply = fetchReply.parse(outcome.value);
      expect("binary" in reply, contentType).toBe(BINARY.includes(contentType));
    }
  });
});
