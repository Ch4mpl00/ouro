// list_monobank_transactions over MCP against a stand-in for the Monobank
// Personal API (undici MockAgent behind the global fetch), playing its
// documented rules: a statement covers at most 31 days + 1 hour, comes
// newest first, holds at most 500 transactions (more need another request
// with an earlier `to`), and an account may be asked once per 60 s.
//
// Rules, from the account's history and the tool's own window:
//   - every transaction it returns is one of the account's, inside the
//     window, newest first, money in major units with its currency;
//   - when it says the list is complete, it is exactly the window's
//     transactions — and it never says so when Monobank cut the list at 500;
//   - asking again within 60 s is a tool error that names the rate limit.

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import fc from "fast-check";
import { Agent, MockAgent, setGlobalDispatcher } from "undici";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { registerMonobankTools } from "../src/tools/monobank";
import { call } from "../specs/support/environment";

const TOKEN = "test-monobank-token";
const PAGE = 500;
const MAX_RANGE_S = 31 * 24 * 3600 + 3600;

interface Item {
  id: string;
  time: number;
  description: string;
  mcc: number;
  originalMcc: number;
  hold: boolean;
  amount: number;
  operationAmount: number;
  currencyCode: number;
  commissionRate: number;
  cashbackAmount: number;
  balance: number;
}

// Monobank, as far as the tool can tell. Its rate limit runs on a clock the
// test advances, so "a minute later" costs nothing.
function fakeMonobank() {
  const accounts = new Map<string, Item[]>();
  const lastAsked = new Map<string, number>();
  const requests: { account: string; from: number; to: number }[] = [];
  const clock = { now: 0 };
  const json = (statusCode: number, body: unknown) => ({
    statusCode,
    data: JSON.stringify(body),
    responseOptions: { headers: { "content-type": "application/json" } },
  });
  const agent = new MockAgent();
  agent.disableNetConnect();
  setGlobalDispatcher(agent);
  agent
    .get("https://api.monobank.ua")
    .intercept({ path: (p) => p.startsWith("/personal/statement/"), method: "GET" })
    .reply((req) => {
      const headers = new Headers(Object.entries(req.headers ?? {}).map(([k, v]) => [k, String(v)]));
      if (headers.get("x-token") !== TOKEN) return json(403, { errorDescription: "Unknown 'X-Token'" });
      const [, , , rawAccount, rawFrom, rawTo] = req.path.split("/");
      const account = decodeURIComponent(rawAccount ?? "");
      const from = Number(rawFrom);
      const to = Number(rawTo ?? Math.floor(Date.now() / 1000));
      const history = accounts.get(account);
      if (!history) return json(400, { errorDescription: "Account not found" });
      if (!Number.isInteger(from) || !Number.isInteger(to) || to < from || to - from > MAX_RANGE_S) {
        return json(400, { errorDescription: "Period must be no more than 31 days" });
      }
      const last = lastAsked.get(account);
      if (last !== undefined && clock.now - last < 60) return json(429, { errorDescription: "Too many requests" });
      lastAsked.set(account, clock.now);
      requests.push({ account, from, to });
      const items = history.filter((i) => i.time >= from && i.time <= to).sort((a, b) => b.time - a.time).slice(0, PAGE);
      return json(200, items);
    })
    .persist();
  return {
    accounts,
    requests,
    clock,
    close: async () => {
      await agent.close();
      setGlobalDispatcher(new Agent());
    },
  };
}

const CURRENCIES: Record<number, string> = { 980: "UAH", 840: "USD", 978: "EUR" };

let fake: ReturnType<typeof fakeMonobank>;
let client: Client;

beforeAll(async () => {
  fake = fakeMonobank();
  process.env.MONOBANK_API_KEY = TOKEN;
  const server = new McpServer({ name: "monobank", version: "0" });
  registerMonobankTools(server);
  const [ct, st] = InMemoryTransport.createLinkedPair();
  await server.connect(st);
  client = new Client({ name: "t", version: "0" });
  await client.connect(ct);
});
afterAll(async () => {
  await client?.close();
  await fake?.close();
  delete process.env.MONOBANK_API_KEY;
});

// An account's history: how many transactions, spread over how many days
// back from now (some older than any window, some in the future of none).
const history = fc.record({
  count: fc.oneof({ weight: 3, arbitrary: fc.integer({ min: 0, max: 40 }) }, { weight: 1, arbitrary: fc.integer({ min: 480, max: 800 }) }),
  spanDays: fc.integer({ min: 1, max: 45 }),
  seed: fc.nat(),
});

