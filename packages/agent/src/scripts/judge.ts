import "dotenv/config";
import { writeFileSync } from "node:fs";
import OpenAI from "openai";
import { config as loadEnv } from "dotenv";
import { fetchRecentTraces } from "./langfuse-api";
import { createAgentDb } from "../db/client";
import { createTraceStore } from "../db/trace-store";
import { assembleNodeMaterials, type NodeMaterial } from "../judging/materials";
import { judgeNodeWithOpenAi } from "../judging/openai-judge";
import { createCodexClient } from "../judging/codex-client";
import { judgeNodeWithCodex } from "../judging/codex-judge";
import { nodeSummaryLine, printNodeJudgement, printTraceHeader } from "../judging/print";
import { rubricFor, type NodeJudgement } from "../judging/schema";
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

// One judge bound to its provider/client, reused across a trace's nodes.
function makeNodeJudge(
  provider: JudgeProvider,
  openai: OpenAI | null,
): (node: NodeMaterial) => Promise<NodeJudgement> {
  if (provider === "codex") {
    const codex = createCodexClient();
    return (node) => judgeNodeWithCodex(codex, node);
  }
  return (node) => judgeNodeWithOpenAi(openai!, node);
}

async function judgeOne(
  source: TraceSource,
  provider: JudgeProvider,
  openai: OpenAI | null,
  traceId: string,
): Promise<void> {
  const { nodes } = await assembleNodeMaterials(source, traceId);
  console.error(`[judge] trace ${traceId} · provider=${provider} · ${nodes.length} judgeable nodes`);
  printTraceHeader(traceId);

  const judge = makeNodeJudge(provider, openai);
  const summary: string[] = [];
  for (const node of nodes) {
    const header = { label: node.label, kind: node.kind, skill: node.skill };
    const verdict = await judge(node);
    printNodeJudgement(header, verdict);
    summary.push(nodeSummaryLine(header, verdict));
  }
  if (nodes.length > 1) {
    console.log(`\n── summary ──`);
    for (const line of summary) console.log(line);
  }
}

// Write per-node judge materials (no LLM call) for the in-session judge-trace
// skill: one block per node, each the exact user prompt that node's rubric
// would receive (contract + node IO), minus the trailing scoring instruction.
function renderNodeDump(node: NodeMaterial, index: number): string {
  const userPrompt = rubricFor(node.kind).buildUserPrompt(
    node.skill,
    node.contract,
    node.inputText,
    node.outputText,
  );
  const materials = userPrompt.replace(/\n\n(?:Score this|Extract F's)[\s\S]*$/, "");
  return [
    `## NODE ${index + 1} · ${node.label} · ${node.kind} · skill ${node.skill}`,
    "",
    materials,
  ].join("\n");
}

async function dumpOne(source: TraceSource, traceId: string): Promise<void> {
  const { nodes } = await assembleNodeMaterials(source, traceId);
  const body = [
    `# JUDGE MATERIALS · trace ${traceId} · ${nodes.length} nodes`,
    "",
    ...nodes.map((n, i) => renderNodeDump(n, i)),
    "",
  ].join("\n\n");
  const path = `/tmp/judge-dump-${traceId}.md`;
  writeFileSync(path, body);
  console.log(`[judge] dumped ${traceId} · ${nodes.length} nodes · ${body.length} chars → ${path}`);
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
