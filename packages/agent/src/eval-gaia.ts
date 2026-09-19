// Registered before anything pulls in `openai` — the SDK auto-detects its
// shim the first time it is imported, so this stays a static import at the
// very top even though it is an entry-point concern (see
// ./openai-native-fetch).
import "./openai-native-fetch";
import { readFileSync, existsSync, mkdirSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { config as loadEnv } from "dotenv";
import OpenAI from "openai";
import {
  DEEPSEEK_BASE_URL,
  DEFAULT_PRESETS,
  GEMINI_BASE_URL,
  createDeepseekProvider,
  createEngine,
  createGeminiProvider,
  createOpenAiProvider,
  createSessionContext,
  createSkillStore,
  gatherEnvData,
  withRetry,
  type EnvDataDeps,
} from "./agent-loop";
import { createCodexClient } from "./codex-client";
import { createAgentDb, createMemoryStore, createTraceStore } from "./db";
import { connectMcp, type McpHandle } from "./mcp-client";
import { createLocalRecorderTracer, langfuseTracerFromEnv, teeTracer } from "./tracing";
import { createWorkflowRunner, type WorkflowSignal } from "./workflow";

// The GAIA benchmark harness, end to end: the dataset loader, the scorer, the
// capability filter that skips tasks this agent cannot reach, the MCP client
// wrapper that suppresses side effects, and the runner that drives it.
//
// Read top to bottom — each section only depends on the ones above it:
//
//   dataset       load and parse the GAIA jsonl + its attachment files
//   scorer        GAIA's exact-match grading rules
//   capabilities  which tasks are reachable with the tools we have
//   bench client  an McpHandle that refuses side-effecting tools
//   run           main(): wire a real engine and walk the task set. Runs only
//                 when this file IS the process entry point (`pnpm bench:gaia`)
//
// Everything it wires is elsewhere: ./agent-loop is the runtime, ./workflow
// the scheduler path, ./mcp-client + ./codex-client the transports, ./db the
// agent-side stores and ./tracing the observability adapters.

// ═══════════════════════════════════════════════════════════════════
// Dataset
// ═══════════════════════════════════════════════════════════════════

// GAIA dataset loader. The repo now ships metadata as parquet (no more
// `metadata.jsonl`), so we read tasks through the HuggingFace datasets-server
// `/rows` API — it returns normalized JSON rows and handles the parquet read
// server-side, no parquet dependency in-process. Attachment files still live
// under the repo at `2023/<split>/<file_name>` and are pulled on demand from
// the hub `resolve` endpoint by `downloadAttachment`. The dataset is gated, so
// every request carries the HF token.

const HF_BASE = "https://huggingface.co/datasets/gaia-benchmark/GAIA/resolve/main";
const ROWS_API = "https://datasets-server.huggingface.co/rows";
// All 165 validation tasks live in this config; we fetch it once and filter
// by level client-side (the per-level configs are just subsets of it).
const CONFIG = "2023_all";
const ROWS_PAGE = 100; // datasets-server hard cap per request

// Cache lives in-repo (gitignored) so a run is reproducible offline once the
// metadata has been pulled at least once. It sits OUTSIDE src/ — it is
// re-fetchable dataset data, not source — and is anchored on this file's
// location, so moving this file means moving this path with it.
const CACHE_DIR = path.resolve(import.meta.dirname, "../eval-fixtures/gaia");

export type GaiaLevel = 1 | 2 | 3;
export type GaiaSplit = "validation" | "test";

// One row of GAIA `metadata.jsonl`. Field names match the dataset verbatim
// (capitalised, spaced) — we keep them as-is rather than renaming so the
// mapping to the source is obvious.
export interface GaiaTask {
  taskId: string;
  question: string;
  level: GaiaLevel;
  // Empty string on the test split (answers are leaderboard-only) and on any
  // task without a published answer.
  finalAnswer: string;
  // "" when the task has no attachment.
  fileName: string;
}

interface RawGaiaRow {
  task_id: string;
  Question: string;
  Level: string | number;
  "Final answer"?: string;
  file_name?: string;
}

function hfToken(): string {
  const token = process.env.HUGGING_FACE_KEY;
  if (!token) {
    throw new Error(
      "HUGGING_FACE_KEY is not set in .env.agent — required to pull the gated GAIA dataset",
    );
  }
  return token;
}

interface RowsResponse {
  rows: { row: RawGaiaRow }[];
  num_rows_total: number;
}

// Pull every row of the split via the datasets-server `/rows` API, paginating
// in `ROWS_PAGE`-sized windows. Cached to a single JSON file so subsequent
// runs are offline + deterministic.
async function fetchAllRows(split: GaiaSplit): Promise<RawGaiaRow[]> {
  mkdirSync(CACHE_DIR, { recursive: true });
  const cachePath = path.join(CACHE_DIR, `${split}.rows.json`);
  if (existsSync(cachePath)) {
    return JSON.parse(readFileSync(cachePath, "utf8")) as RawGaiaRow[];
  }

  const out: RawGaiaRow[] = [];
  let offset = 0;
  for (;;) {
    const url =
      `${ROWS_API}?dataset=gaia-benchmark%2FGAIA&config=${CONFIG}` +
      `&split=${split}&offset=${offset}&length=${ROWS_PAGE}`;
    const res = await fetch(url, { headers: { Authorization: `Bearer ${hfToken()}` } });
    if (!res.ok) {
      const body = await res.text().catch(() => "");
      throw new Error(
        `GAIA rows fetch failed (${res.status} ${res.statusText}) at offset ${offset}. ` +
          (res.status === 401 || res.status === 403
            ? "Accept the dataset terms at https://huggingface.co/datasets/gaia-benchmark/GAIA " +
              "and give the token 'read access to public gated repos'. "
            : "") +
          body.slice(0, 200),
      );
    }
    const page = (await res.json()) as RowsResponse;
    out.push(...page.rows.map((r) => r.row));
    offset += ROWS_PAGE;
    if (offset >= page.num_rows_total || page.rows.length === 0) break;
  }

  writeFileSync(cachePath, JSON.stringify(out));
  return out;
}

function parseLevel(raw: string | number): GaiaLevel {
  const n = typeof raw === "number" ? raw : Number.parseInt(raw, 10);
  if (n !== 1 && n !== 2 && n !== 3) throw new Error(`unexpected GAIA level: ${raw}`);
  return n;
}

export interface LoadGaiaOpts {
  split?: GaiaSplit;
  // Restrict to a single level, or "all".
  level?: GaiaLevel | "all";
  // Cap the number of tasks (after level filtering). Omit for the full split.
  maxTasks?: number;
}

export async function loadGaiaTasks(opts: LoadGaiaOpts = {}): Promise<GaiaTask[]> {
  const split = opts.split ?? "validation";
  const level = opts.level ?? "all";

  const rows = await fetchAllRows(split);
  const tasks: GaiaTask[] = [];
  for (const row of rows) {
    const task: GaiaTask = {
      taskId: row.task_id,
      question: row.Question,
      level: parseLevel(row.Level),
      finalAnswer: row["Final answer"] ?? "",
      fileName: row.file_name ?? "",
    };
    if (level !== "all" && task.level !== level) continue;
    tasks.push(task);
  }

  return typeof opts.maxTasks === "number" ? tasks.slice(0, opts.maxTasks) : tasks;
}

/**
 * Download a task's attachment to the local cache and return its absolute
 * path, or null when the task has no file. The agent reaches it via the
 * `read_file` / `read_pdf` tools, which take a local path.
 */
export async function downloadAttachment(
  task: GaiaTask,
  split: GaiaSplit = "validation",
): Promise<string | null> {
  if (!task.fileName) return null;
  const dir = path.join(CACHE_DIR, "files", split);
  mkdirSync(dir, { recursive: true });
  const localPath = path.join(dir, task.fileName);
  if (existsSync(localPath)) return localPath;

  const url = `${HF_BASE}/2023/${split}/${encodeURIComponent(task.fileName)}`;
  const res = await fetch(url, { headers: { Authorization: `Bearer ${hfToken()}` } });
  if (!res.ok) {
    throw new Error(
      `GAIA attachment fetch failed (${res.status}) for task ${task.taskId} file ${task.fileName}`,
    );
  }
  const buf = Buffer.from(await res.arrayBuffer());
  writeFileSync(localPath, buf);
  return localPath;
}

// ═══════════════════════════════════════════════════════════════════
// Scorer
// ═══════════════════════════════════════════════════════════════════

// Faithful TS port of the official GAIA scorer (HF `gaia-benchmark/GAIA`
// `scorer.py`, mirrored in camel-ai `camel/benchmarks/gaia.py`). Quasi
// exact-match with type-routed normalization: the ground-truth type decides
// how the comparison is done.
//
//   float GT          → strip $ % , from the model answer, compare as numbers
//   list GT (,/;)     → split both, element-wise (numeric or string per elem)
//   string GT         → lowercase, strip ALL whitespace + punctuation, compare
//
// Do NOT "improve" the normalization — published GAIA numbers depend on this
// exact behaviour (e.g. "D.R M.A.R.T.I.N L.U.T.H.E.R K.I.N.G J.R" is meant to
// match "Dr. Martin Luther King Jr.").

// Optional sign, integer/decimal/exponent — the subset of Python `float()`
// syntax GAIA ground truths use (no underscores, no hex).
const PY_FLOAT_RE = /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/;

// Mirror Python `float(str)`: trims surrounding whitespace, accepts decimals
// and scientific notation plus the inf/nan literals, rejects everything else
// (empty/whitespace-only string, thousands commas, trailing units). Returns
// null when the string is not a Python-parseable float.
function pyParseFloat(s: string): number | null {
  const t = s.trim();
  if (t === "") return null;
  const lower = t.toLowerCase().replace(/^[+-]/, "");
  if (lower === "inf" || lower === "infinity") return t.startsWith("-") ? -Infinity : Infinity;
  if (lower === "nan") return NaN;
  if (!PY_FLOAT_RE.test(t)) return null;
  return Number(t);
}

function isFloat(s: string): boolean {
  return pyParseFloat(s) !== null;
}

// normalize_number_str: strip currency/percent/thousands marks, then parse.
// Returns +Infinity on failure so a non-numeric model answer can never
// accidentally equal a finite numeric ground truth.
function normalizeNumberStr(numberStr: string): number {
  let s = numberStr;
  for (const ch of ["$", "%", ","]) s = s.split(ch).join("");
  const v = pyParseFloat(s);
  return v === null ? Number.POSITIVE_INFINITY : v;
}

function splitString(s: string): string[] {
  return s.split(/[,;]/);
}

// Python `string.punctuation`.
const PUNCTUATION = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";
const PUNCT_SET = new Set(PUNCTUATION.split(""));

// normalize_str: drop ALL whitespace, lowercase, and (when removePunct)
// strip every punctuation char. Whole-string comparisons remove punctuation;
// per-list-element string comparisons do NOT (matches the Python defaults).
function normalizeStr(input: string, removePunct = true): string {
  const lower = input.replace(/\s/g, "").toLowerCase();
  if (!removePunct) return lower;
  let out = "";
  for (const ch of lower) if (!PUNCT_SET.has(ch)) out += ch;
  return out;
}

/**
 * GAIA's official success criterion: returns true iff `modelAnswer` matches
 * `groundTruth` under the type-routed normalization above.
 */
export function questionScorer(modelAnswer: string, groundTruth: string): boolean {
  if (isFloat(groundTruth)) {
    return normalizeNumberStr(modelAnswer) === pyParseFloat(groundTruth);
  }

  if (groundTruth.includes(",") || groundTruth.includes(";")) {
    const gtElems = splitString(groundTruth);
    const maElems = splitString(modelAnswer);
    if (gtElems.length !== maElems.length) return false;
    return gtElems.every((gtElem, i) => {
      const maElem = maElems[i]!;
      if (isFloat(gtElem)) {
        return normalizeNumberStr(maElem) === pyParseFloat(gtElem);
      }
      return normalizeStr(maElem, false) === normalizeStr(gtElem, false);
    });
  }

  return normalizeStr(modelAnswer) === normalizeStr(groundTruth);
}

// ═══════════════════════════════════════════════════════════════════
// Capabilities
// ═══════════════════════════════════════════════════════════════════

// Capability model — what a GAIA task needs from the toolbelt, so we can run
// ONLY the tasks our current tools can actually reach and keep tool-coverage
// gaps out of the accuracy number. This is the first, mechanical half of the
// PR2 failure taxonomy: a task excluded here is a known tool-gap, not a
// loop/reasoning result.
//
// `web` is the baseline (search + read a URL). Everything else is a "special"
// capability detected from the attachment type or a cue in the question.
// `calc` (exact arithmetic/logic) is intentionally NOT modelled as a gap —
// it is always available via the `code_agent` DSL step.

export type Capability =
  | "web"
  | "pdf_read"
  | "file_read" // plain text / source / office docs
  | "excel"
  | "vision"
  | "audio"
  | "video"
  | "browser"; // interactive JS-rendered navigation (long tail)

const EXT_CAPABILITY: Record<string, Capability> = {
  ".pdf": "pdf_read",
  ".txt": "file_read",
  ".json": "file_read",
  ".jsonld": "file_read",
  ".xml": "file_read",
  ".py": "file_read",
  ".docx": "file_read",
  ".doc": "file_read",
  ".pptx": "file_read",
  ".ppt": "file_read",
  ".csv": "excel",
  ".xlsx": "excel",
  ".xls": "excel",
  ".png": "vision",
  ".jpg": "vision",
  ".jpeg": "vision",
  ".gif": "vision",
  ".mp3": "audio",
  ".wav": "audio",
  ".m4a": "audio",
  ".flac": "audio",
  ".mp4": "video",
  ".mov": "video",
  ".avi": "video",
};

const VIDEO_CUE = /youtube\.com|youtu\.be|watch\?v=|\bvideo\b|\.mp4\b/i;
const AUDIO_CUE = /\baudio\b|listen to|\bpodcast\b|\brecording\b|\.mp3\b/i;

function extOf(fileName: string): string {
  const i = fileName.lastIndexOf(".");
  return i >= 0 ? fileName.slice(i).toLowerCase() : "";
}

/**
 * The capabilities a task requires. Always includes `web`; adds a special
 * capability per attachment type and per multimodal cue in the question text.
 */
export function requiredCapabilities(task: GaiaTask): Set<Capability> {
  const caps = new Set<Capability>(["web"]);

  if (task.fileName) {
    const cap = EXT_CAPABILITY[extOf(task.fileName)];
    // Unknown extension → treat as a file_read gap rather than silently
    // assuming it's reachable.
    caps.add(cap ?? "file_read");
  }

  if (VIDEO_CUE.test(task.question)) caps.add("video");
  if (AUDIO_CUE.test(task.question)) caps.add("audio");

  return caps;
}

// The toolbelt wired on the current zero-infra path: Tavily web search +
// extract (+ always-on local `code_agent` for calc, which is not a gap cap).
// Widen this as tools land — adding "pdf_read"/"file_read" once the own-MCP
// readers are in the bench path flips those tasks to accessible.
export const AVAILABLE_NOW: ReadonlySet<Capability> = new Set<Capability>(["web"]);

export function isAccessible(
  task: GaiaTask,
  available: ReadonlySet<Capability> = AVAILABLE_NOW,
): boolean {
  for (const cap of requiredCapabilities(task)) {
    if (!available.has(cap)) return false;
  }
  return true;
}

/** The special capabilities a task needs that the toolbelt lacks (for reporting). */
export function missingCapabilities(
  task: GaiaTask,
  available: ReadonlySet<Capability> = AVAILABLE_NOW,
): Capability[] {
  return [...requiredCapabilities(task)].filter((c) => !available.has(c));
}

// ═══════════════════════════════════════════════════════════════════
// Bench MCP client
// ═══════════════════════════════════════════════════════════════════

// Bench-mode MCP client: an `McpHandle` wrapper that exposes ONLY a curated
// read-only toolbelt to the agent and suppresses every side-effecting tool.
// This is the isolation seam from [[eval-agent-e2e]] — no prod tool code is
// touched, and no prod DB is written, because the write tools never reach the
// compiler's schema enum (so they can't be emitted) and any stray call is
// recorded + no-op'd.
//
// The allowlist is the GAIA Phase-A toolbelt: web search + web read + file
// readers. `code_agent` is a DSL step kind (not an MCP tool), so it is always
// available via the executor's injected Codex client and needs no entry here.

export const GAIA_READONLY_ALLOWLIST = [
  // Via our gateway (prefixed) …
  "tavily__tavily_search", // web search (API)
  "tavily__tavily_extract", // fetch + readability a specific URL
  // … or pointing straight at Tavily's hosted MCP (unprefixed). Listing both
  // lets the bench run against either the full own-MCP or Tavily-direct (the
  // zero-infra path: no local PG / gateway needed for a web-lookup L1 slice).
  "tavily_search",
  "tavily_extract",
  "read_pdf",
  "read_file",
  "get_timezone", // read-only — lets gatherEnvData resolve tz without a suppression
] as const;

export interface SideEffectCall {
  name: string;
  args: Record<string, unknown>;
}

export interface BenchMcpClient extends McpHandle {
  // Every suppressed side-effect call, in order — surfaced in the run report
  // so we can see whether the agent tried to act on the world.
  readonly sideEffectLog: readonly SideEffectCall[];
}

export interface BenchMcpClientOpts {
  // Override the default GAIA allowlist (e.g. to add an Excel reader later).
  allowlist?: readonly string[];
}

export function createBenchMcpClient(
  real: McpHandle,
  opts: BenchMcpClientOpts = {},
): BenchMcpClient {
  const allow = new Set<string>(opts.allowlist ?? GAIA_READONLY_ALLOWLIST);
  const tools = real.tools.filter((t) => allow.has(t.function.name));

  const present = new Set(tools.map((t) => t.function.name));
  const missing = [...allow].filter((n) => !present.has(n));
  if (missing.length > 0) {
    console.warn(
      `[bench-mcp] allowlisted tools missing from the MCP server: ${missing.join(", ")}. ` +
        "Check the gateway upstreams (TAVILY_API_KEY) / tool registration.",
    );
  }
  if (tools.length === 0) {
    throw new Error(
      "[bench-mcp] no allowlisted tools available — the workflow compiler needs a non-empty tool set. " +
        "Is the MCP server (and its gateway) reachable?",
    );
  }

  const sideEffectLog: SideEffectCall[] = [];

  return {
    tools,
    sideEffectLog,
    async callTool(name, args) {
      if (allow.has(name)) return real.callTool(name, args);
      sideEffectLog.push({ name, args });
      return `[bench] side-effect tool "${name}" suppressed — no prod writes in GAIA bench mode.`;
    },
    close: () => real.close(),
  };
}

// ═══════════════════════════════════════════════════════════════════
// Run
// ═══════════════════════════════════════════════════════════════════

loadEnv({ path: ".env.agent" });

// GAIA Tier-1 harness (workflow path). Drives the prod plan→act→replan loop
// (`createWorkflowRunner`) over GAIA questions behind a `BenchMCPClient` — the
// same DI seam the e2e sandbox uses: no prod tool code touched, no prod writes
// (side-effect tools are suppressed). Scores each final answer with the
// official GAIA scorer and prints a per-level table.
//
// Run:  MCP_NO_POLLERS=1 pnpm bench:gaia --level 1 --max-tasks 10
//   (or point at a remote MCP: MCP_TRANSPORT=http MCP_URL=... pnpm bench:gaia ...)

interface CliOpts {
  level: GaiaLevel | "all";
  maxTasks: number | null;
  accessibleOnly: boolean;
  dryRun: boolean;
  // Run only tasks whose taskId starts with one of these (full id or short
  // prefix). Overrides --max-tasks (runs every match). null = no id filter.
  taskIds: string[] | null;
  // Workflow plan→act→replan ceiling. null = level-based default. Raise it
  // for the replan-driven variant (the planner iterates via replan instead
  // of delegating to an llm_agent ReAct loop).
  maxPasses: number | null;
}

function parseArgs(argv: string[]): CliOpts {
  let level: GaiaLevel | "all" = "all";
  let maxTasks: number | null = 10;
  let accessibleOnly = false;
  let dryRun = false;
  let taskIds: string[] | null = null;
  let maxPasses: number | null = null;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--max-passes") {
      maxPasses = Number(argv[++i]);
      if (!Number.isInteger(maxPasses) || maxPasses <= 0)
        throw new Error("--max-passes must be a positive integer");
    } else if (arg === "--task-ids") {
      // Comma-separated ids/prefixes, or "@path" to read them from a file
      // (one per line and/or comma-separated). Handy for re-running failures.
      const v = argv[++i] ?? "";
      const raw = v.startsWith("@") ? readFileSync(v.slice(1), "utf8") : v;
      taskIds = raw
        .split(/[\s,]+/)
        .map((s) => s.trim())
        .filter(Boolean);
      if (taskIds.length === 0) throw new Error("--task-ids resolved to an empty list");
    } else if (arg === "--dry-run") {
      // Print the selected/excluded tasks and exit — no LLM calls, no cost.
      dryRun = true;
    } else if (arg === "--level") {
      const v = argv[++i];
      if (v === "all") level = "all";
      else if (v === "1" || v === "2" || v === "3") level = Number(v) as GaiaLevel;
      else throw new Error("--level must be 1, 2, 3, or all");
    } else if (arg === "--max-tasks") {
      const v = argv[++i];
      maxTasks = v === "all" ? null : Number(v);
      if (maxTasks !== null && (!Number.isFinite(maxTasks) || maxTasks <= 0))
        throw new Error("--max-tasks must be a positive number or 'all'");
    } else if (arg === "--accessible-only") {
      // Run only tasks whose required capabilities the current toolbelt has
      // — keeps known tool-coverage gaps out of the accuracy number.
      accessibleOnly = true;
    }
  }
  return { level, maxTasks, accessibleOnly, dryRun, taskIds, maxPasses };
}

