import "dotenv/config";
import "./openai-native-fetch";
import path from "node:path";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { runHttpTransport, type ConnectableServer } from "./http-transport";
import { createServer } from "./mcp-server";
import { createTelegramLog, startTelegramPoller } from "./services/telegram";
import { createUserbot, createUserbotSessions } from "./services/telegram/userbot";
import { createGmailModule, startGmailPoller } from "./services/gmail";
import { createScheduledTasks, startSchedulerPoller } from "./services/scheduler";
import { createSignals } from "./services/signals";
import { createSettings } from "./services/settings";
import { createGuardedFetch } from "./services/fetch";
import { createFileAccess, defaultReadRoots } from "./services/files";
import { createPgClient, databaseUrl } from "./db/pg/client";
import { connectStateDb } from "./db/state";
import { createNewsModule, startNewsModule } from "./services/news";
import { createKnowledgeModule } from "./services/knowledge";
import { createMemoryModule } from "./services/memory";
import { createSkillsModule } from "./services/skills";
import { createGatewayModule, loadGatewayConfig } from "./services/gateway";
import { parseToolsets } from "./toolsets";

async function main(): Promise<void> {
  // Postgres must be up and migrated before any tool handler or poller
  // touches news_items or the MCP state.
  const pg = createPgClient();
  await pg.ensureReady();
  const state = await connectStateDb(databaseUrl());

  const signals = createSignals(state);
  const settings = createSettings(state);
  const scheduler = createScheduledTasks(state);
  const telegramLog = createTelegramLog(state);
  const gmail = createGmailModule({ db: state });
  const userbot = createUserbot(createUserbotSessions(state));
  const fetcher = createGuardedFetch();
  const files = createFileAccess({ roots: defaultReadRoots(process.env, process.cwd()) });

  const newsModule = createNewsModule({ db: pg.db, userbot, fetcher });
  const knowledgeModule = createKnowledgeModule({ db: pg.db });
  const memoryModule = createMemoryModule({ db: pg.db });
  const skillsModule = createSkillsModule({
    liveDir: path.resolve(process.cwd(), "skills"),
    defaultsDir: path.resolve(process.cwd(), "skills.default"),
  });

  // MCP_TOOLSETS narrows the tool surface for an instance serving a specific
  // audience (the ChatGPT tunnel gets news-read,telegram-send,skills). Unset → the
  // full surface, exactly as before.
  const toolsets = parseToolsets(process.env.MCP_TOOLSETS);
  // Who this instance writes to shared memory as. One MCP process serves one
  // audience, so the instance knows its own identity — the client does not
  // declare it (a self-declared actor would be forgeable and bound nothing).
  const memoryActor = process.env.MCP_MEMORY_ACTOR ?? "mcp";
  if (toolsets.restricted) {
    console.error(`[mcp] MCP_TOOLSETS=${toolsets.names.join(",")} — restricted tool surface`);
  }

  // Gateway: if any third-party MCP upstreams are configured + resolvable, front
  // own-MCP with the aggregating gateway so the agent sees one merged, namespaced
  // tool list. With no upstreams, serve own-MCP directly — zero behaviour change.
  //
  // A restricted instance never attaches the gateway: upstream tools arrive
  // namespaced at runtime (tavily__*, …), so they cannot be expressed in the
  // allow-list and would otherwise leak past it — and third-party calls cost
  // money per call. It is also why a restricted instance can afford a server
  // per session: there are no upstream connections to duplicate.
  const upstreams = toolsets.restricted ? [] : loadGatewayConfig();

  const createEndpoint = async (): Promise<ConnectableServer> => {
    const ownServer = createServer({
      news: newsModule.repository,
      knowledge: knowledgeModule.repository,
      skills: skillsModule.catalog,
      memory: memoryModule.service,
      memoryActor,
      signals,
      settings,
      scheduler,
      telegramLog,
      gmail,
      userbot,
      fetcher,
      files,
      toolsets: toolsets.names,
    });
    if (upstreams.length === 0) return ownServer;
    return (await createGatewayModule({ ownServer, upstreams })).server;
  };

  const transport = (process.env.MCP_TRANSPORT ?? "stdio").toLowerCase();

  if (transport === "http") {
    const port = Number(process.env.MCP_PORT ?? 3000);
    if (!Number.isFinite(port)) throw new Error(`MCP_PORT must be numeric, got ${process.env.MCP_PORT}`);
    await runHttpTransport({ port, createEndpoint, multiSession: toolsets.restricted });
  } else {
    const stdio = new StdioServerTransport();
    await (await createEndpoint()).connect(stdio);
  }

  // MCP_NO_POLLERS lets an eval harness spin up a tools-only MCP (gateway +
  // tool handlers) without the event pollers. The Telegram getUpdates poll is
  // exclusive — a second poller against the same bot 409s the droplet — so the
  // GAIA bench (and any local tool smoke-test) sets this to borrow the tool
  // surface without competing for signals.
  if (process.env.MCP_NO_POLLERS === "1") {
    console.error("[mcp] MCP_NO_POLLERS=1 — tools only, pollers disabled");
    return;
  }

  startTelegramPoller({ log: telegramLog, signals });
  startGmailPoller({ gmail, signals });
  startSchedulerPoller({ tasks: scheduler, settings, signals });
  startNewsModule(newsModule);
}

main().catch((err) => {
  console.error("mcp-tools server crashed", err);
  process.exit(1);
});
