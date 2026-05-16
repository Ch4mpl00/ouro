import { Langfuse, type LangfuseTraceClient } from "langfuse";
import type {
  Generation,
  GenerationStartOpts,
  Span,
  SpanStartOpts,
  Trace,
  TraceStartOpts,
  TraceUpdate,
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
    update(data: TraceUpdate): void {
      t.update(data);
    },
    generation(opts: GenerationStartOpts): Generation {
      const g = t.generation({ ...opts, startTime: new Date() });
      return {
        end(end) {
          g.end({
            output: end.output,
            level: end.level,
            statusMessage: end.statusMessage,
            usage: end.usage ? { ...end.usage, unit: "TOKENS" } : undefined,
          });
        },
      };
    },
    span(opts: SpanStartOpts): Span {
      const s = t.span({ ...opts, startTime: new Date() });
      return {
        update(data) {
          s.update(data);
        },
        end(end) {
          s.end(end);
        },
      };
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
