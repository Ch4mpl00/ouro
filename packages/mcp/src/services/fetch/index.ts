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

// Loopback, RFC1918, link-local / cloud metadata, the unspecified address,
// IPv6 loopback / link-local / unique-local, and IPv4-mapped forms of those.
export function isPrivateIp(ip: string): boolean {
  const addr = ip.toLowerCase().replace(/^\[|\]$/g, "");
  if (net.isIPv4(addr)) {
    const octets = addr.split(".").map(Number);
    const a = octets[0] ?? -1;
    const b = octets[1] ?? -1;
    return (
      a === 0 ||
      a === 10 ||
      a === 127 ||
      (a === 169 && b === 254) || // link-local / cloud metadata
      (a === 172 && b >= 16 && b <= 31) ||
      (a === 192 && b === 168)
    );
  }
  // IPv4-mapped IPv6 (::ffff:1.2.3.4) — re-check the embedded v4 (one hop, the
  // embedded value is a pure v4 so no recursion loop).
  if (addr.startsWith("::ffff:")) {
    const v4 = addr.slice("::ffff:".length);
    if (net.isIPv4(v4)) return isPrivateIp(v4);
  }
  return (
    addr === "::1" || // loopback
    addr === "::" ||
    addr.startsWith("fe80") || // link-local
    addr.startsWith("fc") || // unique-local fc00::/7
    addr.startsWith("fd")
  );
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
