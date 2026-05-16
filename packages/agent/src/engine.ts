import OpenAI from "openai";
import { connectMcp, type McpHandle } from "./mcp-client";
import { Session, type SessionOpts } from "./session";
import { readSkill } from "./skills";
import { nullTracer, type Tracer } from "./tracing";
import { langfuseTracerFromEnv } from "./tracing-langfuse";

// Process-level singleton. Owns shared, expensive resources:
//   - one OpenAI client (one API key, one rate-limit bucket)
//   - one MCP connection (one stdio child process for the integrations server)
//   - one Tracer (observability backend; defaults to no-op)
// Hands out Sessions on demand. Each Session has its own context buffer,
// system prompt and iteration budget but reuses these shared resources.

export interface EngineOpts {
  apiKey: string;
  baseURL?: string;
  defaultModel?: string;
  // Engine-level skills — loaded into every session this engine starts
  // (unless a session opts out via `includeEngineSkills: false`). Use for
  // meta-skills that apply across every domain — e.g. `routing` (when to
  // delegate to another skill). Per-session domain skills are passed via
  // `SessionOpts.skills` instead.
  //
  // Resolved at `startSession` time, not engine-create time, so live
  // overlay edits (e.g. by the `dreaming` skill) take effect on the very
  // next session without an engine restart.
  skills?: string[];
  // Optional tracer for observability. Omit → auto-config from env (currently
  // Langfuse, see `tracing-langfuse.ts`). Pass `nullTracer` to disable
  // explicitly, or any other `Tracer` to swap backends (testing, etc.).
  tracer?: Tracer;
}

export class Engine {
  readonly llm: OpenAI;
  readonly mcp: McpHandle;
  readonly defaultModel: string;
  readonly skills: readonly string[];
  readonly tracer: Tracer;
  private sessions = new Map<string, Session>();

  constructor(
    llm: OpenAI,
    mcp: McpHandle,
    defaultModel: string,
    skills: readonly string[],
    tracer: Tracer,
  ) {
    this.llm = llm;
    this.mcp = mcp;
    this.defaultModel = defaultModel;
    this.skills = skills;
    this.tracer = tracer;
  }

  async startSession(opts: SessionOpts): Promise<Session> {
    if (this.sessions.has(opts.id)) {
      throw new Error(`session id ${opts.id} already exists`);
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
    // ordering when composing the actual system message.
    const resolvedSkills: Record<string, string> = {};
    for (const name of sessionSkillNames) {
      const content = await readSkill(name);
      if (content === null) {
        throw new Error(
          `session skill "${name}" not found (skills/${name}.md and skills.default/${name}.md both missing)`,
        );
      }
      resolvedSkills[name] = content;
    }
    for (const name of engineSkillNames) {
      const content = await readSkill(name);
      if (content === null) {
        this.log(opts.id, `[warn] engine skill "${name}" not found, skipping`);
        continue;
      }
      resolvedSkills[name] = content;
    }

    const session = new Session(this, { ...opts, resolvedSkills });
    this.sessions.set(opts.id, session);
    const skillsList = Object.keys(resolvedSkills).join(",");
    this.log(
      opts.id,
      `session opened (model=${opts.model ?? this.defaultModel}, effort=${opts.reasoningEffort ?? "disabled"}, skills=[${skillsList}]${opts.parentId ? `, parent=${opts.parentId}` : ""})`,
    );
    return session;
  }

  endSession(id: string): void {
    const s = this.sessions.get(id);
    if (!s) return;
    s.close();
    this.sessions.delete(id);
    this.log(id, "session closed");
  }

  log(sessionId: string, ...parts: unknown[]): void {
    console.log(`[${new Date().toISOString()}]`, `[${sessionId}]`, ...parts);
  }

  async shutdown(): Promise<void> {
    for (const id of [...this.sessions.keys()]) this.endSession(id);
    // Flush buffered tracer events. Without this, traces from the final
    // session(s) before SIGTERM are silently dropped.
    await this.tracer.shutdown();
    await this.mcp.close();
  }
}

export async function createEngine(opts: EngineOpts): Promise<Engine> {
  if (!opts.apiKey) throw new Error("createEngine: apiKey is required");

  const llm = new OpenAI({
    apiKey: opts.apiKey,
    baseURL: opts.baseURL ?? "https://api.deepseek.com",
  });

  const mcp = await connectMcp();

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

  return new Engine(llm, mcp, opts.defaultModel ?? "deepseek-v4-pro", opts.skills ?? [], tracer);
}
