// Thin wrapper over the Monobank Personal API (api.monobank.ua).
// Auth: X-Token header with the personal token from MONOBANK_API_KEY.
// Rate limit: 1 statement request per 60s per account.

const API_BASE = "https://api.monobank.ua";

export class MonobankConfigError extends Error {}
export class MonobankApiError extends Error {
  status: number;
  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

function getApiKey(): string {
  const k = process.env.MONOBANK_API_KEY;
  if (!k) {
    throw new MonobankConfigError("MONOBANK_API_KEY is not set in .env");
  }
  return k;
}

interface RawStatementItem {
  id: string;
  time: number;
  description: string;
  mcc: number;
  originalMcc: number;
  hold: boolean;
  amount: number;
  operationAmount: number;
  currencyCode: number;
  commissionRate: number;
  cashbackAmount: number;
  balance: number;
  comment?: string;
  receiptId?: string;
  invoiceId?: string;
  counterEdrpou?: string;
  counterIban?: string;
  counterName?: string;
}

const CURRENCY_MAP: Record<number, string> = {
  980: "UAH",
  840: "USD",
  978: "EUR",
  826: "GBP",
  985: "PLN",
  124: "CAD",
  756: "CHF",
  392: "JPY",
  156: "CNY",
  643: "RUB",
};

export function isoCurrency(code: number): string {
  return CURRENCY_MAP[code] ?? String(code);
}

// Monobank stores money in minor units (kopecks/cents). UAH/USD/EUR all use
// 100; we accept that approximation rather than carrying a per-currency
// exponent table for currencies we don't expect to see.
const MINOR_UNIT = 100;

export interface Transaction {
  id: string;
  time: string;
  description: string;
  comment: string | null;
  mcc: number;
  amount: number;
  operationAmount: number;
  currency: string;
  cashbackAmount: number;
  receiptId: string | null;
  invoiceId: string | null;
  counterIban: string | null;
  counterName: string | null;
  counterEdrpou: string | null;
}

function toTransaction(raw: RawStatementItem): Transaction {
  return {
    id: raw.id,
    time: new Date(raw.time * 1000).toISOString(),
    description: raw.description,
    comment: raw.comment ?? null,
    mcc: raw.mcc,
    amount: raw.amount / MINOR_UNIT,
    operationAmount: raw.operationAmount / MINOR_UNIT,
    currency: isoCurrency(raw.currencyCode),
    cashbackAmount: raw.cashbackAmount / MINOR_UNIT,
    receiptId: raw.receiptId ?? null,
    invoiceId: raw.invoiceId ?? null,
    counterIban: raw.counterIban ?? null,
    counterName: raw.counterName ?? null,
    counterEdrpou: raw.counterEdrpou ?? null,
  };
}

async function call<T>(path: string): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    headers: { "X-Token": getApiKey() },
  });
  if (res.status === 429) {
    throw new MonobankApiError(
      "Monobank rate limit hit (1 request per 60s per account). Try again later.",
      429,
    );
  }
  if (!res.ok) {
    let detail = "";
    try {
      detail = await res.text();
    } catch {
      // ignore
    }
    throw new MonobankApiError(
      `Monobank ${path} failed (${res.status}): ${detail || res.statusText}`,
      res.status,
    );
  }
  return (await res.json()) as T;
}

const MAX_RANGE_SECONDS = 31 * 24 * 60 * 60;

export async function getStatement(
  accountId: string,
  from: Date,
  to: Date,
): Promise<Transaction[]> {
  const fromS = Math.floor(from.getTime() / 1000);
  const toS = Math.floor(to.getTime() / 1000);
  if (toS - fromS > MAX_RANGE_SECONDS) {
    const days = ((toS - fromS) / 86400).toFixed(1);
    throw new Error(`Monobank statement range cannot exceed 31 days (got ${days}d)`);
  }

  const path = `/personal/statement/${encodeURIComponent(accountId)}/${fromS}/${toS}`;
  const raw = await call<RawStatementItem[]>(path);
  return raw.map(toTransaction);
}
