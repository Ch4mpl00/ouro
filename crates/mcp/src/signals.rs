// The signal queue (`signals` table). Pollers enqueue rows; the agent pops
// them one at a time via `get_next_signal`. The agent never polls external
// systems itself — every event it sees was built and queued by a poller in
// this process.
//
// Sections:
//   1. queue       — record / pop / count / list
//   2. env context — the per-signal environment note for the agent's prompt
//   3. tools       — `signals` toolset (get_next_signal),
//                    `dreaming` toolset (list_signals)

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use rusqlite::{OptionalExtension, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::server::{McpTools, ToolResult, invalid_params, json_result};
use crate::telegram::TelegramConfig;

// ── 1. queue ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Signals {
    db: Db,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct PendingSignal {
    pub id: i64,
    pub source: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct SignalRow {
    pub id: i64,
    pub source: String,
    pub content: String,
    pub created_at: String,
    pub consumed_at: Option<String>,
}

#[derive(Default)]
pub struct ListSignals {
    // Exclusive lower bound on created_at.
    pub since: Option<String>,
    pub source: Option<String>,
    pub limit: Option<u32>,
}

const DEFAULT_LIST_LIMIT: u32 = 200;

impl Signals {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn record(&self, source: &str, content: &str) -> rusqlite::Result<i64> {
        let conn = self.db.conn();
        conn.execute("INSERT INTO signals (source, content) VALUES (?1, ?2)", [source, content])?;
        Ok(conn.last_insert_rowid())
    }

    // Atomically pops the oldest pending signal: one UPDATE … RETURNING, so a
    // concurrent caller can never be handed the same row.
    pub fn pop_next(&self) -> rusqlite::Result<Option<PendingSignal>> {
        self.db
            .conn()
            .query_row(
                "UPDATE signals
                    SET consumed_at = datetime('now')
                  WHERE id = (SELECT id FROM signals WHERE consumed_at IS NULL ORDER BY id ASC LIMIT 1)
                  RETURNING id, source, content, created_at",
                [],
                |r| Ok(PendingSignal { id: r.get(0)?, source: r.get(1)?, content: r.get(2)?, created_at: r.get(3)? }),
            )
            .optional()
    }

    pub fn count_pending(&self) -> rusqlite::Result<i64> {
        self.db.conn().query_row("SELECT COUNT(*) FROM signals WHERE consumed_at IS NULL", [], |r| r.get(0))
    }

    // Read-only view; never pops. The dreaming session reviews what happened
    // since its previous fire with this.
    pub fn list(&self, filter: &ListSignals) -> rusqlite::Result<Vec<SignalRow>> {
        let mut clauses = Vec::new();
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(since) = &filter.since {
            clauses.push("created_at > ?");
            args.push(since.clone().into());
        }
        if let Some(source) = &filter.source {
            clauses.push("source = ?");
            args.push(source.clone().into());
        }
        let filter_sql = if clauses.is_empty() { String::new() } else { format!("WHERE {}", clauses.join(" AND ")) };
        args.push(i64::from(filter.limit.unwrap_or(DEFAULT_LIST_LIMIT)).into());

        let conn = self.db.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, source, content, created_at, consumed_at FROM signals {filter_sql} ORDER BY id ASC LIMIT ?"
        ))?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok(SignalRow {
                id: r.get(0)?,
                source: r.get(1)?,
                content: r.get(2)?,
                created_at: r.get(3)?,
                consumed_at: r.get(4)?,
            })
        })?;
        rows.collect()
    }
}

// ── 2. env context ───────────────────────────────────────────────────────────

// Attached to every popped signal so the agent knows where to send
// notifications and which forum topics exist. Skill text is not attached —
// the agent loads `skills/<source>.md` itself.
pub fn env_context(telegram: &TelegramConfig) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(chat_id) = &telegram.default_chat_id {
        lines.push(format!("Default Telegram chat id: {chat_id}."));
    }
    if !telegram.topics.is_empty() {
        lines.push("Available Telegram forum topics (name → thread_id):".to_owned());
        for (name, id) in &telegram.topics {
            lines.push(format!("  - {name}: {id}"));
        }
        lines.push(
            "When sending a message that semantically belongs to one of these topics, \
             pass the matching messageThreadId to send_telegram_message."
                .to_owned(),
        );
    }
    if lines.is_empty() {
        return None;
    }
    Some(["", "## Environment"].into_iter().map(str::to_owned).chain(lines).collect::<Vec<_>>().join("\n"))
}

// ── 3. tools ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct NextSignal {
    #[serde(flatten)]
    signal: PendingSignal,
    #[serde(rename = "envContext")]
    env_context: Option<String>,
}

