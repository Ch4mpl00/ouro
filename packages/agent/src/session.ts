import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { Engine } from "./engine";
import { setMemory } from "./db/memory";
import type { PresetName, ReasoningEffort } from "./models";
import { listSkills, readSkill, saveSkill } from "./skills";
import type { Trace, TraceContext } from "./tracing";
import {
  SYNTHETIC_TOOLS,
  SYNTHETIC_TOOLS_BY_NAME,
  type InvokeSubAgentArgs,
  type SetMemoryArgs,
  type SkillNameArg,
  type WriteSkillArgs,
} from "./synthetic-tools";

// One isolated conversation thread. Owns its own message buffer, system
// prompt, model and iteration budget. Shares the engine's OpenAI client
// and MCP connection — does not create or close them.
//
// Role in the post-workflow architecture: this is the agentic ReAct body,
// used in exactly two places — (1) an `llm_agent` workflow step (the
// executor spawns a sub-session with a bounded tool whitelist), and (2)
// the supervisor fallback path (when the compiler can't produce a valid
// workflow, or to phrase a failure via the `recovery` skill). The default
// workflow path does NOT go through here — it runs tool / llm_compose
// steps directly against the engine.

export interface SessionOpts {
  id: string;
  // Pre-assembled context that goes at the top of the system prompt (e.g.
  // the session-context block + the signal's envContext). Skills are
  // resolved separately by the engine and appended after this.
  systemPrompt?: string;
  // Per-session skills — typically the primary domain skill matching the
  // signal source (e.g. `nashdom-bill`). The engine resolves these via
  // `readSkill` at `startSession` time. Missing here is a hard error —
  // the caller decides whether to skip the signal. Engine-level
  // meta-skill (`routing`) comes from `EngineOpts.skills` and is added
  // on top unless `includeEngineSkills: false`.
  skills?: string[];
  // Pre-resolved skill contents (name → markdown). Set by the engine
  // after readSkill; Session uses these to (a) compose the actual system
  // message sent to the LLM and (b) expose each skill separately on
  // `trace.metadata.skills` so the tracing UI isn't flooded with skill
  // text in every generation's input.
  resolvedSkills?: Record<string, string>;
  // Union of `tools:` from every loaded skill's frontmatter. Set by the
  // engine; Session uses it to filter `mcp.tools` per LLM call so the
  // model only sees tools relevant to this session's skills. Synthetic
  // agent-side tools are NOT affected — they stay always-available
  // regardless of frontmatter. `null` means "no filter, all MCP tools
  // available" (used when any loaded skill has `tools: *`).
  allowedTools?: Set<string> | null;
  // Caller-side narrowing of the effective tool set, intersected with
  // the engine-resolved `allowedTools` from skills. Used by the workflow
  // executor to enforce a per-step `llm_agent` tool whitelist on top of
  // what the skill already allows. Engine applies the intersection at
  // `startSession` time; callers don't touch `allowedTools` directly.
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

const DEFAULT_MAX_ITERATIONS = 100;


// DeepSeek extends OpenAI's assistant message shape with `reasoning_content`
// (the thinking text). Required in the request history whenever the next
// call uses thinking-mode — even if it's empty.
type DeepSeekAssistantHistory = ChatCompletionMessageParam & {
  reasoning_content?: string;
};

function ensureReasoningContentOnHistory(messages: ChatCompletionMessageParam[]): void {
  for (const m of messages) {
    if (m.role !== "assistant") continue;
    const extended = m as DeepSeekAssistantHistory;
    if (extended.reasoning_content === undefined) {
      extended.reasoning_content = "";
    }
  }
}

export class Session {
  readonly id: string;
  readonly parentId?: string;
  // Trace-grouping session id. Stored so spawned sub-agents can inherit it
  // (their child trace lands in the same group as the parent's).
  readonly sessionId?: string;
  readonly messages: ChatCompletionMessageParam[] = [];
  // Resolved preset name + concrete (model, reasoningEffort) it expanded
  // to. Both exposed so engine logging and trace metadata can show what
  // the session is actually running with.
  readonly preset: PresetName;
  readonly model: string;
  readonly reasoningEffort: ReasoningEffort;
  private readonly engine: Engine;
  private readonly maxIterations: number;
  // Trace surface for this session. For top-level sessions this is a Trace
  // created from `engine.tracer.trace(...)`; for sub-agents it's the parent's
  // `invoke_sub_agent` span passed in via `opts.traceScope`. The Session
  // API is the same either way — both implement TraceContext.
  private readonly scope: TraceContext;
  // Non-null when this session owns its trace scope (top-level). Used as
  // both a "may touch input/output" gate and the handle for explicitly
  // ending the root trace span at end-of-run (v5/OTel needs it; v3 was
  // implicit). For sub-agents this is null — the parent's dispatch loop
  // owns the wrapping span's lifecycle.
  private readonly trace: Trace | null;
  // MCP tool allow-list, derived from union of loaded skills' frontmatter.
  // null = legacy "all tools" fallback (used when caller skipped skills
  // entirely; not expected in current call sites but kept as a sane default).
  private readonly allowedTools: ReadonlySet<string> | null;
  private subAgentCounter = 0;
  private closed = false;

