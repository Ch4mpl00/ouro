// A Chunker returns the chunks to feed to the embedder. Returning an
// array leaves room for sliding-window / semantic-split strategies, but
// today's only impl returns a single truncated chunk — multi-chunk
// would require a separate news_item_chunks table.

export type Chunker = (text: string) => string[];

// 8000 chars sits under the ~8191-token limit of text-embedding-3-small
// at ~4 chars/token.
export const DEFAULT_MAX_CHARS = 8000;

export function truncateChunker(maxChars: number = DEFAULT_MAX_CHARS): Chunker {
  return (text) => {
    if (text.length === 0) return [""];
    if (text.length <= maxChars) return [text];
    return [text.slice(0, maxChars)];
  };
}
