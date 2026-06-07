import OpenAI from "openai";
import { connectMcp, type McpHandle } from "./mcp-client";
import { DEFAULT_PRESETS, type ModelPreset, type PresetName } from "./models";
import {
  createDeepseekProvider,
  createOpenAiProvider,
  type ChatProvider,
} from "./providers";
import { AgentLoop, type AgentLoopOpts } from "./agent-loop";
import { readSkill, validateAllSkills } from "./skills";
import { nullTracer, type Tracer } from "./tracing";
import { langfuseTracerFromEnv } from "./tracing/langfuse";

// Process-level singleton. Owns shared, expensive resources:
//   - two OpenAI-shaped clients, one per provider (DeepSeek for
//     thinking-mode sessions, OpenAI for non-thinking — each on its own
//     API key + rate-limit bucket)
//   - one MCP connection (one stdio child process for the integrations server)
//   - one Tracer (observability backend; defaults to no-op)
// Hands out AgentLoops on demand. Each AgentLoop has its own context
// buffer, system prompt and iteration budget but reuses these shared
// resources.

// Provider kind + the ChatProvider wrapper live in ./providers now. Re-export
// the kind for callers that only need the discriminator.
export type { ProviderKind } from "./providers";

export interface EngineOpts {
  deepseekApiKey: string;
  openaiApiKey: string;
  // Optional override for the model-preset registry. Omit to use
  // DEFAULT_PRESETS from `./models`. Per-preset env overrides
  // (AGENT_BASE_MODEL / AGENT_SMART_MODEL) are applied by the supervisor
  // before constructing the engine.
  presets?: Record<PresetName, ModelPreset>;
  // Engine-level skills — loaded into every session this engine starts
  // (unless a session opts out via `includeEngineSkills: false`). Use for
  // meta-skills that apply across every domain — e.g. `routing` (when to
  // delegate to another skill). Per-session domain skills are passed via
  // `AgentLoopOpts.skills` instead.
  //
  // Resolved at `startAgentLoop` time, not engine-create time, so live
  // overlay edits (e.g. by the `dreaming` skill) take effect on the very
  // next session without an engine restart.
  skills?: string[];
  // Optional tracer for observability. Omit → auto-config from env (currently
  // Langfuse, see `tracing/langfuse.ts`). Pass `nullTracer` to disable
  // explicitly, or any other `Tracer` to swap backends (testing, etc.).
  tracer?: Tracer;
}

export class Engine {
  readonly mcp: McpHandle;
  readonly presets: Record<PresetName, ModelPreset>;
  readonly skills: readonly string[];
  readonly tracer: Tracer;
  private readonly providers: { deepseek: ChatProvider; openai: ChatProvider };
  private agentLoops = new Map<string, AgentLoop>();

  constructor(
    providers: { deepseek: ChatProvider; openai: ChatProvider },
    mcp: McpHandle,
    presets: Record<PresetName, ModelPreset>,
    skills: readonly string[],
    tracer: Tracer,
  ) {
    this.providers = providers;
    this.mcp = mcp;
    this.presets = presets;
    this.skills = skills;
    this.tracer = tracer;
  }

  // Pick the provider wrapper based on the model name. The model name is the
  // source of truth — Session resolves a preset name to a concrete model at
  // construction time; this method only routes that model to its endpoint.
  resolveProvider(model: string): ChatProvider {
    return model.startsWith("deepseek")
      ? this.providers.deepseek
      : this.providers.openai;
  }

