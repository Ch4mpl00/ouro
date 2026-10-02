// MCP-owned sqlite state: `packages/mcp/data/tokens.db`. The same file the TS
// server uses — the Rust port must open a production DB in place, so the
// schema here is byte-for-byte the one in `packages/mcp/data/schema.sql` and
// the additive migrations are the ones `db/client.ts` runs on boot.
//
// Sections:
//   1. handle   — `Db`, a cloneable, injected connection
//   2. schema   — CREATE TABLE IF NOT EXISTS for every table
//   3. migrate  — additive ALTERs for DBs older than the schema
//   4. seed     — system scheduled tasks, once per fresh DB

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};

// ── 1. handle ────────────────────────────────────────────────────────────────

// One connection behind a mutex, built once in the composition root and
// cloned into every module that needs it. Statements are short point
// lookups on tiny tables, so holding a std mutex inside an async handler is
// cheaper than a spawn_blocking round-trip; never hold it across an await.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // CLI scripts (gmail:auth, userbot:auth) open the same file while the
        // server runs; wait for their write lock instead of failing.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&conn)?;
        seed_system_tasks(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves sqlite itself consistent
        // (statements are atomic), so a poisoned mutex is safe to reuse.
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ── 2. schema ────────────────────────────────────────────────────────────────

// Mirrors packages/mcp/data/schema.sql (comments there explain each table).
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS integration_account (
  provider      TEXT NOT NULL,
  account_key   TEXT NOT NULL,
  access_token  TEXT,
  refresh_token TEXT,
  expires_at    TEXT,
  metadata      TEXT,
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at    TEXT NOT NULL DEFAULT (datetime('now')),
  PRIMARY KEY (provider, account_key)
);

CREATE TABLE IF NOT EXISTS telegram_messages (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  chat_id        INTEGER NOT NULL,
  tg_message_id  INTEGER,
  thread_id      INTEGER,
  role           TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
  text           TEXT NOT NULL,
  created_at     TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS telegram_messages_chat_id_id ON telegram_messages(chat_id, id);

CREATE TABLE IF NOT EXISTS telegram_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS gmail_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS news_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS dreaming_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS news_digest_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);

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
);
CREATE INDEX IF NOT EXISTS channel_posts_posted_at ON channel_posts(posted_at);
CREATE INDEX IF NOT EXISTS channel_posts_chat_posted_at ON channel_posts(chat_id, posted_at);

CREATE TABLE IF NOT EXISTS settings (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS scheduled_tasks (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  cron_expr   TEXT NOT NULL,
  recurring   INTEGER NOT NULL CHECK (recurring IN (0, 1)),
  prompt      TEXT NOT NULL,
  source      TEXT,
  last_run_at INTEGER,
  created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS signals (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  source       TEXT NOT NULL,
  content      TEXT NOT NULL,
  created_at   TEXT NOT NULL DEFAULT (datetime('now')),
  consumed_at  TEXT
);
CREATE INDEX IF NOT EXISTS signals_pending ON signals(consumed_at, id);
"#;

// ── 3. migrate ───────────────────────────────────────────────────────────────

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>("name"))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

// Idempotent. CREATE IF NOT EXISTS covers new tables; columns added after a
// table first shipped need an explicit ALTER on older DBs, and their indexes
// can only be created once the column exists.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;
    if !has_column(conn, "telegram_messages", "thread_id")? {
        conn.execute_batch("ALTER TABLE telegram_messages ADD COLUMN thread_id INTEGER")?;
    }
    if !has_column(conn, "scheduled_tasks", "source")? {
        conn.execute_batch("ALTER TABLE scheduled_tasks ADD COLUMN source TEXT")?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS telegram_messages_chat_thread_id
           ON telegram_messages(chat_id, thread_id, id);
         CREATE INDEX IF NOT EXISTS scheduled_tasks_pending
           ON scheduled_tasks(recurring, last_run_at);",
    )
}

// ── 4. seed ──────────────────────────────────────────────────────────────────