  constructor(engine: Engine, opts: SessionOpts) {
    this.engine = engine;
    this.id = opts.id;
    this.parentId = opts.parentId;
    this.sessionId = opts.sessionId;
    // Pick a preset from the engine registry. `base` (cheap chat,
    // non-thinking) is the default — most signals stay here. Callers
    // pass "smart" for sub-agents that do real editorial / parsing work.
    this.preset = opts.preset ?? "base";
    const preset = engine.presets[this.preset];
    this.model = preset.model;
    this.reasoningEffort = preset.reasoningEffort;
    this.maxIterations = opts.maxIterations ?? DEFAULT_MAX_ITERATIONS;
    this.allowedTools = opts.allowedTools ?? null;

    // Compose the actual system message sent to the LLM: caller's prompt
    // first, then each resolved skill body, joined with `---`. Skills
    // additionally land in `trace.metadata.skills` as a name→content
    // dict so the tracing UI can present them structured instead of as
    // one giant blob inside every generation's input.
    const skillsMap = opts.resolvedSkills ?? {};
    const systemParts: string[] = [];
    if (opts.systemPrompt) systemParts.push(opts.systemPrompt);
    for (const content of Object.values(skillsMap)) systemParts.push(content);
    const combinedSystem = systemParts.join("\n\n---\n\n");
    if (combinedSystem.length > 0) {
      this.messages.push({ role: "system", content: combinedSystem });
    }

    // Set up the trace scope. Two paths:
    //   - top-level: create a new Trace (and own its input/output/metadata).
    //   - sub-agent: reuse the parent's `invoke_sub_agent` span so all the
    //     child's iter generations + tool spans render nested inside the
    //     parent's trace. We don't take ownership of the scope's
    //     input/output — those describe the tool call from the parent's
    //     POV — but we annotate it with sub-agent identity & config so the
    //     UI can show what was loaded into the child.
    if (opts.traceScope) {
      this.scope = opts.traceScope;
      this.trace = null;
      // Annotate the parent's `invoke_sub_agent` span with sub-agent
      // identity. Skills go in as NAMES, not bodies — metadata is for
      // short K/V; the full skill text already lives in this session's
      // first generation.input (system message), where it belongs.
      this.scope.update({
        metadata: {
          sub_agent_id: this.id,
          sub_agent_skills: Object.keys(skillsMap),
          sub_agent_preset: this.preset,
          sub_agent_model: this.model,
          sub_agent_reasoning_effort: this.reasoningEffort,
          sub_agent_max_iterations: this.maxIterations,
        },
      });
    } else {
      // Generations + tool spans get attached to this trace inside
      // `runUntilSettled`. Output + final status are updated when the
      // loop exits (success, error, or maxIterations). `trace.input` is
      // intentionally NOT set here — it's populated in `runUntilSettled`
      // from the first user message so the Session-replay UI shows a clean
      // `user → assistant` exchange instead of the long system prompt.
      // System prompt + skills live in metadata. When tracing is disabled
      // the engine's tracer is a no-op, so the .generation()/.span() calls
      // below stay safe and Session itself doesn't null-check.
      // Trace metadata = short key-value filtering fields only. The big
      // strings (full system prompt, skill bodies) live where they
      // actually belong: inside generation.input (as the system message
      // of the first messages array). Stuffing them into metadata both
      // bloats the UI and misuses the field — Langfuse's propagated
      // metadata caps values at 200 chars.
      this.trace = engine.tracer.trace({
        id: this.id,
        name: this.id,
        sessionId: opts.sessionId,
        tags: opts.tags,
        metadata: {
          ...opts.metadata,
          agent_id: this.id,
          skills: Object.keys(skillsMap),
          preset: this.preset,
          model: this.model,
          reasoning_effort: this.reasoningEffort,
          max_iterations: this.maxIterations,
          ...(this.parentId ? { parent_id: this.parentId } : {}),
        },
      });
      this.scope = this.trace;
    }
  }

