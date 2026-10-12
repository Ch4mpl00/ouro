// What a 5-field cron means on the user's wall clock, restated for tests:
// a wall-clock minute either matches the expression or it doesn't. This is
// deliberately not cron-parser's algorithm (no "next" arithmetic) — tests
// scan instants minute by minute and ask the wall clock, so a bug in the
// scheduler's cron library can't hide behind an oracle built on it.
//
// Supported syntax: `*`, `n`, `a-b`, `*/s`, `a-b/s`, comma lists; day of week
// 0-7 with 7 = Sunday. Day-of-month and day-of-week combine the classic way:
// both restricted → either may match; otherwise both must.

export interface WallCron {
  minute: ReadonlySet<number>;
  hour: ReadonlySet<number>;
  dom: ReadonlySet<number>;
  month: ReadonlySet<number>;
  dow: ReadonlySet<number>;
  domRestricted: boolean;
  dowRestricted: boolean;
}

function field(text: string, min: number, max: number): Set<number> {
  const out = new Set<number>();
  for (const part of text.split(",")) {
    const [range, stepText] = part.split("/");
    const step = stepText === undefined ? 1 : Number(stepText);
    let lo = min;
    let hi = max;
    if (range !== "*") {
      const [a, b] = range!.split("-");
      lo = Number(a);
      hi = b === undefined ? (stepText === undefined ? lo : max) : Number(b);
    }
    if (![lo, hi, step].every(Number.isInteger) || step < 1 || lo < min || hi > max || lo > hi) {
      throw new Error(`wall-cron: bad field ${JSON.stringify(text)}`);
    }
    for (let v = lo; v <= hi; v += step) out.add(v);
  }
  return out;
}

export function parseWallCron(expr: string): WallCron {
  const parts = expr.trim().split(/\s+/);
  if (parts.length !== 5) throw new Error(`wall-cron: not 5 fields: ${JSON.stringify(expr)}`);
  const [minute, hour, dom, month, dow] = parts as [string, string, string, string, string];
  const days = field(dow, 0, 7);
  if (days.has(7)) days.add(0);
  return {
    minute: field(minute, 0, 59),
    hour: field(hour, 0, 23),
    dom: field(dom, 1, 31),
    month: field(month, 1, 12),
    dow: days,
    domRestricted: !dom.startsWith("*"),
    dowRestricted: !dow.startsWith("*"),
  };
}

// A wall-clock minute, as minutes since 1970-01-01T00:00 *on that wall
// clock* (not an instant): comparable, and its calendar fields read back with
// the UTC getters.
export type WallMinute = number;

export function matches(cron: WallCron, wall: WallMinute): boolean {
  const d = new Date(wall * 60_000);
  if (!cron.minute.has(d.getUTCMinutes()) || !cron.hour.has(d.getUTCHours()) || !cron.month.has(d.getUTCMonth() + 1)) {
    return false;
  }
  const domOk = cron.dom.has(d.getUTCDate());
  const dowOk = cron.dow.has(d.getUTCDay());
  return cron.domRestricted && cron.dowRestricted ? domOk || dowOk : domOk && dowOk;
}

const formatters = new Map<string, Intl.DateTimeFormat>();
const wallCache = new Map<string, WallMinute>();

// The wall-clock minute in `tz` at the instant `minute` (minutes since epoch).
export function wallAt(tz: string, minute: number): WallMinute {
  const key = `${tz}|${minute}`;
  const cached = wallCache.get(key);
  if (cached !== undefined) return cached;
  let fmt = formatters.get(tz);
  if (fmt === undefined) {
    fmt = new Intl.DateTimeFormat("en-US", {
      timeZone: tz,
      hourCycle: "h23",
      year: "numeric",
      month: "numeric",
      day: "numeric",
      hour: "numeric",
      minute: "numeric",
    });
    formatters.set(tz, fmt);
  }
  const p = Object.fromEntries(fmt.formatToParts(new Date(minute * 60_000)).map((x) => [x.type, Number(x.value)]));
  const wall = Date.UTC(p.year!, p.month! - 1, p.day!, p.hour!, p.minute!) / 60_000;
  if (wallCache.size > 2_000_000) wallCache.clear();
  wallCache.set(key, wall);
  return wall;
}

// The instant `minute` shows a wall time the clock already showed earlier
// (the repeated hour — or half hour — after clocks go back). Such a repeat is
// a second chance at the same wall time, never an obligation.
export function isRepeat(tz: string, minute: number): boolean {
  const wall = wallAt(tz, minute);
  return wallAt(tz, minute - 30) === wall || wallAt(tz, minute - 60) === wall;
}

// The first instant (whole minute, strictly after `afterMs`, at most
// `untilMs`) at which a scheduler owes a fire: the wall clock shows a
// matching time it hasn't shown before. null if none.
export function firstOwed(cron: WallCron, tz: string, afterMs: number, untilMs: number): number | null {
  for (let m = Math.floor(afterMs / 60_000) + 1; m * 60_000 <= untilMs; m++) {
    if (matches(cron, wallAt(tz, m)) && !isRepeat(tz, m)) return m * 60_000;
  }
  return null;
}

// `slotMs` is a legitimate fire time: its wall time matches, or it stands in
// for a matching wall time the clock skipped (clocks going forward) — the
// skipped time shifted by the size of the jump, within two hours of it.
export function legitimateSlot(cron: WallCron, tz: string, slotMs: number): boolean {
  if (slotMs % 60_000 !== 0) return false;
  const slot = slotMs / 60_000;
  const wall = wallAt(tz, slot);
  if (matches(cron, wall)) return true;
  for (let g = slot - 120; g <= slot; g++) {
    const jump = wallAt(tz, g) - wallAt(tz, g - 1) - 1;
    if (jump <= 0) continue;
    const skipped = wall - jump;
    if (skipped > wallAt(tz, g - 1) && skipped < wallAt(tz, g) && matches(cron, skipped)) return true;
  }
  return false;
}

const LONGEST_MONTH = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

// Some selected month has one of the selected days (Nov 31 doesn't; Feb 29
// does). With day-of-week also restricted the expression can still match on
// weekdays alone; cron-parser refuses it anyway (see Findings).
export function daysExist(cron: WallCron): boolean {
  return [...cron.month].some((m) => [...cron.dom].some((d) => d <= LONGEST_MONTH[m - 1]!));
}