#[derive(Serialize)]
struct NextSignalResult {
    signal: Option<NextSignal>,
    #[serde(rename = "pendingAfter")]
    pending_after: i64,
}

#[tool_router(router = signals_tools, vis = "pub(crate)")]
impl McpTools {
    #[tool(
        name = "get_next_signal",
        title = "Get the next pending signal",
        description = "Atomically pop the oldest pending signal from the MCP queue. \
            Returns `{ signal, pendingAfter }`. The signal carries `content` \
            (the user-message payload for the agent) and `envContext` (a \
            short note describing the default Telegram chat id and the \
            configured forum topics — agent prepends this to the system \
            prompt). Skill instructions are NOT attached; the agent loads \
            `skills/<source>.md` itself. Returns `signal: null` when the \
            queue is empty. This is the agent's only way to learn about \
            external events."
    )]
    async fn get_next_signal(&self) -> ToolResult {
        let signals = &self.deps.signals;
        let Some(signal) = crate::try_tool!(signals.pop_next()) else {
            return json_result(&NextSignalResult { signal: None, pending_after: 0 });
        };
        json_result(&NextSignalResult {
            signal: Some(NextSignal { signal, env_context: env_context(&self.deps.telegram.config) }),
            pending_after: crate::try_tool!(signals.count_pending()),
        })
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListSignalsParams {
    /// ISO timestamp. Only signals with created_at > since are returned.
    since: Option<String>,
    /// Restrict to a single signal source.
    source: Option<String>,
    /// Max rows. Default 200.
    #[schemars(range(min = 1, max = 2000))]
    limit: Option<u32>,
}

#[derive(Serialize)]
struct ListSignalsResult {
    count: usize,
    signals: Vec<SignalRow>,
}

// "dreaming" is a historical toolset name: only list_signals lives in it.
#[tool_router(router = dreaming_tools, vis = "pub(crate)")]
impl McpTools {
    #[tool(
        name = "list_signals",
        title = "List past signals",
        description = "Read-only view of past signals (does not pop or mutate the queue). \
            Optional filters: `since` (ISO timestamp, returns signals created \
            after this), `source` (e.g. 'telegram', 'nashdom-bill'). Default \
            limit 200. Used by the dreaming skill to review what happened \
            since the previous reflection."
    )]
    async fn list_signals(
        &self,
        Parameters(ListSignalsParams { since, source, limit }): Parameters<ListSignalsParams>,
    ) -> ToolResult {
        if limit.is_some_and(|l| !(1..=2000).contains(&l)) {
            return Err(invalid_params("limit must be between 1 and 2000"));
        }
        let signals = crate::try_tool!(self.deps.signals.list(&ListSignals { since, source, limit }));
        json_result(&ListSignalsResult { count: signals.len(), signals })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> Signals {
        Signals::new(Db::open_in_memory().unwrap())
    }

    #[test]
    fn pops_in_fifo_order_exactly_once() {
        let s = signals();
        let a = s.record("telegram", "hi").unwrap();
        let b = s.record("scheduler", "tick").unwrap();
        assert_eq!(s.count_pending().unwrap(), 2);
        assert_eq!(s.pop_next().unwrap().map(|p| p.id), Some(a));
        assert_eq!(s.pop_next().unwrap().map(|p| p.id), Some(b));
        assert_eq!(s.pop_next().unwrap(), None);
        assert_eq!(s.count_pending().unwrap(), 0);
    }

    #[test]
    fn list_filters_without_consuming() {
        let s = signals();
        s.record("telegram", "a").unwrap();
        s.record("gmail", "b").unwrap();
        s.record("telegram", "c").unwrap();
        let only_tg = s.list(&ListSignals { source: Some("telegram".into()), ..Default::default() }).unwrap();
        assert_eq!(only_tg.iter().map(|r| r.content.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(s.list(&ListSignals { limit: Some(1), ..Default::default() }).unwrap().len(), 1);
        assert_eq!(s.count_pending().unwrap(), 3);
    }

    #[test]
    fn env_context_matches_the_ts_wording() {
        assert_eq!(env_context(&TelegramConfig::default()), None);
        let cfg =
            TelegramConfig { bot_token: None, default_chat_id: Some("123".into()), topics: vec![("bills".into(), 42)] };
        assert_eq!(
            env_context(&cfg).unwrap(),
            "\n## Environment\nDefault Telegram chat id: 123.\n\
             Available Telegram forum topics (name → thread_id):\n  - bills: 42\n\
             When sending a message that semantically belongs to one of these topics, \
             pass the matching messageThreadId to send_telegram_message."
        );
    }
}
