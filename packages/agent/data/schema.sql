-- Agent-side state. Domain memory for the signal-driven supervisor.
-- Re-run via `pnpm db:init:agent`. Idempotent (IF NOT EXISTS).

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- Tracked utility bills extracted from NashDom emails.
-- One row per Gmail message (dedup on message_id).
CREATE TABLE IF NOT EXISTS bills (
  id                   INTEGER PRIMARY KEY AUTOINCREMENT,
  message_id           TEXT NOT NULL UNIQUE,
  subject              TEXT,
  "from"               TEXT,
  "date"               TEXT,                              -- email received date, YYYY-MM-DD
  invoice_date         TEXT,                              -- billing period, YYYY-MM
  account              TEXT,
  address              TEXT,
  type                 TEXT,
  amount               REAL,
  currency             TEXT,
  ibans                TEXT,                              -- JSON array of strings
  telegram_chat_id     TEXT,
  telegram_message_id  INTEGER,
  telegram_message_text TEXT,                              -- original notification body, for append-only edits
  paid                 INTEGER NOT NULL DEFAULT 0,        -- 0|1
  paid_at              TEXT,
  paid_transaction_id  TEXT,
  notes                TEXT,
  created_at           TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at           TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS bills_paid ON bills(paid);

-- Freeform key-value store. Use for anything Claude wants to remember that
-- doesn't fit a typed table. Value is a JSON-stringified payload by convention.
CREATE TABLE IF NOT EXISTS memory (
  key         TEXT PRIMARY KEY,
  value       TEXT NOT NULL,
  updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Local mirror of every agent run's trace. Written by the local-recorder
-- tracer (tee'd alongside Langfuse) so the judge and the self-improvement
-- loop read runs from here — fast, and independent of Langfuse uptime.
-- `id` IS the Langfuse/OTel trace id, so scores written back to Langfuse
-- link with zero mapping. The full observation tree is one JSON blob:
-- the judge reads it whole, never queries individual steps.
CREATE TABLE IF NOT EXISTS traces (
  id            TEXT PRIMARY KEY,                  -- OTel/Langfuse trace id (hex)
  name          TEXT NOT NULL,
  source        TEXT,                              -- signal.source (tags[0])
  skill         TEXT,                              -- resolved composer skill, nullable
  session_id    TEXT,                              -- signalLabel, e.g. scheduler:242
  tags          TEXT NOT NULL DEFAULT '[]',        -- json array of strings
  input         TEXT,                              -- json
  output        TEXT,                              -- json (often null on workflow path)
  metadata      TEXT,                              -- json
  observations  TEXT NOT NULL DEFAULT '[]',        -- json: full observation tree
  started_at    TEXT NOT NULL,                     -- trace root start (ISO), judge age filter
  created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS traces_started ON traces(started_at);
CREATE INDEX IF NOT EXISTS traces_skill ON traces(skill);

-- One row per JUDGED NODE: (trace, observation, judge provider, prompt
-- version). A node is one generative LLM observation — the planner generation,
-- an llm_compose, or an llm_agent step — owned by exactly one skill (or the
-- planner). Per-node scores localize the signal to the unit the improver
-- patches (a skill), so the PK is the node, not the run. Axis scores live in
-- numeric columns so the improver can filter/aggregate cheaply
-- (WHERE composition < 0.7, GROUP BY skill); the rich payload (labels,
-- rationale, evidence, faithfulness claims) is the `detail` JSON. Axes are
-- owner-type specific: a planner node fills query_formulation/process, a
-- composer/agent node fills coverage/composition/faithfulness — the others
-- stay null. A null axis the rubric DID emit means the judge marked it n/a.
CREATE TABLE IF NOT EXISTS judgements (
  trace_id           TEXT NOT NULL,
  observation_id     TEXT NOT NULL,                -- the judged node's observation id
  provider           TEXT NOT NULL,                -- codex|openai
  prompt_version     TEXT NOT NULL,                -- e.g. n1
  node_kind          TEXT NOT NULL,                -- planner|compose|agent
  skill              TEXT NOT NULL,                -- owner skill (planner for prompt-only)
  query_formulation  REAL,
  process            REAL,
  coverage           REAL,
  composition        REAL,
  faithfulness       REAL,
  detail             TEXT NOT NULL,                -- json: scorecard (+ faithfulness)
  created_at         TEXT NOT NULL DEFAULT (datetime('now')),
  PRIMARY KEY (trace_id, observation_id, provider, prompt_version),
  FOREIGN KEY (trace_id) REFERENCES traces(id)
);
-- The improver aggregates low-score clusters per (skill, version) → patch.
CREATE INDEX IF NOT EXISTS judgements_skill
  ON judgements(skill, prompt_version);
