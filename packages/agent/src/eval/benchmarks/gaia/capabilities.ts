import type { GaiaTask } from "./dataset";

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
