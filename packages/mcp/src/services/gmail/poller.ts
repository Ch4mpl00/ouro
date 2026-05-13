import { getDb } from "../../db/client";
import { findAttachments } from "./attachments";
import { getRawMessage, listMessages, type MessageSummary } from "./messages";
import { recordSignal } from "../signals";
import { getKv, setKv } from "./storage";
import { GMAIL_SUBSCRIPTIONS, type GmailSubscription } from "./subscriptions";

// Generic Gmail poller. Iterates over GMAIL_SUBSCRIPTIONS and polls each on
// its own cadence. For each subscription:
//   1. read watermark (last seen internalDate, ms) from gmail_kv
//   2. on first run with no watermark, set watermark = now() and emit nothing
//      (avoid flooding the queue with old emails on a fresh install)
//   3. otherwise: query with `after:` filter, take rows newer than watermark,
//      enqueue one signal per row, advance watermark
//
// The poller is best-effort: errors log + retry on next interval. Polling
// is paused if no Gmail account is authorized yet.

function logPrefix(): string {
  return `[${new Date().toISOString()}] [gmail-poller]`;
}

function watermarkKey(sub: GmailSubscription): string {
  return `subscription.${sub.name}.last_internal_date_ms`;
}

function resolveAccountKey(): string | null {
  const fromEnv = process.env.GMAIL_ACCOUNT_KEY;
  if (fromEnv) return fromEnv;
  const row = getDb()
    .prepare(
      `SELECT account_key FROM integration_account
       WHERE provider = 'gmail'
       ORDER BY created_at DESC LIMIT 1`,
    )
    .get() as { account_key: string } | undefined;
  return row?.account_key ?? null;
}

async function pollSubscription(sub: GmailSubscription, accountKey: string): Promise<void> {
  const watermark = getKv(watermarkKey(sub));

  if (watermark === null) {
    setKv(watermarkKey(sub), String(Date.now()));
    console.log(`${logPrefix()} ${sub.name}: bootstrapping watermark, no emit`);
    return;
  }

  const watermarkMs = Number(watermark);
  const watermarkSeconds = Math.floor(watermarkMs / 1000);
  const query = `${sub.query} after:${watermarkSeconds}`;

  const { messages } = await listMessages(accountKey, { query, maxResults: 50 });
  if (messages.length === 0) return;

  // Sort ascending by internalDate so we emit signals in chronological order
  // and advance watermark monotonically.
  const sorted: MessageSummary[] = [...messages].sort(
    (a, b) => Number(a.internalDate ?? 0) - Number(b.internalDate ?? 0),
  );

  let newWatermark = watermarkMs;
  let emitted = 0;
  for (const m of sorted) {
    const md = Number(m.internalDate ?? 0);
    if (md <= watermarkMs) continue; // gmail's `after:` is non-strict (in seconds), dedupe defensively

    // Enrich the bare metadata with attachment refs so the agent gets a
    // self-contained signal (messageId + attachmentId inline) — no need
    // to re-query list_nashdom_mails and hunt for the matching row.
    const raw = await getRawMessage(accountKey, m.id);
    const attachments = findAttachments(raw);

    recordSignal({ source: sub.signalSource, content: sub.buildContent(m, attachments) });
    emitted++;
    if (md > newWatermark) newWatermark = md;
  }

  if (newWatermark !== watermarkMs) {
    setKv(watermarkKey(sub), String(newWatermark));
  }
  console.log(`${logPrefix()} ${sub.name}: emitted ${emitted} signal(s)`);
}

function startSubscription(sub: GmailSubscription): void {
  const tick = async (): Promise<void> => {
    const accountKey = resolveAccountKey();
    if (!accountKey) {
      console.warn(`${logPrefix()} ${sub.name}: no Gmail account authorized — skipping`);
      return;
    }
    try {
      await pollSubscription(sub, accountKey);
    } catch (err) {
      console.error(`${logPrefix()} ${sub.name}: poll failed:`, err instanceof Error ? err.message : err);
    }
  };

  void tick();
  setInterval(() => void tick(), sub.intervalMs);
  console.log(`${logPrefix()} started ${sub.name} (every ${sub.intervalMs / 1000}s, source=${sub.signalSource})`);
}

export function startGmailPoller(): void {
  for (const sub of GMAIL_SUBSCRIPTIONS) {
    startSubscription(sub);
  }
}
