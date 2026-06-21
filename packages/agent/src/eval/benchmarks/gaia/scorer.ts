// Faithful TS port of the official GAIA scorer (HF `gaia-benchmark/GAIA`
// `scorer.py`, mirrored in camel-ai `camel/benchmarks/gaia.py`). Quasi
// exact-match with type-routed normalization: the ground-truth type decides
// how the comparison is done.
//
//   float GT          → strip $ % , from the model answer, compare as numbers
//   list GT (,/;)     → split both, element-wise (numeric or string per elem)
//   string GT         → lowercase, strip ALL whitespace + punctuation, compare
//
// Do NOT "improve" the normalization — published GAIA numbers depend on this
// exact behaviour (e.g. "D.R M.A.R.T.I.N L.U.T.H.E.R K.I.N.G J.R" is meant to
// match "Dr. Martin Luther King Jr.").

// Optional sign, integer/decimal/exponent — the subset of Python `float()`
// syntax GAIA ground truths use (no underscores, no hex).
const PY_FLOAT_RE = /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/;

// Mirror Python `float(str)`: trims surrounding whitespace, accepts decimals
// and scientific notation plus the inf/nan literals, rejects everything else
// (empty/whitespace-only string, thousands commas, trailing units). Returns
// null when the string is not a Python-parseable float.
function pyParseFloat(s: string): number | null {
  const t = s.trim();
  if (t === "") return null;
  const lower = t.toLowerCase().replace(/^[+-]/, "");
  if (lower === "inf" || lower === "infinity") return t.startsWith("-") ? -Infinity : Infinity;
  if (lower === "nan") return NaN;
  if (!PY_FLOAT_RE.test(t)) return null;
  return Number(t);
}

function isFloat(s: string): boolean {
  return pyParseFloat(s) !== null;
}

// normalize_number_str: strip currency/percent/thousands marks, then parse.
// Returns +Infinity on failure so a non-numeric model answer can never
// accidentally equal a finite numeric ground truth.
function normalizeNumberStr(numberStr: string): number {
  let s = numberStr;
  for (const ch of ["$", "%", ","]) s = s.split(ch).join("");
  const v = pyParseFloat(s);
  return v === null ? Number.POSITIVE_INFINITY : v;
}

function splitString(s: string): string[] {
  return s.split(/[,;]/);
}

// Python `string.punctuation`.
const PUNCTUATION = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";
const PUNCT_SET = new Set(PUNCTUATION.split(""));

// normalize_str: drop ALL whitespace, lowercase, and (when removePunct)
// strip every punctuation char. Whole-string comparisons remove punctuation;
// per-list-element string comparisons do NOT (matches the Python defaults).
function normalizeStr(input: string, removePunct = true): string {
  const lower = input.replace(/\s/g, "").toLowerCase();
  if (!removePunct) return lower;
  let out = "";
  for (const ch of lower) if (!PUNCT_SET.has(ch)) out += ch;
  return out;
}

/**
 * GAIA's official success criterion: returns true iff `modelAnswer` matches
 * `groundTruth` under the type-routed normalization above.
 */
export function questionScorer(modelAnswer: string, groundTruth: string): boolean {
  if (isFloat(groundTruth)) {
    return normalizeNumberStr(modelAnswer) === pyParseFloat(groundTruth);
  }

  if (groundTruth.includes(",") || groundTruth.includes(";")) {
    const gtElems = splitString(groundTruth);
    const maElems = splitString(modelAnswer);
    if (gtElems.length !== maElems.length) return false;
    return gtElems.every((gtElem, i) => {
      const maElem = maElems[i]!;
      if (isFloat(gtElem)) {
        return normalizeNumberStr(maElem) === pyParseFloat(gtElem);
      }
      return normalizeStr(maElem, false) === normalizeStr(gtElem, false);
    });
  }

  return normalizeStr(modelAnswer) === normalizeStr(groundTruth);
}
