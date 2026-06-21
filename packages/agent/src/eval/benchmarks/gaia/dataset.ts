import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import path from "node:path";

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
// metadata has been pulled at least once.
const CACHE_DIR = path.resolve(import.meta.dirname, "../../fixtures/gaia");

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
