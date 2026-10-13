import path from "node:path";
import { realpath } from "node:fs/promises";

// Which local files the read tools (read_file, read_pdf) may open. They take
// a path from the model, and the model reads text outsiders wrote — mail,
// channel posts, fetched pages. Unconfined, an injected
// `read_file("/proc/self/environ")` handed it every secret of this process,
// and send_telegram_message / fetch_url could carry them out. A path is
// readable only when its real location — symlinks resolved, `..` collapsed —
// lies under one of the roots.

export interface FileAccess {
  readonly roots: readonly string[];
  // The real path to open, or an error saying the path is outside the roots
  // (or doesn't exist).
  readablePath(input: string): Promise<string>;
}

export interface FileAccessDeps {
  roots: readonly string[];
  // Relative paths resolve against this. Default: process.cwd().
  cwd?: string;
}

// MCP_READ_ROOTS (path-list, ":"-separated) when set; otherwise where the
// read tools' files actually live: downloaded attachments (STORAGE_DIR), the
// skill directories, and the eval fixtures (GAIA attachments).
export function defaultReadRoots(env: NodeJS.ProcessEnv, cwd: string): string[] {
  const listed = (env.MCP_READ_ROOTS ?? "").split(path.delimiter).filter((r) => r.trim().length > 0);
  if (listed.length > 0) return listed.map((r) => path.resolve(cwd, r));
  return [
    path.resolve(cwd, env.STORAGE_DIR ?? "./storage"),
    path.resolve(cwd, "skills"),
    path.resolve(cwd, "skills.default"),
    path.resolve(cwd, "packages/agent/src/eval/fixtures"),
  ];
}

export function createFileAccess(deps: FileAccessDeps): FileAccess {
  const cwd = deps.cwd ?? process.cwd();
  // Roots are compared by real path too, so a root that is itself a symlink
  // (a mounted volume) still contains its files. A missing root contains
  // nothing.
  const realRoots = Promise.all(deps.roots.map((r) => realpath(path.resolve(cwd, r)).catch(() => null)));

  return {
    roots: deps.roots,
    async readablePath(input) {
      const real = await realpath(path.resolve(cwd, input)).catch((err: unknown) => {
        throw new Error(`cannot read ${JSON.stringify(input)}: ${err instanceof Error ? err.message : String(err)}`);
      });
      const inside = (await realRoots).some((root) => {
        if (root === null) return false;
        const rel = path.relative(root, real);
        return rel === "" || (!rel.startsWith("..") && !path.isAbsolute(rel));
      });
      if (!inside) {
        throw new Error(`refusing to read ${JSON.stringify(input)}: outside the readable roots (${deps.roots.join(", ")})`);
      }
      return real;
    },
  };
}
