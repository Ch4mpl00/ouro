import type {
  EventStartOpts,
  Generation,
  GenerationEndOpts,
  GenerationStartOpts,
  Span,
  SpanEndOpts,
  SpanStartOpts,
  Trace,
  TraceContextUpdate,
  TraceStartOpts,
  Tracer,
} from "./index";

// Fan one tracer's calls out to two backends. Named after unix `tee`: one
// input stream, two sinks. Used to mirror every run into the local store
// (../db/trace-store) while still shipping to Langfuse — the agent emits
// through ONE tracer and stays unaware there are two.
//
// The trace id comes from the PRIMARY (Langfuse = the OTel trace id) and is
// forced onto the SECONDARY (local) so the mirror keys on the same id — that
// is what lets scores written later link back to Langfuse with no mapping.
// Children (span/generation/event) get independent ids in each backend; the
// judge never cross-references child ids across systems, so no override is
// needed below the root.

function teeGeneration(a: Generation, b: Generation): Generation {
  return {
    end(opts: GenerationEndOpts): void {
      a.end(opts);
      b.end(opts);
    },
  };
}

function teeSpan(a: Span, b: Span): Span {
  return {
    update(data: TraceContextUpdate): void {
      a.update(data);
      b.update(data);
    },
    end(opts: SpanEndOpts): void {
      a.end(opts);
      b.end(opts);
    },
    generation(opts: GenerationStartOpts): Generation {
      return teeGeneration(a.generation(opts), b.generation(opts));
    },
    span(opts: SpanStartOpts): Span {
      return teeSpan(a.span(opts), b.span(opts));
    },
    event(opts: EventStartOpts): void {
      a.event(opts);
      b.event(opts);
    },
  };
}

function teeTrace(a: Trace, b: Trace): Trace {
  return {
    id: a.id,
    update(data: TraceContextUpdate): void {
      a.update(data);
      b.update(data);
    },
    generation(opts: GenerationStartOpts): Generation {
      return teeGeneration(a.generation(opts), b.generation(opts));
    },
    span(opts: SpanStartOpts): Span {
      return teeSpan(a.span(opts), b.span(opts));
    },
    event(opts: EventStartOpts): void {
      a.event(opts);
      b.event(opts);
    },
    end(): void {
      a.end();
      b.end();
    },
  };
}

export function teeTracer(primary: Tracer, secondary: Tracer): Tracer {
  return {
    trace(opts: TraceStartOpts): Trace {
      const a = primary.trace(opts);
      // Force the primary's id onto the secondary so the local mirror shares
      // the Langfuse trace id.
      const b = secondary.trace({ ...opts, id: a.id });
      return teeTrace(a, b);
    },
    async shutdown(): Promise<void> {
      await Promise.all([primary.shutdown(), secondary.shutdown()]);
    },
  };
}
