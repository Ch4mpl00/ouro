import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { Engine } from "./engine";
import type { PresetName, ReasoningEffort } from "./models";
import type { Trace, TraceContext } from "./tracing";
import {
  INVOKE_SUB_AGENT_TOOL_NAME,
  SYNTHETIC_TOOLS,
  SYNTHETIC_TOOLS_BY_NAME,
  type SyntheticToolContext,
} from "./synthetic-tools";

// One agentic ReAct loop: an isolated conversation thread that owns its
// own message buffer, system prompt, model and iteration budget. Shares
// the engine's providers and MCP connection — does not create or close
// them.
//
// An AgentLoop is one of the two sibling strategies for handling a signal
// (the other being a compiled Workflow). The unit of work — handling one
// signal — is the "session" (see `sessionId`, the trace-grouping id); an
// AgentLoop is the LLM-driven loop that may run inside it. It's used in
// exactly two places: (1) an `llm_agent` workflow step (the executor
// spawns a sub-loop with a bounded tool whitelist), and (2) the
// supervisor fallback path (when the compiler can't produce a valid
// workflow, or to phrase a failure via the `recovery` skill). The default
// workflow path does NOT go through here — it runs tool / llm_compose
// steps directly against the engine.

export interface AgentLoopOpts {
  id: string;
  // Pre-assembled context that goes at the top of the system prompt (e.g.
  // the session-context block + the signal's envContext). Skills are
  // resolved separately by the engine and appended after this.
  systemPrompt?: string;
  // Per-session skills — typically the primary domain skill matching the
  // signal source (e.g. `nashdom-bill`). The engine resolves these via
  // the skill store at `startAgentLoop` time. Missing here is a hard
  // error — the caller decides whether to skip the signal. Engine-level
  // meta-skill (`routing`) comes from `EngineDeps.skills` and is added
  // on top unless `includeEngineSkills: false`.
  skills?: string[];
  // Pre-resolved skill contents (name → markdown). Set by the engine
  // after skill resolution; the loop uses these to (a) compose the actual
  // system message sent to the LLM and (b) expose each skill separately
  // on `trace.metadata.skills` so the tracing UI isn't flooded with skill
  // text in every generation's input.
  resolvedSkills?: Record<string, string>;
  // Union of `tools:` from every loaded skill's frontmatter. Set by the
  // engine; used to filter `mcp.tools` per LLM call so the model only
  // sees tools relevant to this session's skills. Synthetic agent-side
  // tools are NOT affected — they stay always-available regardless of
  // frontmatter. `null` means "no filter, all MCP tools available" (used
  // when any loaded skill has `tools: *`).
  allowedTools?: Set<string> | null;
  // Caller-side narrowing of the effective tool set, intersected with
  // the engine-resolved `allowedTools` from skills. Used by the workflow
  // executor to enforce a per-step `llm_agent` tool whitelist on top of
  // what the skill already allows. Engine applies the intersection at
  // `startAgentLoop` time; callers don't touch `allowedTools` directly.
  toolWhitelist?: Set<string>;
  // Opt out of engine-level meta-skill (`routing`) for this session.
  // Default true. Sub-agents set this to false so they get only the
  // focused per-task skill set without the always-on parent extras.
  includeEngineSkills?: boolean;
  // Named entry from the engine's preset registry (see `./models.ts`).
  // Resolves to a concrete model + reasoning_effort pair at session
  // start. Default is "base" (cheap chat). Sub-agents that do real
  // editorial / parsing work pass "smart".
  preset?: PresetName;
  maxIterations?: number;
  parentId?: string;
  // Optional trace metadata. `tags` show up as filter chips in the UI
  // (use for `signal.source` so you can slice by domain); `metadata` is
  // freeform key/value (use for `signal.id`, watermarks, anything you'd
  // grep traces for later). No-op when the engine's tracer is the
  // null tracer.
  tags?: string[];
  metadata?: Record<string, unknown>;
  // Trace-grouping session id. Traces sharing a sessionId group together in
  // the tracing UI's "Sessions" view. We use `${signal.source}:${signal.id}`
  // for the primary session AND its recovery — so a crashed run and its
  // user-facing error report end up side-by-side under one session.
  sessionId?: string;
  // Pre-created trace scope from a caller. When present, this session
  // nests its generations and tool spans inside the given scope instead
  // of creating a new top-level trace. Used for sub-agents — the parent's
  // `invoke_sub_agent` tool span IS the child's scope, so a sub-agent's
  // iters/tool calls render under the parent's trace in the UI. Omit for
  // top-level sessions (primary + recovery).
  traceScope?: TraceContext;
}

