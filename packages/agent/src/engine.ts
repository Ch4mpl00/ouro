import OpenAI from "openai";
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
}

export class Engine {
  readonly llm: OpenAI;
  readonly mcp: McpHandle;
  readonly defaultModel: string;
  readonly skills: readonly string[];
  private sessions = new Map<string, Session>();

  constructor(llm: OpenAI, mcp: McpHandle, defaultModel: string, skills: readonly string[]) {
    this.llm = llm;
    this.mcp = mcp;
    this.defaultModel = defaultModel;
    this.skills = skills;
  }

  async startSession(opts: SessionOpts): Promise<Session> {
    if (this.sessions.has(opts.id)) {
      throw new Error(`session id ${opts.id} already exists`);
    }

    const sessionSkillNames = opts.skills ?? [];
    const engineSkillNames = this.skills ?? []

    // Session-level skills are required: missing one is a signal-handling
    // error and the caller decides whether to skip. Engine-level skills
    // are best-effort: a missing meta-skill is logged and dropped so a
    // typo in engine config doesn't take down every session.
    const sessionSkillContents = await Promise.all(
      sessionSkillNames.map(async (name) => {
        const content = await readSkill(name);
        if (content === null) {
          throw new Error(
            `session skill "${name}" not found (skills/${name}.md and skills.default/${name}.md both missing)`,
          );
        }
        return content;
      }),
    );

    const engineSkillContents = (
      await Promise.all(
        engineSkillNames.map(async (name) => {
          const content = await readSkill(name);
          if (content === null) {
            this.log(opts.id, `[warn] engine skill "${name}" not found, skipping`);
            return null;
          }
          return content;
        }),
      )
    ).filter((c): c is string => c !== null);

    // Final order: caller-built context (state, env) → session skills
    // (primary domain) → engine skills (always-on meta-skills).
    const parts: string[] = [];
    if (opts.systemPrompt) parts.push(opts.systemPrompt);
    parts.push(...sessionSkillContents, ...engineSkillContents);
    const systemPrompt = parts.join("\n\n---\n\n");

    const session = new Session(this, { ...opts, systemPrompt });
    this.sessions.set(opts.id, session);
    const skillsList = [...sessionSkillNames, ...engineSkillNames].join(",");
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

  return new Engine(llm, mcp, opts.defaultModel ?? "deepseek-v4-pro", opts.skills ?? []);
}
