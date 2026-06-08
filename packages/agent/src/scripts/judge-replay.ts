import "dotenv/config";
import OpenAI from "openai";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { ReasoningEffort } from "openai/resources/shared";
import { config as loadEnv } from "dotenv";
import { fetchTraceById, type Observation } from "./langfuse-api";

// A/B replay over a captured trace. Both tests reduce to ONE pattern: take a
// generation's recorded input (which already pins everything but the model —
// the compiler's input holds the signal, the composer's input holds the
// already-rendered tool results) and replay it under a different model.
//
//   Test A (plan):    replay the `planner` generation     -> a Workflow JSON
//   Test B (compose): replay the `llm_compose` generation -> a digest text
//
//   tsx judge-replay.ts <traceId>                 # stage 1: extract + inspect
//   tsx judge-replay.ts <traceId> --plan <model>  # Test A: A=original vs B=model
//
// Stage 3 (pairwise judge of A vs B) comes next.

loadEnv({ path: ".env.agent" });

// ─── providers ───────────────────────────────────────────────────────

const openai = new OpenAI({ apiKey: process.env.OPENAI_API_KEY });
const deepseek = new OpenAI({
  apiKey: process.env.DEEPSEEK_API_KEY,
  baseURL: "https://api.deepseek.com",
});
function clientFor(model: string): OpenAI {
  return model.startsWith("deepseek") ? deepseek : openai;
}

// ─── helpers ─────────────────────────────────────────────────────────

function truncate(s: string, n: number): string {
  return s.length <= n ? s : `${s.slice(0, n)}… (+${s.length - n} chars)`;
}

