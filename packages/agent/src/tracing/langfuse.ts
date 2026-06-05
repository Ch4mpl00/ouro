import { LangfuseSpanProcessor } from "@langfuse/otel";
import {
  LangfuseOtelSpanAttributes,
  startObservation,
  type LangfuseGeneration,
  type LangfuseGenerationAttributes,
  type LangfuseSpan,
  type LangfuseSpanAttributes,
} from "@langfuse/tracing";
import { NodeTracerProvider } from "@opentelemetry/sdk-trace-node";
import type {
  EventStartOpts,
  Generation,
  GenerationEndOpts,
  GenerationStartOpts,
  Span,
  SpanEndOpts,
  SpanKind,
  SpanStartOpts,
  Trace,
  TraceContextUpdate,
  TraceStartOpts,
  Tracer,
} from "./index";

// Langfuse v5 adapter. Built on OpenTelemetry — the LangfuseSpanProcessor
// reads spans created via @langfuse/tracing's `startObservation` chain and
// ships them to Langfuse Cloud. v5 sends the `x-langfuse-ingestion-version: 4`
// header which moves traces into the fast-ingestion lane (sub-second UI
// freshness), unlike v3 which had a 5-10min compatibility-tier delay.
//
// We use the imperative `startObservation` / `parent.startObservation(...)`
// chain rather than the callback patterns (`startActiveObservation`,
// `propagateAttributes`) so Session keeps its current shape: open scope,
// open children, close. Children are always created via explicit parent —
// no OTel active-context juggling needed.

interface LangfuseTracerConfig {
  publicKey: string;
  secretKey: string;
  baseUrl?: string;
}

export function createLangfuseTracer(config: LangfuseTracerConfig): Tracer {
  const processor = new LangfuseSpanProcessor({
    publicKey: config.publicKey,
    secretKey: config.secretKey,
    baseUrl: config.baseUrl,
  });
  const provider = new NodeTracerProvider({ spanProcessors: [processor] });
  provider.register();

  return {
    trace(opts: TraceStartOpts): Trace {
      // No active OTel context here → the new observation becomes a root
      // span, which Langfuse renders as a top-level trace. `kind` badges
      // the whole trace (e.g. "agent" for a signal-handling session).
      const root = startRootObservation(opts.name, opts.metadata, opts.kind);
      // Trace-level attributes (sessionId, tags, traceName) live on
      // well-known OTel attribute keys that LangfuseSpanProcessor reads
      // off the root span. Children created via `root.startObservation`
      // pick up the trace association from the OTel parent link.
      const otel = root.otelSpan;
      otel.setAttribute(LangfuseOtelSpanAttributes.TRACE_NAME, opts.name);
      if (opts.sessionId !== undefined) {
        otel.setAttribute(LangfuseOtelSpanAttributes.TRACE_SESSION_ID, opts.sessionId);
      }
      if (opts.tags && opts.tags.length > 0) {
        otel.setAttribute(LangfuseOtelSpanAttributes.TRACE_TAGS, opts.tags);
      }
      return wrapTrace(root);
    },
    async shutdown(): Promise<void> {
      // forceFlush ships any buffered spans; provider.shutdown closes
      // the OTel pipeline. Both are required to avoid dropping the
      // final batch on SIGTERM.
      await processor.forceFlush();
      await provider.shutdown();
    },
  };
}

// ─── wrappers ────────────────────────────────────────────────────────

function applyContextUpdate(s: LangfuseSpan, data: TraceContextUpdate): void {
  const patch: Record<string, unknown> = {};
  if (data.input !== undefined) patch.input = data.input;
  if (data.output !== undefined) patch.output = data.output;
  if (data.metadata !== undefined) patch.metadata = data.metadata;
  if (Object.keys(patch).length > 0) s.update(patch);
}

function startGenerationChild(parent: LangfuseSpan, opts: GenerationStartOpts): LangfuseGeneration {
  const attrs: LangfuseGenerationAttributes = {
    input: opts.input,
    model: opts.model,
    modelParameters: opts.modelParameters,
    metadata: opts.metadata,
  };
  return parent.startObservation(opts.name, attrs, { asType: "generation" });
}