// Apply the explicit --task-ids selection (full id or short prefix). Errors
// loudly on a prefix that matches nothing — a typo'd id should not silently
// shrink the run.
function selectByIds(pool: GaiaTask[], ids: string[]): GaiaTask[] {
  const unmatched = ids.filter((id) => !pool.some((t) => t.taskId.startsWith(id)));
  if (unmatched.length > 0) {
    throw new Error(`--task-ids: no task matches: ${unmatched.join(", ")}`);
  }
  return pool.filter((t) => ids.some((id) => t.taskId.startsWith(id)));
}

// Resolve the task set to run from the CLI opts. Explicit --task-ids wins
// (runs exactly those, ignoring the capability filter + --max-tasks). Else
// apply the accessibility filter (reporting tool-gap exclusions), then the
// --max-tasks cap.
async function selectTasks(opts: CliOpts): Promise<GaiaTask[]> {
  let pool = await loadGaiaTasks({ level: opts.level });
  if (opts.taskIds) return selectByIds(pool, opts.taskIds);
  if (opts.accessibleOnly) {
    reportExcluded(pool.filter((t) => !isAccessible(t)));
    pool = pool.filter((t) => isAccessible(t));
  }
  return opts.maxTasks === null ? pool : pool.slice(0, opts.maxTasks);
}

