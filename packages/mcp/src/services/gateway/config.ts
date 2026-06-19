import { readFileSync } from "node:fs";
import path from "node:path";
import { z } from "zod";

// Gateway config: a git-tracked JSON file listing the third-party MCP servers
// own-MCP aggregates and re-exposes to the agent. Secrets are referenced as
// ${ENV_VAR} and resolved from process.env at load time, so the file itself
// stays secret-free and commitable. Onboarding a new MCP = one entry here +
// one secret in .env.mcp + one tool name in the relevant skill's frontmatter.
//
// Only StreamableHTTP upstreams are supported. A stdio upstream would spawn a
// child process inside the poller process — deliberately out of scope until the
// gateway is extracted to its own container (see .claude/tasks/mcp-gateway.md).

const upstreamSchema = z.object({
  // Identifier for logs; also the default tool namespace.
  name: z
    .string()
    .min(1)
    .regex(/^[a-zA-Z0-9_-]+$/, "name must be [a-zA-Z0-9_-]"),
  transport: z.literal("http").default("http"),
  url: z.string().min(1),
  // Tool namespace: upstream tool `x` is re-exposed as `${prefix}__x`. Defaults
  // to `name`. Keeps third-party tools from colliding with own-MCP's tools and
  // with each other.
  prefix: z
    .string()
    .regex(/^[a-zA-Z0-9_-]+$/, "prefix must be [a-zA-Z0-9_-]")
    .optional(),
  headers: z.record(z.string()).default({}),
  enabled: z.boolean().default(true),
});

const configSchema = z.object({
  upstreams: z.array(upstreamSchema).default([]),
});

export interface ResolvedUpstream {
  name: string;
  url: string;
  prefix: string;
  headers: Record<string, string>;
}

// Replace ${VAR} with process.env[VAR]. Returns null (and records the offending
// names) if any referenced var is unset — the caller skips that upstream rather
// than connecting with an empty credential.
function interpolate(value: string, missing: string[]): string {
  return value.replace(/\$\{([A-Z0-9_]+)\}/g, (_match, name: string) => {
    const v = process.env[name];
    if (v === undefined || v === "") {
      missing.push(name);
      return "";
    }
    return v;
  });
}

function defaultConfigPath(): string {
  // config.ts lives at packages/mcp/src/services/gateway/ → package root is ../../..
  return path.resolve(import.meta.dirname, "../../../gateway.config.json");
}

// Loads the config file, validates it, resolves ${VAR} references, and returns
// only the upstreams that are enabled AND fully resolvable. A missing file is
// not an error — it just means no upstreams (gateway stays a no-op and own-MCP
// is served directly). Per-upstream problems (missing secret) are logged and
// skipped so a single misconfigured server can't take down the pollers.
export function loadGatewayConfig(configPath?: string): ResolvedUpstream[] {
  const file = configPath ?? process.env.GATEWAY_CONFIG ?? defaultConfigPath();

  let raw: string;
  try {
    raw = readFileSync(file, "utf-8");
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw err;
  }

  const parsed = configSchema.parse(JSON.parse(raw));

  const resolved: ResolvedUpstream[] = [];
  for (const u of parsed.upstreams) {
    if (!u.enabled) continue;

    const missing: string[] = [];
    const url = interpolate(u.url, missing);
    const headers: Record<string, string> = {};
    for (const [k, v] of Object.entries(u.headers)) headers[k] = interpolate(v, missing);

    if (missing.length > 0) {
      console.warn(
        `[gateway] skipping upstream '${u.name}': unresolved env var(s) ${[...new Set(missing)].join(", ")}`,
      );
      continue;
    }

    resolved.push({ name: u.name, url, prefix: u.prefix ?? u.name, headers });
  }

  return resolved;
}