  async startAgentLoop(opts: AgentLoopOpts): Promise<AgentLoop> {
    if (this.agentLoops.has(opts.id)) {
      throw new Error(`agent-loop id ${opts.id} already exists`);
    }

    const sessionSkillNames = opts.skills ?? [];
    const includeEngineSkills = opts.includeEngineSkills ?? true;
    const engineSkillNames = includeEngineSkills ? (this.skills ?? []) : [];

    // Resolve session-level skills first (required: missing one is a
    // signal-handling error). Then engine-level skills (best-effort:
    // missing meta-skill is logged and dropped so a typo in engine
    // config doesn't take down every session).
    //
    // Final iteration order (Object insertion order) is preserved:
    // session domain skills → engine meta-skills. The Session uses this
    // ordering when composing the actual system message. The union of
    // each skill's frontmatter `tools:` list defines what MCP tools
    // this session is allowed to see — synthetic agent-side tools
    // (set_memory, read_skill, invoke_sub_agent, ...) stay always-on.
    const resolvedSkills: Record<string, string> = {};
    const accumulated = new Set<string>();
    let wildcard = false;
    const mergeSkill = (skill: { body: string; tools: string[] | "*" }, name: string) => {
      resolvedSkills[name] = skill.body;
      if (skill.tools === "*") {
        wildcard = true;
      } else {
        for (const t of skill.tools) accumulated.add(t);
      }
    };
    for (const name of sessionSkillNames) {
      const skill = await readSkill(name);
      if (skill === null) {
        throw new Error(
          `session skill "${name}" not found (skills/${name}.md and skills.default/${name}.md both missing)`,
        );
      }
      mergeSkill(skill, name);
    }
    for (const name of engineSkillNames) {
      const skill = await readSkill(name);
      if (skill === null) {
        this.log(opts.id, `[warn] engine skill "${name}" not found, skipping`);
        continue;
      }
      mergeSkill(skill, name);
    }

    // Wildcard from ANY loaded skill collapses the union to "all MCP tools" —
    // expressed as a null allow-list (Session treats null as no filter).
    let allowedTools: Set<string> | null = wildcard ? null : accumulated;

    // Caller-side narrowing on top of the skill-derived set. The workflow
    // executor passes a step's `tools: [...]` whitelist this way so the
    // sub-agent sees an intersection of (skill says OK) ∩ (workflow says
    // OK). null skill-side means wildcard → fall back to the workflow's
    // list verbatim.
    if (opts.toolWhitelist) {
      if (allowedTools === null) {
        allowedTools = new Set(opts.toolWhitelist);
      } else {
        allowedTools = new Set(
          [...allowedTools].filter((t) => opts.toolWhitelist!.has(t)),
        );
      }
    }

    const loop = new AgentLoop(this, { ...opts, resolvedSkills, allowedTools });
    this.agentLoops.set(opts.id, loop);
    const skillsList = Object.keys(resolvedSkills).join(",");
    const toolsLabel = allowedTools === null ? "*" : String(allowedTools.size);
    this.log(
      opts.id,
      `agent-loop opened (preset=${loop.preset} → model=${loop.model}, effort=${loop.reasoningEffort}, skills=[${skillsList}], tools=${toolsLabel}${opts.parentId ? `, parent=${opts.parentId}` : ""})`,
    );
    return loop;
  }

  endAgentLoop(id: string): void {
    const loop = this.agentLoops.get(id);
    if (!loop) return;
    loop.close();
    this.agentLoops.delete(id);
    this.log(id, "agent-loop closed");
  }

  log(sessionId: string, ...parts: unknown[]): void {
    console.log(`[${new Date().toISOString()}]`, `[${sessionId}]`, ...parts);
  }

  async shutdown(): Promise<void> {
    for (const id of [...this.agentLoops.keys()]) this.endAgentLoop(id);
    // Flush buffered tracer events. Without this, traces from the final
    // session(s) before SIGTERM are silently dropped.
    await this.tracer.shutdown();
    await this.mcp.close();
  }
}

export async function createEngine(opts: EngineOpts): Promise<Engine> {
  if (!opts.deepseekApiKey) throw new Error("createEngine: deepseekApiKey is required");
  if (!opts.openaiApiKey) throw new Error("createEngine: openaiApiKey is required");

  const deepseek = createDeepseekProvider(
    new OpenAI({
      apiKey: opts.deepseekApiKey,
      baseURL: "https://api.deepseek.com",
    }),
  );
  const openai = createOpenAiProvider(new OpenAI({ apiKey: opts.openaiApiKey }));

  const mcp = await connectMcp();

  // Validate every skill on disk against the live MCP registry. Crashes
  // early with a precise error if any skill is missing frontmatter, has
  // a malformed `tools:` line, or names a tool that doesn't exist —
  // instead of failing mid-signal handling.
  const mcpToolNames = mcp.tools.map((t) => t.function.name);
  await validateAllSkills(mcpToolNames);
  console.log(`[engine] skill validation passed (mcp tools: ${mcpToolNames.length})`);

  // Tracer: caller override > env auto-config > null. Logged once at startup.
  let tracer: Tracer;
  if (opts.tracer) {
    tracer = opts.tracer;
  } else {
    const auto = langfuseTracerFromEnv();
    if (auto) {
      tracer = auto;
      console.log(`[engine] tracing enabled (langfuse v5, ${process.env.LANGFUSE_BASE_URL ?? "default host"})`);
    } else {
      tracer = nullTracer;
      console.log("[engine] tracing disabled (LANGFUSE_*_KEY not set)");
    }
  }

  return new Engine(
    { deepseek, openai },
    mcp,
    opts.presets ?? DEFAULT_PRESETS,
    opts.skills ?? [],
    tracer,
  );
}
