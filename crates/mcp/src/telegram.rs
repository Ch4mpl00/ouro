// Telegram bot domain — the canonical Telegram surface. Incoming messages
// become signals; outgoing ones are tool calls; both land in one chat log.
//
// Sections:
//   1. config  — token, default chat, forum topic map (parsed once at boot)
//   2. bot api — the handful of Bot API methods used, over plain HTTP
//   3. chat log — `telegram_messages` + the poller's `telegram_kv` cursor
//   4. typing  — chat-action keep-alive (the indicator only lives ~5s)
//   5. status  — one live progress bubble edited in place, self-animating
//   6. poller  — getUpdates long-poll → chat log + `telegram` signals
//   7. tools   — `telegram-send` (send only) and `telegram` (everything)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::db::{Db, sql_time};
use crate::server::{McpTools, ToolResult, invalid_params, json_result, respond};
use crate::signals::Signals;
use crate::time::iso_from_unix;

// ── 1. config ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct TelegramConfig {
    // TELEGRAM_ASSISTANT_BOT_TOKEN (BotFather). Optional at boot: only the
    // calls that need it fail without it, as in the TS server.
    pub bot_token: Option<String>,
    // TELEGRAM_DEFAULT_CHAT_ID: the one chat the poller accepts messages from
    // and the default destination for agent replies.
    pub default_chat_id: Option<String>,
    // TELEGRAM_TOPICS_JSON='{"bills":42,"bank":43}' — name → forum thread id.
    // Bot API has no method to list forum topics, so this static map is the
    // only way the agent learns which topics exist. Kept in the order the
    // JSON lists them, which is the order the prompt shows them in.
    pub topics: Vec<(String, i64)>,
}

impl TelegramConfig {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Self {
            bot_token: var("TELEGRAM_ASSISTANT_BOT_TOKEN"),
            default_chat_id: var("TELEGRAM_DEFAULT_CHAT_ID"),
            topics: parse_topics(var("TELEGRAM_TOPICS_JSON").as_deref()),
        }
    }
}

// Lenient by design: a malformed map degrades to "no topics" rather than
// failing boot, and non-integer ids are dropped.
fn parse_topics(raw: Option<&str>) -> Vec<(String, i64)> {
    let Some(raw) = raw else { return Vec::new() };
    let Ok(Value::Object(entries)) = serde_json::from_str(raw) else {
        if !raw.trim().is_empty() {
            tracing::warn!("TELEGRAM_TOPICS_JSON is not a JSON object, ignoring");
        }
        return Vec::new();
    };
    entries.into_iter().filter_map(|(name, id)| id.as_i64().map(|id| (name, id))).collect()
}