type Ask = { days: number | undefined; wait: boolean };
const ask = fc.record({
  days: fc.option(fc.integer({ min: 1, max: 31 }), { nil: undefined }),
  // Usually a minute after the previous request; sometimes right away.
  wait: fc.oneof({ weight: 4, arbitrary: fc.constant(true) }, { weight: 1, arbitrary: fc.constant(false) }),
});

describe("list_monobank_transactions", () => {
  it("returns the window's transactions, says when Monobank cut them short, and surfaces the rate limit", async () => {
    const witnessed = new Set<string>();
    let run = 0;
    await fc.assert(
      fc.asyncProperty(history, fc.array(ask, { minLength: 1, maxLength: 4 }), async (h, asks) => {
        const account = `acc-${++run}`;
        const nowS = Math.floor(Date.now() / 1000);
        const items: Item[] = Array.from({ length: h.count }, (_, i) => {
          const code = [980, 980, 840, 978][(h.seed + i) % 4]!;
          return {
            id: `${account}-tx${i}`,
            time: nowS - Math.floor(((i * 7919 + h.seed) % (h.spanDays * 86400)) + 1),
            description: `Покупка ${i}`,
            mcc: 5411,
            originalMcc: 5411,
            hold: false,
            amount: -((i * 37 + h.seed) % 100_000) - 1,
            operationAmount: -((i * 37 + h.seed) % 100_000) - 1,
            currencyCode: code,
            commissionRate: 0,
            cashbackAmount: i % 3,
            balance: 1_000_000,
          };
        });
        fake.accounts.set(account, items);
        for (const a of asks) {
          fake.clock.now += a.wait ? 61 : 1;
          const args: Record<string, unknown> = { accountId: account };
          if (a.days !== undefined) args.days = a.days;
          const out = await call(client, "list_monobank_transactions", args);
          const where = `${JSON.stringify({ count: h.count, spanDays: h.spanDays, asks })}`;
          if (!a.wait && fake.requests.some((r) => r.account === account)) {
            const lastOk = fake.requests.filter((r) => r.account === account).length;
            if (out.kind !== "ok") {
              witnessed.add("rate limited");
              expect(out.kind, `${where}: a 429 is a tool error`).toBe("toolError");
              expect(out.kind === "toolError" ? out.message : "", where).toMatch(/rate limit/i);
              continue;
            }
            expect(lastOk, where).toBeGreaterThan(0);
          }
          expect(out.kind, `${where}: ${JSON.stringify(out).slice(0, 300)}`).toBe("ok");
          const value: unknown = out.kind === "ok" ? out.value : null;
          const fromS = Math.floor(Date.parse(String(Reflect.get(Object(value), "from"))) / 1000);
          const toS = Math.floor(Date.parse(String(Reflect.get(Object(value), "to"))) / 1000);
          const got = Reflect.get(Object(value), "transactions");
          if (!Array.isArray(got)) throw new Error(`${where}: no transaction list`);
          const inWindow = items.filter((i) => i.time >= fromS && i.time <= toS).sort((x, y) => y.time - x.time);
          expect(toS - fromS, `${where}: the window is the days asked for`).toBe((a.days ?? 7) * 86400);

          // Each returned transaction is the account's own, rendered right,
          // newest first.
          const ids = got.map((t) => String(Reflect.get(Object(t), "id")));
          expect(ids, `${where}: newest first, from the window`).toEqual(inWindow.slice(0, ids.length).map((i) => i.id));
          for (const t of got) {
            const src = items.find((i) => i.id === Reflect.get(Object(t), "id"))!;
            expect(Reflect.get(Object(t), "amount"), where).toBe(src.amount / 100);
            expect(Reflect.get(Object(t), "currency"), where).toBe(CURRENCIES[src.currencyCode]);
            expect(Reflect.get(Object(t), "time"), where).toBe(new Date(src.time * 1000).toISOString());
          }
          // Complete means complete.
          const complete = Reflect.get(Object(value), "complete");
          if (inWindow.length >= PAGE) {
            witnessed.add("more than a page in the window");
            expect(complete, `${where}: ${inWindow.length} in the window, Monobank sent ${PAGE} — not complete`).toBe(false);
          }
          if (complete === true) expect(ids.length, `${where}: complete means all of them`).toBe(inWindow.length);
          else expect(inWindow.length, `${where}: only a cut list is incomplete`).toBeGreaterThanOrEqual(PAGE);
        }
      }),
      { numRuns: Number(process.env.PROPERTY_RUNS ?? 80) },
    );
    expect([...witnessed].sort()).toEqual(["more than a page in the window", "rate limited"]);
  });
});
