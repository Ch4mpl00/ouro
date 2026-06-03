// `${path}` substitution for plan step args, llm_compose inputs, and
// llm_*  prompts. Two modes:
//
//   "whole string" — the string IS one placeholder, e.g. `"${posts}"`.
//     Returns the bound value AS-IS (preserves type). This is how an
//     array of post objects gets passed to a tool: caller writes
//     `args: { posts: "${posts}" }` and the tool sees an actual array,
//     not a stringified JSON dump.
//
//   "interpolation" — the string contains one or more placeholders
//     mixed with literal text. Each placeholder resolves and is
//     stringified (JSON.stringify for non-string values), then concat'd.
//     Used for prompts: `"Hello ${name}, today is ${env.date}"`.
//
// Missing bindings throw MissingBindingError — never silently expand
// to "undefined". Planner mistakes should surface loudly in the trace.

export interface VariableStore {
  get(path: string): unknown;
  set(name: string, value: unknown): void;
  has(path: string): boolean;
  snapshot(): Record<string, unknown>;
}

export class MissingBindingError extends Error {
  constructor(public readonly path: string) {
    super(`unbound substitution: \${${path}}`);
    this.name = "MissingBindingError";
  }
}

export class DuplicateBindingError extends Error {
  constructor(public readonly name: string) {
    super(`duplicate binding: ${name}`);
    this.name = "DuplicateBindingError";
  }
}

// Dot-notation only for v1. No array indices (`results[0]`), no
// optional chaining, no expressions. If we need them, add in the
// minimal form when the first hot-path case appears.
const FULL_PLACEHOLDER = /^\$\{([^}]+)\}$/;
const PARTIAL_PLACEHOLDER = /\$\{([^}]+)\}/g;

export function createStore(initial: Record<string, unknown>): VariableStore {
  const data = new Map<string, unknown>(Object.entries(initial));

  const resolvePath = (path: string): { found: boolean; value: unknown } => {
    const segments = path.split(".");
    const first = segments[0] ?? "";
    if (!data.has(first)) return { found: false, value: undefined };
    let current: unknown = data.get(first);
    for (let i = 1; i < segments.length; i++) {
      const seg = segments[i] ?? "";
      if (current == null || typeof current !== "object") {
        return { found: false, value: undefined };
      }
      const obj = current as Record<string, unknown>;
      if (!(seg in obj)) return { found: false, value: undefined };
      current = obj[seg];
    }
    return { found: true, value: current };
  };

  return {
    get(path) {
      const r = resolvePath(path);
      if (!r.found) throw new MissingBindingError(path);
      return r.value;
    },
    set(name, value) {
      if (data.has(name)) throw new DuplicateBindingError(name);
      data.set(name, value);
    },
    has(path) {
      return resolvePath(path).found;
    },
    snapshot() {
      return Object.fromEntries(data);
    },
  };
}

// Recursively walk a value substituting `${path}` placeholders. Returns
// a NEW value (no in-place mutation) — caller can hold onto the input
// safely. Non-string primitives pass through untouched.
export function substitute(value: unknown, store: VariableStore): unknown {
  if (typeof value === "string") return substituteString(value, store);
  if (Array.isArray(value)) return value.map((v) => substitute(v, store));
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value)) {
      out[k] = substitute(v, store);
    }
    return out;
  }
  return value;
}

function substituteString(s: string, store: VariableStore): unknown {
  const fullMatch = s.match(FULL_PLACEHOLDER);
  if (fullMatch) {
    return store.get(fullMatch[1]!);
  }
  return s.replace(PARTIAL_PLACEHOLDER, (_, path: string) => {
    const v = store.get(path);
    if (typeof v === "string") return v;
    if (v === undefined || v === null) return String(v);
    return JSON.stringify(v);
  });
}
