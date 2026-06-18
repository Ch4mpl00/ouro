import { describe, expect, it } from "vitest";
import { createLocalRecorderTracer } from "./local-recorder";
import { teeTracer } from "./tee";
import type {
  Generation,
  Span,
  Trace,
  Tracer,
} from "./index";
import type { StoredTraceInput, TraceStore } from "../db/trace-store";

// A fake "Langfuse" tracer that hands out deterministic ids, standing in for
// the OTel/Langfuse backend whose observation ids the local mirror must adopt.
function fakePrimary(): Tracer {
  let n = 0;
  const nextId = (p: string) => `lf-${p}-${n++}`;
  function span(id: string): Span {
    return {
      id,
      update() {},
      end() {},
      generation: () => ({ id: nextId("gen"), end() {} }),
      span: () => span(nextId("span")),
      event() {},
    };
  }
  return {
    trace() {
      const root = span("lf-trace");
      const t: Trace = {
        id: root.id,
        update() {},
        generation: (o) => root.generation(o),
        span: (o) => root.span(o),
        event() {},
        end() {},
      };
      return t;
    },
    async shutdown() {},
  };
}

function captureStore(): { store: TraceStore; written: StoredTraceInput[] } {
  const written: StoredTraceInput[] = [];
  const store: TraceStore = {
    writeTrace: (t) => void written.push(t),
    getTrace: () => null,
    listRecent: () => [],
    writeJudgement: () => {},
    listJudgements: () => [],
    listJudgedSkills: () => [],
  };
  return { store, written };
}

describe("teeTracer — child observation ids are forced onto the local mirror", () => {
  it("the local mirror records each child with the PRIMARY's observation id", () => {
    const { store, written } = captureStore();
    const tracer = teeTracer(fakePrimary(), createLocalRecorderTracer(store));

    const trace = tracer.trace({ id: "ignored", name: "signal:telegram", kind: "agent" });
    const gen: Generation = trace.generation({ name: "attempt-1", model: "m" });
    const step: Span = trace.span({ name: "step[0]:tool", kind: "tool" });
    const innerGen: Generation = step.generation({ name: "llm_compose:x", model: "m" });
    gen.end({ output: "{}" });
    innerGen.end({ output: "ok" });
    step.end({ output: "ok" });
    trace.end();

    expect(written).toHaveLength(1);
    const obs = written[0]!.observations;
    const byName = new Map(obs.map((o) => [o.name, o]));

    // Trace id is the primary's (existing behaviour).
    expect(written[0]!.id).toBe("lf-trace");
    // Every teed handle's id matches what the mirror stored — and it's the
    // primary's id, NOT a local random UUID (the orphan-score bug).
    expect(gen.id).toBe(byName.get("attempt-1")!.id);
    expect(step.id).toBe(byName.get("step[0]:tool")!.id);
    expect(innerGen.id).toBe(byName.get("llm_compose:x")!.id);
    expect(byName.get("attempt-1")!.id).toMatch(/^lf-gen-/);
    expect(byName.get("step[0]:tool")!.id).toMatch(/^lf-span-/);
    expect(byName.get("llm_compose:x")!.id).toMatch(/^lf-gen-/);
  });
});
