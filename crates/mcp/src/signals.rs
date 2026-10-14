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

use chrono::{DateTime, Utc};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::{Db, sql_time};
use crate::pg::{Query, where_clause};
use crate::server::{McpTools, ToolResult, invalid_params, json_result};
use crate::telegram::TelegramConfig;
use crate::time::require_js_date;

// ── 1. queue ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Signals {
    db: Db,
}

// Timestamps go out as "YYYY-MM-DD HH:MM:SS" UTC, the shape the agent has
// always been handed.
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
    pub since: Option<DateTime<Utc>>,
    pub source: Option<String>,
    pub limit: Option<u32>,
}

const DEFAULT_LIST_LIMIT: u32 = 200;

impl Signals {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub async fn record(&self, source: &str, content: &str) -> anyhow::Result<i64> {
        let row = self
            .db
            .client()
            .await?
            .query_one("INSERT INTO signals (source, content) VALUES ($1, $2) RETURNING id", &[&source, &content])
            .await?;
        Ok(row.get(0))
    }

    // Atomically pops the oldest pending signal. SKIP LOCKED: a concurrent
    // popper takes the next row instead of waiting, never the same one.
    pub async fn pop_next(&self) -> anyhow::Result<Option<PendingSignal>> {
        let row = self
            .db
            .client()
            .await?
            .query_opt(
                "UPDATE signals SET consumed_at = now()
                  WHERE id = (SELECT id FROM signals WHERE consumed_at IS NULL
                               ORDER BY id ASC LIMIT 1 FOR UPDATE SKIP LOCKED)
                  RETURNING id, source, content, created_at",
                &[],
            )
            .await?;
        Ok(row.map(|r| PendingSignal {
            id: r.get(0),
            source: r.get(1),
            content: r.get(2),
            created_at: sql_time(r.get(3)),
        }))
    }

    pub async fn count_pending(&self) -> anyhow::Result<i64> {
        Ok(self
            .db
            .client()
            .await?
            .query_one("SELECT count(*) FROM signals WHERE consumed_at IS NULL", &[])
            .await?
            .get(0))
    }

    // Read-only view; never pops. The dreaming session reviews what happened
    // since its previous fire with this.
    pub async fn list(&self, filter: &ListSignals) -> anyhow::Result<Vec<SignalRow>> {
        let mut q = Query::default();
        let mut filters = Vec::new();
        if let Some(since) = filter.since {
            filters.push(format!("created_at > {}", q.bind(since)));
        }
        if let Some(source) = &filter.source {
            filters.push(format!("source = {}", q.bind(source.clone())));
        }
        let limit = q.bind(i64::from(filter.limit.unwrap_or(DEFAULT_LIST_LIMIT)));
        let sql = format!(
            "SELECT id, source, content, created_at, consumed_at FROM signals {} ORDER BY id ASC LIMIT {limit}",
            where_clause(&filters)
        );
        let rows = self.db.client().await?.query(&sql, &q.params()).await?;
        Ok(rows
            .iter()
            .map(|r| SignalRow {
                id: r.get(0),
                source: r.get(1),
                content: r.get(2),
                created_at: sql_time(r.get(3)),
                consumed_at: r.get::<_, Option<DateTime<Utc>>>(4).map(sql_time),
            })
            .collect())
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
        let Some(signal) = crate::try_tool!(signals.pop_next().await) else {
            return json_result(&NextSignalResult { signal: None, pending_after: 0 });
        };
        json_result(&NextSignalResult {
            signal: Some(NextSignal { signal, env_context: env_context(&self.deps.telegram.config) }),
            pending_after: crate::try_tool!(signals.count_pending().await),
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
        // "YYYY-MM-DD HH:MM:SS" and full ISO both parse, so a `since` copied
        // from a signal or from a "Previous fire:" header both work.
        let since = crate::try_tool!(since.map(|s| require_js_date("since", &s)).transpose());
        let signals = crate::try_tool!(self.deps.signals.list(&ListSignals { since, source, limit }).await);
        json_result(&ListSignalsResult { count: signals.len(), signals })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pops_in_fifo_order_exactly_once_even_concurrently() {
        let Some(db) = Db::test().await else { return };
        let s = Signals::new(db);
        let a = s.record("telegram", "hi").await.unwrap();
        let b = s.record("scheduler", "tick").await.unwrap();
        assert_eq!(s.count_pending().await.unwrap(), 2);
        let (x, y) = tokio::join!(s.pop_next(), s.pop_next());
        let mut got = vec![x.unwrap().unwrap().id, y.unwrap().unwrap().id];
        got.sort();
        assert_eq!(got, [a, b]);
        assert_eq!(s.pop_next().await.unwrap(), None);
        assert_eq!(s.count_pending().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn list_filters_by_time_and_source_without_consuming() {
        let Some(db) = Db::test().await else { return };
        let s = Signals::new(db.clone());
        s.record("telegram", "a").await.unwrap();
        s.record("gmail", "b").await.unwrap();
        s.record("telegram", "c").await.unwrap();
        db.client()
            .await
            .unwrap()
            .execute("UPDATE signals SET created_at = '2026-10-01 10:00:00+00' WHERE content = 'a'", &[])
            .await
            .unwrap();
        let only_tg = s.list(&ListSignals { source: Some("telegram".into()), ..Default::default() }).await.unwrap();
        assert_eq!(only_tg.iter().map(|r| r.content.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(only_tg[0].created_at, "2026-10-01 10:00:00");
        // A real time comparison: same-day ISO no longer loses to the sqlite
        // text format the way string comparison did.
        let since = require_js_date("since", "2026-10-01T10:00:00.000Z").unwrap();
        let after: Vec<String> = s
            .list(&ListSignals { since: Some(since), ..Default::default() })
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.content)
            .collect();
        assert_eq!(after, ["b", "c"]);
        assert_eq!(s.list(&ListSignals { limit: Some(1), ..Default::default() }).await.unwrap().len(), 1);
        assert_eq!(s.count_pending().await.unwrap(), 3);
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
