// The Gmail poller against a fake Gmail API (test/support/fake-gmail.ts) and
// the real state database: mails arrive, the poller ticks — sometimes a
// fetch or a signal write fails midway — and once it has caught up, every
// matching mail that arrived after the bootstrap must have become exactly one
// signal, in arrival order, carrying its attachments; nothing else may.
//
// PROPERTY_RUNS raises the number of generated timelines.

import fc from "fast-check";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectStateDb, type StateDb } from "../src/db/state";
import { createGmailModule, GMAIL_SUBSCRIPTIONS, pollSubscriptionOnce, type GmailModule } from "../src/services/gmail";
import { createSignals, type Signals } from "../src/services/signals";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";
import { startFakeGmail, type FakeGmail, type FakeMail } from "./support/fake-gmail";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 25);
const SUB = GMAIL_SUBSCRIPTIONS[0]!;

type Step =
  | { kind: "arrive"; mails: { matches: boolean; gapMs: number; pdfs: number; others: number }[] }
  | { kind: "tick"; fault: { kind: "none" } | { kind: "get"; pick: number } | { kind: "record"; at: number } };

const mail = fc.record({
  matches: fc.oneof({ weight: 4, arbitrary: fc.constant(true) }, { weight: 1, arbitrary: fc.constant(false) }),
  gapMs: fc.integer({ min: 1, max: 4_000 }),
  pdfs: fc.integer({ min: 0, max: 2 }),
  others: fc.integer({ min: 0, max: 1 }),
});
const step: fc.Arbitrary<Step> = fc.oneof(
  { weight: 4, arbitrary: fc.array(mail, { minLength: 1, maxLength: 4 }).map((mails): Step => ({ kind: "arrive", mails })) },
  // A burst after an outage: more than one page of the list.
  { weight: 1, arbitrary: fc.integer({ min: 51, max: 70 }).map((n): Step => ({ kind: "arrive", mails: Array.from({ length: n }, () => ({ matches: true, gapMs: 1_000, pdfs: 1, others: 0 })) })) },
  {
    weight: 5,
    arbitrary: fc.oneof(
      { weight: 3, arbitrary: fc.constant<Step>({ kind: "tick", fault: { kind: "none" } }) },
      { weight: 1, arbitrary: fc.nat().map((pick): Step => ({ kind: "tick", fault: { kind: "get", pick } })) },
      { weight: 1, arbitrary: fc.constantFrom(1, 1, 2, 3).map((at): Step => ({ kind: "tick", fault: { kind: "record", at } })) },
    ),
  },
);

// Signals whose `at`-th record fails, as a dropped connection would.
function failingAt(signals: Signals, at: number): Signals {
  let n = 0;
  return {
    ...signals,
    record: async (input) => {
      if (++n === at) throw new Error("injected: connection terminated");
      return signals.record(input);
    },
  };
}

const available = await databaseAvailable();