  async send(userText: string): Promise<string> {
    this.messages.push({ role: "user", content: userText });
    return this.run();
  }

  // Run the agent loop with whatever is currently in `this.messages`.
  // Useful when the caller pre-loaded history (e.g. a Telegram batch) and
  // just wants the LLM to react — no extra user message to push.
  async run(): Promise<string> {
    if (this.closed) throw new Error(`session ${this.id} is closed`);
    return this.runUntilSettled();
  }

  // Identity stamp written into every observation this session creates
  // (iter generations + tool spans). Without it, sub-agent observations
  // are visually indistinguishable from the parent's in the UI — the
  // observation pane shows only trace.metadata, and that's the parent's.
  // This way every iter / tool span carries who-ran-it on its own row.
  private get observationMeta(): Record<string, unknown> {
    return this.parentId
      ? { agent_id: this.id, parent_id: this.parentId }
      : { agent_id: this.id };
  }

  private async runUntilSettled(): Promise<string> {
    const { mcp } = this.engine;
    // Resolve provider once per run. Model is fixed for the session's
    // lifetime (set in constructor), so the provider does not change
    // across iterations either.
    const { client: llm, kind: providerKind } = this.engine.resolveProvider(this.model);

    // Surface the user's prompt on the trace so the Session-replay UI
    // renders a real `user → assistant` exchange. Done here (not in the
    // constructor) because the caller pushes the user message AFTER
    // startSession returns. Skip for sub-agents — their scope is the
    // parent's tool span and the input there is the tool args, not the
    // user message; overwriting would erase the parent's view.
    if (this.trace) {
      const firstUserMessage = this.messages.find((m) => m.role === "user");
      if (firstUserMessage) {
        this.scope.update({ input: firstUserMessage.content });
      }
    }

    try {
      for (let i = 0; i < this.maxIterations; i++) {
        // DeepSeek's thinking mode requires every prior assistant turn in
        // the history to carry a `reasoning_content` field. Turns produced
        // under `thinking=disabled` lack it — sub-agents launched with
        // `reasoning_effort: max` would otherwise 400 on their first call
        // if they inherit any thinking-disabled history. Stamp an empty
        // string on every assistant message missing the field whenever
        // we're about to send in thinking-enabled mode. OpenAI ignores the
        // field, but we only hit this branch on the DeepSeek path anyway.
        if (providerKind === "deepseek" && this.reasoningEffort !== "disabled") {
          ensureReasoningContentOnHistory(this.messages);
        }

        // MCP tools filtered by the per-session allow-list (union of
        // loaded skills' frontmatter). Synthetic agent-side tools are
        // not gated by skill frontmatter — they're cheap and universal
        // (set_memory, read_skill, list_skills, write_skill,
        // invoke_sub_agent — the last one stays parent-only via its own
        // `visibleTo` predicate).
        const mcpTools = this.allowedTools === null
          ? mcp.tools
          : mcp.tools.filter((t) => this.allowedTools!.has(t.function.name));
        const baseBody = {
          model: this.model,
          messages: this.messages,
          tools: [
            ...mcpTools,
            ...SYNTHETIC_TOOLS.filter((t) => t.visibleTo?.(this) ?? true).map(
              (t) => t.def,
            ),
          ],
        };
        // Provider-specific extras. DeepSeek understands `thinking` +
        // `reasoning_effort`; OpenAI rejects both (we use OpenAI only for
        // non-thinking chat sessions — gpt-5.4-mini and friends — so
        // there's nothing to pass).
        const body = providerKind === "deepseek"
          ? {
              ...baseBody,
              ...(this.reasoningEffort === "disabled"
                ? { thinking: { type: "disabled" as const } }
                : { thinking: { type: "enabled" as const }, reasoning_effort: this.reasoningEffort }),
            }
          : baseBody;

        // Generation span = one LLM call. The tracer computes latency
        // from start/end timestamps and stores prompt/completion tokens
        // from `usage`. Input is the full messages array — system
        // message included. That's the actual LLM input and the right
        // place for it (Langfuse UI collapses long content). metadata
        // stays a short K/V marker (agent_id) for filtering only.
        const generation = this.scope.generation({
          name: `iter-${i}`,
          model: this.model,
          modelParameters: {
            reasoning_effort: this.reasoningEffort,
            thinking: this.reasoningEffort === "disabled" ? "disabled" : "enabled",
          },
          input: this.messages,
          metadata: this.observationMeta,
        });

        let response;
        try {
          // DeepSeek adds `thinking` + `reasoning_effort` on top of the
          // OpenAI request shape; since `body` is a variable (not an
          // inline object), TS skips excess-property checks and we don't
          // need a directive here.
          response = await llm.chat.completions.create(body);
        } catch (err) {
          generation.end({
            output: { error: (err as Error).message },
            level: "ERROR",
            statusMessage: (err as Error).message,
          });
          throw err;
        }

        const choice = response.choices[0]!;
        const { message } = choice;
        this.messages.push(message);

        generation.end({
          output: message,
          usage: response.usage
            ? {
                input: response.usage.prompt_tokens,
                output: response.usage.completion_tokens,
                total: response.usage.total_tokens,
              }
            : undefined,
        });

        this.engine.log(this.id, `iter ${i} finish=${choice.finish_reason} tool_calls=${message.tool_calls?.length ?? 0}`);

        if (!message.tool_calls?.length) {
          if (this.trace) {
            this.scope.update({ output: message.content ?? "" });
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
            // malformed-JSON throw still leaves a measurable, attributed
            // span in the trace.
            const span = this.scope.span({
              name: call.function.name,
              input: { raw_arguments: call.function.arguments },
              metadata: this.observationMeta,
            });

            let result: string;
            try {
              const args = JSON.parse(call.function.arguments || "{}") as Record<string, unknown>;
              span.update({ input: args });
              this.engine.log(this.id, `→ ${call.function.name}(${JSON.stringify(args)})`);

              const name = call.function.name;
              const synthetic = SYNTHETIC_TOOLS_BY_NAME.get(name);
              if (synthetic) {
                // Pass the span as the third arg — `invoke_sub_agent` reuses
                // it as the child session's trace scope so the sub-agent's
                // iters render nested here in the UI.
                result = await synthetic.handle(this, args, span);
              } else {
                result = await mcp.callTool(name, args);
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
            this.engine.log(this.id, `← ${call.function.name} ${result.length}b: ${result}`);

            return { call, result };
          }),
        );

        for (const { call, result } of toolResults) {
          this.messages.push({
            role: "tool",
            tool_call_id: call.id,
            content: result,
          });
        }
      }

      throw new Error(`session ${this.id} exceeded maxIterations=${this.maxIterations}`);
    } catch (err) {
      // Trace-level error marker. Individual generation/span errors are
      // already attributed above; this surfaces failed sessions in the
      // tracing list view (sort by metadata.error or filter by output.error).
      // Sub-agents skip this — the parent's `invoke_sub_agent` span will
      // capture the error via its own `.end({ level: ERROR })` once the
      // exception propagates back to the dispatch loop.
      if (this.trace) {
        this.scope.update({
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
      this.trace?.end();
    }
  }

  async applyReadSkill(args: SkillNameArg): Promise<string> {
    if (typeof args.name !== "string" || args.name.length === 0) {
      return `[read_skill error] name must be a non-empty string`;
    }
    try {
      const skill = await readSkill(args.name);
      if (skill === null) {
        return JSON.stringify({ name: args.name, found: false, content: null });
      }
      return JSON.stringify({
        name: args.name,
        found: true,
        content: skill.body,
        tools: skill.tools,
        source: skill.source,
        sizeBytes: Buffer.byteLength(skill.body, "utf-8"),
      });
    } catch (err) {
      return `[read_skill error] ${(err as Error).message}`;
    }
  }

  async applyWriteSkill(args: WriteSkillArgs): Promise<string> {
    if (typeof args.name !== "string" || args.name.length === 0) {
      return `[write_skill error] name must be a non-empty string`;
    }
    if (typeof args.content !== "string" || args.content.length === 0) {
      return `[write_skill error] content must be a non-empty string`;
    }
    try {
      const written = await saveSkill(args.name, args.content);
      this.engine.log(this.id, `write_skill ${args.name} (${written.sizeBytes}b → ${written.path})`);
      return JSON.stringify({ ok: true, name: args.name, ...written });
    } catch (err) {
      return `[write_skill error] ${(err as Error).message}`;
    }
  }

  async applyListSkills(): Promise<string> {
    try {
      const skills = await listSkills();
      return JSON.stringify({ count: skills.length, skills });
    } catch (err) {
      return `[list_skills error] ${(err as Error).message}`;
    }
  }

  async applyInvokeSubAgent(args: InvokeSubAgentArgs, parentSpan: TraceContext): Promise<string> {
    if (!Array.isArray(args.skills) || args.skills.length === 0) {
      return `[invoke_sub_agent error] "skills" must be a non-empty string array`;
    }
    if (args.skills.some((s) => s.length === 0)) {
      return `[invoke_sub_agent error] every entry in "skills" must be a non-empty string`;
    }
    if (typeof args.prompt !== "string" || args.prompt.length === 0) {
      return `[invoke_sub_agent error] "prompt" must be a non-empty string`;
    }

    this.subAgentCounter += 1;
    // `__sub` (double underscore) keeps the id ASCII-only and unique
    // (used for log lines and metadata; not a trace id anymore — the
    // sub-agent renders inside the parent's `invoke_sub_agent` span).
    const childId = `${this.id}__sub${this.subAgentCounter}`;

    let child: Session;
    try {
      child = await this.engine.startSession({
        id: childId,
        // Sub-agent's system message = optional parent-provided framing
        // + the named skills' content. NO session-context, NO envContext,
        // NO engine meta-skills. This is the entire point — a slim,
        // focused worker with exactly what the parent decided it needs.
        systemPrompt: args.system_prompt,
        skills: args.skills,
        includeEngineSkills: false,
        preset: args.preset ?? "base",
        maxIterations: args.max_iterations ?? 50,
        parentId: this.id,
        sessionId: this.sessionId,
        // Nest the sub-agent inside the parent's `invoke_sub_agent` span.
        // All iter generations + tool spans the child opens land here, so
        // the parent's trace view shows the whole sub-session inline.
        traceScope: parentSpan,
      });
    } catch (err) {
      return `[invoke_sub_agent error] failed to start: ${(err as Error).message}`;
    }

    child.messages.push({ role: "user", content: args.prompt });

    try {
      return await child.run();
    } catch (err) {
      return `[invoke_sub_agent error] sub-agent crashed: ${(err as Error).message}`;
    } finally {
      this.engine.endSession(childId);
    }
  }

  applySetMemory(args: SetMemoryArgs): string {
    if (typeof args.key !== "string" || args.key.length === 0) {
      return `[set_memory error] key must be a non-empty string`;
    }
    if (typeof args.value !== "string") {
      return `[set_memory error] value must be a string (got ${typeof args.value})`;
    }
    setMemory(args.key, args.value);
    this.engine.log(this.id, `set_memory ${args.key} = ${args.value.slice(0, 80)}`);
    return `ok — stored ${args.key}`;
  }

  close(): void {
    this.closed = true;
  }
}

