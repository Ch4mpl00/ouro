import type { BuildTextMode, CorpusRow } from "./types";

export function buildText(row: CorpusRow, mode: BuildTextMode): string {
  const title = (row.title ?? "").trim();
  const body = row.body.trim();
  if (mode === "body-only") return body;
  return title ? `${title}\n\n${body}` : body;
}
