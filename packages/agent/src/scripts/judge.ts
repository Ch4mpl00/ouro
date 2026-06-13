import "dotenv/config";
import { writeFileSync } from "node:fs";
import OpenAI from "openai";
import { config as loadEnv } from "dotenv";
import { fetchRecentTraces } from "./langfuse-api";
import { createAgentDb } from "../db/client";
import { createTraceStore } from "../db/trace-store";
import { assembleMaterials } from "../judging/materials";
import { judgeWithOpenAi } from "../judging/openai-judge";
import { createCodexClient } from "../judging/codex-client";
import { judgeWithCodex } from "../judging/codex-judge";
import { printFaithfulness, printScorecard } from "../judging/print";
import { buildUserPrompt } from "../judging/schema";
import { createLangfuseTraceSource, createLocalTraceSource, type TraceSource } from "../judging/trace-source";

loadEnv({ path: ".env.agent" });

type JudgeProvider = "openai" | "codex";

interface CliOpts {
  dump: boolean;
  provider: JudgeProvider;
  args: string[];
}

function parseArgs(argv: string[]): CliOpts {
  const args: string[] = [];
  let dump = false;
  let provider: JudgeProvider = "openai";
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    if (arg === "--dump") {
      dump = true;
      continue;
    }
    if (arg === "--provider") {
      const value = argv[++i];
      if (value !== "openai" && value !== "codex") {
        throw new Error("--provider must be openai or codex");
      }
      provider = value;
      continue;
    }
    if (arg.startsWith("--provider=")) {
      const value = arg.slice("--provider=".length);
      if (value !== "openai" && value !== "codex") {
        throw new Error("--provider must be openai or codex");
      }
      provider = value;
      continue;
    }
    args.push(arg);
  }
  return { dump, provider, args };
}

async function judgeOne(
  source: TraceSource,
  provider: JudgeProvider,
  openai: OpenAI | null,
  traceId: string,
): Promise<void> {
  const m = await assembleMaterials(source, traceId);

  console.error(
    `[judge] trace ${traceId} · provider=${provider} · skill=${m.skillName ?? "—"} · ${m.obsCount} obs · ` +
      `transcript ${m.transcript.length} chars`,
  );

  const params = {
    skillName: m.skillName,
    composerContract: m.composerContract,
    orchestratorContract: m.orchestratorContract,
    transcript: m.transcript,
  };
  const result = provider === "codex"
    ? await judgeWithCodex(createCodexClient(), params)
    : await judgeWithOpenAi(openai!, params);
  printScorecard(traceId, m.skillName, result.scorecard);
  printFaithfulness(result.faithfulness);
}

async function dumpOne(source: TraceSource, traceId: string): Promise<void> {
  const m = await assembleMaterials(source, traceId);
  const body = [
    `# JUDGE MATERIALS · trace ${traceId} · composer skill ${m.skillName ?? "—"}`,
    "",
    buildUserPrompt(m.skillName, m.composerContract, m.orchestratorContract, m.transcript)
      .replace(/\n\nScore this run\.[\s\S]*$/, ""),
    "",
  ].join("\n");
  const path = `/tmp/judge-dump-${traceId}.md`;
  writeFileSync(path, body);
  console.log(
    `[judge] dumped ${traceId} · skill=${m.skillName ?? "—"} · ${m.obsCount} obs · ` +
      `${body.length} chars → ${path}`,
  );
}

async function main(): Promise<void> {
  const { args, dump, provider } = parseArgs(process.argv.slice(2));

  let openai: OpenAI | null = null;
  if (!dump && provider === "openai") {
    const apiKey = process.env.OPENAI_API_KEY;
    if (!apiKey) {
      console.error("OPENAI_API_KEY missing in env");
      process.exit(1);
    }
    openai = new OpenAI({ apiKey });
  }

  // Local mirror first (fast, and present when run on the droplet), Langfuse
  // as fallback for ids not mirrored locally. --recent still lists from
  // Langfuse (recent prod traces), below.
  const db = createAgentDb();
  const source = createLocalTraceSource(createTraceStore(db), createLangfuseTraceSource());

  const runOne = (traceId: string): Promise<void> =>
    dump ? dumpOne(source, traceId) : judgeOne(source, provider, openai, traceId);

  if (args[0] === "--recent") {
    const parsed = Number(args[1] ?? "5");
    const n = Number.isFinite(parsed) && parsed > 0 ? parsed : 5;
    const recent = await fetchRecentTraces(n);
    console.error(`[judge] fetched ${recent.length} recent traces`);
    for (const t of recent) {
      console.log(`\n${"—".repeat(72)}`);
      console.log(`trace ${t.id} · ${t.name} · [${t.tags.join(",")}] · ${t.timestamp}`);
      try {
        await runOne(t.id);
      } catch (err) {
        console.error(`[judge] ${t.id} failed: ${err instanceof Error ? err.message : String(err)}`);
      }
    }
    return;
  }

  const traceId = args[0];
  if (!traceId || traceId.startsWith("--")) {
    console.error(
      "usage: pnpm judge [--provider openai|codex] [--dump] <traceId>  |  pnpm judge [--provider openai|codex] [--dump] --recent [N]",
    );
    process.exit(1);
  }
  await runOne(traceId);
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
