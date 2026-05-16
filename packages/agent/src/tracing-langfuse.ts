import {
  Langfuse,
  type LangfuseGenerationClient,
  type LangfuseSpanClient,
  type LangfuseTraceClient,
} from "langfuse";
import type {
  Generation,
  GenerationStartOpts,
  Span,
  SpanStartOpts,
  Trace,
  TraceContextUpdate,
  TraceStartOpts,
  Tracer,
} from "./tracing";

// Langfuse adapter. Maps our provider-agnostic Trace/Span/Generation onto
// Langfuse SDK objects. The only file in the agent that imports `langfuse`.

export function createLangfuseTracer(client: Langfuse): Tracer {
  return {
    trace(opts: TraceStartOpts): Trace {
      return wrapTrace(client.trace(opts));
    },
    async shutdown() {
      await client.shutdownAsync();
    },
  };
}

function wrapTrace(t: LangfuseTraceClient): Trace {
  return {
    update(data: TraceContextUpdate): void {
      t.update(data);
    },
    generation(opts: GenerationStartOpts): Generation {
      return wrapGeneration(t.generation({ ...opts, startTime: new Date() }));
    },
    span(opts: SpanStartOpts): Span {
      return wrapSpan(t.span({ ...opts, startTime: new Date() }));
    },
  };
}

function wrapSpan(s: LangfuseSpanClient): Span {
  return {
    update(data: TraceContextUpdate): void {
      s.update(data);
    },
    end(opts): void {
      s.end(opts);
    },
    // Spans host their own nested generations/spans — this is what enables
    // a sub-agent's iter-N generations to render inside the parent's
    // `invoke_sub_agent` span instead of in a separate top-level trace.
    generation(opts: GenerationStartOpts): Generation {
      return wrapGeneration(s.generation({ ...opts, startTime: new Date() }));
    },
    span(opts: SpanStartOpts): Span {
      return wrapSpan(s.span({ ...opts, startTime: new Date() }));
    },
  };
}

function wrapGeneration(g: LangfuseGenerationClient): Generation {
  return {
    end(end): void {
      g.end({
        output: end.output,
        level: end.level,
        statusMessage: end.statusMessage,
        usage: end.usage ? { ...end.usage, unit: "TOKENS" } : undefined,
      });
    },
  };
}

// Auto-configure from env. Returns null if LANGFUSE_*_KEY are missing — the
// caller substitutes `nullTracer`. Single source of truth for the env-var
// names lives here.
export function langfuseFromEnv(): Langfuse | null {
  const secretKey = process.env.LANGFUSE_SECRET_KEY;
  const publicKey = process.env.LANGFUSE_PUBLIC_KEY;
  const baseUrl = process.env.LANGFUSE_BASE_URL;
  if (!secretKey || !publicKey) return null;
  return new Langfuse({ secretKey, publicKey, baseUrl });
}