// Root-level counterpart of startSpanChild: opens a top-level observation
// (which Langfuse renders as a trace) with the requested kind. No active
// OTel parent context → it becomes a root span.
function startRootObservation(
  name: string,
  metadata: Record<string, unknown> | undefined,
  kind: SpanKind | undefined,
): LangfuseSpan {
  switch (kind) {
    case "tool":
      return startObservation(name, { metadata }, { asType: "tool" });
    case "agent":
      return startObservation(name, { metadata }, { asType: "agent" });
    case "chain":
      return startObservation(name, { metadata }, { asType: "chain" });
    default:
      return startObservation(name, { metadata });
  }
}

// Map our backend-agnostic SpanKind to Langfuse's `asType`. The `tool` /
// `agent` / `chain` observation classes are structurally identical to
// LangfuseSpan (their attribute type IS LangfuseSpanAttributes), differing
// only in the UI badge — so they wrap with the same LangfuseSpan logic.
// The literal `asType` is required for overload resolution; a switch keeps
// it literal without casting a dynamic string.
function startSpanChild(parent: LangfuseSpan, opts: SpanStartOpts): LangfuseSpan {
  const attrs: LangfuseSpanAttributes = { input: opts.input, metadata: opts.metadata };
  switch (opts.kind) {
    case "tool":
      return parent.startObservation(opts.name, attrs, { asType: "tool" });
    case "agent":
      return parent.startObservation(opts.name, attrs, { asType: "agent" });
    case "chain":
      return parent.startObservation(opts.name, attrs, { asType: "chain" });
    default:
      return parent.startObservation(opts.name, attrs, { asType: "span" });
  }
}

// Point-in-time marker. Langfuse auto-ends `event` observations, so there
// is nothing to close and no handle to return.
function startEventChild(parent: LangfuseSpan, opts: EventStartOpts): void {
  parent.startObservation(
    opts.name,
    { input: opts.input, metadata: opts.metadata, level: opts.level },
    { asType: "event" },
  );
}

function wrapTrace(s: LangfuseSpan): Trace {
  return {
    update(data: TraceContextUpdate): void {
      applyContextUpdate(s, data);
    },
    generation(opts: GenerationStartOpts): Generation {
      return wrapGeneration(startGenerationChild(s, opts));
    },
    span(opts: SpanStartOpts): Span {
      return wrapSpan(startSpanChild(s, opts));
    },
    event(opts: EventStartOpts): void {
      startEventChild(s, opts);
    },
    end(): void {
      s.end();
    },
  };
}

function wrapSpan(s: LangfuseSpan): Span {
  return {
    update(data: TraceContextUpdate): void {
      applyContextUpdate(s, data);
    },
    end(opts: SpanEndOpts): void {
      const patch: Record<string, unknown> = {};
      if (opts.output !== undefined) patch.output = opts.output;
      if (opts.level !== undefined) patch.level = opts.level;
      if (opts.statusMessage !== undefined) patch.statusMessage = opts.statusMessage;
      if (Object.keys(patch).length > 0) s.update(patch);
      s.end();
    },
    generation(opts: GenerationStartOpts): Generation {
      return wrapGeneration(startGenerationChild(s, opts));
    },
    span(opts: SpanStartOpts): Span {
      return wrapSpan(startSpanChild(s, opts));
    },
    event(opts: EventStartOpts): void {
      startEventChild(s, opts);
    },
  };
}

function wrapGeneration(g: LangfuseGeneration): Generation {
  return {
    end(opts: GenerationEndOpts): void {
      const patch: Record<string, unknown> = {};
      if (opts.output !== undefined) patch.output = opts.output;
      if (opts.level !== undefined) patch.level = opts.level;
      if (opts.statusMessage !== undefined) patch.statusMessage = opts.statusMessage;
      if (opts.usage) {
        patch.usageDetails = {
          input: opts.usage.input,
          output: opts.usage.output,
          total: opts.usage.total,
          // Langfuse renders extra usageDetails keys as-is — show the
          // cached-input portion when the provider reported it.
          ...(opts.usage.cached !== undefined ? { cached: opts.usage.cached } : {}),
        };
      }
      if (Object.keys(patch).length > 0) g.update(patch);
      g.end();
    },
  };
}

// Auto-configure from env. Returns null if keys missing — the caller
// substitutes `nullTracer`.
export function langfuseTracerFromEnv(): Tracer | null {
  const secretKey = process.env.LANGFUSE_SECRET_KEY;
  const publicKey = process.env.LANGFUSE_PUBLIC_KEY;
  const baseUrl = process.env.LANGFUSE_BASE_URL;
  if (!secretKey || !publicKey) return null;
  return createLangfuseTracer({ secretKey, publicKey, baseUrl });
}
