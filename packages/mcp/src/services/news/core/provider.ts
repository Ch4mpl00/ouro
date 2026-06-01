import type { NewsItem } from "./types";

// A provider is a self-contained source of news. The shared poller
// ticks it every `cadenceMs`, calls `fetch()`, and feeds the result
// into the unified store. Providers own their source-specific quirks
// (API endpoints, retry policy, watermarks, normalization). The core
// knows nothing about them beyond this contract.

export interface NewsProvider {
  source: string;
  cadenceMs: number;
  fetch(): Promise<NewsItem[]>;
}