function argFlag(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

interface ChatMessage {
  role: string;
  content: unknown;
}

function asMessages(input: unknown): ChatMessage[] {
  if (!Array.isArray(input)) return [];
  return input.filter(
    (m): m is ChatMessage => typeof m === "object" && m !== null && "role" in m,
  );
}

function messageText(content: unknown): string {
  return typeof content === "string" ? content : JSON.stringify(content);
}

// Rebuild ChatCompletionMessageParam[] from the recorded input — no cast: we
// reconstruct each message from its role + stringified content. compose/plan
// inputs are only system/user (no tool messages).
function toChatMessages(input: unknown): ChatCompletionMessageParam[] {
  return asMessages(input).map((m) => {
    const content = messageText(m.content);
    if (m.role === "system") return { role: "system", content };
    if (m.role === "assistant") return { role: "assistant", content };
    return { role: "user", content };
  });
}

// The last generation whose name starts with `prefix`. For the compiler that's
// the last `attempt-N` (the successful one after retries); compose is single.
function findGeneration(obs: Observation[], prefix: string): Observation | null {
  const matches = obs
    .filter((o) => o.type === "GENERATION" && o.name.startsWith(prefix))
    .sort((a, b) => new Date(a.startTime).getTime() - new Date(b.startTime).getTime());
  return matches.length > 0 ? matches[matches.length - 1]! : null;
}

function outputText(gen: Observation): string {
  return typeof gen.output === "string" ? gen.output : JSON.stringify(gen.output);
}

async function replayGeneration(
  messages: ChatCompletionMessageParam[],
  model: string,
  jsonMode: boolean,
  reasoningEffort?: ReasoningEffort,
): Promise<string> {
  const res = await clientFor(model).chat.completions.create({
    model,
    messages,
    ...(jsonMode ? { response_format: { type: "json_object" } } : {}),
    ...(reasoningEffort ? { reasoning_effort: reasoningEffort } : {}),
  });
  return res.choices[0]?.message.content ?? "";
}

function printWorkflow(label: string, model: string | null, json: string): void {
  console.log(`\n=== ${label} (${model ?? "—"}) ===`);
  let wf: unknown;
  try {
    wf = JSON.parse(json);
  } catch (e) {
    console.log(`(invalid JSON: ${(e as Error).message})`);
    console.log(truncate(json, 400));
    return;
  }
  const steps = (wf as { steps?: unknown[] }).steps ?? [];
  console.log(`${steps.length} top-level steps:`);
  printSteps(steps, "  ");
}

function printSteps(steps: unknown[], indent: string): void {
  for (const s of steps) {
    if (typeof s !== "object" || s === null) continue;
    const step = s as Record<string, unknown>;
    const parts = [String(step.kind ?? "?")];
    if (step.tool) parts.push(`${String(step.tool)}(${JSON.stringify(step.args ?? {})})`);
    if (step.skill) parts.push(`skill=${String(step.skill)} preset=${String(step.preset)}`);
    console.log(`${indent}- ${parts.join(" ")}`);
    if (Array.isArray(step.steps)) printSteps(step.steps, `${indent}  `);
  }
}

// ─── modes ───────────────────────────────────────────────────────────

function inspect(observations: Observation[]): void {
  for (const [label, prefix] of [
    ["PLANNER (Test A input)", "attempt-"],
    ["COMPOSE (Test B input)", "llm_compose"],
  ] as const) {
    const gen = findGeneration(observations, prefix);
    console.log(`\n=== ${label} ===`);
    if (!gen) {
      console.log("(not found)");
      continue;
    }
    const msgs = asMessages(gen.input);
    console.log(`name=${gen.name}  model=${gen.model ?? "—"}  messages=${msgs.length}`);
    for (const m of msgs) {
      const text = messageText(m.content);
      console.log(`  • ${m.role} (${text.length} chars): ${truncate(text, 180).replace(/\n/g, " ")}`);
    }
    console.log(`output (${outputText(gen).length} chars): ${truncate(outputText(gen), 300)}`);
  }
}

// Always sample N times — one lucky first shot is a poor metric in a
// non-deterministic system. Each sample is an INDEPENDENT call on the same
// input (no retry-feedback between them), so the spread reflects the model's
// raw reliability at building a workflow, not the retry loop's.
const SAMPLES = 5;

// Lite structural validity — JSON-parses + has a steps[] where every step
// carries a `kind`. Catches the format failures (markdown wrapper, trailing
// text) without needing the full tool/skill enum schema.
function validateWorkflowLite(json: string): { ok: boolean; error?: string; steps?: number } {
  let wf: unknown;
  try {
    wf = JSON.parse(json);
  } catch (e) {
    return { ok: false, error: `JSON: ${(e as Error).message}` };
  }
  if (typeof wf !== "object" || wf === null) return { ok: false, error: "not an object" };
  const steps = (wf as Record<string, unknown>).steps;
  if (!Array.isArray(steps)) return { ok: false, error: "missing steps[]" };
  for (const s of steps) {
    if (typeof s !== "object" || s === null || !("kind" in s)) {
      return { ok: false, error: "a step has no kind" };
    }
  }
  return { ok: true, steps: steps.length };
}

// Test A: replay the planner's input under model B, SAMPLES times. A is the
// original output (one reference); B is the distribution of fresh samples.
async function testPlan(
  observations: Observation[],
  modelB: string,
  reasoningEffort?: ReasoningEffort,
): Promise<void> {
  const planner = findGeneration(observations, "attempt-");
  if (!planner) throw new Error("no planner generation in trace");
  const messages = toChatMessages(planner.input);

  printWorkflow("PLAN A — original", planner.model, outputText(planner));

  const thinkLabel = reasoningEffort ? ` (thinking=${reasoningEffort})` : "";
  console.log(`\n=== PLAN B — ${modelB}${thinkLabel}, ${SAMPLES} independent samples ===`);
  const samples = await Promise.all(
    Array.from({ length: SAMPLES }, () => replayGeneration(messages, modelB, true, reasoningEffort)),
  );
  let valid = 0;
  samples.forEach((out, i) => {
    const v = validateWorkflowLite(out);
    if (v.ok) valid += 1;
    console.log(`\n[sample ${i + 1}] ${v.ok ? `✓ valid (${v.steps} steps)` : `✗ ${v.error}`}`);
    if (v.ok) {
      printSteps((JSON.parse(out) as { steps: unknown[] }).steps, "  ");
    } else {
      console.log(`  ${truncate(out, 220).replace(/\n/g, " ")}`);
    }
  });
  console.log(`\nB valid: ${valid}/${SAMPLES}`);
}

async function main(): Promise<void> {
  const traceId = process.argv[2];
  if (!traceId || traceId.startsWith("--")) {
    console.error("usage: tsx judge-replay.ts <traceId> [--plan <model>]");
    process.exit(1);
  }
  const { trace, observations } = await fetchTraceById(traceId);
  console.log(`trace ${traceId} · ${trace.name} · tags=[${trace.tags.join(",")}]`);

  const planModel = argFlag("--plan");
  if (planModel !== undefined) {
    const thinking: ReasoningEffort | undefined = process.argv.includes("--thinking")
      ? "high"
      : undefined;
    await testPlan(observations, planModel || "gpt-5.4-mini", thinking);
  } else {
    inspect(observations);
  }
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
