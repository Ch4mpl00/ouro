// download_gmail_attachment over MCP against the fake Gmail and the real
// state database, then read_file on the storage root — the way the bill flow
// uses them. The ids and the filename come from the model, so they are
// generated hostile as often as not; sometimes the fake Gmail answers any
// attachment path at all, so the tool cannot lean on Gmail rejecting a
// malformed id. Whatever is asked: every file the tool writes sits in a
// directory named exactly after its message, inside the account's directory,
// and holds the bytes Gmail sent for it; nothing lands anywhere else.

import { mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import fc from "fast-check";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectStateDb, type StateDb } from "../src/db/state";
import { createFileAccess } from "../src/services/files";
import { createGmailModule } from "../src/services/gmail";
import { registerFsTools } from "../src/tools/fs";
import { registerGmailTools } from "../src/tools/gmail";
import { call } from "../specs/support/environment";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";
import { attachmentBytes, startFakeGmail, type FakeGmail, type FakeMail } from "./support/fake-gmail";

const ACCOUNT = "me@example.com";
const MAILS = [
  { id: "19e0794763036c82", attachments: ["ANGjdJcn2zNfQ1", "ANGjdJ8BO3C4Q2"] },
  { id: "19df30756e0b1641", attachments: ["ANGjdJloZQ6bQ3"] },
];

const realMail = fc.constantFrom(...MAILS.map((m) => m.id));
const hostileId = fc.constantFrom("..", ".", "../..", "../outside", "a/b", "a\\b", "", "%2e%2e", "x\u0000y", "/abs", "..\\..");
const realAttachment = fc.constantFrom(...MAILS.flatMap((m) => m.attachments));
const filename = fc.option(
  fc.constantFrom("bill.pdf", "травень_ДГ.pdf", "../../evil.pdf", "..", ".", "/etc/passwd", "a\\b.pdf", "", " ", "a/../../b.pdf"),
  { nil: undefined },
);
const download = fc.record({
  messageId: fc.oneof({ weight: 2, arbitrary: realMail }, { weight: 2, arbitrary: hostileId }),
  attachmentId: fc.oneof({ weight: 3, arbitrary: realAttachment }, { weight: 1, arbitrary: hostileId }),
  filename,
});

function filesUnder(dir: string): string[] {
  return readdirSync(dir, { recursive: true, withFileTypes: true })
    .filter((e) => e.isFile())
    .map((e) => path.join(e.parentPath, e.name));
}

const available = await databaseAvailable();

