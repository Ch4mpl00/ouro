// The digest filter (decisions.ts `filter`) through the real decision client
// and the real OpenAI SDK, against a stand-in for OpenRouter's decisions API
// behind the SDK's fetch. The fake answers every item from the item itself,
// sometimes badly; the rules say from the items alone what must be asked,
// how it is batched, and what comes back — or that the whole filter fails.

import fc from "fast-check";
import OpenAI from "openai";
import { describe, expect, it } from "vitest";
import { createDecisionClient, filter, FILTER_CONCURRENCY, FILTER_MAX_BATCH_CHARS, FILTER_MAX_BATCH_ITEMS, FILTER_THRESHOLD } from "../src/decisions";
import { nullTracer } from "../src/tracing";

interface Item {
  id: number;
  p: number;
  body: string;
}

type Fault = "none" | "extra" | "missing" | "outOfRange" | "wrongType" | "rejected";

interface Seen {
  ids: number[];
  ns: number[];
  questionKeys: string[];
  chars: number;
}

function fakeApi(faultFor: (firstId: number) => Fault) {
  const seen: Seen[] = [];
  let inFlight = 0;
  let maxInFlight = 0;
  const client = new OpenAI({
    apiKey: "test",
    baseURL: "https://openrouter.test/api/v1",
    fetch: async (_url, init) => {
      const body: unknown = JSON.parse(String(init?.body));
      const state = Reflect.get(Object(body), "state");
      const items: unknown = Reflect.get(Object(state), "items");
      const questions: unknown = Reflect.get(Object(body), "questions");
      if (!Array.isArray(items) || typeof questions !== "object" || questions === null) throw new Error("malformed request");
      const entries = items.map((e) => ({ n: Number(Reflect.get(Object(e), "n")), item: Reflect.get(Object(e), "item") }));
      const ids = entries.map((e) => Number(Reflect.get(Object(e.item), "id")));
      seen.push({ ids, ns: entries.map((e) => e.n), questionKeys: Object.keys(questions), chars: entries.reduce((n, e) => n + JSON.stringify(e.item).length, 0) });
      inFlight++;
      maxInFlight = Math.max(maxInFlight, inFlight);
      await new Promise((r) => setTimeout(r, 2));
      inFlight--;
      const fault = faultFor(ids[0] ?? -1);
      if (fault === "rejected") {
        return new Response(JSON.stringify({ error: { message: "bad schema" } }), { status: 400, headers: { "content-type": "application/json" } });
      }
      const answers: Record<string, unknown> = {};
      for (const e of entries) {
        const p = Number(Reflect.get(Object(e.item), "p"));
        answers[`item_${e.n}`] = { type: "noul", noul: p };
      }
      const firstKey = `item_${entries[0]?.n ?? 1}`;
      if (fault === "extra") answers.item_999 = { type: "noul", noul: 0.5 };
      if (fault === "missing") delete answers[firstKey];
      if (fault === "outOfRange") answers[firstKey] = { type: "noul", noul: 1.7 };
      if (fault === "wrongType") answers[firstKey] = { type: "choice", choice: "yes" };
      return new Response(JSON.stringify({ model: "typesafe/jev-test", id: `dec-${seen.length}`, answers }), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    },
  });
  return { client, seen, maxInFlight: () => maxInFlight };
}

const P = fc.constantFrom(0, 0.1, 0.29, FILTER_THRESHOLD, 0.31, 0.8, 1);
const item = fc.record({
  p: P,
  // Mostly short, sometimes big enough to fill a batch by size.
  body: fc.oneof({ weight: 6, arbitrary: fc.string({ maxLength: 60 }) }, { weight: 1, arbitrary: fc.integer({ min: 2_000, max: 30_000 }).map((n) => "я".repeat(n)) }),
});
const scope = nullTracer.trace({ id: "t", name: "t" });

describe("filter through the decision client", () => {
  it("asks about every item once, in bounded batches, and keeps exactly the items at or over the threshold", async () => {
    await fc.assert(
      fc.asyncProperty(
        fc.array(item, { maxLength: 80, size: "max" }).map((xs): Item[] => xs.map((x, id) => ({ id, ...x }))),
        fc.option(fc.constantFrom(0.1, 0.3, 0.5, 0.9), { nil: undefined }),
        fc.constantFrom<Fault>("none", "none", "none", "extra", "missing", "outOfRange", "wrongType", "rejected"),
        fc.nat(),
        async (items, threshold, fault, faultPick) => {
          // The fault hits one batch: the one that starts at this item.
          const api = fakeApi((first) => (items.length > 0 && first === items[faultPick % items.length]?.id ? fault : "none"));
          const decisions = createDecisionClient({ client: api.client, model: "typesafe/jev-test" });
          const settled = await filter(decisions, { items, prompt: "Keep tech news.", threshold, scope }).then(
            (r) => ({ ok: true as const, r }),
            (e: unknown) => ({ ok: false as const, e }),
          );
          const where = `${items.length} items, fault ${fault}`;

          // Every item asked about exactly once, in contiguous ascending
          // batches within both limits, numbered from 1 with one question each.
          const asked = api.seen.flatMap((s) => s.ids).sort((a, b) => a - b);
          const faultHit = api.seen.some((s) => s.ids[0] === items[faultPick % Math.max(items.length, 1)]?.id) && fault !== "none" && fault !== "extra";
          if (!faultHit) expect(asked, `${where}: every item once`).toEqual(items.map((i) => i.id));
          for (const s of api.seen) {
            expect(s.ids, `${where}: a batch is a run of consecutive items`).toEqual(s.ids.map((_, k) => (s.ids[0] ?? 0) + k));
            expect(s.ns, `${where}: numbered from 1`).toEqual(s.ids.map((_, k) => k + 1));
            expect(s.questionKeys.sort(), `${where}: one question per item`).toEqual(s.ns.map((n) => `item_${n}`).sort());
            expect(s.ids.length, `${where}: at most ${FILTER_MAX_BATCH_ITEMS} items`).toBeLessThanOrEqual(FILTER_MAX_BATCH_ITEMS);
            if (s.ids.length > 1) expect(s.chars, `${where}: within the char budget`).toBeLessThanOrEqual(FILTER_MAX_BATCH_CHARS);
          }
          expect(api.maxInFlight(), "bounded concurrency").toBeLessThanOrEqual(FILTER_CONCURRENCY);
          if (items.length === 0) expect(api.seen, "nothing to ask").toEqual([]);

          if (faultHit) {
            // One bad batch fails the whole filter: nothing passes through.
            expect(settled.ok, `${where}: a malformed or refused answer fails the filter`).toBe(false);
            return;
          }
          if (!settled.ok) throw settled.e;
          const t = threshold ?? FILTER_THRESHOLD;
          expect(settled.r.probabilities, where).toEqual(items.map((i) => i.p));
          expect(settled.r.kept.map((i) => i.id), `${where}: kept, in input order`).toEqual(items.filter((i) => i.p >= t).map((i) => i.id));
        },
      ),
      { numRuns: 150 },
    );
  }, 300_000);
});