const SEEDED_FLAG: &str = "system.seeded_default_tasks";

struct SystemTask {
    cron: &'static str,
    // None → fires as a generic `scheduler` signal; the scheduler skill maps
    // the prompt body to the right composer sub-agent.
    source: Option<&'static str>,
    prompt: &'static str,
}

const SYSTEM_TASKS: &[SystemTask] = &[
    SystemTask {
        cron: "0 9 * * *",
        source: None,
        prompt: "Daily news-digest tick. Read posts from the user's subscribed \
                 Telegram channels since the watermark in your session context, \
                 filter to the four predefined categories (Одеса/Україна, ПМР/Молдова, \
                 Конфликт РФ-Украина, Мир), and post a topical digest to Telegram.",
    },
    SystemTask {
        cron: "0 8 * * *",
        source: None,
        prompt: "Daily tech-digest tick. Compose a personalized IT news digest \
                 for the user (Hacker News, Habr) and post to Telegram. Query \
                 the news store via search_news with topics matching the \
                 interests in the system prompt — the pollers keep it fresh, \
                 no need to fetch articles.",
    },
    SystemTask {
        cron: "0 4 * * *",
        source: Some("dreaming"),
        prompt: "Daily dreaming tick. Review the signals processed since the \
                 previous dreaming fire (see 'Previous fire' header above) and \
                 consider whether any skill files deserve an edit based on \
                 patterns, recurring user feedback, or failure modes you \
                 observed. Use list_signals(since=<previous fire>) to scope \
                 the review. Edit skills via write_skill when warranted.",
    },
];

// Once per fresh DB, guarded by a flag in `settings` rather than by row
// presence: a system task the user cancelled must stay cancelled.
fn seed_system_tasks(conn: &Connection) -> rusqlite::Result<()> {
    let seeded: Option<String> =
        conn.query_row("SELECT value FROM settings WHERE key = ?1", [SEEDED_FLAG], |r| r.get(0)).optional()?;
    if seeded.is_some() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    for task in SYSTEM_TASKS {
        tx.execute(
            "INSERT INTO scheduled_tasks (cron_expr, recurring, prompt, source) VALUES (?1, 1, ?2, ?3)",
            params![task.cron, task.prompt, task.source],
        )?;
    }
    tx.execute(
        "INSERT INTO settings (key, value) VALUES (?1, '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
        [SEEDED_FLAG],
    )?;
    tx.commit()?;
    tracing::info!(count = SYSTEM_TASKS.len(), "seeded system scheduled tasks");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_system_tasks_once() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.conn();
        let count =
            |c: &Connection| -> i64 { c.query_row("SELECT COUNT(*) FROM scheduled_tasks", [], |r| r.get(0)).unwrap() };
        assert_eq!(count(&conn), 3);

        // A cancelled system task stays cancelled across reboots.
        conn.execute("DELETE FROM scheduled_tasks WHERE source = 'dreaming'", []).unwrap();
        migrate(&conn).unwrap();
        seed_system_tasks(&conn).unwrap();
        assert_eq!(count(&conn), 2);
    }

    #[test]
    fn migrates_a_pre_thread_id_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE telegram_messages (
               id INTEGER PRIMARY KEY AUTOINCREMENT, chat_id INTEGER NOT NULL,
               tg_message_id INTEGER, role TEXT NOT NULL, text TEXT NOT NULL,
               created_at TEXT NOT NULL DEFAULT (datetime('now')));
             CREATE TABLE scheduled_tasks (
               id INTEGER PRIMARY KEY AUTOINCREMENT, cron_expr TEXT NOT NULL,
               recurring INTEGER NOT NULL, prompt TEXT NOT NULL, last_run_at INTEGER,
               created_at TEXT NOT NULL DEFAULT (datetime('now')));",
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert!(has_column(&conn, "telegram_messages", "thread_id").unwrap());
        assert!(has_column(&conn, "scheduled_tasks", "source").unwrap());
    }
}