// ── 2. bot api ───────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum TelegramError {
    #[error("TELEGRAM_ASSISTANT_BOT_TOKEN is not set. Add it to .env (BotFather token).")]
    NoToken,
    #[error("Telegram {method} failed ({status}): {description}")]
    Api { method: &'static str, status: u16, description: String },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

impl TelegramError {
    fn description(&self) -> &str {
        match self {
            TelegramError::Api { description, .. } => description,
            _ => "",
        }
    }
}

#[derive(Clone)]
pub struct BotApi {
    http: reqwest::Client,
    token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentMessage {
    pub message_id: i64,
    pub date: i64,
    pub chat: SentChat,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentChat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<UpdateMessage>,
    pub edited_message: Option<UpdateMessage>,
    pub channel_post: Option<UpdateMessage>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMessage {
    pub message_id: i64,
    pub chat: UpdateChat,
    pub text: Option<String>,
    pub message_thread_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub username: Option<String>,
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    description: Option<String>,
    result: Option<Value>,
}

// Long-poll window inside getUpdates, seconds. The HTTP timeout must outlast it.
const LONG_POLL_TIMEOUT: u64 = 25;

impl BotApi {
    pub fn new(http: reqwest::Client, token: Option<String>) -> Self {
        Self { http, token }
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        body: Value,
    ) -> Result<T, TelegramError> {
        let token = self.token.as_deref().ok_or(TelegramError::NoToken)?;
        let res = self
            .http
            .post(format!("https://api.telegram.org/bot{token}/{method}"))
            .timeout(Duration::from_secs(LONG_POLL_TIMEOUT + 15))
            .json(&body)
            .send()
            .await?;
        let status = res.status().as_u16();
        let envelope: Envelope = res.json().await?;
        if !envelope.ok {
            let description = envelope.description.unwrap_or_else(|| "unknown error".into());
            return Err(TelegramError::Api { method, status, description });
        }
        let result = envelope.result.unwrap_or(Value::Null);
        serde_json::from_value(result).map_err(|err| TelegramError::Api {
            method,
            status,
            description: format!("unexpected result shape: {err}"),
        })
    }

    pub async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        thread_id: Option<i64>,
    ) -> Result<SentMessage, TelegramError> {
        self.call(
            "sendMessage",
            without_nulls(json!({ "chat_id": chat_id, "text": text, "message_thread_id": thread_id })),
        )
        .await
    }

    // "message is not modified" when the text is unchanged — a TelegramError
    // callers may ignore.
    pub async fn edit_message_text(
        &self,
        chat_id: &str,
        message_id: i64,
        text: &str,
    ) -> Result<SentMessage, TelegramError> {
        self.call("editMessageText", json!({ "chat_id": chat_id, "message_id": message_id, "text": text })).await
    }

    // A bot may only delete its own messages, within 48h.
    pub async fn delete_message(&self, chat_id: &str, message_id: i64) -> Result<(), TelegramError> {
        self.call::<Value>("deleteMessage", json!({ "chat_id": chat_id, "message_id": message_id })).await.map(drop)
    }

    pub async fn send_chat_action(
        &self,
        chat_id: &str,
        action: ChatAction,
        thread_id: Option<i64>,
    ) -> Result<(), TelegramError> {
        let body = without_nulls(json!({ "chat_id": chat_id, "action": action, "message_thread_id": thread_id }));
        self.call::<Value>("sendChatAction", body).await.map(drop)
    }

    pub async fn get_updates(&self, offset: Option<i64>, timeout: Option<u64>) -> Result<Vec<Update>, TelegramError> {
        let body = without_nulls(json!({
            "offset": offset,
            "timeout": timeout,
            "allowed_updates": timeout.map(|_| ["message", "edited_message"]),
        }));
        self.call("getUpdates", body).await
    }
}

// JSON.stringify drops `undefined` fields; Telegram rejects explicit nulls
// for some of them (message_thread_id: null → "Bad Request").
fn without_nulls(mut value: Value) -> Value {
    if let Value::Object(map) = &mut value {
        map.retain(|_, v| !v.is_null());
    }
    value
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatAction {
    #[default]
    Typing,
    UploadPhoto,
    RecordVideo,
    UploadVideo,
    RecordVoice,
    UploadVoice,
    UploadDocument,
    ChooseSticker,
    FindLocation,
    RecordVideoNote,
    UploadVideoNote,
}

// ── 3. chat log ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ChatLog {
    db: Db,
}

#[derive(Debug, Serialize)]
pub struct StoredMessage {
    pub id: i64,
    pub chat_id: i64,
    pub tg_message_id: Option<i64>,
    pub thread_id: Option<i64>,
    pub role: String,
    pub text: String,
    pub created_at: String,
}

#[derive(Clone, Copy)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

impl ChatLog {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub async fn record(
        &self,
        chat_id: i64,
        tg_message_id: Option<i64>,
        thread_id: Option<i64>,
        role: Role,
        text: &str,
    ) -> anyhow::Result<i64> {
        let row = self
            .db
            .client()
            .await?
            .query_one(
                "INSERT INTO telegram_messages (chat_id, tg_message_id, thread_id, role, text)
                 VALUES ($1, $2, $3, $4, $5) RETURNING id",
                &[&chat_id, &tg_message_id, &thread_id, &role.as_str(), &text],
            )
            .await?;
        Ok(row.get(0))
    }

    // Last `limit` messages, chronological. `thread_id` scopes to one forum
    // topic; None means every topic interleaved.
    pub async fn history(
        &self,
        chat_id: i64,
        limit: i64,
        thread_id: Option<i64>,
    ) -> anyhow::Result<Vec<StoredMessage>> {
        const COLUMNS: &str = "id, chat_id, tg_message_id, thread_id, role, text, created_at";
        let client = self.db.client().await?;
        let rows = match thread_id {
            None => {
                client
                    .query(&format!("SELECT {COLUMNS} FROM telegram_messages WHERE chat_id = $1 ORDER BY id DESC LIMIT $2"), &[
                        &chat_id, &limit,
                    ])
                    .await?
            }
            Some(thread) => {
                client
                    .query(
                        &format!(
                            "SELECT {COLUMNS} FROM telegram_messages WHERE chat_id = $1 AND thread_id = $2 ORDER BY id DESC LIMIT $3"
                        ),
                        &[&chat_id, &thread, &limit],
                    )
                    .await?
            }
        };
        Ok(rows
            .iter()
            .rev()
            .map(|r| StoredMessage {
                id: r.get(0),
                chat_id: r.get(1),
                tg_message_id: r.get(2),
                thread_id: r.get(3),
                role: r.get(4),
                text: r.get(5),
                created_at: sql_time(r.get(6)),
            })
            .collect())
    }

    async fn last_update_id(&self) -> anyhow::Result<Option<i64>> {
        let row = self
            .db
            .client()
            .await?
            .query_opt("SELECT value FROM telegram_kv WHERE key = 'last_update_id'", &[])
            .await?;
        Ok(row.and_then(|r| r.get::<_, String>(0).parse().ok()))
    }

    async fn set_last_update_id(&self, id: i64) -> anyhow::Result<()> {
        self.db
            .client()
            .await?
            .execute(
                "INSERT INTO telegram_kv (key, value) VALUES ('last_update_id', $1)
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
                &[&id.to_string()],
            )
            .await?;
        Ok(())
    }
}

// ── 4. typing ────────────────────────────────────────────────────────────────

// The agent calls start_typing once; this re-sends the action every ~4s until
// send_telegram_message to the same chat/thread clears it, or a safety TTL
// passes so a crashed session can't leave the dots on forever.
#[derive(Clone)]
pub struct Typing {
    bot: BotApi,
    active: Arc<Mutex<HashMap<String, TypingEntry>>>,
}

#[derive(Clone)]
struct TypingEntry {
    chat_id: String,
    action: ChatAction,
    thread_id: Option<i64>,
    expires_at: Instant,
}

const TYPING_TICK: Duration = Duration::from_secs(4);
const TYPING_TTL: Duration = Duration::from_secs(5 * 60);

fn typing_key(chat_id: &str, thread_id: Option<i64>) -> String {
    format!("{chat_id}:{}", thread_id.unwrap_or(0))
}

impl Typing {
    pub fn new(bot: BotApi) -> Self {
        Self { bot, active: Arc::default() }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, TypingEntry>> {
        self.active.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub async fn start(&self, chat_id: &str, action: ChatAction, thread_id: Option<i64>) {
        let entry =
            TypingEntry { chat_id: chat_id.to_owned(), action, thread_id, expires_at: Instant::now() + TYPING_TTL };
        self.entries().insert(typing_key(chat_id, thread_id), entry);
        // Fire once now so the indicator shows without waiting for a tick.
        if let Err(err) = self.bot.send_chat_action(chat_id, action, thread_id).await {
            tracing::error!(chat_id, %err, "typing: initial send failed");
        }
    }

    pub fn stop(&self, chat_id: &str, thread_id: Option<i64>) {
        self.entries().remove(&typing_key(chat_id, thread_id));
    }

    pub async fn run(self, cancel: CancellationToken) {
        let mut interval = tokio::time::interval(TYPING_TICK);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = interval.tick() => {}
            }
            let due: Vec<(String, TypingEntry)> = {
                let mut active = self.entries();
                let now = Instant::now();
                active.retain(|_, e| e.expires_at >= now);
                active.iter().map(|(k, e)| (k.clone(), e.clone())).collect()
            };
            for (key, entry) in due {
                if let Err(err) = self.bot.send_chat_action(&entry.chat_id, entry.action, entry.thread_id).await {
                    tracing::error!(key, %err, "typing: keepalive failed");
                }
            }
        }
    }
}

// ── 5. status ────────────────────────────────────────────────────────────────

// One progress bubble per caller-chosen id (e.g. `status:<signalId>`): the
// first call sends it, later calls edit it, empty text deletes it. On top of
// that the bubble animates itself — trailing dots cycle 0 → . → .. → ... —
// so "aliveness" is MCP's job, not the workflow's. Never written to the chat
// log: it is ephemeral progress, not conversation.
#[derive(Clone)]
pub struct StatusBubbles {
    bot: BotApi,
    entries: Arc<Mutex<HashMap<String, StatusEntry>>>,
}

#[derive(Clone)]
struct StatusEntry {
    chat_id: String,
    message_id: i64,
    base_text: String,
    frame: usize,
    // Freeze (stop editing) once this passes — a forgotten bubble must not
    // edit forever.
    expires_at: Instant,
    // Back-off after a 429 / transient error.
    next_edit_at: Instant,
    // An edit is in flight; skip the tick rather than race it.
    busy: bool,
    animating: bool,
}

const STATUS_TICK: Duration = Duration::from_secs(1);
const STATUS_TTL: Duration = Duration::from_secs(3 * 60);
const STATUS_BACKOFF: Duration = Duration::from_secs(5);
// Adjacent frames always differ, so an edit is never "not modified".
const DOT_CYCLE: usize = 4;

fn render_status(base: &str, frame: usize) -> String {
    format!("{base}{}", ".".repeat(frame % DOT_CYCLE))
}

#[derive(Debug, Serialize)]
pub struct StatusResult {
    // created · updated · deleted · noop (clear with nothing tracked)
    action: &'static str,
    id: String,
    #[serde(rename = "chatId", skip_serializing_if = "Option::is_none")]
    chat_id: Option<String>,
    #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
    message_id: Option<i64>,
    // Present (possibly null) only on `created`.
    #[serde(rename = "messageThreadId", skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<Option<i64>>,
}

impl StatusBubbles {
    pub fn new(bot: BotApi) -> Self {
        Self { bot, entries: Arc::default() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, StatusEntry>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub async fn send(
        &self,
        id: &str,
        text: &str,
        chat_id: &str,
        thread_id: Option<i64>,
    ) -> Result<StatusResult, TelegramError> {
        let text = text.trim();
        let existing = self.lock().get(id).cloned();

        if text.is_empty() {
            let Some(existing) = existing else {
                return Ok(StatusResult {
                    action: "noop",
                    id: id.into(),
                    chat_id: None,
                    message_id: None,
                    message_thread_id: None,
                });
            };
            self.lock().remove(id);
            match self.bot.delete_message(&existing.chat_id, existing.message_id).await {
                // Already gone (deleted by the user, or >48h old): cleared either way.
                Ok(()) | Err(TelegramError::Api { .. }) => {}
                Err(err) => return Err(err),
            }
            return Ok(StatusResult {
                action: "deleted",
                id: id.into(),
                chat_id: Some(existing.chat_id),
                message_id: Some(existing.message_id),
                message_thread_id: None,
            });
        }

        if let Some(existing) = existing {
            let refresh = |entries: &mut HashMap<String, StatusEntry>, reset_frame: bool| {
                if let Some(e) = entries.get_mut(id) {
                    e.base_text = text.to_owned();
                    if reset_frame {
                        e.frame = 0;
                        e.next_edit_at = Instant::now();
                    }
                    e.expires_at = Instant::now() + STATUS_TTL;
                    e.animating = true;
                }
            };
            match self.bot.edit_message_text(&existing.chat_id, existing.message_id, &render_status(text, 0)).await {
                Ok(edited) => {
                    refresh(&mut self.lock(), true);
                    return Ok(StatusResult {
                        action: "updated",
                        id: id.into(),
                        chat_id: Some(existing.chat_id),
                        message_id: Some(edited.message_id),
                        message_thread_id: None,
                    });
                }
                Err(err) if is_gone(&err) => {
                    // The bubble vanished upstream — re-create below.
                    self.lock().remove(id);
                }
                Err(err) if is_not_modified(&err) => {
                    refresh(&mut self.lock(), false);
                    return Ok(StatusResult {
                        action: "updated",
                        id: id.into(),
                        chat_id: Some(existing.chat_id),
                        message_id: Some(existing.message_id),
                        message_thread_id: None,
                    });
                }
                Err(err) => return Err(err),
            }
        }

        let sent = self.bot.send_message(chat_id, &render_status(text, 0), thread_id).await?;
        self.lock().insert(
            id.to_owned(),
            StatusEntry {
                chat_id: chat_id.to_owned(),
                message_id: sent.message_id,
                base_text: text.to_owned(),
                frame: 0,
                expires_at: Instant::now() + STATUS_TTL,
                next_edit_at: Instant::now(),
                busy: false,
                animating: true,
            },
        );
        Ok(StatusResult {
            action: "created",
            id: id.into(),
            chat_id: Some(chat_id.to_owned()),
            message_id: Some(sent.message_id),
            message_thread_id: Some(thread_id),
        })
    }

    pub async fn run(self, cancel: CancellationToken) {
        let mut interval = tokio::time::interval(STATUS_TICK);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = interval.tick() => {}
            }
            self.tick().await;
        }
    }

    async fn tick(&self) {
        let due: Vec<(String, StatusEntry)> = {
            let mut entries = self.lock();
            let now = Instant::now();
            let mut due = Vec::new();
            for (id, e) in entries.iter_mut() {
                if !e.animating {
                    continue;
                }
                if e.expires_at < now {
                    e.animating = false;
                    continue;
                }
                if e.busy || now < e.next_edit_at {
                    continue;
                }
                e.frame += 1;
                e.busy = true;
                due.push((id.clone(), e.clone()));
            }
            due
        };
        for (id, entry) in due {
            let result = self
                .bot
                .edit_message_text(&entry.chat_id, entry.message_id, &render_status(&entry.base_text, entry.frame))
                .await;
            let mut entries = self.lock();
            if let Some(e) = entries.get_mut(&id) {
                e.busy = false;
                if let Err(err) = &result
                    && !is_not_modified(err)
                {
                    e.next_edit_at = Instant::now() + STATUS_BACKOFF;
                }
            }
        }
    }
}

fn is_not_modified(err: &TelegramError) -> bool {
    matches!(err, TelegramError::Api { .. }) && err.description().to_lowercase().contains("not modified")
}

fn is_gone(err: &TelegramError) -> bool {
    let d = err.description().to_lowercase();
    matches!(err, TelegramError::Api { .. }) && (d.contains("not found") || d.contains("to edit"))
}

// ── 6. poller ────────────────────────────────────────────────────────────────

const ERROR_BACKOFF: Duration = Duration::from_secs(5);

// getUpdates allows one consumer per bot, so this lives in the MCP process —
// nothing else polls. Messages from any chat but the default are ignored:
// without a default chat the bot would accept anyone, so it stays off.
pub async fn run_poller(
    bot: BotApi,
    log: ChatLog,
    signals: Signals,
    config: TelegramConfig,
    cancel: CancellationToken,
) {
    let Some(raw) = config.default_chat_id else {
        tracing::warn!("TELEGRAM_DEFAULT_CHAT_ID is not set — telegram poller disabled (would accept anyone)");
        return;
    };
    let Ok(allowed_chat) = raw.parse::<i64>() else {
        tracing::warn!(raw, "TELEGRAM_DEFAULT_CHAT_ID is not a number — telegram poller disabled");
        return;
    };
    tracing::info!(chat = allowed_chat, timeout = LONG_POLL_TIMEOUT, "telegram poller started");

    loop {
        let offset = match log.last_update_id().await {
            Ok(last) => last.map(|id| id + 1),
            Err(err) => {
                tracing::error!(%err, "telegram poller: reading cursor failed");
                None
            }
        };
        let updates = tokio::select! {
            _ = cancel.cancelled() => break,
            updates = bot.get_updates(offset, Some(LONG_POLL_TIMEOUT)) => updates,
        };
        let outcome = match updates {
            Ok(updates) => ingest(&updates, allowed_chat, &log, &signals).await,
            Err(err) => Err(err.into()),
        };
        if let Err(err) = outcome {
            tracing::error!(error = %format!("{err:#}"), "telegram poll error");
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(ERROR_BACKOFF) => {}
            }
        }
    }
    tracing::info!("telegram poller stopped");
}

async fn ingest(updates: &[Update], allowed_chat: i64, log: &ChatLog, signals: &Signals) -> anyhow::Result<()> {
    for update in updates {
        if let Some(msg) = update.message.as_ref().or(update.edited_message.as_ref()) {
            if msg.chat.id != allowed_chat {
                tracing::warn!(chat = msg.chat.id, "ignoring message from a chat that is not the default one");
            } else if let Some(text) = msg.text.as_deref().filter(|t| !t.is_empty()) {
                log.record(msg.chat.id, Some(msg.message_id), msg.message_thread_id, Role::User, text).await?;
                signals.record("telegram", &signal_content(msg.chat.id, msg.message_thread_id, text)).await?;
                tracing::info!(message = msg.message_id, thread = ?msg.message_thread_id, "stored message + signal queued");
            }
        }
        log.set_last_update_id(update.update_id).await?;
    }
    Ok(())
}

// Signal content is DATA, not instructions: the planner knows how to reply
// (send step + chat id from envContext). Text is JSON-quoted so a message
// can't masquerade as more header lines.
fn signal_content(chat_id: i64, thread_id: Option<i64>, text: &str) -> String {
    let topic = thread_id.map(|t| format!(" (forum topic thread_id={t})")).unwrap_or_default();
    let quoted = serde_json::to_string(text).unwrap_or_default();
    format!("Telegram message in chat {chat_id}{topic}.\nText: {quoted}")
}

// ── 7. tools ─────────────────────────────────────────────────────────────────

// Models send chat ids as strings and as numbers. Accept both, normalise to
// one string so the typing keep-alive key can't split into two entries.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ChatId {
    Text(String),
    Number(i64),
}

