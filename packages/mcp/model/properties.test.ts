// What must hold of the model, for every world the calls can reach and every
// call — the six theorems the Lean model proves (Theorems.lean on
// feat/rust-rewrite), here checked by fast-check on random worlds instead of
// proved. A failure prints the smallest call sequence that breaks one.
//
//   1. a failed call changes nothing
//   2. version n means exactly n recorded patches; patch i moved the
//      document from version i to i+1
//   3. reverting the newest patch restores the body it replaced
//   4. a patch followed by its revert is a no-op on the text
//   5. append keeps the old text and contains the new
//   6. "the newest patch" and "the patch that produced the current version"
//      are the same patch, so an implementation may decide either way

import fc from "fast-check";
import { describe, expect, it } from "vitest";
import { canonical } from "./answers";
import { concretise, DOCS, step as stepArb, text, type Step } from "./calls";
import { chars, emptyWorld, isError, step, str, trim, trimEnd, type Doc, type World } from "./model";

const RUNS = Number(process.env.MODEL_CASES ?? 1000);

// Every world reachable from no documents: replay a random call sequence.
const reachable: fc.Arbitrary<Step[]> = fc.array(stepArb(DOCS.length), { maxLength: 25 });

async function reach(steps: Step[]): Promise<World> {
  let w = emptyWorld;
  for (const s of steps) [w] = step(w, (await concretise(s, w)).cmd);
  return w;
}

const docs = (w: World): [string, Doc][] => [...w];

const check = <Ts extends unknown[]>(property: fc.IAsyncPropertyWithHooks<Ts>) =>
  fc.assert(property, { numRuns: RUNS });

describe("the memory model", () => {
  it("1. a failed call changes nothing", async () => {
    await check(
      fc.asyncProperty(reachable, stepArb(DOCS.length), async (steps, s) => {
        const w = await reach(steps);
        const [after, reply] = step(w, (await concretise(s, w)).cmd);
        if (isError(reply)) expect(canonical([...after])).toBe(canonical([...w]));
      }),
    );
  });

  it("2. versions match history", async () => {
    await check(
      fc.asyncProperty(reachable, async (steps) => {
        for (const [, d] of docs(await reach(steps))) {
          expect(d.version).toBe(d.history.length);
          d.history.forEach((p, i) => expect([p.versionBefore, p.versionAfter]).toEqual([i, i + 1]));
        }
      }),
    );
  });

  it("3. reverting the newest patch restores the body it replaced", async () => {
    await check(
      fc.asyncProperty(reachable, fc.boolean(), async (steps, rollback) => {
        const w = await reach(steps);
        for (const [name, d] of docs(w)) {
          const newest = d.history.at(-1)!;
          const [after] = step(w, { op: "revert", doc: name, index: d.history.length - 1, rollback });
          expect(after.get(name)!.body).toBe(newest.bodyBefore);
        }
      }),
    );
  });

  it("4. a patch followed by its revert is a no-op on the text", async () => {
    const patchStep = stepArb(DOCS.length).filter((s) => s.op === "patch");
    await check(
      fc.asyncProperty(reachable, patchStep, async (steps, s) => {
        const w = await reach(steps);
        const call = await concretise(s, w);
        const before = w.get(call.cmd.doc);
        const [patched, reply] = step(w, call.cmd);
        fc.pre(before !== undefined && reply.kind === "written");
        const [reverted] = step(patched, { op: "revert", doc: call.cmd.doc, index: before!.history.length, rollback: false });
        expect(reverted.get(call.cmd.doc)!.body).toBe(before!.body);
      }),
    );
  });

  it("5. append keeps the old text and contains the new", async () => {
    await check(
      fc.asyncProperty(reachable, text(6).filter((t) => t !== ""), async (steps, t) => {
        const w = await reach(steps);
        for (const [name, d] of docs(w)) {
          const [after, reply] = step(w, { op: "append", doc: name, text: t });
          expect(reply.kind).toBe("written");
          const body = after.get(name)!.body;
          expect(body.startsWith(str(trimEnd(chars(d.body))))).toBe(true);
          expect(body.includes(str(trim(chars(t))))).toBe(true);
        }
      }),
    );
  });

  it("6. the newest patch is the one that produced the current version", async () => {
    await check(
      fc.asyncProperty(reachable, async (steps) => {
        for (const [, d] of docs(await reach(steps))) {
          d.history.forEach((p, i) => {
            expect(i + 1 === d.history.length).toBe(p.versionAfter === d.version);
          });
        }
      }),
    );
  });
});
