import type { McpHandle } from "./mcp-client";
import type { ModelPreset, PresetName } from "./models";
import type { ChatProvider } from "./providers";
import { createAgentLoop, type AgentLoop, type AgentLoopOpts } from "./agent-loop";
import type { MemoryStore } from "./db/memory";
import type { SkillStore } from "./skills";
import type { Tracer } from "./tracing";

// Process-level hub for shared, expensive resources:
//   - three ChatProviders (DeepSeek for thinking-mode sessions, OpenAI for
//     non-thinking, Gemini — each on its own API key + rate-limit bucket)
//   - one MCP connection
//   - one Tracer (observability backend)
//   - the skill store and the agent-side memory KV
// Hands out AgentLoops on demand. Each AgentLoop has its own context
// buffer, system prompt and iteration budget but reuses these shared
// resources.
//
// Pure wiring: `createEngine` performs NO IO and reads NO env — clients,
// the MCP connection, tracer config and skill validation all happen in
// the composition root (supervisor main / a script's main), which passes
// the finished resources in. That keeps the full dependency graph visible
// in one place and makes the engine trivially mockable (it's an
// interface; tests pass plain objects).

// Provider kind + the ChatProvider wrapper live in ./providers. Re-export
// the kind for callers that only need the discriminator.
export type { ProviderKind } from "./providers";

export interface Engine {
  readonly mcp: McpHandle;
  readonly presets: Record<PresetName, ModelPreset>;
  // Engine-level meta-skill names loaded into every session unless the
  // session opts out (`includeEngineSkills: false`).
  readonly skills: readonly string[];
  readonly tracer: Tracer;
  readonly skillStore: SkillStore;
  readonly memory: MemoryStore;
  // Pick the provider wrapper based on the model name. The model name is
  // the source of truth — a loop resolves a preset name to a concrete
  // model at construction time; this method only routes that model to
  // its endpoint.
  resolveProvider(model: string): ChatProvider;
  startAgentLoop(opts: AgentLoopOpts): Promise<AgentLoop>;
  endAgentLoop(id: string): void;
  log(sessionId: string, ...parts: unknown[]): void;
  // Ends open loops, flushes the tracer, closes the MCP connection.
  shutdown(): Promise<void>;
}

export interface EngineDeps {
  providers: {
    deepseek: ChatProvider;
    openai: ChatProvider;
    gemini: ChatProvider;
  };
  mcp: McpHandle;
  presets: Record<PresetName, ModelPreset>;
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
  skillStore: SkillStore;
  memory: MemoryStore;
  tracer: Tracer;
}

export function createEngine(deps: EngineDeps): Engine {
  const { providers, mcp, presets, skillStore, memory, tracer } = deps;
  const engineSkills: readonly string[] = deps.skills ?? [];
  const agentLoops = new Map<string, AgentLoop>();

  function log(sessionId: string, ...parts: unknown[]): void {
    console.log(`[${new Date().toISOString()}]`, `[${sessionId}]`, ...parts);
  }

  const engine: Engine = {
    mcp,
    presets,
    skills: engineSkills,
    tracer,
    skillStore,
    memory,
    log,

    resolveProvider(model) {
      if (model.startsWith("deepseek")) return providers.deepseek;
      if (model.startsWith("gemini")) return providers.gemini;
      return providers.openai;
    },

    async startAgentLoop(opts) {
      if (agentLoops.has(opts.id)) {
        throw new Error(`agent-loop id ${opts.id} already exists`);
      }

      const sessionSkillNames = opts.skills ?? [];
      const includeEngineSkills = opts.includeEngineSkills ?? true;
      const engineSkillNames = includeEngineSkills ? engineSkills : [];

      // Resolve session-level skills first (required: missing one is a
      // signal-handling error). Then engine-level skills (best-effort:
      // missing meta-skill is logged and dropped so a typo in engine
      // config doesn't take down every session).
      //
      // Final iteration order (Object insertion order) is preserved:
      // session domain skills → engine meta-skills. The loop uses this
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
        const skill = await skillStore.readSkill(name);
        if (skill === null) {
          throw new Error(
            `session skill "${name}" not found (skills/${name}.md and skills.default/${name}.md both missing)`,
          );
        }
        mergeSkill(skill, name);
      }
      for (const name of engineSkillNames) {
        const skill = await skillStore.readSkill(name);
        if (skill === null) {
          log(opts.id, `[warn] engine skill "${name}" not found, skipping`);
          continue;
        }
        mergeSkill(skill, name);
      }

      // Wildcard from ANY loaded skill collapses the union to "all MCP
      // tools" — expressed as a null allow-list (the loop treats null as
      // no filter).
      let allowedTools: Set<string> | null = wildcard ? null : accumulated;

      // Caller-side narrowing on top of the skill-derived set. The
      // workflow executor passes a step's `tools: [...]` whitelist this
      // way so the sub-agent sees an intersection of (skill says OK) ∩
      // (workflow says OK). null skill-side means wildcard → fall back
      // to the workflow's list verbatim.
      if (opts.toolWhitelist) {
        if (allowedTools === null) {
          allowedTools = new Set(opts.toolWhitelist);
        } else {
          allowedTools = new Set(
            [...allowedTools].filter((t) => opts.toolWhitelist!.has(t)),
          );
        }
      }

      const loop = createAgentLoop(engine, { ...opts, resolvedSkills, allowedTools });
      agentLoops.set(opts.id, loop);
      const skillsList = Object.keys(resolvedSkills).join(",");
      const toolsLabel = allowedTools === null ? "*" : String(allowedTools.size);
      log(
        opts.id,
        `agent-loop opened (preset=${loop.preset} → model=${loop.model}, effort=${loop.reasoningEffort}, skills=[${skillsList}], tools=${toolsLabel}${opts.parentId ? `, parent=${opts.parentId}` : ""})`,
      );
      return loop;
    },

    endAgentLoop(id) {
      const loop = agentLoops.get(id);
      if (!loop) return;
      loop.close();
      agentLoops.delete(id);
      log(id, "agent-loop closed");
    },

    async shutdown() {
      for (const id of [...agentLoops.keys()]) engine.endAgentLoop(id);
      // Flush buffered tracer events. Without this, traces from the final
      // session(s) before SIGTERM are silently dropped.
      await tracer.shutdown();
      await mcp.close();
    },
  };

  return engine;
}