// Outcome category — a coarse first cut at the failure taxonomy. The full
// tool-coverage-vs-loop/reasoning split is PR2; here we just separate the
// buckets the harness can already tell apart.
type Outcome =
  | "correct"
  | "wrong"
  | "no_answer" // workflow ok but never bound `answer` (contract miss)
  | "compile_fail"
  | "execute_fail"
  | "replan_exhausted"
  | "crash";

interface TaskResult {
  task: GaiaTask;
  outcome: Outcome;
  predicted: string | null;
}

async function main(): Promise<void> {
  const opts = parseArgs(process.argv.slice(2));

  // --dry-run: preview the selection (no API keys, no MCP, no cost) and exit.
  if (opts.dryRun) {
    const selected = await selectTasks(opts);
    console.log(`[dry-run] would run ${selected.length} task(s) (level=${opts.level}):`);
    for (const t of selected) {
      console.log(`  L${t.level} ${t.taskId.slice(0, 8)} :: ${t.question.slice(0, 90)}`);
    }
    return;
  }

  const deepseekApiKey = process.env.DEEPSEEK_API_KEY;
  if (!deepseekApiKey) throw new Error("DEEPSEEK_API_KEY is not set in .env.agent");
  const openaiApiKey = process.env.OPENAI_API_KEY;
  if (!openaiApiKey) throw new Error("OPENAI_API_KEY is not set in .env.agent");

  // Default presets route base/smartest/compiler → OpenAI, smart → DeepSeek;
  // no preset routes to Gemini, so its key is optional (the provider is built
  // anyway to satisfy the engine's shape, but never invoked unless an env
  // override points a model name at "gemini-*").
  const geminiApiKey = process.env.GEMINI_API_KEY ?? "";

  const withEnvModel = (name: keyof typeof DEFAULT_PRESETS, envVar: string) => ({
    ...DEFAULT_PRESETS[name],
    model: process.env[envVar] ?? DEFAULT_PRESETS[name].model,
  });
  const presets = {
    base: withEnvModel("base", "AGENT_BASE_MODEL"),
    smart: withEnvModel("smart", "AGENT_SMART_MODEL"),
    smartest: withEnvModel("smartest", "AGENT_SMARTEST_MODEL"),
    compiler: withEnvModel("compiler", "AGENT_COMPILER_MODEL"),
  };

  const providers = {
    deepseek: withRetry(
      createDeepseekProvider(new OpenAI({ apiKey: deepseekApiKey, baseURL: DEEPSEEK_BASE_URL })),
    ),
    openai: withRetry(createOpenAiProvider(new OpenAI({ apiKey: openaiApiKey }))),
    gemini: withRetry(
      createGeminiProvider(new OpenAI({ apiKey: geminiApiKey, baseURL: GEMINI_BASE_URL })),
    ),
  };

  const db = createAgentDb();
  const memory = createMemoryStore(db);
  const traceStore = createTraceStore(db);
  const skillStore = createSkillStore();
  const codex = createCodexClient();

  // Zero-infra default: with no MCP endpoint configured but a Tavily key
  // present, point straight at Tavily's hosted MCP (web search/extract only).
  // Enough for a web-lookup L1 slice without standing up our own MCP + PG.
  // Set MCP_TRANSPORT/MCP_URL explicitly to use the full own-MCP toolbelt.
  if (!process.env.MCP_URL && process.env.TAVILY_API_KEY) {
    process.env.MCP_TRANSPORT = "http";
    process.env.MCP_URL = `https://mcp.tavily.com/mcp/?tavilyApiKey=${process.env.TAVILY_API_KEY}`;
    console.log("[bench] no MCP_URL set — using Tavily-hosted MCP directly (search/extract only)");
  }

  const realMcp = await connectMcp();
  const mcp = createBenchMcpClient(realMcp);
  console.log(`[bench] toolbelt: ${mcp.tools.map((t) => t.function.name).join(", ")}`);

  // Local mirror always on (agent.db); tee to Langfuse too when creds are
  // present — gives the trace UI + lets the per-node judge score GAIA runs,
  // exactly like the prod supervisor.
  const local = createLocalRecorderTracer(traceStore);
  const langfuse = langfuseTracerFromEnv();
  const tracer = langfuse ? teeTracer(langfuse, local) : local;
  console.log(`[bench] tracing: ${langfuse ? "langfuse + local mirror" : "local mirror only"}`);

  // Unique per-run id so successive benchmark runs don't clobber each other's
  // traces (trace id is the PRIMARY KEY; a fixed gaia:<index> scheme would
  // upsert-overwrite the prior run). Stamped once here, woven into every
  // task's trace id + session id.
  const runId = new Date().toISOString().replace(/[:.]/g, "-");
  console.log(`[bench] run id: ${runId}`);
  const engine = createEngine({
    providers,
    mcp,
    presets,
    skills: [],
    skillStore,
    memory,
    tracer,
    codex,
  });

  const skillEntries = await skillStore.listSkills();
  const NON_WORKFLOW_SKILLS = new Set(["planner", "routing", "recovery"]);
  const knownSkills = skillEntries.map((s) => s.name).filter((n) => !NON_WORKFLOW_SKILLS.has(n));

  const runner = createWorkflowRunner({
    engine,
    readSkill: async (name) => (await skillStore.readSkill(name))?.body ?? null,
    readPatch: (name) => skillStore.readPatch(name),
    mcpTools: mcp.tools,
    knownSkills,
    setMemory: (key, value) => memory.set(key, value),
    codex,
    // Autonomous-loop ceiling. Explicit --max-passes wins; else a level-based
    // default. The replan-driven planner variant needs this raised (it does
    // research as gather→replan hops rather than one llm_agent ReAct loop).
    maxPasses: opts.maxPasses ?? (opts.level === 1 ? 3 : 5),
  });
  if (opts.maxPasses) console.log(`[bench] maxPasses: ${opts.maxPasses}`);

  const envDeps: EnvDataDeps = {
    mcp,
    memory,
    userEmail: process.env.USER_EMAIL ?? null,
  };

  const tasks = await selectTasks(opts);
  console.log(
    `[bench] running ${tasks.length} GAIA task(s) (level=${opts.level}` +
      `${opts.taskIds ? ", task-ids" : opts.accessibleOnly ? ", accessible-only" : ""})\n`,
  );

  const results: TaskResult[] = [];
  for (const [i, task] of tasks.entries()) {
    const result = await runOne(runId, i, task, runner, engine, envDeps);
    results.push(result);
    const gold = task.finalAnswer || "(no gold)";
    console.log(
      `  [${i + 1}/${tasks.length}] L${task.level} ${task.taskId.slice(0, 8)} → ${result.outcome}` +
        `  pred=${JSON.stringify(result.predicted)} gold=${JSON.stringify(gold)}`,
    );
  }

  printReport(results, mcp.sideEffectLog.length);

  await mcp.close();
  await engine.shutdown();
}

