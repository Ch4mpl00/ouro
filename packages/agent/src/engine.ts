import OpenAI from "openai";
import { connectMcp, type McpHandle } from "./mcp-client";
import { Session, type SessionOpts } from "./session";

// Process-level singleton. Owns shared, expensive resources:
//   - one OpenAI client (one API key, one rate-limit bucket)
//   - one MCP connection (one stdio child process for the integrations server)
// Hands out Sessions on demand. Each Session has its own context buffer,
// system prompt and iteration budget but reuses these shared resources.

export interface EngineOpts {
  apiKey: string;
  baseURL?: string;
  defaultModel?: string;
}

export class Engine {
  readonly llm: OpenAI;
  readonly mcp: McpHandle;
  readonly defaultModel: string;
  private sessions = new Map<string, Session>();

  constructor(llm: OpenAI, mcp: McpHandle, defaultModel: string) {
    this.llm = llm;
    this.mcp = mcp;
    this.defaultModel = defaultModel;
  }

  startSession(opts: SessionOpts): Session {
    if (this.sessions.has(opts.id)) {
      throw new Error(`session id ${opts.id} already exists`);
    }
    const session = new Session(this, opts);
    this.sessions.set(opts.id, session);
    this.log(opts.id, `session opened (model=${opts.model ?? this.defaultModel}${opts.parentId ? `, parent=${opts.parentId}` : ""})`);
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

  return new Engine(llm, mcp, opts.defaultModel ?? "deepseek-v4-pro");
}