// Public surface of one loop. Consumers (engine registry, workflow
// executor, fallback, invoke_sub_agent) speak only this interface.
export interface AgentLoop {
  readonly id: string;
  readonly parentId?: string;
  readonly sessionId?: string;
  readonly messages: ChatCompletionMessageParam[];
  // Resolved preset name + concrete (model, reasoningEffort) it expanded
  // to. Exposed so engine logging and trace metadata can show what the
  // session is actually running with.
  readonly preset: PresetName;
  readonly model: string;
  readonly reasoningEffort: ReasoningEffort;
  send(userText: string): Promise<string>;
  // Run the loop with whatever is currently in `messages`. Useful when
  // the caller pre-loaded history and just wants the LLM to react.
  run(): Promise<string>;
  close(): void;
}

const DEFAULT_MAX_ITERATIONS = 100;

export function createAgentLoop(engine: Engine, opts: AgentLoopOpts): AgentLoop {
  const { id, parentId, sessionId } = opts;
  const messages: ChatCompletionMessageParam[] = [];
  // Pick a preset from the engine registry. `base` (cheap chat,
  // non-thinking) is the default — most signals stay here. Callers
  // pass "smart" for sub-agents that do real editorial / parsing work.
  const presetName: PresetName = opts.preset ?? "base";
  const preset = engine.presets[presetName];
  const model = preset.model;
  const reasoningEffort = preset.reasoningEffort;
  const maxIterations = opts.maxIterations ?? DEFAULT_MAX_ITERATIONS;
  const allowedTools: ReadonlySet<string> | null = opts.allowedTools ?? null;
  let subAgentCounter = 0;
  let closed = false;

  // Compose the actual system message sent to the LLM: caller's prompt
  // first, then each resolved skill body, joined with `---`. Skills
  // additionally land in `trace.metadata.skills` as a name list so the
  // tracing UI can present them structured instead of as one giant blob
  // inside every generation's input.
  const skillsMap = opts.resolvedSkills ?? {};
  const systemParts: string[] = [];
  if (opts.systemPrompt) systemParts.push(opts.systemPrompt);
  for (const content of Object.values(skillsMap)) systemParts.push(content);
  const combinedSystem = systemParts.join("\n\n---\n\n");
  if (combinedSystem.length > 0) {
    messages.push({ role: "system", content: combinedSystem });
  }

  // Set up the trace scope. Two paths:
  //   - top-level: create a new Trace (and own its input/output/metadata).
  //   - sub-agent: reuse the parent's `invoke_sub_agent` span so all the
  //     child's iter generations + tool spans render nested inside the
  //     parent's trace. We don't take ownership of the scope's
  //     input/output — those describe the tool call from the parent's
  //     POV — but we annotate it with sub-agent identity & config so the
  //     UI can show what was loaded into the child.
  let scope: TraceContext;
  // Non-null when this loop owns its trace scope (top-level). Used as
  // both a "may touch input/output" gate and the handle for explicitly
  // ending the root trace span at end-of-run (v5/OTel needs it).
  let trace: Trace | null;
  if (opts.traceScope) {
    scope = opts.traceScope;
    trace = null;
    // Annotate the parent's `invoke_sub_agent` span with sub-agent
    // identity. Skills go in as NAMES, not bodies — metadata is for
    // short K/V; the full skill text already lives in this session's
    // first generation.input (system message), where it belongs.
    scope.update({
      metadata: {
        sub_agent_id: id,
        sub_agent_skills: Object.keys(skillsMap),
        sub_agent_preset: presetName,
        sub_agent_model: model,
        sub_agent_reasoning_effort: reasoningEffort,
        sub_agent_max_iterations: maxIterations,
      },
    });
  } else {
    // Generations + tool spans get attached to this trace inside
    // `runUntilSettled`. `trace.input` is intentionally NOT set here —
    // it's populated in `runUntilSettled` from the first user message so
    // the Session-replay UI shows a clean `user → assistant` exchange
    // instead of the long system prompt. Trace metadata = short
    // key-value filtering fields only; the big strings (system prompt,
    // skill bodies) live inside generation.input where they belong —
    // Langfuse's propagated metadata caps values at 200 chars.
    trace = engine.tracer.trace({
      id,
      name: id,
      kind: "agent",
      sessionId,
      tags: opts.tags,
      metadata: {
        ...opts.metadata,
        agent_id: id,
        skills: Object.keys(skillsMap),
        preset: presetName,
        model,
        reasoning_effort: reasoningEffort,
        max_iterations: maxIterations,
        ...(parentId ? { parent_id: parentId } : {}),
      },
    });
    scope = trace;
  }

  // Identity stamp written into every observation this loop creates
  // (iter generations + tool spans). Without it, sub-agent observations
  // are visually indistinguishable from the parent's in the UI — the
  // observation pane shows only trace.metadata, and that's the parent's.
  const observationMeta: Record<string, unknown> = parentId
    ? { agent_id: id, parent_id: parentId }
    : { agent_id: id };

  // Narrow dependency surface handed to synthetic-tool handlers — the
  // contract is the signature, not "whatever the loop has".
  const toolCtx: SyntheticToolContext = {
    loopId: id,
    parentId,
    sessionId,
    log: (...parts) => engine.log(id, ...parts),
    skillStore: engine.skillStore,
    memory: engine.memory,
    startAgentLoop: (o) => engine.startAgentLoop(o),
    endAgentLoop: (loopId) => engine.endAgentLoop(loopId),
    // `__sub` (double underscore) keeps the id ASCII-only and unique
    // (used for log lines and metadata; not a trace id — the sub-agent
    // renders inside the parent's `invoke_sub_agent` span).
    allocSubAgentId: () => `${id}__sub${++subAgentCounter}`,
  };

  async function runUntilSettled(): Promise<string> {
    const { mcp } = engine;
    // Resolve provider once per run. Model is fixed for the loop's
    // lifetime, so the provider does not change across iterations either.
    const provider = engine.resolveProvider(model);

    // Surface the user's prompt on the trace so the Session-replay UI
    // renders a real `user → assistant` exchange. Done here (not at
    // construction) because the caller pushes the user message AFTER
    // startAgentLoop returns. Skip for sub-agents — their scope is the
    // parent's tool span and the input there is the tool args, not the
    // user message; overwriting would erase the parent's view.
    if (trace) {
      const firstUserMessage = messages.find((m) => m.role === "user");
      if (firstUserMessage) {
        scope.update({ input: firstUserMessage.content });
      }
    }

    try {
      for (let i = 0; i < maxIterations; i++) {
        // MCP tools filtered by the per-session allow-list (union of
        // loaded skills' frontmatter). Synthetic agent-side tools are
        // not gated by skill frontmatter — they're cheap and universal
        // (invoke_sub_agent stays parent-only via its own `visibleTo`).
        const mcpTools = allowedTools === null
          ? mcp.tools
          : mcp.tools.filter((t) => allowedTools.has(t.function.name));
        const tools = [
          ...mcpTools,
          ...SYNTHETIC_TOOLS.filter((t) => t.visibleTo?.(toolCtx) ?? true).map(
            (t) => t.def,
          ),
        ];

        // Generation span = one LLM call. Input is the full messages
        // array — that's the actual LLM input and the right place for it
        // (Langfuse UI collapses long content). metadata stays a short
        // K/V marker (agent_id) for filtering only.
        const generation = scope.generation({
          name: `iter-${i}`,
          model,
          modelParameters: {
            reasoning_effort: reasoningEffort,
            thinking: reasoningEffort === "disabled" ? "disabled" : "enabled",
          },
          input: messages,
          metadata: observationMeta,
        });

        // The provider wrapper owns request shaping (thinking /
        // reasoning_effort, history repair) and usage normalization —
        // the loop stays provider-agnostic.
        let result;
        try {
          result = await provider.complete({
            model,
            messages,
            reasoningEffort,
            tools,
            // Retry attempts (withRetry decorator) land as WARNING events
            // on this session's scope, next to the iter generations.
            trace: scope,
          });
        } catch (err) {
          generation.end({
            output: { error: (err as Error).message },
            level: "ERROR",
            statusMessage: (err as Error).message,
          });
          throw err;
        }

        const { message } = result;
        messages.push(message);

        generation.end({ output: message, usage: result.usage });

        engine.log(id, `iter ${i} finish=${result.finishReason} tool_calls=${message.tool_calls?.length ?? 0}`);

        if (!message.tool_calls?.length) {
          if (trace) {
            scope.update({ output: message.content ?? "" });
          }
          return message.content ?? "";
        }

        // Dispatch all tool calls in this round in parallel. The model
        // expects parallel-tool-call semantics — if it emits 3 tool_calls,
        // running them sequentially adds N×latency for no reason. Results
        // are still pushed in the original tool_calls order so the
        // message buffer is deterministic regardless of completion order.
        const toolResults = await Promise.all(
          message.tool_calls.map(async (call) => {
            // Span per tool call. We open it BEFORE parsing args so a
            // malformed-JSON case still leaves a measurable, attributed
            // span in the trace.
            const span = scope.span({
              name: call.function.name,
              // `invoke_sub_agent` spawns a whole sub-session inside this
              // span (its iters/tool calls nest here), so badge it as an
              // agent; every other call is a plain tool invocation.
              kind:
                call.function.name === INVOKE_SUB_AGENT_TOOL_NAME ? "agent" : "tool",
              input: { raw_arguments: call.function.arguments },
              metadata: observationMeta,
            });

            // Malformed JSON in the arguments is the MODEL's mistake, not an
            // infra failure — feed it back as a tool result so the model can
            // correct itself, instead of crashing the whole session (which
            // would also leave the sibling tool_calls in this round without
            // tool messages, corrupting the buffer for good).
            let args: Record<string, unknown>;
            try {
              args = JSON.parse(call.function.arguments || "{}") as Record<string, unknown>;
            } catch (err) {
              const msg = `[tool error] invalid JSON in arguments: ${(err as Error).message}`;
              span.end({ output: { error: msg }, level: "ERROR", statusMessage: msg });
              return { call, result: msg };
            }

            let result: string;
            try {
              span.update({ input: args });
              engine.log(id, `→ ${call.function.name}(${JSON.stringify(args)})`);

              const synthetic = SYNTHETIC_TOOLS_BY_NAME.get(call.function.name);
              if (synthetic) {
                // Pass the span — `invoke_sub_agent` reuses it as the child
                // loop's trace scope so the sub-agent's iters render nested
                // here in the UI.
                result = await synthetic.run(toolCtx, args, span);
              } else {
                result = await mcp.callTool(call.function.name, args);
              }
            } catch (err) {
              span.end({
                output: { error: (err as Error).message },
                level: "ERROR",
                statusMessage: (err as Error).message,
              });
              throw err;
            }

            span.end({ output: result });
            engine.log(id, `← ${call.function.name} ${result.length}b: ${result}`);

            return { call, result };
          }),
        );

        for (const { call, result } of toolResults) {
          messages.push({
            role: "tool",
            tool_call_id: call.id,
            content: result,
          });
        }
      }

      throw new Error(`session ${id} exceeded maxIterations=${maxIterations}`);
    } catch (err) {
      // Trace-level error marker. Individual generation/span errors are
      // already attributed above; this surfaces failed sessions in the
      // tracing list view. Sub-agents skip this — the parent's
      // `invoke_sub_agent` span captures the error via its own
      // `.end({ level: ERROR })` once the exception propagates back.
      if (trace) {
        scope.update({
          output: { error: (err as Error).message },
          metadata: { error: true },
        });
      }
      throw err;
    } finally {
      // Close the root trace span for top-level sessions. v5/OTel keeps
      // a trace "open" until its root span is explicitly ended, even after
      // all child observations have closed. Sub-agents skip this — the
      // parent's dispatch loop ends the wrapping span.
      trace?.end();
    }
  }

  return {
    id,
    parentId,
    sessionId,
    messages,
    preset: presetName,
    model,
    reasoningEffort,
    async send(userText) {
      messages.push({ role: "user", content: userText });
      return this.run();
    },
    async run() {
      if (closed) throw new Error(`session ${id} is closed`);
      return runUntilSettled();
    },
    close() {
      closed = true;
    },
  };
}