async function runOne(
  runId: string,
  index: number,
  task: GaiaTask,
  runner: ReturnType<typeof createWorkflowRunner>,
  engine: ReturnType<typeof createEngine>,
  envDeps: EnvDataDeps,
): Promise<TaskResult> {
  // GAIA attachments are addressed by local path; pass it in the per-signal
  // env addendum (the channel prod uses for source-specific context).
  const filePath = await downloadAttachment(task);
  const envContext = [
    "## GAIA bench task",
    "Answer the question in `signal.content`. Finish with an `llm_compose`",
    "step (skill `gaia`) that binds the final answer to the variable `answer`,",
    "formatted per the gaia skill's strict rules.",
    filePath ? `Attached file (read with read_pdf / read_file): ${filePath}` : "No attached file.",
  ].join("\n");

  const signal: WorkflowSignal = {
    id: 900_000 + index,
    source: "gaia",
    content: task.question,
    envContext,
  };

  // Trace id unique per (run, task) so runs are preserved side by side; the
  // human-readable task-id prefix makes traces easy to find. Session groups
  // all tasks of one run together in the Langfuse Sessions view.
  const traceId = `gaia:${runId}:${task.taskId.slice(0, 8)}`;
  const trace = engine.tracer.trace({
    id: traceId,
    name: "signal:gaia",
    kind: "agent",
    sessionId: `gaia:${runId}`,
    tags: ["gaia", "bench", `level-${task.level}`],
    metadata: { gaia_task_id: task.taskId, gaia_level: task.level, run_id: runId },
  });

  try {
    const envData = await gatherEnvData(envDeps);
    const sessionContext = createSessionContext({ id: traceId, env: envData });
    const result = await runner.runForSignal(signal, sessionContext, trace);

    if (!result.ok) {
      const outcome: Outcome =
        result.stage === "compile"
          ? "compile_fail"
          : result.stage === "execute"
            ? "execute_fail"
            : "replan_exhausted";
      return { task, outcome, predicted: null };
    }

    const predicted = result.store.has("answer") ? String(result.store.get("answer")) : null;
    if (predicted === null) return { task, outcome: "no_answer", predicted: null };

    const correct = task.finalAnswer ? questionScorer(predicted, task.finalAnswer) : false;
    return { task, outcome: correct ? "correct" : "wrong", predicted };
  } catch (err) {
    console.error(`  [task ${task.taskId}] crashed:`, err);
    return { task, outcome: "crash", predicted: null };
  } finally {
    trace.end();
  }
}

