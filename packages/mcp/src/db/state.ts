// MCP-owned state: the `mcp_state` database in the same Postgres cluster as
// the news store. OAuth tokens and the userbot session, the signal queue,
// scheduled tasks, settings, the Telegram chat log and poller cursors. (It
// used to be the sqlite file `tokens.db`.)
//
// The schema, the migration table, the advisory-lock key and the seed are the
// Rust server's (crates/mcp/src/db.rs on feat/rust-rewrite) to the letter, so
// either server runs on a database the other created.
//
// A separate database rather than tables next to news_items: a different
// lifecycle (small, hot, transactional vs. large and append-mostly), and it
// can be backed up or moved on its own.

import pg from "pg";

export interface StateDb {
  pool: pg.Pool;
  close(): Promise<void>;
}

const DEFAULT_STATE_DB = "mcp_state";

// "mcp_sta" — the key the Rust server takes around the same work.
const STATE_LOCK = "30790106750350433";

// bigint columns (ids, last_run_at) come back as numbers, not strings: every
// value here fits in a double, and the agent has always seen numbers.
const types = new pg.TypeOverrides();
types.setTypeParser(pg.types.builtins.INT8, (value: string) => Number(value));

// STATE_DATABASE_URL wins; otherwise DATABASE_URL with the database name
// swapped, so compose needs no extra secret.
export function stateDatabaseUrl(databaseUrl: string): string {
  const explicit = process.env.STATE_DATABASE_URL;
  if (explicit) return explicit;
  const url = new URL(databaseUrl);
  url.pathname = `/${DEFAULT_STATE_DB}`;
  return url.toString();
}

// Creates the database if missing, migrates it, seeds it. `stateUrl` is for
// tests, which point at a database of their own.
export async function connectStateDb(databaseUrl: string, stateUrl = stateDatabaseUrl(databaseUrl)): Promise<StateDb> {
  await ensureDatabase(databaseUrl, stateUrl);
  const pool = new pg.Pool({ connectionString: stateUrl, types });
  const client = await pool.connect();
  try {
    await client.query("SELECT pg_advisory_lock($1)", [STATE_LOCK]);
    try {
      await migrate(client);
      await seedSystemTasks(client);
    } finally {
      await client.query("SELECT pg_advisory_unlock($1)", [STATE_LOCK]);
    }
  } finally {
    client.release();
  }
  return { pool, close: () => pool.end() };
}

// CREATE DATABASE through the news database's connection. Both MCP instances
// may try at once; the loser's duplicate_database is success — and so is the
// unique violation on pg_database's name index, which is what Postgres raises
// instead when the two CREATEs overlap.
async function ensureDatabase(adminUrl: string, stateUrl: string): Promise<void> {
  const name = decodeURIComponent(new URL(stateUrl).pathname.slice(1));
  if (!name) return;
  const admin = new pg.Client({ connectionString: adminUrl });
  await admin.connect();
  try {
    const { rowCount } = await admin.query("SELECT 1 FROM pg_database WHERE datname = $1", [name]);
    if (rowCount) return;
    try {
      await admin.query(`CREATE DATABASE "${name.replace(/"/g, '""')}"`);
      console.log(`[mcp-state] created database ${name}`);
    } catch (err) {
      const code = err instanceof Error && "code" in err ? err.code : undefined;
      if (code !== "42P04" && code !== "23505") throw err;
    }
  } finally {
    await admin.end();
  }
}

// Append-only: a change is a new entry, never an edit to an applied one.
const MIGRATIONS: { version: number; sql: string }[] = [
  {
    version: 1,
    sql: `
-- OAuth / session credentials per integration: Gmail tokens (expires_at is
-- an ISO string, as googleapis wrote it), the userbot's MTProto session.
CREATE TABLE integration_account (
  provider      text NOT NULL,
  account_key   text NOT NULL,
  access_token  text,
  refresh_token text,
  expires_at    text,
  metadata      text,
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (provider, account_key)
);

-- Every message the bot sees or sends: role 'user' incoming, 'assistant'
-- outgoing. thread_id is the forum topic (NULL for non-topic / General).
CREATE TABLE telegram_messages (
  id            bigserial PRIMARY KEY,
  chat_id       bigint NOT NULL,
  tg_message_id bigint,
  thread_id     bigint,
  role          text NOT NULL CHECK (role IN ('user', 'assistant')),
  text          text NOT NULL,
  created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX telegram_messages_chat_id_id ON telegram_messages (chat_id, id);
CREATE INDEX telegram_messages_chat_thread_id ON telegram_messages (chat_id, thread_id, id);

-- Poller cursors: Telegram's last_update_id, Gmail's per-subscription
-- watermarks.
CREATE TABLE telegram_kv (key text PRIMARY KEY, value text NOT NULL);
CREATE TABLE gmail_kv (key text PRIMARY KEY, value text NOT NULL);

-- User-facing settings: \`timezone\` (IANA), the system-task seed flag.
CREATE TABLE settings (
  key        text PRIMARY KEY,
  value      text NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- Cron-driven tasks in the user's timezone. One-shots retire once
-- last_run_at (unix seconds of the slot fired for) is set; cancel = DELETE.
-- source NULL fires as \`scheduler\`.
CREATE TABLE scheduled_tasks (
  id          bigserial PRIMARY KEY,
  cron_expr   text NOT NULL,
  recurring   boolean NOT NULL,
  prompt      text NOT NULL,
  source      text,
  last_run_at bigint,
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX scheduled_tasks_pending ON scheduled_tasks (recurring, last_run_at);

-- The signal queue: pollers enqueue, the agent pops via get_next_signal.
CREATE TABLE signals (
  id          bigserial PRIMARY KEY,
  source      text NOT NULL,
  content     text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  consumed_at timestamptz
);
CREATE INDEX signals_pending ON signals (id) WHERE consumed_at IS NULL;
`,
  },
];