impl ChatId {
    fn into_string(self) -> String {
        match self {
            ChatId::Text(s) => s,
            ChatId::Number(n) => n.to_string(),
        }
    }
}

const NO_CHAT_TARGET: &str = "No chat target. Pass chatId, or set TELEGRAM_DEFAULT_CHAT_ID in .env (find your id with `pnpm telegram:get-chat-id`).";

fn check_text(text: &str, allow_empty: bool) -> Result<(), ErrorData> {
    let len = text.encode_utf16().count();
    if (!allow_empty && len == 0) || len > 4096 {
        return Err(invalid_params("text must be 1–4096 characters"));
    }
    Ok(())
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SendParams {
    text: String,
    /// Telegram chat id. Falls back to TELEGRAM_DEFAULT_CHAT_ID if omitted.
    chat_id: Option<ChatId>,
    /// Forum topic thread_id. Required to reply inside a topic; omit for non-topic chats or the General topic.
    message_thread_id: Option<i64>,
}

#[tool_router(router = telegram_send_tools, vis = "pub(crate)")]
impl McpTools {
    #[tool(
        name = "send_telegram_message",
        title = "Send Telegram message",
        description = "Send a Telegram message via the assistant bot. If chatId is omitted, \
            TELEGRAM_DEFAULT_CHAT_ID env is used. Pass messageThreadId to send into \
            a specific forum topic (replies to a topic message must keep the same \
            messageThreadId so they land in the same topic; the system prompt \
            lists configured topic name → thread_id pairs if available). The \
            returned messageId can be persisted and passed to edit_telegram_message \
            later (e.g. to mark a bill as paid). The outgoing message is also \
            recorded in the local Telegram chat log so the conversation history \
            stays in sync."
    )]
    async fn send_telegram_message(&self, Parameters(p): Parameters<SendParams>) -> ToolResult {
        check_text(&p.text, false)?;
        let tg = &self.deps.telegram;
        respond(
            async {
                let target = p.chat_id.map(ChatId::into_string).or_else(|| tg.config.default_chat_id.clone());
                let target = target.ok_or_else(|| anyhow::anyhow!(NO_CHAT_TARGET))?;
                let sent = tg.bot.send_message(&target, &p.text, p.message_thread_id).await?;
                // The outgoing message clears the indicator client-side; stop the
                // keep-alive so it doesn't bleed into the next session.
                tg.typing.stop(&target, p.message_thread_id);
                tg.log
                    .record(sent.chat.id, Some(sent.message_id), p.message_thread_id, Role::Assistant, &p.text)
                    .await?;
                Ok(json!({
                    "delivered": true,
                    "chatId": target,
                    "messageId": sent.message_id,
                    "messageThreadId": p.message_thread_id,
                    "date": iso_from_unix(sent.date),
                }))
            }
            .await,
        )
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct EditParams {
    /// Telegram chat id (the one the original message was sent to).
    chat_id: ChatId,
    /// messageId returned by send_telegram_message.
    message_id: i64,
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct StatusParams {
    /// Stable per-workflow status id, e.g. `status:<signalId>`. Same id edits the same message.
    id: String,
    /// Status text. Empty string deletes the status message.
    text: String,
    /// Telegram chat id. Falls back to TELEGRAM_DEFAULT_CHAT_ID if omitted.
    chat_id: Option<ChatId>,
    /// Forum topic thread_id (used when first creating the bubble).
    message_thread_id: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct StartTypingParams {
    /// Telegram chat id.
    chat_id: ChatId,
    /// Chat action to display. Defaults to 'typing'.
    action: Option<ChatAction>,
    /// Forum topic thread_id (display the indicator inside a specific topic).
    message_thread_id: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ChatActionParams {
    /// Telegram chat id.
    chat_id: ChatId,
    /// Chat action to display.
    action: ChatAction,
    message_thread_id: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct HistoryParams {
    /// Telegram chat id.
    chat_id: ChatId,
    /// Max messages. Default 50.
    #[schemars(range(min = 1, max = 500))]
    limit: Option<i64>,
    /// Forum topic thread_id. Restricts results to a single topic. Omit for unfiltered history.
    thread_id: Option<i64>,
}

#[tool_router(router = telegram_tools, vis = "pub(crate)")]
impl McpTools {
    #[tool(
        name = "edit_telegram_message",
        title = "Edit Telegram message",
        description = "Edit a previously-sent Telegram message in place. Use to update a bill \
            notification when status changes (e.g. mark as PAID). messageId is the value \
            returned by send_telegram_message; chatId is the same chat the message was sent to."
    )]
    async fn edit_telegram_message(&self, Parameters(p): Parameters<EditParams>) -> ToolResult {
        check_text(&p.text, false)?;
        let tg = &self.deps.telegram;
        let chat_id = p.chat_id.into_string();
        respond(async {
            let edited = tg.bot.edit_message_text(&chat_id, p.message_id, &p.text).await?;
            Ok(json!({ "edited": true, "chatId": chat_id, "messageId": edited.message_id, "date": iso_from_unix(edited.date) }))
        }
        .await)
    }

    #[tool(
        name = "telegram_send_status",
        title = "Send / update / clear a live status message",
        description = "Show live progress in a SINGLE Telegram message edited in place, \
            instead of posting a new message per step. Call with the same `id` \
            repeatedly to update the same bubble: the first call sends it, later \
            calls edit it. Call with an EMPTY `text` to delete the bubble when \
            the work is done. Use a stable per-workflow id like `status:<signalId>`. \
            chatId falls back to TELEGRAM_DEFAULT_CHAT_ID. Status messages are \
            ephemeral — they are NOT written to the chat log; ship the real answer \
            with send_telegram_message."
    )]
    async fn telegram_send_status(&self, Parameters(p): Parameters<StatusParams>) -> ToolResult {
        if p.id.is_empty() {
            return Err(invalid_params("id must be non-empty"));
        }
        check_text(&p.text, true)?;
        let tg = &self.deps.telegram;
        respond(
            async {
                let target = p.chat_id.map(ChatId::into_string).or_else(|| tg.config.default_chat_id.clone());
                let target = target.ok_or_else(|| anyhow::anyhow!(NO_CHAT_TARGET))?;
                Ok(tg.status.send(&p.id, &p.text, &target, p.message_thread_id).await?)
            }
            .await,
        )
    }

    #[tool(
        name = "start_typing",
        title = "Start typing indicator (auto-refresh until reply)",
        description = "Show a chat action indicator (typing by default) in a Telegram chat \
            and keep it alive. MCP re-sends the action every ~4s in the \
            background — call this ONCE at the start of a session, no need to \
            ping it on every reasoning round. The indicator clears \
            automatically when your `send_telegram_message` to the same chat/\
            thread is delivered. A safety TTL stops the keep-alive after \
            5 minutes if no message ever ships."
    )]
    async fn start_typing(&self, Parameters(p): Parameters<StartTypingParams>) -> ToolResult {
        let tg = &self.deps.telegram;
        tg.typing.start(&p.chat_id.into_string(), p.action.unwrap_or_default(), p.message_thread_id).await;
        json_result(&json!({ "started": true }))
    }

    // An escape hatch for one-off non-typing actions (an `upload_photo` blip
    // before posting an image); start_typing is the self-refreshing one.
    #[tool(
        name = "send_telegram_chat_action",
        title = "Send a one-shot Telegram chat action (no auto-refresh)",
        description = "Send a single chat action ping (~5s lifespan, no keep-alive). \
            Prefer `start_typing` for the common 'show typing while I work' \
            case — this one is for one-off non-typing actions like a brief \
            `upload_photo` before sending an image."
    )]
    async fn send_telegram_chat_action(&self, Parameters(p): Parameters<ChatActionParams>) -> ToolResult {
        let tg = &self.deps.telegram;
        let chat_id = p.chat_id.into_string();
        respond(
            async {
                tg.bot.send_chat_action(&chat_id, p.action, p.message_thread_id).await?;
                Ok(json!({ "sent": true }))
            }
            .await,
        )
    }

    #[tool(
        name = "get_telegram_chat_history",
        title = "Get Telegram chat history",
        description = "Read the last N messages of a Telegram chat from the local log, in \
            chronological order. Pass threadId to scope to a single forum topic \
            (reply context for a topic message). Omit threadId to see all topics \
            interleaved."
    )]
    async fn get_telegram_chat_history(&self, Parameters(p): Parameters<HistoryParams>) -> ToolResult {
        if p.limit.is_some_and(|l| !(1..=500).contains(&l)) {
            return Err(invalid_params("limit must be between 1 and 500"));
        }
        let tg = &self.deps.telegram;
        respond(
            async {
                let chat_id = match p.chat_id {
                    ChatId::Number(n) => n,
                    ChatId::Text(s) => {
                        s.trim().parse().map_err(|_| anyhow::anyhow!("chatId must be numeric, got {s}"))?
                    }
                };
                Ok(json!({ "messages": tg.log.history(chat_id, p.limit.unwrap_or(50), p.thread_id).await? }))
            }
            .await,
        )
    }
}