function reportExcluded(excluded: GaiaTask[]): void {
  if (excluded.length === 0) return;
  const byCap = new Map<string, number>();
  for (const t of excluded) {
    for (const cap of missingCapabilities(t)) byCap.set(cap, (byCap.get(cap) ?? 0) + 1);
  }
  console.log(`[bench] excluded ${excluded.length} task(s) as tool-coverage gaps:`);
  for (const [cap, n] of [...byCap.entries()].sort((a, b) => b[1] - a[1])) {
    console.log(`    needs ${cap}: ${n}`);
  }
  console.log("");
}

function printReport(results: TaskResult[], suppressedCalls: number): void {
  console.log("\n=== GAIA results ===");
  const levels: GaiaLevel[] = [1, 2, 3];
  for (const level of levels) {
    const rows = results.filter((r) => r.task.level === level);
    if (rows.length === 0) continue;
    const correct = rows.filter((r) => r.outcome === "correct").length;
    const pct = ((correct / rows.length) * 100).toFixed(1);
    console.log(`  L${level}: ${correct}/${rows.length} correct (${pct}%)`);
  }
  const total = results.length;
  const totalCorrect = results.filter((r) => r.outcome === "correct").length;
  console.log(`  ALL: ${totalCorrect}/${total} correct (${((totalCorrect / total) * 100).toFixed(1)}%)`);

  console.log("\n  outcome breakdown:");
  const buckets = new Map<Outcome, number>();
  for (const r of results) buckets.set(r.outcome, (buckets.get(r.outcome) ?? 0) + 1);
  for (const [outcome, n] of [...buckets.entries()].sort((a, b) => b[1] - a[1])) {
    console.log(`    ${outcome}: ${n}`);
  }
  if (suppressedCalls > 0) {
    console.log(`\n  (${suppressedCalls} side-effect tool call(s) suppressed by the bench client)`);
  }
}

// Runs only when this file IS the process entry point (`pnpm bench:gaia`),
// never when a test or another module imports it for the scorer. `dotenv` is
// loaded here rather than at the top: reading a developer's .env is an
// entry-point concern, not a side effect an importer should inherit.
if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await import("dotenv/config");
  main().catch((err) => {
    console.error("bench:gaia crashed:", err);
    process.exit(1);
  });
}