async function migrate(client: pg.PoolClient): Promise<void> {
  await client.query(
    `CREATE TABLE IF NOT EXISTS state_migrations (
       version    integer PRIMARY KEY,
       applied_at timestamptz NOT NULL DEFAULT now()
     )`,
  );
  const { rows } = await client.query<{ current: number | null }>(
    "SELECT max(version) AS current FROM state_migrations",
  );
  const current = rows[0]?.current ?? null;
  for (const migration of MIGRATIONS.filter((m) => current === null || m.version > current)) {
    await client.query("BEGIN");
    try {
      await client.query(migration.sql);
      await client.query("INSERT INTO state_migrations (version) VALUES ($1)", [migration.version]);
      await client.query("COMMIT");
    } catch (err) {
      await client.query("ROLLBACK");
      throw new Error(`state migration ${migration.version}: ${(err as Error).message}`, { cause: err });
    }
    console.log(`[mcp-state] migration ${migration.version} applied`);
  }
}

const SEEDED_FLAG = "system.seeded_default_tasks";

// `source: null` → fires as a generic `scheduler` signal; the scheduler skill
// maps the prompt body to the right composer sub-agent. Only `dreaming` uses a
// dedicated source because its skill is the top-level itself.
const SYSTEM_TASKS: { cron: string; source: string | null; prompt: string }[] = [
  {
    cron: "0 9 * * *",
    source: null,
    prompt:
      "Daily news-digest tick. Read posts from the user's subscribed " +
      "Telegram channels since the watermark in your session context, " +
      "filter to the four predefined categories (Одеса/Україна, ПМР/Молдова, " +
      "Конфликт РФ-Украина, Мир), and post a topical digest to Telegram.",
  },
  {
    cron: "0 8 * * *",
    source: null,
    prompt:
      "Daily tech-digest tick. Compose a personalized IT news digest " +
      "for the user (Hacker News, Habr) and post to Telegram. Query " +
      "the news store via search_news with topics matching the " +
      "interests in the system prompt — the pollers keep it fresh, " +
      "no need to fetch articles.",
  },
  {
    cron: "0 4 * * *",
    source: "dreaming",
    prompt:
      "Daily dreaming tick. Review the signals processed since the " +
      "previous dreaming fire (see 'Previous fire' header above) and " +
      "consider whether any skill files deserve an edit based on " +
      "patterns, recurring user feedback, or failure modes you " +
      "observed. Use list_signals(since=<previous fire>) to scope " +
      "the review. Edit skills via write_skill when warranted.",
  },
];

// Once per fresh database, guarded by a settings flag rather than row
// presence: a system task the user cancelled must stay cancelled.
async function seedSystemTasks(client: pg.PoolClient): Promise<void> {
  const { rowCount } = await client.query("SELECT 1 FROM settings WHERE key = $1", [SEEDED_FLAG]);
  if (rowCount) return;
  await client.query("BEGIN");
  try {
    for (const task of SYSTEM_TASKS) {
      await client.query(
        "INSERT INTO scheduled_tasks (cron_expr, recurring, prompt, source) VALUES ($1, true, $2, $3)",
        [task.cron, task.prompt, task.source],
      );
    }
    await client.query("INSERT INTO settings (key, value) VALUES ($1, '1')", [SEEDED_FLAG]);
    await client.query("COMMIT");
  } catch (err) {
    await client.query("ROLLBACK");
    throw err;
  }
  console.log(`[mcp-state] seeded ${SYSTEM_TASKS.length} system scheduled tasks`);
}

// A timestamptz column as "YYYY-MM-DD HH:MM:SS" UTC — sqlite's
// datetime('now'), which is what created_at / consumed_at have always looked
// like to the agent.
export function sqlTime(column: string): string {
  return `to_char(${column} AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')`;
}