describe.skipIf(!available)("download_gmail_attachment", () => {
  let db: TestDatabase;
  let state: StateDb;
  let fake: FakeGmail;
  let base: string;
  let storage: string;
  let client: Client;

  beforeAll(async () => {
    db = await testDatabase();
    state = await connectStateDb(db.url);
    fake = await startFakeGmail();
    for (const m of MAILS) {
      fake.mails.push({
        id: m.id,
        internalDate: 1_790_000_000_000,
        subject: "Рахунок",
        matches: true,
        attachments: m.attachments.map((attachmentId, i) => ({ attachmentId, filename: `bill-${i}.pdf`, mimeType: "application/pdf", size: 100 })),
      });
    }
    process.env.GOOGLE_CLIENT_ID = "test-client";
    process.env.GOOGLE_CLIENT_SECRET = "test-secret";
    process.env.GOOGLE_REDIRECT_URI = "http://127.0.0.1/oauth";
    delete process.env.GMAIL_ACCOUNT_KEY;
    await state.pool.query(
      `INSERT INTO integration_account (provider, account_key, access_token, refresh_token, expires_at)
       VALUES ('gmail', $1, 'test-token', 'test-refresh', $2)`,
      [ACCOUNT, new Date(Date.now() + 24 * 3600_000).toISOString()],
    );
    // base/storage is the storage root; base/outside is where an escape lands.
    base = realpathSync(mkdtempSync(path.join(tmpdir(), "gmail-attachments-")));
    storage = path.join(base, "storage");
    process.env.STORAGE_DIR = storage;

    const server = new McpServer({ name: "gmail-attachments", version: "0" });
    registerGmailTools(server, createGmailModule({ db: state, apiRootUrl: fake.rootUrl }));
    registerFsTools(server, createFileAccess({ roots: [storage], cwd: base }));
    const [ct, st] = InMemoryTransport.createLinkedPair();
    await server.connect(st);
    client = new Client({ name: "t", version: "0" });
    await client.connect(ct);
  }, 180_000);

  afterAll(async () => {
    delete process.env.STORAGE_DIR;
    await client?.close();
    await fake?.close();
    await state?.close();
    await db?.drop();
    if (base) rmSync(base, { recursive: true, force: true });
  });

  it("writes only inside the message's own directory, the bytes Gmail sent, readable back", async () => {
    const accountDir = path.join(storage, "gmail", ACCOUNT);
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(download, { minLength: 1, maxLength: 8 }), fc.boolean(), async (downloads, lenient) => {
        rmSync(base, { recursive: true, force: true });
        mkdirSync(base);
        fake.lenient = lenient;
        // The bytes each written path must hold: the last download to it.
        const want = new Map<string, string>();
        for (const d of downloads) {
          const where = `${JSON.stringify(d)} lenient=${lenient}`;
          const out = await call(client, "download_gmail_attachment", d);
          const known = MAILS.find((m) => m.id === d.messageId)?.attachments.includes(d.attachmentId) === true;
          if (out.kind !== "ok") {
            expect(out.kind, `${where}: a failure is a tool error`).toBe("toolError");
            expect(known, `${where}: a real attachment downloads (${JSON.stringify(out)})`).toBe(false);
            if (!known) witnessed.add("a bad id refused");
            continue;
          }
          const value: unknown = out.value;
          const filePath = String(Reflect.get(Object(value), "filePath"));
          expect(path.dirname(filePath), `${where}: in the message's own directory`).toBe(path.join(accountDir, d.messageId));
          expect(path.basename(path.dirname(filePath)), `${where}: named exactly after the message`).toBe(d.messageId);
          const bytes = known
            ? attachmentBytes({ attachmentId: d.attachmentId })
            : `%PDF lenient ${d.messageId}/${d.attachmentId}`;
          want.set(filePath, bytes);
          witnessed.add(known ? "a real attachment lands" : "a lenient Gmail answers a bad id");
        }
        // Nothing anywhere but the files the tool reported, each holding its
        // latest bytes and readable through read_file.
        expect(filesUnder(base).sort(), `${JSON.stringify(downloads)}: no stray files`).toEqual([...want.keys()].sort());
        for (const [filePath, bytes] of want) {
          expect(readFileSync(filePath, "utf8")).toBe(bytes);
          const read = await call(client, "read_file", { path: filePath });
          expect(read, `read_file ${filePath}`).toMatchObject({ kind: "ok", value: { content: bytes } });
        }
      }),
      { numRuns: 120 },
    );
    for (const w of ["a bad id refused", "a real attachment lands", "a lenient Gmail answers a bad id"]) expect(witnessed, w).toContain(w);
  }, 300_000);
  // Paging through list_nashdom_mails with nextPageToken: every matching
  // mail exactly once, newest first, nothing else; each listing exactly its
  // PDFs — by type or by a .pdf name in any case — wherever in the MIME tree
  // they sit.
  it("pages through every NashDom mail once, newest first, with exactly its PDFs", async () => {
    const attachment = fc.record({
      kind: fc.constantFrom("pdf", "octet-pdf-name", "upper-name", "image", "doc"),
      nested: fc.boolean(),
    });
    const mail = fc.record({ matches: fc.oneof({ weight: 3, arbitrary: fc.constant(true) }, { weight: 1, arbitrary: fc.constant(false) }), attachments: fc.array(attachment, { maxLength: 3 }) });
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(mail, { maxLength: 30 }), fc.integer({ min: 1, max: 12 }), async (mails, limit) => {
        fake.mails.length = 0;
        const t0 = 1_790_000_000_000;
        const made: FakeMail[] = mails.map((m, i) => ({
          id: `m${i.toString(16).padStart(4, "0")}`,
          internalDate: t0 + i * 60_000,
          subject: `Рахунок ${i}`,
          matches: m.matches,
          attachments: m.attachments.map((a, j) => {
            const [filename, mimeType] =
              a.kind === "pdf" ? [`bill-${i}-${j}.pdf`, "application/pdf"]
              : a.kind === "octet-pdf-name" ? [`bill-${i}-${j}.pdf`, "application/octet-stream"]
              : a.kind === "upper-name" ? [`BILL-${i}-${j}.PDF`, "application/octet-stream"]
              : a.kind === "image" ? [`logo-${i}-${j}.png`, "image/png"]
              : [`terms-${i}-${j}.docx`, "application/vnd.openxmlformats-officedocument.wordprocessingml.document"];
            return { attachmentId: `ANGjdJ${i}x${j}`, filename, mimeType, size: 100, nested: a.nested };
          }),
        }));
        fake.mails.push(...made);
        const seen: { id: string; attachments: string[] }[] = [];
        let token: string | undefined;
        for (let page = 0; page < 100; page++) {
          const out = await call(client, "list_nashdom_mails", { limit, ...(token ? { pageToken: token } : {}) });
          if (out.kind !== "ok") throw new Error(JSON.stringify(out));
          const value: unknown = out.value;
          const messages = Reflect.get(Object(value), "messages");
          if (!Array.isArray(messages)) throw new Error("no messages");
          expect(messages.length, "a page holds at most `limit`").toBeLessThanOrEqual(limit);
          for (const m of messages) {
            const atts = Reflect.get(Object(m), "attachments");
            seen.push({ id: String(Reflect.get(Object(m), "messageId")), attachments: (Array.isArray(atts) ? atts : []).map((a) => String(Reflect.get(Object(a), "attachmentId"))).sort() });
          }
          const next = Reflect.get(Object(value), "nextPageToken");
          if (typeof next !== "string" || next === "") break;
          token = next;
          witnessed.add("more than one page");
        }
        const want = made
          .filter((m) => m.matches)
          .sort((a, b) => b.internalDate - a.internalDate)
          .map((m) => ({
            id: m.id,
            attachments: m.attachments.filter((a) => a.mimeType === "application/pdf" || a.filename.toLowerCase().endsWith(".pdf")).map((a) => a.attachmentId).sort(),
          }));
        if (made.some((m) => m.attachments.some((a) => a.nested && a.filename.toLowerCase().endsWith(".pdf")))) witnessed.add("a nested PDF");
        expect(seen, `${JSON.stringify(mails)} limit=${limit}`).toEqual(want);
      }),
      { numRuns: Number(process.env.PROPERTY_RUNS ?? 40) },
    );
    expect([...witnessed].sort()).toEqual(["a nested PDF", "more than one page"]);
  }, 300_000);
});