// Everything the telegram tools reach, bundled so `Deps` carries one field.
#[derive(Clone)]
pub struct TelegramModule {
    pub config: TelegramConfig,
    pub bot: BotApi,
    pub log: ChatLog,
    pub typing: Typing,
    pub status: StatusBubbles,
}

impl TelegramModule {
    pub fn new(config: TelegramConfig, http: reqwest::Client, db: Db) -> Self {
        let bot = BotApi::new(http, config.bot_token.clone());
        Self {
            typing: Typing::new(bot.clone()),
            status: StatusBubbles::new(bot.clone()),
            log: ChatLog::new(db),
            bot,
            config,
        }
    }
}

pub fn chat_label(chat: &UpdateChat) -> String {
    if let Some(title) = &chat.title {
        return title.clone();
    }
    [chat.first_name.clone(), chat.last_name.clone(), chat.username.as_ref().map(|u| format!("@{u}"))]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_topics_leniently_in_json_order() {
        assert!(parse_topics(None).is_empty());
        assert!(parse_topics(Some("[1,2]")).is_empty());
        assert!(parse_topics(Some("not json")).is_empty());
        let topics = parse_topics(Some(r#"{"news":7,"bank":"43","bills":42}"#));
        assert_eq!(topics, vec![("news".into(), 7), ("bills".into(), 42)]);
    }

    #[test]
    fn signal_content_quotes_the_text_and_names_the_topic() {
        assert_eq!(
            signal_content(5, Some(42), "hi \"there\"\nnext"),
            "Telegram message in chat 5 (forum topic thread_id=42).\nText: \"hi \\\"there\\\"\\nnext\""
        );
        assert_eq!(signal_content(5, None, "x"), "Telegram message in chat 5.\nText: \"x\"");
    }

    #[tokio::test]
    async fn ingest_keeps_only_the_default_chat_and_advances_the_cursor() {
        let Some(db) = Db::test().await else { return };
        let (log, signals) = (ChatLog::new(db.clone()), Signals::new(db));
        let updates: Vec<Update> = serde_json::from_value(json!([
            { "update_id": 10, "message": { "message_id": 1, "chat": { "id": 7, "type": "private" }, "text": "hello" } },
            { "update_id": 11, "message": { "message_id": 2, "chat": { "id": 99, "type": "private" }, "text": "spam" } },
            { "update_id": 12, "edited_message": { "message_id": 3, "chat": { "id": 7, "type": "supergroup" }, "text": "edited", "message_thread_id": 4 } },
        ]))
        .unwrap();
        ingest(&updates, 7, &log, &signals).await.unwrap();

        assert_eq!(log.last_update_id().await.unwrap(), Some(12));
        let history = log.history(7, 50, None).await.unwrap();
        assert_eq!(history.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["hello", "edited"]);
        assert_eq!(log.history(7, 50, Some(4)).await.unwrap().len(), 1);
        assert_eq!(signals.count_pending().await.unwrap(), 2);
    }

    #[test]
    fn status_frames_cycle_trailing_dots() {
        let frames: Vec<String> = (0..5).map(|f| render_status("Работаю", f)).collect();
        assert_eq!(frames, ["Работаю", "Работаю.", "Работаю..", "Работаю...", "Работаю"]);
    }
}
