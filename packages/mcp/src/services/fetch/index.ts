import dns from "node:dns";
import net from "node:net";
import { Agent, fetch, type Response } from "undici";

// Fetching URLs that came from an LLM or a feed is an SSRF vector: a crafted
// URL could hit cloud metadata (169.254.169.254) or internal services on
// localhost / RFC1918. Only http(s), and EVERY address a host resolves to
// must be public.
//
// The check runs where it cannot be skipped:
//   - at connect time, in the agent's DNS lookup, so every connection —
//     each redirect hop, and a DNS answer that changed since validation —
//     is checked, not just the URL the model typed;
//   - on every redirect target before it is followed, because a literal IP
//     host never goes through DNS.

// Everything that isn't the public internet: this network, RFC1918,
// carrier-grade NAT (Tailscale), loopback, link-local / cloud metadata, the
// same through NAT64, and IPv6 unspecified / loopback / IPv4-compatible,
// unique-local, link-local, site-local and local-use NAT64. BlockList
// matches IPv4 rules against IPv4-mapped IPv6 in any spelling — a URL host
// like [::ffff:127.0.0.1] normalises to [::ffff:7f00:1], which a string
// prefix check let straight through to loopback.
const NON_PUBLIC = new net.BlockList();
for (const [address, prefix] of [
  ["0.0.0.0", 8],
  ["10.0.0.0", 8],
  ["100.64.0.0", 10],
  ["127.0.0.0", 8],
  ["169.254.0.0", 16],
  ["172.16.0.0", 12],
  ["192.168.0.0", 16],
] as const) {
  NON_PUBLIC.addSubnet(address, prefix, "ipv4");
  NON_PUBLIC.addSubnet(`64:ff9b::${address}`, 96 + prefix, "ipv6");
}
for (const [address, prefix] of [
  ["::", 96],
  ["fc00::", 7],
  ["fe80::", 10],
  ["fec0::", 10],
  ["64:ff9b:1::", 48],
] as const) {
  NON_PUBLIC.addSubnet(address, prefix, "ipv6");
}

export function isPrivateIp(ip: string): boolean {
  // Brackets from a URL host, a zone id from a link-local DNS answer.
  const addr = ip.replace(/^\[|\]$/g, "").split("%")[0] ?? "";
  if (net.isIPv4(addr)) return NON_PUBLIC.check(addr, "ipv4");
  if (net.isIPv6(addr)) return NON_PUBLIC.check(addr, "ipv6");
  // Not an address at all: refuse rather than guess.
  return true;
}

export interface FetchGuardOpts {
  // Which addresses count as private. Tests narrow it so a local server can
  // stand in for a public one.
  isPrivate?: (ip: string) => boolean;
  maxRedirects?: number;
}

export interface GuardedFetchInit {
  signal?: AbortSignal;
  headers?: Record<string, string>;
}

export interface GuardedResponse {
  // The URL that answered, after redirects.
  url: string;
  response: Response;
}

export type GuardedFetch = (url: string, init?: GuardedFetchInit) => Promise<GuardedResponse>;

// Some sites 403 a missing/unknown User-Agent; a normal browser UA fixes the
// "search can't retrieve this URL" cases.
export const BROWSER_UA =
  "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) " +
  "Chrome/124.0.0.0 Safari/537.36";

const REDIRECTS = new Set([301, 302, 303, 307, 308]);

export function createGuardedFetch(opts: FetchGuardOpts = {}): GuardedFetch {
  const isPrivate = opts.isPrivate ?? isPrivateIp;
  const maxRedirects = opts.maxRedirects ?? 10;

  const lookup: net.LookupFunction = (hostname, options, callback) => {
    dns.lookup(hostname, { ...options, all: true }, (err, addresses) => {
      if (err) return callback(err, "", 0);
      const bad = addresses.find((a) => isPrivate(a.address));
      if (bad) {
        return callback(
          new Error(`refusing to fetch private/loopback address (${hostname} → ${bad.address})`),
          "",
          0,
        );
      }
      const first = addresses[0];
      if (options.all) callback(null, addresses);
      else if (first) callback(null, first.address, first.family);
      else callback(new Error(`could not resolve host "${hostname}"`), "", 0);
    });
  };
  const dispatcher = new Agent({ connect: { lookup } });

  return async (raw, init = {}) => {
    let url = await assertPublicUrl(raw, isPrivate);
    for (let hop = 0; ; hop++) {
      const response = await fetch(url, {
        redirect: "manual",
        dispatcher,
        signal: init.signal,
        headers: { "user-agent": BROWSER_UA, ...init.headers },
      }).catch((err: unknown) => {
        // undici reports every connection failure as "fetch failed" and keeps
        // the reason (a refused address, a DNS error) in `cause`.
        const cause = err instanceof Error ? err.cause : undefined;
        throw cause instanceof Error ? cause : err;
      });
      const location = response.headers.get("location");
      if (!REDIRECTS.has(response.status) || !location) return { url: url.toString(), response };
      await response.body?.cancel();
      if (hop >= maxRedirects) throw new Error("too many redirects");
      url = await assertPublicUrl(new URL(location, url).toString(), isPrivate);
    }
  };
}

// Scheme and every resolved address (or the literal IP), before connecting.
export async function assertPublicUrl(raw: string, isPrivate: (ip: string) => boolean = isPrivateIp): Promise<URL> {
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    throw new Error(`invalid URL: ${raw}`);
  }
  if (u.protocol !== "http:" && u.protocol !== "https:") {
    throw new Error(`unsupported scheme "${u.protocol}" — only http/https`);
  }
  const host = u.hostname.replace(/^\[|\]$/g, "");
  let addrs: { address: string }[];
  if (net.isIP(host)) {
    addrs = [{ address: host }];
  } else {
    try {
      addrs = await dns.promises.lookup(host, { all: true });
    } catch {
      throw new Error(`could not resolve host "${u.hostname}"`);
    }
  }
  for (const { address } of addrs) {
    if (isPrivate(address)) {
      throw new Error(`refusing to fetch private/loopback address (${u.hostname} → ${address})`);
    }
  }
  return u;
}

// Reads past the cap by at most one chunk, so a body of exactly `maxBytes`
// isn't reported as truncated, and cuts on a character boundary: a streaming
// decoder holds back a multi-byte sequence the cap split in two.
export async function readCapped(
  res: Response,
  maxBytes: number,
): Promise<{ text: string; bytes: number; truncated: boolean }> {
  const reader = res.body?.getReader();
  if (!reader) return { text: "", bytes: 0, truncated: false };
  const chunks: Buffer[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    chunks.push(Buffer.from(value));
    if (total > maxBytes) {
      await reader.cancel();
      break;
    }
  }
  const body = Buffer.concat(chunks);
  if (total <= maxBytes) return { text: body.toString("utf-8"), bytes: total, truncated: false };
  return { text: new TextDecoder().decode(body.subarray(0, maxBytes), { stream: true }), bytes: total, truncated: true };
}
