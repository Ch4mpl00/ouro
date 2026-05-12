import path from "node:path";
import Database from "better-sqlite3";

// Singleton sqlite handle for MCP-owned state (OAuth tokens). The MCP server
// is otherwise stateless and does not see the agent's DB. Default path is
// resolved relative to this source file so the lookup works regardless of
// cwd (the server may be launched from the repo root via .mcp.json).

declare global {
  // eslint-disable-next-line no-var
  var __mcp_db: Database.Database | undefined;
}

const DEFAULT_PATH = path.resolve(import.meta.dirname, "../../data/tokens.db");

function dbPath(): string {
  return process.env.MCP_DB_PATH ?? DEFAULT_PATH;
}

export function getDb(): Database.Database {
  if (!global.__mcp_db) {
    const db = new Database(dbPath());
    db.pragma("journal_mode = WAL");
    db.pragma("foreign_keys = ON");
    runMigrations(db);
    global.__mcp_db = db;
  }
  return global.__mcp_db;
}

// Idempotent at-startup migrations for additive schema changes that aren't
// covered by `CREATE TABLE IF NOT EXISTS` (i.e. ALTER TABLE for new columns).
// Re-running schema.sql on existing DBs is a no-op for these.
function runMigrations(db: Database.Database): void {
  const tableExists = db
    .prepare(`SELECT 1 FROM sqlite_master WHERE type='table' AND name=?`)
    .get("telegram_messages");
  if (tableExists) {
    const cols = db.prepare(`PRAGMA table_info(telegram_messages)`).all() as { name: string }[];
    if (!cols.some((c) => c.name === "thread_id")) {
      db.exec(`ALTER TABLE telegram_messages ADD COLUMN thread_id INTEGER`);
      db.exec(
        `CREATE INDEX IF NOT EXISTS telegram_messages_chat_thread_id ON telegram_messages(chat_id, thread_id, id)`,
      );
    }
  }
  db.exec(`
    CREATE TABLE IF NOT EXISTS news_kv (
      key   TEXT PRIMARY KEY,
      value TEXT NOT NULL
    )
  `);
  db.exec(`
    CREATE TABLE IF NOT EXISTS dreaming_kv (
      key   TEXT PRIMARY KEY,
      value TEXT NOT NULL
    )
  `);
  db.exec(`
    CREATE TABLE IF NOT EXISTS news_digest_kv (
      key   TEXT PRIMARY KEY,
      value TEXT NOT NULL
    )
  `);
  db.exec(`
    CREATE TABLE IF NOT EXISTS settings (
      key        TEXT PRIMARY KEY,
      value      TEXT NOT NULL,
      updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    )
  `);
  db.exec(`
    CREATE TABLE IF NOT EXISTS scheduled_tasks (
      id          INTEGER PRIMARY KEY AUTOINCREMENT,
      cron_expr   TEXT NOT NULL,
      recurring   INTEGER NOT NULL CHECK (recurring IN (0, 1)),
      prompt      TEXT NOT NULL,
      source      TEXT,
      last_run_at INTEGER,
      created_at  TEXT NOT NULL DEFAULT (datetime('now'))
    )
  `);
  // Existing DBs (pre-source) need ALTER TABLE; new DBs already have it
  // from the CREATE above.
  const schedCols = db.prepare(`PRAGMA table_info(scheduled_tasks)`).all() as { name: string }[];
  if (!schedCols.some((c) => c.name === "source")) {
    db.exec(`ALTER TABLE scheduled_tasks ADD COLUMN source TEXT`);
  }
  db.exec(
    `CREATE INDEX IF NOT EXISTS scheduled_tasks_pending
       ON scheduled_tasks(recurring, last_run_at)`,
  );
  seedSystemTasks(db);
  db.exec(`
    CREATE TABLE IF NOT EXISTS channel_posts (
      id            INTEGER PRIMARY KEY AUTOINCREMENT,
      chat_id       TEXT NOT NULL,
      chat_title    TEXT,
      chat_username TEXT,
      tg_message_id INTEGER NOT NULL,
      posted_at     TEXT NOT NULL,
      text          TEXT NOT NULL,
      views         INTEGER,
      forwards      INTEGER,
      fetched_at    TEXT NOT NULL DEFAULT (datetime('now')),
      UNIQUE (chat_id, tg_message_id)
    )
  `);
  db.exec(
    `CREATE INDEX IF NOT EXISTS channel_posts_posted_at
       ON channel_posts(posted_at)`,
  );
  db.exec(
    `CREATE INDEX IF NOT EXISTS channel_posts_chat_posted_at
       ON channel_posts(chat_id, posted_at)`,
  );
}

// System scheduled tasks (daily digests + dreaming). Seeded once into
// `scheduled_tasks` on first boot of a fresh DB. Idempotency is via the
// `system.seeded_default_tasks` flag in `settings` — once set, future
// boots leave the table alone. This means the user can delete a system
// task via cancel_scheduled_task and it will stay deleted across
// restarts; conversely, the user can edit cron/prompt via DB or re-add
// via schedule_task with the same source.
function seedSystemTasks(db: Database.Database): void {
  const flag = db
    .prepare(`SELECT value FROM settings WHERE key = ?`)
    .get("system.seeded_default_tasks") as { value: string } | undefined;
  if (flag) return;

  const insert = db.prepare(
    `INSERT INTO scheduled_tasks (cron_expr, recurring, prompt, source)
     VALUES (?, 1, ?, ?)`,
  );
  const DEFAULTS: { cron: string; source: string; prompt: string }[] = [
    {
      cron: "0 9 * * *",
      source: "news-digest",
      prompt:
        "Daily news-digest tick. Read posts from the user's subscribed " +
        "Telegram channels since the watermark in your session context, " +
        "filter to the four predefined categories (Одеса/Україна, ПМР/Молдова, " +
        "Конфликт РФ-Украина, Мир), and post a topical digest to Telegram.",
    },
    {
      cron: "0 8 * * *",
      source: "tech-digest",
      prompt:
        "Daily tech-digest tick. Compose a personalized IT news digest " +
        "for the user (Hacker News, Habr) and post to Telegram. Use " +
        "list_news_headlines first (titles only), pick items matching " +
        "the interests in the system prompt, then fetch_article(url) " +
        "for each pick before summarizing.",
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
  const tx = db.transaction(() => {
    for (const t of DEFAULTS) insert.run(t.cron, t.prompt, t.source);
    db.prepare(
      `INSERT INTO settings (key, value) VALUES (?, ?)
       ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')`,
    ).run("system.seeded_default_tasks", "1");
  });
  tx();
  console.log(
    `[mcp-db] seeded ${DEFAULTS.length} system scheduled tasks (${DEFAULTS.map((d) => d.source).join(", ")})`,
  );
}

export function closeDb(): void {
  if (global.__mcp_db) {
    global.__mcp_db.close();
    global.__mcp_db = undefined;
  }
}