describe.skipIf(!available)("Gmail poller against a fake Gmail and Postgres", () => {
  let db: TestDatabase;
  let state: StateDb;
  let fake: FakeGmail;
  let gmail: GmailModule;
  let signals: Signals;

  beforeAll(async () => {
    db = await testDatabase();
    state = await connectStateDb(db.url);
    fake = await startFakeGmail();
    process.env.GOOGLE_CLIENT_ID = "test-client";
    process.env.GOOGLE_CLIENT_SECRET = "test-secret";
    process.env.GOOGLE_REDIRECT_URI = "http://127.0.0.1/oauth";
    delete process.env.GMAIL_ACCOUNT_KEY;
    await state.pool.query(
      `INSERT INTO integration_account (provider, account_key, access_token, refresh_token, expires_at)
       VALUES ('gmail', 'me@example.com', 'test-token', 'test-refresh', $1)`,
      [new Date(Date.now() + 24 * 3600_000).toISOString()],
    );
    gmail = createGmailModule({ db: state, apiRootUrl: fake.rootUrl });
    signals = createSignals(state);
  }, 180_000);

  afterAll(async () => {
    await fake?.close();
    await state?.close();
    await db?.drop();
  });

  it("turns every matching mail into exactly one signal, in order, across failures and bursts", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(step, { minLength: 1, maxLength: 12, size: "max" }), async (steps) => {
        await state.pool.query("TRUNCATE signals, gmail_kv RESTART IDENTITY");
        fake.mails.length = 0;

        // Bootstrap: the first poll only sets the watermark.
        await pollSubscriptionOnce({ gmail, signals }, SUB);
        const bootstrap = await state.pool.query("SELECT value FROM gmail_kv");
        const watermark = Number(bootstrap.rows[0]?.value);
        expect(Number.isFinite(watermark), "bootstrap sets a watermark").toBe(true);
        expect((await signals.list({})).length, "bootstrap emits nothing").toBe(0);

        let clock = watermark + 1_000;
        let n = 0;
        for (const s of steps) {
          if (s.kind === "arrive") {
            if (s.mails.length > 50) witnessed.add("a burst of more than one page");
            for (const m of s.mails) {
              n++;
              clock += m.gapMs;
              const mailItem: FakeMail = {
                id: `m${n}`,
                internalDate: clock,
                subject: `Рахунок ${n}`,
                matches: m.matches,
                attachments: [
                  ...Array.from({ length: m.pdfs }, (_, i) => ({ attachmentId: `att-${n}-pdf${i}`, filename: `bill-${n}-${i}.pdf`, mimeType: "application/pdf", size: 1000 })),
                  ...Array.from({ length: m.others }, (_, i) => ({ attachmentId: `att-${n}-img${i}`, filename: `logo-${n}.png`, mimeType: "image/png", size: 10 })),
                ],
              };
              fake.mails.push(mailItem);
            }
            continue;
          }
          let deps = { gmail, signals };
          if (s.fault.kind === "get") {
            // One of the newest matching mails: likely not signalled yet, so
            // the poll really reaches it and fails there.
            const pending = fake.mails.filter((m) => m.matches);
            const target = pending[pending.length - 1 - (s.fault.pick % Math.min(3, Math.max(pending.length, 1)))];
            if (target) fake.failGet(target.id);
          }
          if (s.fault.kind === "record") deps = { gmail, signals: failingAt(signals, s.fault.at) };
          const failed = await pollSubscriptionOnce(deps, SUB).then(() => false, () => true);
          if (failed) witnessed.add("a poll fails midway");
        }
        // Catch up on healthy ticks.
        for (let i = 0; i < 6; i++) await pollSubscriptionOnce({ gmail, signals }, SUB);

        const emitted = (await signals.list({ limit: 2000 })).map((sig) => {
          const id = /messageId: (\S+)/.exec(sig.content)?.[1];
          if (!id) throw new Error(`a Gmail signal without a messageId: ${sig.content.slice(0, 200)}`);
          return { id, source: sig.source, content: sig.content };
        });
        const want = fake.mails.filter((m) => m.matches).sort((a, b) => a.internalDate - b.internalDate).map((m) => m.id);
        const where = JSON.stringify(steps).slice(0, 600);
        expect(emitted.map((e) => e.id), `${where}: one signal per matching mail, in arrival order`).toEqual(want);
        for (const e of emitted) {
          expect(e.source, where).toBe(SUB.signalSource);
          const m = fake.mails.find((x) => x.id === e.id)!;
          for (const a of m.attachments) {
            expect(e.content.includes(a.attachmentId), `${where}: ${e.id} lists its PDFs, and only them (${a.filename})`).toBe(a.mimeType === "application/pdf");
          }
        }
      }),
      { numRuns: RUNS },
    );
    expect([...witnessed].sort()).toEqual(["a burst of more than one page", "a poll fails midway"]);
  }, 600_000);
});
