// The environment the specification runs in: a Postgres, the real server
// in-process over HTTP, and an MCP client that calls it.
//
//   1. database — Testcontainers pgvector, or TEST_DATABASE_URL
//   2. server   — every dependency wired as server.ts main() does, nothing
//                 external (no pollers, no OpenAI, no Telegram)
//   3. client   — one MCP session, a call and its outcome

import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { McpError } from "@modelcontextprotocol/sdk/types.js";
import { PostgreSqlContainer, type StartedPostgreSqlContainer } from "@testcontainers/postgresql";
import { createPgClient, type PgClient } from "../../src/db/pg/client";
import { connectStateDb, type StateDb } from "../../src/db/state";
import { runHttpTransport, type RunningHttpTransport } from "../../src/http-transport";
import { createServer } from "../../src/mcp-server";
import type { EmbeddingProvider } from "../../src/services/embeddings/provider";
import { createGuardedFetch, type GuardedFetch } from "../../src/services/fetch";
import { createGmailModule } from "../../src/services/gmail";
import { createKnowledgeModule } from "../../src/services/knowledge";
import { createMemoryModule } from "../../src/services/memory";
import { createNewsModule } from "../../src/services/news";
import { createScheduledTasks, type SchedulerDeps } from "../../src/services/scheduler";
import { createSettings } from "../../src/services/settings";
import { createSignals, type Signals } from "../../src/services/signals";
import { createSkillsModule } from "../../src/services/skills";
import { createTelegramLog } from "../../src/services/telegram";
import { createUserbot, createUserbotSessions } from "../../src/services/telegram/userbot";
import type { ToolsetName } from "../../src/toolsets";

// ── 1. database ──────────────────────────────────────────────────────────────

// A container unless a throwaway database is named. Stop the container when
// the run ends.
export async function database(): Promise<{ url: string; container: StartedPostgreSqlContainer | null }> {
  const named = process.env.TEST_DATABASE_URL;
  if (named) return { url: named, container: null };
  const container = await new PostgreSqlContainer("pgvector/pgvector:pg16").start();
  return { url: container.getConnectionUri(), container };
}

// ── 2. server ────────────────────────────────────────────────────────────────

// Deterministic stand-in for the embedding model: a bag-of-words vector, so
// distance tracks word overlap. Recall works without OpenAI.
export const wordEmbedder: EmbeddingProvider = {
  dimensions: 1536,
  embed: async (text) => embedWords(text),
  embedBatch: async (texts) => texts.map(embedWords),
};

function embedWords(text: string): number[] {
  const v = new Array<number>(1536).fill(0);
  for (const token of text.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter(Boolean)) {
    let hash = 0;
    for (const c of token) hash = (hash * 31 + c.codePointAt(0)!) % 1536;
    v[hash]! += 1;
  }
  const norm = Math.max(Math.sqrt(v.reduce((sum, x) => sum + x * x, 0)), Number.EPSILON);
  return v.map((x) => x / norm);
}

export interface Server {
  url: string;
  pg: PgClient;
  state: StateDb;
  signals: Signals;
  scheduler: SchedulerDeps;
  stop(): Promise<void>;
}

// `toolsets`: the MCP_TOOLSETS selection (omitted = every group). A restricted
// instance is multi-session, as in production.
// `fetcher`: the guarded fetch behind fetch_url and the news providers —
// tests narrow what counts as private so a local server can play the public
// internet.
export async function startServer(
  databaseUrl: string,
  toolsets?: readonly ToolsetName[],
  fetcher: GuardedFetch = createGuardedFetch(),
): Promise<Server> {
  // Nothing may leave the box: no chat to send to, no bot, no OpenAI key.
  for (const name of [
    "TELEGRAM_DEFAULT_CHAT_ID",
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_ASSISTANT_BOT_TOKEN",
    "OPENAI_API_KEY",
    "GMAIL_ACCOUNT_KEY",
  ]) {
    delete process.env[name];
  }
  process.env.DATABASE_URL = databaseUrl;
  const pg = createPgClient();
  await pg.ensureReady();
  const state = await connectStateDb(databaseUrl);

  const signals = createSignals(state);
  const settings = createSettings(state);
  const tasks = createScheduledTasks(state);
  const skillsDir = mkdtempSync(path.join(tmpdir(), "mcp-specs-skills-"));
  const deps = {
    news: createNewsModule({ db: pg.db, embeddingProvider: wordEmbedder }).repository,
    knowledge: createKnowledgeModule({ db: pg.db, embeddingProvider: wordEmbedder }).repository,
    memory: createMemoryModule({ db: pg.db, embeddingProvider: wordEmbedder }).service,
    skills: createSkillsModule({ liveDir: skillsDir, defaultsDir: skillsDir }).catalog,
    memoryActor: "spec",
    signals,
    settings,
    scheduler: tasks,
    telegramLog: createTelegramLog(state),
    gmail: createGmailModule({ db: state }),
    userbot: createUserbot(createUserbotSessions(state)),
    fetcher,
  };

  const transport: RunningHttpTransport = await runHttpTransport({
    port: 0,
    createEndpoint: async () => createServer({ ...deps, toolsets }),
    multiSession: toolsets !== undefined,
  });

  return {
    url: `http://127.0.0.1:${transport.port}/mcp`,
    pg,
    state,
    signals,
    scheduler: { tasks, settings, signals },
    stop: async () => {
      await transport.close();
      await state.close();
      await pg.close();
    },
  };
}

// ── 3. client ────────────────────────────────────────────────────────────────

export async function connect(url: string): Promise<Client> {
  const client = new Client({ name: "specs", version: "0.0.0" });
  await client.connect(new StreamableHTTPClientTransport(new URL(url)));
  return client;
}

export type Outcome =
  | { kind: "ok"; value: unknown }
  // The tool ran and failed (an `isError` result the model reads).
  | { kind: "toolError"; message: string }
  // The call never reached the tool: bad parameters, unknown tool.
  | { kind: "rejected"; code: number; message: string };

export async function call(client: Client, tool: string, args: Record<string, unknown>): Promise<Outcome> {
  try {
    const result = await client.callTool({ name: tool, arguments: args });
    const content = Array.isArray(result.content) ? result.content : [];
    const first = content[0];
    const text = first && first.type === "text" && typeof first.text === "string" ? first.text : "";
    if (result.isError === true) return { kind: "toolError", message: text };
    try {
      return { kind: "ok", value: JSON.parse(text) };
    } catch {
      return { kind: "ok", value: text };
    }
  } catch (err) {
    if (err instanceof McpError) return { kind: "rejected", code: err.code, message: err.message };
    throw err;
  }
}
