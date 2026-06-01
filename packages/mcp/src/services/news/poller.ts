import type { NewsProvider } from "./core/provider";
import type { NewsRepository } from "./core/repository";

// Shared cadence-based loop. One tick every TICK_MS checks each
// provider: if (now - lastTickAt) >= provider.cadenceMs, fire its
// fetch and pipe the result into the repository. Errors from one
// provider don't kill the loop. Initial tick is delayed (BOOT_DELAY)
// so transport/HTTP startup completes first.

const TICK_MS = 30_000;
const BOOT_DELAY_MS = 10_000;

function prefix(): string {
  return `[${new Date().toISOString()}] [news-poller]`;
}

export interface NewsPollerDeps {
  providers: NewsProvider[];
  repository: NewsRepository;
}

export function startNewsPoller(deps: NewsPollerDeps): void {
  const { providers, repository } = deps;
  if (providers.length === 0) {
    console.log(`${prefix()} no providers registered, poller idle`);
    return;
  }

  const lastTickAt = new Map<string, number>();
  let running = false;

  const tickOnce = async (): Promise<void> => {
    if (running) return;
    running = true;
    try {
      const now = Date.now();
      for (const p of providers) {
        const last = lastTickAt.get(p.source) ?? 0;
        if (now - last < p.cadenceMs) continue;
        lastTickAt.set(p.source, now);
        try {
          const items = await p.fetch();
          if (items.length === 0) continue;
          const result = await repository.save(items);
          console.log(
            `${prefix()} ${p.source}: fetched ${items.length}, saved=${result.saved}, embedded=${result.embedded}, failed=${result.failed}`,
          );
        } catch (err) {
          console.error(
            `${prefix()} ${p.source}: tick failed:`,
            err instanceof Error ? err.message : err,
          );
        }
      }
    } finally {
      running = false;
    }
  };

  console.log(
    `${prefix()} starting with ${providers.length} providers: ${providers
      .map((p) => `${p.source}(${Math.round(p.cadenceMs / 60_000)}min)`)
      .join(", ")}`,
  );
  setTimeout(() => {
    void tickOnce();
    setInterval(() => void tickOnce(), TICK_MS);
  }, BOOT_DELAY_MS);
}
