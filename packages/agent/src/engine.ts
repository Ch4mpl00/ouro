import OpenAI from "openai";
import { Langfuse } from "langfuse";
import { connectMcp, type McpHandle } from "./mcp-client";
import { Session, type SessionOpts } from "./session";
import { readSkill } from "./skills";

// Process-level singleton. Owns shared, expensive resources:
//   - one OpenAI client (one API key, one rate-limit bucket)
//   - one MCP connection (one stdio child process for the integrations server)
// Hands out Sessions on demand. Each Session has its own context buffer,
// system prompt and iteration budget but reuses these shared resources.

export interface EngineOpts {
  apiKey: string;
  baseURL?: string;
  defaultModel?: string;
  // Engine-level skills — loaded into every session this engine starts
  // (unless a session opts out via `includeEngineSkills: false`). Use for
  // meta-skills that apply across every domain — e.g. `routing` (when to
  // delegate to another skill), `handoff` (when to escalate reasoning).
  // Per-session domain skills are passed via `SessionOpts.skills` instead.
  //
  // Resolved at `startSession` time, not engine-create time, so live
  // overlay edits (e.g. by the `dreaming` skill) take effect on the very
  // next session without an engine restart.
  skills?: string[];
  // Optional Langfuse client for tracing. Sessions create one trace each
  // (per-LLM-call generations + per-tool-call spans). When null, all
  // tracing calls in `session.ts` short-circuit to no-ops.
  langfuse?: Langfuse | null;
}

export class Engine {
  readonly llm: OpenAI;
  readonly mcp: McpHandle;
  readonly defaultModel: string;
  readonly skills: readonly string[];
  readonly langfuse: Langfuse | null;
  private sessions = new Map<string, Session>();

  constructor(
    llm: OpenAI,
    mcp: McpHandle,
    defaultModel: string,
    skills: readonly string[],
    langfuse: Langfuse | null,
  ) {
    this.llm = llm;
    this.mcp = mcp;
    this.defaultModel = defaultModel;
    this.skills = skills;
    this.langfuse = langfuse;
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
    // shutdownAsync flushes all batched events. Without this, traces from
    // the final session(s) before SIGTERM are silently dropped.
    if (this.langfuse) await this.langfuse.shutdownAsync();
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

  // Caller may pass `langfuse: null` to explicitly disable. If `undefined`
  // (the common case), auto-configure from env. Missing keys → no tracing,
  // logged once at startup.
  let langfuse: Langfuse | null;
  if (opts.langfuse !== undefined) {
    langfuse = opts.langfuse;
  } else {
    const secretKey = process.env.LANGFUSE_SECRET_KEY;
    const publicKey = process.env.LANGFUSE_PUBLIC_KEY;
    const baseUrl = process.env.LANGFUSE_BASE_URL;
    if (secretKey && publicKey) {
      langfuse = new Langfuse({ secretKey, publicKey, baseUrl });
      console.log(`[engine] langfuse tracing enabled (${baseUrl ?? "default host"})`);
    } else {
      langfuse = null;
      console.log("[engine] langfuse tracing disabled (LANGFUSE_*_KEY not set)");
    }
  }

  return new Engine(llm, mcp, opts.defaultModel ?? "deepseek-v4-pro", opts.skills ?? [], langfuse);
}
