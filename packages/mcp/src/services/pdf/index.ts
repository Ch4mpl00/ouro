import { readFile } from "node:fs/promises";
import { extractText } from "unpdf";

export interface PdfExtractResult {
  text: string;
  numPages: number;
}

// Extracts plain text from a PDF on disk. Pages are joined with a single
// blank line between them so the agent can still tell where one page ends
// and the next begins. Cyrillic / mixed-script content works.
export async function readPdf(filePath: string): Promise<PdfExtractResult> {
  const buffer = await readFile(filePath);
  const result = await extractText(new Uint8Array(buffer), { mergePages: false });

  const pages = Array.isArray(result.text) ? result.text : [result.text];
  const text = pages.join("\n\n");

  return { text, numPages: result.totalPages };
}
