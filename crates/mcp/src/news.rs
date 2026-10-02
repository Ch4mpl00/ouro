// The news / RAG store: HN, Habr and harvested Telegram channel posts in one
// `news_items` table, embedded on the way in, searched by meaning.
//
// Sections:
//   1. items      — the unified item and the shapes the tools return
//   2. article    — URL → readable plain text (Readability)
//   3. providers  — Hacker News, Habr, Telegram channels
//   4. repository — save / upsert / list / vector search over Postgres
//   5. ranking    — merging per-query pools: min distance, then dedup
//   6. poller     — one cadence loop over every provider
//   7. tools      — `news-read` toolset: search_news, list_news, fetch_article

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_postgres::Row;
use tokio_util::sync::CancellationToken;

use crate::embeddings::{DEFAULT_DEDUP_THRESHOLD, SharedEmbedder, dedup_by_pairwise_cosine};
use crate::fetch::BROWSER_UA;
use crate::pg::{PgPool, Query, parse_vector, vector_literal, where_clause};
use crate::server::{McpTools, ToolResult, invalid_params, json_result, respond};
use crate::time::{iso, parse_js_date, require_js_date};
use crate::userbot::Userbot;

// ── 1. items ─────────────────────────────────────────────────────────────────

// `external_id` is the natural key in the source: the URL for HN/Habr,
// "<chat_id>:<tg_message_id>" for channel posts.
#[derive(Debug, Clone, PartialEq)]
pub struct NewsItem {
    pub source: String,
    pub external_id: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub body: String,
    pub metadata: Map<String, Value>,
    pub posted_at: Option<DateTime<Utc>>,
}

impl NewsItem {
    fn to_json(&self) -> Value {
        json!({
            "source": self.source,
            "externalId": self.external_id,
            "title": self.title,
            "url": self.url,
            "body": self.body,
            "metadata": self.metadata,
            "postedAt": self.posted_at.map(iso),
        })
    }
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct SaveResult {
    pub saved: usize,
    pub embedded: usize,
    pub failed: usize,
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct EmbedResult {
    pub embedded: usize,
    pub failed: usize,
}

#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub source: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    // Point-in-time replay for evals: what was searchable (embedded_at) or
    // stored (fetched_at) at this instant. Not the same as `until`, which
    // bounds posted_at.
    pub as_of: Option<DateTime<Utc>>,
    // Channel posts only: metadata.chat_username OR chat_id.
    pub channel: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub id: i64,
    pub source: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub snippet: String,
    pub posted_at: Option<String>,
    pub distance: f64,
    pub metadata: Map<String, Value>,
    // Batch path only: indices of the queries that surfaced this item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_queries: Option<Vec<usize>>,
}

// ── 2. article ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Article {
    pub url: String,
    pub title: String,
    pub text: String,
    pub site: Option<String>,
    // Unparseable dates are dropped rather than poisoning the row.
    pub published_at: Option<DateTime<Utc>>,
    pub author: Option<String>,
}

#[derive(Clone)]
pub struct ArticleFetcher {
    http: reqwest::Client,
}

impl ArticleFetcher {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    pub async fn fetch(&self, url: &str) -> anyhow::Result<Article> {
        let res = self.http.get(url).header("user-agent", BROWSER_UA).timeout(Duration::from_secs(30)).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("No article extracted from {url} (HTTP {})", res.status().as_u16());
        }
        let html = res.text().await?;
        let url = url.to_owned();
        tokio::task::spawn_blocking(move || extract_article(&url, &html)).await?
    }

    // Three attempts with a short backoff; None on terminal failure so a
    // poller simply drops the item.
    pub async fn fetch_with_retry(&self, url: &str) -> Option<Article> {
        let mut last = None;
        for attempt in 0..3u64 {
            match self.fetch(url).await {
                Ok(article) => return Some(article),
                Err(err) => last = Some(err),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(500 * (attempt + 1))).await;
            }
        }
        if let Some(err) = last {
            tracing::warn!(url, error = %format!("{err:#}"), "article fetch failed after 3 attempts");
        }
        None
    }
}

pub fn extract_article(url: &str, html: &str) -> anyhow::Result<Article> {
    let mut readability = dom_smoothie::Readability::new(html, Some(url), None)?;
    let article = readability.parse().map_err(|err| anyhow::anyhow!("No article extracted from {url}: {err}"))?;
    let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_owned()));
    Ok(Article {
        url: url.to_owned(),
        title: article.title,
        text: strip_html(&article.content),
        site: article.site_name.filter(|s| !s.is_empty()).or(host),
        published_at: article.published_time.as_deref().and_then(parse_js_date),
        author: article.byline.filter(|s| !s.is_empty()),
    })
}

// A compact text blob for the LLM: whatever markup the extractor left, gone.
pub fn strip_html(html: &str) -> String {
    let re = |p: &str| regex::Regex::new(p).expect("valid regex");
    let s = re(r"(?is)<script[\s\S]*?</script>").replace_all(html, " ");
    let s = re(r"(?is)<style[\s\S]*?</style>").replace_all(&s, " ");
    let s = re(r"<[^>]+>").replace_all(&s, " ");
    let s = s
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    re(r"\s+").replace_all(&s, " ").trim().to_owned()
}

// ── 3. providers ─────────────────────────────────────────────────────────────

// A self-contained source. The poller ticks it every `cadence`, stores what
// it returns; quirks (endpoints, watermarks) stay inside the provider.
#[async_trait]
pub trait NewsProvider: Send + Sync {
    fn source(&self) -> &'static str;
    fn cadence(&self) -> Duration;
    async fn fetch(&self) -> anyhow::Result<Vec<NewsItem>>;
}

#[derive(Debug, Clone)]
pub struct Headline {
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    pub posted_at: Option<DateTime<Utc>>,
    // Source-specific extras, written into metadata ahead of author/site.
    pub extra: Map<String, Value>,
}

// The shared article mapping. Keys whose value is absent are omitted, as
// JSON.stringify dropped `undefined`; the headline's author wins.
pub fn article_item(source: &str, headline: &Headline, article: Option<Article>) -> Option<NewsItem> {
    let article = article.filter(|a| !a.text.trim().is_empty())?;
    let mut metadata = headline.extra.clone();
    if let Some(author) = headline.author.clone().or(article.author.clone()) {
        metadata.insert("author".into(), author.into());
    }
    if let Some(site) = article.site.clone() {
        metadata.insert("site".into(), site.into());
    }
    Some(NewsItem {
        source: source.to_owned(),
        external_id: headline.url.clone(),
        title: Some(if article.title.is_empty() { headline.title.clone() } else { article.title.clone() }),
        url: Some(headline.url.clone()),
        body: article.text,
        metadata,
        posted_at: article.published_at.or(headline.posted_at),
    })
}

async fn fetch_articles(source: &str, articles: &ArticleFetcher, headlines: Vec<Headline>) -> Vec<NewsItem> {
    let fetched = futures::future::join_all(headlines.iter().map(|h| articles.fetch_with_retry(&h.url))).await;
    headlines.iter().zip(fetched).filter_map(|(h, a)| article_item(source, h, a)).collect()
}

const HEADLINE_LIMIT: usize = 30;

pub struct HackerNews {
    http: reqwest::Client,
    articles: ArticleFetcher,
}

impl HackerNews {
    pub fn new(http: reqwest::Client) -> Self {
        Self { articles: ArticleFetcher::new(http.clone()), http }
    }

    async fn headlines(&self, limit: usize) -> anyhow::Result<Vec<Headline>> {
        const API: &str = "https://hacker-news.firebaseio.com/v0";
        #[derive(Deserialize)]
        struct Item {
            title: Option<String>,
            url: Option<String>,
            score: Option<i64>,
            descendants: Option<i64>,
            by: Option<String>,
            time: Option<i64>,
        }
        let res = self.http.get(format!("{API}/topstories.json")).send().await?;
        anyhow::ensure!(res.status().is_success(), "HN topstories failed: {}", res.status().as_u16());
        let ids: Vec<i64> = res.json().await?;
        let items = futures::future::join_all(ids.into_iter().take(limit).map(|id| async move {
            let item: Item = self.http.get(format!("{API}/item/{id}.json")).send().await.ok()?.json().await.ok()?;
            let mut extra = Map::new();
            extra.insert("hn_id".into(), id.into());
            if let Some(score) = item.score {
                extra.insert("score".into(), score.into());
            }
            if let Some(comments) = item.descendants {
                extra.insert("comments".into(), comments.into());
            }
            Some(Headline {
                title: item.title?,
                url: item.url.unwrap_or_else(|| format!("https://news.ycombinator.com/item?id={id}")),
                author: item.by,
                posted_at: item.time.and_then(|t| DateTime::from_timestamp(t, 0)),
                extra,
            })
        }))
        .await;
        Ok(items.into_iter().flatten().collect())
    }
}

#[async_trait]
impl NewsProvider for HackerNews {
    fn source(&self) -> &'static str {
        "hackernews"
    }

    fn cadence(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }

    async fn fetch(&self) -> anyhow::Result<Vec<NewsItem>> {
        Ok(fetch_articles(self.source(), &self.articles, self.headlines(HEADLINE_LIMIT).await?).await)
    }
}

// The overall feed; downstream filters by interest.
pub struct Habr {
    http: reqwest::Client,
    articles: ArticleFetcher,
}

impl Habr {
    pub fn new(http: reqwest::Client) -> Self {
        Self { articles: ArticleFetcher::new(http.clone()), http }
    }

    async fn headlines(&self, limit: usize) -> anyhow::Result<Vec<Headline>> {
        let bytes = self
            .http
            .get("https://habr.com/ru/rss/all/")
            .timeout(Duration::from_secs(15))
            .send()
            .await?
            .bytes()
            .await?;
        let feed = feed_rs::parser::parse(bytes.as_ref())?;
        Ok(feed
            .entries
            .into_iter()
            .take(limit)
            .filter_map(|entry| {
                Some(Headline {
                    title: entry.title.map(|t| t.content).filter(|t| !t.is_empty())?,
                    url: entry.links.first().map(|l| l.href.clone()).filter(|u| !u.is_empty())?,
                    author: entry.authors.first().and_then(|a| a.name.clone()).filter(|n| !n.is_empty()),
                    posted_at: entry.published.or(entry.updated),
                    extra: Map::new(),
                })
            })
            .collect())
    }
}

#[async_trait]
impl NewsProvider for Habr {
    fn source(&self) -> &'static str {
        "habr"
    }

    fn cadence(&self) -> Duration {
        Duration::from_secs(30 * 60)
    }

    async fn fetch(&self) -> anyhow::Result<Vec<NewsItem>> {
        Ok(fetch_articles(self.source(), &self.articles, self.headlines(HEADLINE_LIMIT).await?).await)
    }
}

// Every channel dialog the userbot follows. Per-channel watermark = the max
// tg_message_id already stored for that chat. A new channel gets its last
// BOOTSTRAP posts, then DELTA at a time; a short pause between channels
// keeps clear of FLOOD_WAIT. No userbot session → nothing, not an error.
pub struct TelegramChannels {
    userbot: Userbot,
    pool: PgPool,
    inter_channel_delay: Duration,
}

const CHANNEL_SOURCE: &str = "channel";
const BOOTSTRAP_LIMIT: usize = 50;
const DELTA_LIMIT: usize = 200;

impl TelegramChannels {
    pub fn new(userbot: Userbot, pool: PgPool) -> Self {
        Self { userbot, pool, inter_channel_delay: Duration::from_millis(200) }
    }
}

// The highest tg_message_id already stored for a channel.
pub async fn channel_watermark(pool: &PgPool, chat_id: &str) -> anyhow::Result<Option<i64>> {
    let row = pool
        .get()
        .await?
        .query_one(
            "SELECT max((metadata ->> 'tg_message_id')::int)::bigint FROM news_items
              WHERE source = $1 AND metadata ->> 'chat_id' = $2",
            &[&CHANNEL_SOURCE, &chat_id],
        )
        .await?;
    Ok(row.get(0))
}

pub fn channel_item(
    chat_id: &str,
    title: Option<&str>,
    username: Option<&str>,
    m: &crate::userbot::ChannelMessage,
) -> NewsItem {
    let mut metadata = Map::new();
    metadata.insert("chat_id".into(), chat_id.into());
    metadata.insert("chat_title".into(), title.into());
    metadata.insert("chat_username".into(), username.into());
    metadata.insert("tg_message_id".into(), m.id.into());
    metadata.insert("views".into(), m.views.into());
    metadata.insert("forwards".into(), m.forwards.into());
    NewsItem {
        source: CHANNEL_SOURCE.into(),
        external_id: format!("{chat_id}:{}", m.id),
        // Always null: the channel name repeated on every post would only
        // pollute the embeddings. It lives in metadata.chat_title.
        title: None,
        url: username.map(|u| format!("https://t.me/{u}/{}", m.id)),
        body: m.text.clone(),
        metadata,
        posted_at: Some(m.date),
    }
}

#[async_trait]
impl NewsProvider for TelegramChannels {
    fn source(&self) -> &'static str {
        CHANNEL_SOURCE
    }

    fn cadence(&self) -> Duration {
        Duration::from_secs(30 * 60)
    }

    async fn fetch(&self) -> anyhow::Result<Vec<NewsItem>> {
        if !self.userbot.has_session() {
            tracing::warn!("no saved userbot session — run `pnpm userbot:auth`. Skipping channels tick.");
            return Ok(Vec::new());
        }
        let mut collected = Vec::new();
        for channel in self.userbot.list_channels().await? {
            let watermark = channel_watermark(&self.pool, &channel.chat_id).await?;
            let limit = if watermark.is_none() { BOOTSTRAP_LIMIT } else { DELTA_LIMIT };
            match self.userbot.fetch_messages(&channel, watermark, limit).await {
                Ok(messages) => {
                    collected.extend(messages.iter().map(|m| {
                        channel_item(&channel.chat_id, channel.title.as_deref(), channel.username.as_deref(), m)
                    }))
                }
                Err(err) => {
                    let name = channel.title.as_deref().unwrap_or(&channel.chat_id);
                    tracing::warn!(channel = name, error = %format!("{err:#}"), "channel fetch failed");
                }
            }
            tokio::time::sleep(self.inter_channel_delay).await;
        }
        Ok(collected)
    }
}

// ── 4. repository ────────────────────────────────────────────────────────────

const SNIPPET_CHARS: usize = 400;
const ITEM_COLUMNS: &str = "id, source, external_id, title, url, body, metadata, posted_at";

#[derive(Clone)]
pub struct NewsRepository {
    pool: PgPool,
    embedder: SharedEmbedder,
}

struct EmbedRow {
    id: i64,
    title: Option<String>,
    body: String,
}

fn embed_text(row: &EmbedRow) -> String {
    [row.title.as_deref().unwrap_or("").trim(), row.body.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn metadata_of(row: &Row, column: &str) -> Map<String, Value> {
    match row.get::<_, Option<Value>>(column) {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn item_of(row: &Row) -> NewsItem {
    NewsItem {
        source: row.get("source"),
        external_id: row.get("external_id"),
        title: row.get("title"),
        url: row.get("url"),
        body: row.get("body"),
        metadata: metadata_of(row, "metadata"),
        posted_at: row.get("posted_at"),
    }
}

impl NewsRepository {
    pub fn new(pool: PgPool, embedder: SharedEmbedder) -> Self {
        Self { pool, embedder }
    }

    // Embedding failures never fail a write: rows keep a NULL vector and the
    // backfill picks them up.
    async fn embed(&self, rows: &[EmbedRow]) -> anyhow::Result<EmbedResult> {
        if rows.is_empty() {
            return Ok(EmbedResult::default());
        }
        let texts: Vec<String> = rows.iter().map(embed_text).collect();
        let vectors = match self.embedder.embed_batch(&texts).await {
            Ok(vectors) => vectors,
            Err(err) => {
                tracing::error!(rows = rows.len(), error = %format!("{err:#}"), "news embed failed");
                return Ok(EmbedResult { embedded: 0, failed: rows.len() });
            }
        };
        let client = self.pool.get().await?;
        let mut embedded = 0;
        for (row, vector) in rows.iter().zip(vectors) {
            client
                .execute(
                    "UPDATE news_items SET embedding = $1::text::vector, embedded_at = now() WHERE id = $2",
                    &[&vector_literal(&vector), &row.id],
                )
                .await?;
            embedded += 1;
        }
        Ok(EmbedResult { embedded, failed: 0 })
    }

    pub async fn save(&self, items: &[NewsItem]) -> anyhow::Result<SaveResult> {
        if items.is_empty() {
            return Ok(SaveResult::default());
        }
        let mut q = Query::default();
        let values: Vec<String> = items
            .iter()
            .map(|i| {
                format!(
                    "({}, {}, {}, {}, {}, {}, {})",
                    q.bind(i.source.clone()),
                    q.bind(i.external_id.clone()),
                    q.bind(i.title.clone()),
                    q.bind(i.url.clone()),
                    q.bind(i.body.clone()),
                    q.bind(Value::Object(i.metadata.clone())),
                    q.bind(i.posted_at),
                )
            })
            .collect();
        let sql = format!(
            "INSERT INTO news_items (source, external_id, title, url, body, metadata, posted_at) VALUES {}
             ON CONFLICT (source, external_id) DO NOTHING RETURNING id, title, body",
            values.join(", ")
        );
        let rows = self.pool.get().await?.query(&sql, &q.params()).await?;
        let inserted: Vec<EmbedRow> =
            rows.iter().map(|r| EmbedRow { id: r.get(0), title: r.get(1), body: r.get(2) }).collect();
        if inserted.is_empty() {
            return Ok(SaveResult::default());
        }
        let result = self.embed(&inserted).await?;
        Ok(SaveResult { saved: inserted.len(), embedded: result.embedded, failed: result.failed })
    }

    // Insert-or-replace by (source, external_id); a replaced body is
    // re-embedded, so the old vector is cleared first.
    pub async fn upsert(&self, item: &NewsItem) -> anyhow::Result<SaveResult> {
        let row = self
            .pool
            .get()
            .await?
            .query_one(
                "INSERT INTO news_items (source, external_id, title, url, body, metadata, posted_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (source, external_id) DO UPDATE SET
                   title = EXCLUDED.title, body = EXCLUDED.body, metadata = EXCLUDED.metadata,
                   embedding = NULL, embedded_at = NULL
                 RETURNING id, title, body",
                &[
                    &item.source,
                    &item.external_id,
                    &item.title,
                    &item.url,
                    &item.body,
                    &Value::Object(item.metadata.clone()),
                    &item.posted_at,
                ],
            )
            .await?;
        let result = self.embed(&[EmbedRow { id: row.get(0), title: row.get(1), body: row.get(2) }]).await?;
        Ok(SaveResult { saved: 1, embedded: result.embedded, failed: result.failed })
    }

    pub async fn find_by_external_id(&self, source: &str, external_id: &str) -> anyhow::Result<Option<NewsItem>> {
        let row = self
            .pool
            .get()
            .await?
            .query_opt(
                &format!("SELECT {ITEM_COLUMNS} FROM news_items WHERE source = $1 AND external_id = $2 LIMIT 1"),
                &[&source, &external_id],
            )
            .await?;
        Ok(row.as_ref().map(item_of))
    }

    fn filters(q: &mut Query, filter: &Filter, as_of_column: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(source) = &filter.source {
            out.push(format!("source = {}", q.bind(source.clone())));
        }
        if let Some(since) = filter.since {
            out.push(format!("posted_at > {}", q.bind(since)));
        }
        if let Some(until) = filter.until {
            out.push(format!("posted_at <= {}", q.bind(until)));
        }
        if let Some(as_of) = filter.as_of {
            out.push(format!("{as_of_column} <= {}", q.bind(as_of)));
        }
        if let Some(channel) = &filter.channel {
            let p = q.bind(channel.clone());
            out.push(format!("(metadata ->> 'chat_username' = {p} OR metadata ->> 'chat_id' = {p})"));
        }
        out
    }

    // Chronological: ascending when `since` is set, descending otherwise.
    // as_of bounds fetched_at here — list needs no embedding, so "was it in
    // the store" is the visibility question.
    pub async fn list(&self, filter: &Filter, limit: usize, dedup_threshold: f64) -> anyhow::Result<Vec<NewsItem>> {
        let dedup = dedup_threshold > 0.0;
        // A 2× pool leaves room for near-duplicates to drop out.
        let fetch_limit = if dedup { (limit * 2).min(2000) } else { limit };
        let mut q = Query::default();
        let filters = Self::filters(&mut q, filter, "fetched_at");
        let order = if filter.since.is_some() { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT {ITEM_COLUMNS}, embedding::text AS embedding FROM news_items {} ORDER BY posted_at {order} LIMIT {}",
            where_clause(&filters),
            q.bind(fetch_limit as i64)
        );
        let rows = self.pool.get().await?.query(&sql, &q.params()).await?;
        let items: Vec<(NewsItem, Option<Vec<f32>>)> = rows
            .iter()
            .map(|r| (item_of(r), r.get::<_, Option<String>>("embedding").as_deref().and_then(parse_vector)))
            .collect();
        let kept = dedup_by_pairwise_cosine(items, |(_, v)| v.as_deref(), dedup_threshold, true);
        Ok(kept.into_iter().take(limit).map(|(item, _)| item).collect())
    }

    // One batch embed for every query, then one vector search per query
    // (each with its own pool), merged and de-duplicated across the batch.
    // A single query is the N=1 case.
    pub async fn search(
        &self,
        queries: &[String],
        k: usize,
        filter: &Filter,
        annotate: bool,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let threshold = DEFAULT_DEDUP_THRESHOLD;
        let pool_size = (k * 2).max(30);
        let vectors = self.embedder.embed_batch(queries).await?;
        let pools = futures::future::try_join_all(vectors.iter().map(|vector| async move {
            let mut q = Query::default();
            let v = q.bind(vector_literal(vector));
            let mut filters = vec!["embedding IS NOT NULL".to_owned()];
            // as_of bounds embedded_at here: a row embedded later wasn't
            // retrievable then, whenever it was posted.
            filters.extend(Self::filters(&mut q, filter, "embedded_at"));
            let sql = format!(
                "SELECT id, source, title, url, body, metadata, posted_at,
                        (embedding <=> {v}::text::vector) AS distance, embedding::text AS embedding
                   FROM news_items {} ORDER BY distance LIMIT {}",
                where_clause(&filters),
                q.bind(pool_size as i64)
            );
            let rows = self.pool.get().await?.query(&sql, &q.params()).await?;
            anyhow::Ok(rows.iter().map(pool_row).collect::<Vec<_>>())
        }))
        .await?;
        Ok(merge_ranked_pools(pools, k, threshold, annotate))
    }

    pub async fn embed_missing_batch(&self, batch: i64) -> anyhow::Result<EmbedResult> {
        let rows = self
            .pool
            .get()
            .await?
            .query("SELECT id, title, body FROM news_items WHERE embedding IS NULL LIMIT $1", &[&batch])
            .await?;
        let rows: Vec<EmbedRow> =
            rows.iter().map(|r| EmbedRow { id: r.get(0), title: r.get(1), body: r.get(2) }).collect();
        self.embed(&rows).await
    }
}

// ── 5. ranking ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PoolRow {
    pub id: i64,
    pub source: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub body: String,
    pub metadata: Map<String, Value>,
    pub posted_at: Option<DateTime<Utc>>,
    pub distance: f64,
    pub embedding: Option<Vec<f32>>,
}

fn pool_row(r: &Row) -> PoolRow {
    PoolRow {
        id: r.get("id"),
        source: r.get("source"),
        title: r.get("title"),
        url: r.get("url"),
        body: r.get("body"),
        metadata: metadata_of(r, "metadata"),
        posted_at: r.get("posted_at"),
        distance: r.get("distance"),
        embedding: r.get::<_, Option<String>>("embedding").as_deref().and_then(parse_vector),
    }
}

fn snippet(body: &str) -> String {
    match body.char_indices().nth(SNIPPET_CHARS) {
        Some((at, _)) => format!("{}…", &body[..at]),
        None => body.to_owned(),
    }
}

// Each pool is one query's rows, ascending by distance. An item several
// queries found is kept once at its best distance; `matched_queries` says
// which facets surfaced it. Pure, so the ranking contract is testable
// without a database.
pub fn merge_ranked_pools(pools: Vec<Vec<PoolRow>>, k: usize, threshold: f64, annotate: bool) -> Vec<SearchResult> {
    let mut order: Vec<i64> = Vec::new();
    let mut by_id: HashMap<i64, (PoolRow, Vec<usize>)> = HashMap::new();
    for (qi, rows) in pools.into_iter().enumerate() {
        for row in rows {
            match by_id.get_mut(&row.id) {
                Some((best, matched)) => {
                    matched.push(qi);
                    if row.distance < best.distance {
                        *best = row;
                    }
                }
                None => {
                    order.push(row.id);
                    by_id.insert(row.id, (row, vec![qi]));
                }
            }
        }
    }
    let mut merged: Vec<(PoolRow, Vec<usize>)> = order.into_iter().filter_map(|id| by_id.remove(&id)).collect();
    merged.sort_by(|a, b| a.0.distance.total_cmp(&b.0.distance));
    let kept = dedup_by_pairwise_cosine(merged, |(row, _)| row.embedding.as_deref(), threshold, false);
    kept.into_iter()
        .take(k)
        .map(|(row, matched)| SearchResult {
            id: row.id,
            snippet: snippet(&row.body),
            source: row.source,
            title: row.title,
            url: row.url,
            posted_at: row.posted_at.map(iso),
            distance: row.distance,
            metadata: row.metadata,
            matched_queries: annotate.then_some(matched),
        })
        .collect()
}

// ── 6. poller ────────────────────────────────────────────────────────────────

const POLL_TICK: Duration = Duration::from_secs(30);
const BOOT_DELAY: Duration = Duration::from_secs(10);

// One loop; each tick fires every provider whose cadence has elapsed. One
// provider failing never stops the others. Starts after a boot delay so
// transport startup finishes first.
pub async fn run_poller(providers: Vec<Box<dyn NewsProvider>>, repository: NewsRepository, cancel: CancellationToken) {
    let names: Vec<String> =
        providers.iter().map(|p| format!("{}({}min)", p.source(), p.cadence().as_secs() / 60)).collect();
    tracing::info!(providers = names.join(", "), "news poller starting");
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = tokio::time::sleep(BOOT_DELAY) => {}
    }
    let mut last: HashMap<&'static str, tokio::time::Instant> = HashMap::new();
    let mut interval = tokio::time::interval(POLL_TICK);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = interval.tick() => {}
        }
        for provider in &providers {
            let now = tokio::time::Instant::now();
            if last.get(provider.source()).is_some_and(|at| now.duration_since(*at) < provider.cadence()) {
                continue;
            }
            last.insert(provider.source(), now);
            let outcome = async {
                let items = provider.fetch().await?;
                if items.is_empty() {
                    return anyhow::Ok(());
                }
                let r = repository.save(&items).await?;
                tracing::info!(
                    source = provider.source(),
                    fetched = items.len(),
                    saved = r.saved,
                    embedded = r.embedded,
                    failed = r.failed,
                    "news tick"
                );
                Ok(())
            }
            .await;
            if let Err(err) = outcome {
                tracing::error!(source = provider.source(), error = %format!("{err:#}"), "news tick failed");
            }
        }
    }
}

// ── 7. tools ─────────────────────────────────────────────────────────────────

// Exactly n contiguous parts, sizes differing by at most one (trailing ones
// may be empty) — a workflow references ${bind.chunks.0} … statically, and
// neighbouring items (time- or relevance-ordered) are the ones most likely to
// describe the same event.
pub fn split_chunks<T: Clone>(items: &[T], n: usize) -> Vec<Vec<T>> {
    let (base, extra) = (items.len() / n, items.len() % n);
    let mut offset = 0;
    (0..n)
        .map(|i| {
            let size = base + usize::from(i < extra);
            let chunk = items[offset..offset + size].to_vec();
            offset += size;
            chunk
        })
        .collect()
}

#[derive(Deserialize, schemars::JsonSchema, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum KnownSource {
    Hackernews,
    Habr,
    Channel,
}

impl KnownSource {
    fn name(self) -> &'static str {
        match self {
            KnownSource::Hackernews => "hackernews",
            KnownSource::Habr => "habr",
            KnownSource::Channel => "channel",
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SearchParams {
    /// Natural-language search query. Use for a single facet.
    query: Option<String>,
    /// Batch of 1–8 independent queries for a multi-facet ask. Mutually exclusive with `query`.
    queries: Option<Vec<String>>,
    /// Number of results to return. Default 10.
    #[schemars(range(min = 1, max = 50))]
    k: Option<usize>,
    /// Restrict results to one source.
    source: Option<KnownSource>,
    /// Only items with posted_at > this ISO timestamp.
    #[serde(rename = "sinceISO")]
    since_iso: Option<String>,
    /// Only items with posted_at <= this ISO timestamp.
    #[serde(rename = "untilISO")]
    until_iso: Option<String>,
    /// Eval/judge point-in-time replay: only items already searchable at this ISO instant (embedded_at <= asOfISO). Reconstructs what search could have returned at a past moment, excluding rows embedded later (poller/backfill lag). Normal runs omit this.
    #[serde(rename = "asOfISO")]
    as_of_iso: Option<String>,
    /// For source='channel' only: restrict to one Telegram channel by chat_username or chat_id.
    channel: Option<String>,
    /// Map-reduce mode: split the result into EXACTLY this many contiguous chunks and return { count, chunks: [...] } instead of a flat list — one chunk per parallel map step. The chunk count is fixed so a workflow can reference ${bind.chunks.0}, ${bind.chunks.1}, … statically; trailing chunks may be empty when there are few items.
    #[schemars(range(min = 2, max = 8))]
    chunks: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ListParams {
    /// Restrict to one source.
    source: Option<KnownSource>,
    /// Only items with posted_at > this ISO timestamp. Typical use: now - 24h for a daily digest.
    #[serde(rename = "sinceISO")]
    since_iso: Option<String>,
    /// Only items with posted_at <= this ISO timestamp.
    #[serde(rename = "untilISO")]
    until_iso: Option<String>,
    /// Eval/judge point-in-time replay: only items already in the store at this ISO instant (fetched_at <= asOfISO). Reconstructs what the store held at a past moment, excluding rows fetched later (poller lag). Normal runs omit this.
    #[serde(rename = "asOfISO")]
    as_of_iso: Option<String>,
    /// For source='channel' only: restrict to one Telegram channel by chat_username or chat_id.
    channel: Option<String>,
    /// Max rows. Default 500.
    #[schemars(range(min = 1, max = 2000))]
    limit: Option<usize>,
    /// Map-reduce mode: split the result into EXACTLY this many contiguous chunks and return { count, chunks: [...] } instead of a flat list — one chunk per parallel map step. The chunk count is fixed so a workflow can reference ${bind.chunks.0}, ${bind.chunks.1}, … statically; trailing chunks may be empty when there are few items.
    #[schemars(range(min = 2, max = 8))]
    chunks: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct FetchArticleParams {
    /// Article URL to fetch and extract.
    url: String,
}

fn build_filter(
    source: Option<KnownSource>,
    since: Option<&str>,
    until: Option<&str>,
    as_of: Option<&str>,
    channel: Option<String>,
) -> anyhow::Result<Filter> {
    let date = |field, v: Option<&str>| v.map(|v| require_js_date(field, v)).transpose();
    Ok(Filter {
        source: source.map(|s| s.name().to_owned()),
        since: date("sinceISO", since)?,
        until: date("untilISO", until)?,
        as_of: date("asOfISO", as_of)?,
        channel,
    })
}

fn chunks_in_range(chunks: Option<usize>) -> bool {
    chunks.is_none_or(|c| (2..=8).contains(&c))
}

fn source_for_url(url: &str) -> &'static str {
    match url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned)) {
        Some(host) if host.ends_with("habr.com") => "habr",
        Some(host) if host.ends_with("ycombinator.com") => "hackernews",
        _ => "external",
    }
}

#[tool_router(router = news_tools, vis = "pub(crate)")]
impl McpTools {
    #[tool(
        name = "search_news",
        title = "Semantic search over the news store",
        description = "Vector search across every news item the pollers have ingested \
            (Hacker News, Habr, harvested Telegram channels). Returns the \
            closest matches by semantic similarity. Use this when the user \
            asks about a topic — the background pollers keep the store fresh, \
            so there is no need to fetch articles before searching. Returns \
            id, source, title, url, snippet (first ~400 chars of the body), \
            posted_at, distance (lower = closer), and source-specific metadata.\n\n\
            For a multi-facet ask (e.g. one topic spanning several distinct \
            subjects), pass `queries: [...]` — one entry per facet — instead \
            of calling this tool N times or blurring everything into one \
            `query`. Each query is searched independently; results are merged \
            and already de-duplicated across the batch (an item's `distance` \
            is its best match across the facets, `matchedQueries` lists which \
            facets surfaced it), so do NOT re-query per facet or re-dedup. \
            Pass exactly one of `query` or `queries`."
    )]
    async fn search_news(&self, Parameters(p): Parameters<SearchParams>) -> ToolResult {
        if p.query.as_deref().is_some_and(str::is_empty)
            || p.queries.as_ref().is_some_and(|q| q.is_empty() || q.len() > 8 || q.iter().any(String::is_empty))
            || p.k.is_some_and(|k| !(1..=50).contains(&k))
            || !chunks_in_range(p.chunks)
        {
            return Err(invalid_params("query must be non-empty, queries 1–8 non-empty strings, k 1–50, chunks 2–8"));
        }
        let news = self.deps.news()?;
        let queries = match (p.query, p.queries) {
            (Some(q), None) => (vec![q], false),
            (None, Some(qs)) => (qs, true),
            _ => return json_result(&json!({ "error": "Pass exactly one of `query` or `queries`." })),
        };
        respond(
            async {
                let filter = build_filter(
                    p.source,
                    p.since_iso.as_deref(),
                    p.until_iso.as_deref(),
                    p.as_of_iso.as_deref(),
                    p.channel,
                )?;
                let results = news.search(&queries.0, p.k.unwrap_or(10), &filter, queries.1).await?;
                Ok(match p.chunks {
                    Some(n) => json!({ "count": results.len(), "chunks": split_chunks(&results, n) }),
                    None => json!({ "count": results.len(), "results": results }),
                })
            }
            .await,
        )
    }

    #[tool(
        name = "list_news",
        title = "List news items chronologically",
        description = "Read items from the news store ordered by posted_at. Use when \
            you need everything in a time window (e.g. a 24h channel digest) \
            rather than a topical match. Ascending when sinceISO is provided, \
            descending otherwise."
    )]
    async fn list_news(&self, Parameters(p): Parameters<ListParams>) -> ToolResult {
        if p.limit.is_some_and(|l| !(1..=2000).contains(&l)) || !chunks_in_range(p.chunks) {
            return Err(invalid_params("limit must be 1–2000, chunks 2–8"));
        }
        let news = self.deps.news()?;
        respond(
            async {
                let filter = build_filter(
                    p.source,
                    p.since_iso.as_deref(),
                    p.until_iso.as_deref(),
                    p.as_of_iso.as_deref(),
                    p.channel,
                )?;
                let items: Vec<Value> = news
                    .list(&filter, p.limit.unwrap_or(500), DEFAULT_DEDUP_THRESHOLD)
                    .await?
                    .iter()
                    .map(NewsItem::to_json)
                    .collect();
                Ok(match p.chunks {
                    Some(n) => json!({ "count": items.len(), "chunks": split_chunks(&items, n) }),
                    None => json!({ "count": items.len(), "items": items }),
                })
            }
            .await,
        )
    }

    #[tool(
        name = "fetch_article",
        title = "Fetch and store an arbitrary article URL",
        description = "Download a web article (Mozilla Readability) and save it to the \
            news store so it becomes searchable. Manual override — the HN \
            and Habr pollers already cover their feeds. Use this for ad-hoc \
            URLs the user shares. Returns clean plaintext (title + body). \
            If the URL is already cached, returns the cached row without \
            re-fetching."
    )]
    async fn fetch_article(&self, Parameters(p): Parameters<FetchArticleParams>) -> ToolResult {
        if url::Url::parse(&p.url).is_err() {
            return Err(invalid_params("url must be a valid URL"));
        }
        let news = self.deps.news()?;
        let fetcher = ArticleFetcher::new(self.deps.fetcher.clone());
        respond(
            async {
                for source in ["hackernews", "habr", "external"] {
                    if let Some(cached) = news.find_by_external_id(source, &p.url).await?
                        && !cached.body.trim().is_empty()
                    {
                        let mut out = json!({
                            "url": p.url,
                            "title": cached.title.clone().unwrap_or_default(),
                            "text": cached.body,
                            "source": cached.source,
                            "sizeChars": cached.body.encode_utf16().count(),
                            "cached": true,
                        });
                        if let Some(at) = cached.posted_at {
                            out["publishedAt"] = iso(at).into();
                        }
                        return Ok(out);
                    }
                }

                let article = fetcher.fetch(&p.url).await?;
                let source = source_for_url(&p.url);
                let mut metadata = Map::new();
                if let Some(author) = &article.author {
                    metadata.insert("author".into(), author.clone().into());
                }
                if let Some(site) = &article.site {
                    metadata.insert("site".into(), site.clone().into());
                }
                let item = NewsItem {
                    source: source.into(),
                    external_id: p.url.clone(),
                    title: Some(article.title.clone()).filter(|t| !t.is_empty()),
                    url: Some(p.url.clone()),
                    body: article.text.clone(),
                    metadata,
                    posted_at: article.published_at,
                };
                if let Err(err) = news.upsert(&item).await {
                    tracing::error!(error = %format!("{err:#}"), "fetch_article: save step failed");
                }
                let mut out = json!({ "url": article.url, "title": article.title, "text": article.text });
                if let Some(site) = &article.site {
                    out["site"] = site.clone().into();
                }
                if let Some(at) = article.published_at {
                    out["publishedAt"] = iso(at).into();
                }
                if let Some(author) = &article.author {
                    out["author"] = author.clone().into();
                }
                out["source"] = source.into();
                out["sizeChars"] = article.text.encode_utf16().count().into();
                out["cached"] = false.into();
                Ok(out)
            }
            .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(xs: &[f32]) -> Vec<f32> {
        let n = xs.iter().map(|x| x * x).sum::<f32>().sqrt();
        xs.iter().map(|x| x / n).collect()
    }

    fn row(id: i64, distance: f64, embedding: Vec<f32>) -> PoolRow {
        PoolRow {
            id,
            source: "hackernews".into(),
            title: Some(format!("t{id}")),
            url: None,
            body: "body".into(),
            metadata: Map::new(),
            posted_at: None,
            distance,
            embedding: Some(embedding),
        }
    }

    fn ids(results: &[SearchResult]) -> Vec<i64> {
        results.iter().map(|r| r.id).collect()
    }

    #[test]
    fn keeps_a_multiply_retrieved_item_once_at_its_min_distance() {
        let (ex, ey, ez) = (unit(&[1., 0., 0.]), unit(&[0., 1., 0.]), unit(&[0., 0., 1.]));
        let out = merge_ranked_pools(
            vec![vec![row(1, 0.2, ex.clone()), row(2, 0.5, ey)], vec![row(1, 0.1, ex), row(3, 0.4, ez)]],
            10,
            DEFAULT_DEDUP_THRESHOLD,
            true,
        );
        assert_eq!(ids(&out), [1, 3, 2]);
        assert!((out[0].distance - 0.1).abs() < 1e-9);
        assert_eq!(out[0].matched_queries, Some(vec![0, 1]));
    }

    #[test]
    fn collapses_cross_query_near_duplicates_keeping_the_closer_one() {
        let out = merge_ranked_pools(
            vec![vec![row(1, 0.2, unit(&[1., 0., 0.]))], vec![row(2, 0.15, unit(&[1., 0.1, 0.]))]],
            10,
            DEFAULT_DEDUP_THRESHOLD,
            true,
        );
        assert_eq!(ids(&out), [2]);
        // Threshold 0 disables dedup: both survive.
        let out = merge_ranked_pools(
            vec![vec![row(1, 0.2, unit(&[1., 0., 0.]))], vec![row(2, 0.15, unit(&[1., 0.1, 0.]))]],
            10,
            0.0,
            true,
        );
        assert_eq!(ids(&out), [2, 1]);
    }

    #[test]
    fn k_caps_after_dedup_and_single_query_omits_matches() {
        let out = merge_ranked_pools(
            vec![
                vec![row(1, 0.5, unit(&[1., 0., 0.])), row(2, 0.1, unit(&[0., 1., 0.]))],
                vec![
                    row(3, 0.3, unit(&[0., 0., 1.])),
                    row(4, 0.2, unit(&[1., 1., 0.])),
                    row(5, 0.9, unit(&[0., 1., 1.])),
                ],
            ],
            2,
            DEFAULT_DEDUP_THRESHOLD,
            true,
        );
        assert_eq!(ids(&out), [2, 4]);
        let single =
            merge_ranked_pools(vec![vec![row(1, 0.3, unit(&[1., 0.])), row(2, 0.1, unit(&[0., 1.]))]], 10, 0.03, false);
        assert_eq!(ids(&single), [2, 1]);
        assert!(serde_json::to_value(&single[0]).unwrap().get("matchedQueries").is_none());
    }

    #[test]
    fn snippets_cut_at_400_characters() {
        assert_eq!(snippet(&"я".repeat(401)).chars().count(), 401);
        assert!(snippet(&"я".repeat(401)).ends_with('…'));
        assert_eq!(snippet("short"), "short");
    }

    #[test]
    fn splits_into_exactly_n_contiguous_chunks() {
        let items: Vec<i32> = (0..10).collect();
        assert_eq!(split_chunks(&items, 3), vec![vec![0, 1, 2, 3], vec![4, 5, 6], vec![7, 8, 9]]);
        assert_eq!(split_chunks(&[1, 2], 4), vec![vec![1], vec![2], vec![], vec![]]);
        assert_eq!(split_chunks::<i32>(&[], 3), vec![Vec::<i32>::new(), vec![], vec![]]);
    }

    fn headline(extra: Map<String, Value>) -> Headline {
        Headline {
            title: "Headline title".into(),
            url: "https://example.com/a".into(),
            author: Some("alice".into()),
            posted_at: parse_js_date("2026-01-01T00:00:00Z"),
            extra,
        }
    }

    fn article(title: &str, text: &str) -> Article {
        Article {
            url: "https://example.com/a".into(),
            title: title.into(),
            text: text.into(),
            site: Some("example.com".into()),
            published_at: parse_js_date("2026-01-02T00:00:00Z"),
            author: Some("bob".into()),
        }
    }

    #[test]
    fn maps_an_article_with_the_headline_author_winning() {
        let mut extra = Map::new();
        extra.insert("hn_id".into(), 1.into());
        let item =
            article_item("hackernews", &headline(extra), Some(article("Article title", "body content"))).unwrap();
        assert_eq!(item.title.as_deref(), Some("Article title"));
        assert_eq!(
            item.metadata,
            json!({ "hn_id": 1, "author": "alice", "site": "example.com" }).as_object().unwrap().clone()
        );
        assert_eq!(item.posted_at, parse_js_date("2026-01-02T00:00:00Z"));
        // Empty extracted title falls back to the headline's.
        assert_eq!(
            article_item("habr", &headline(Map::new()), Some(article("", "b"))).unwrap().title.as_deref(),
            Some("Headline title")
        );
    }

    #[test]
    fn drops_failed_or_blank_extractions() {
        assert!(article_item("habr", &headline(Map::new()), None).is_none());
        assert!(article_item("habr", &headline(Map::new()), Some(article("t", "   \n "))).is_none());
    }

    #[test]
    fn channel_posts_have_no_title_and_link_when_public() {
        let m = crate::userbot::ChannelMessage {
            id: 7,
            date: parse_js_date("2026-01-01T00:00:00Z").unwrap(),
            text: "body text".into(),
            views: Some(123),
            forwards: Some(4),
        };
        let item = channel_item("100", Some("Channel One"), Some("channel_one"), &m);
        assert_eq!(item.external_id, "100:7");
        assert_eq!(item.title, None);
        assert_eq!(item.url.as_deref(), Some("https://t.me/channel_one/7"));
        assert_eq!(
            Value::Object(item.metadata),
            json!({ "chat_id": "100", "chat_title": "Channel One", "chat_username": "channel_one", "tg_message_id": 7, "views": 123, "forwards": 4 })
        );
        assert_eq!(channel_item("100", None, None, &m).url, None);
    }

    #[test]
    fn strips_markup_and_entities_to_compact_text() {
        assert_eq!(
            strip_html("<p>Hello&nbsp;<b>world</b></p><script>x()</script>\n<style>.a{}</style> &amp; more"),
            "Hello world & more"
        );
    }

    #[test]
    fn extracts_a_readable_article() {
        let body = "Графы — это набор вершин и рёбер. ".repeat(40);
        let html = format!(
            "<html><head><title>Про графы</title><meta property=\"og:site_name\" content=\"Хабр\">\
             <meta property=\"article:published_time\" content=\"2026-01-02T00:00:00Z\"></head>\
             <body><nav>menu</nav><article><h1>Про графы</h1><p>{body}</p><p>{body}</p></article></body></html>"
        );
        let article = extract_article("https://habr.com/ru/articles/1/", &html).unwrap();
        assert!(article.text.contains("набор вершин"));
        assert_eq!(article.site.as_deref(), Some("Хабр"));
        assert_eq!(article.published_at, parse_js_date("2026-01-02T00:00:00Z"));
    }

    // ── against Postgres (TEST_DATABASE_URL) ────────────────────────────────

    fn item(source: &str, id: &str, title: &str, body: &str, posted: &str) -> NewsItem {
        NewsItem {
            source: source.into(),
            external_id: id.into(),
            title: Some(title.into()),
            url: Some(format!("https://example.com/{id}")),
            body: body.into(),
            metadata: Map::new(),
            posted_at: parse_js_date(posted),
        }
    }

    #[tokio::test]
    async fn pg_saves_lists_and_searches() {
        let Some(pool) = crate::pg::test_pool().await else { return };
        let embedder = crate::embeddings::testing::FakeEmbedder::with_dims(1536);
        let repo = NewsRepository::new(pool.clone(), embedder.clone());
        let source = format!("test-{}", rand::random::<u32>());
        let items = vec![
            item(&source, "a", "Rust async runtimes", "tokio executor scheduling futures", "2026-01-01T00:00:00Z"),
            item(&source, "b", "Postgres vector search", "pgvector cosine distance ivfflat", "2026-01-02T00:00:00Z"),
            item(&source, "c", "Одесса новости", "удары дронов по Одессе ночью", "2026-01-03T00:00:00Z"),
        ];
        assert_eq!(repo.save(&items).await.unwrap(), SaveResult { saved: 3, embedded: 3, failed: 0 });
        // Re-saving the same natural keys is a no-op.
        assert_eq!(repo.save(&items).await.unwrap().saved, 0);

        let filter = Filter { source: Some(source.clone()), ..Filter::default() };
        let listed: Vec<String> =
            repo.list(&filter, 10, 0.03).await.unwrap().into_iter().map(|i| i.external_id).collect();
        assert_eq!(listed, ["c", "b", "a"]);
        let since = Filter { since: parse_js_date("2026-01-01T12:00:00Z"), ..filter.clone() };
        let ascending: Vec<String> =
            repo.list(&since, 10, 0.03).await.unwrap().into_iter().map(|i| i.external_id).collect();
        assert_eq!(ascending, ["b", "c"]);

        let hits = repo.search(&["pgvector cosine distance".into()], 2, &filter, false).await.unwrap();
        assert_eq!(hits[0].url.as_deref(), Some("https://example.com/b"));
        assert!(hits[0].matched_queries.is_none());
        let multi = repo.search(&["tokio futures".into(), "Одесса дроны".into()], 5, &filter, true).await.unwrap();
        assert_eq!(multi.len(), 3);
        assert!(multi.iter().all(|h| h.matched_queries.is_some()));

        // Upsert replaces the body and re-embeds it.
        let mut changed = items[0].clone();
        changed.body = "completely new body about borrow checker".into();
        assert_eq!(repo.upsert(&changed).await.unwrap().embedded, 1);
        assert_eq!(repo.find_by_external_id(&source, "a").await.unwrap().unwrap().body, changed.body);

        // A failed inline embed leaves a NULL vector for the backfill.
        embedder.set_down(true);
        let late = item(&source, "d", "late", "embedder was down", "2026-01-04T00:00:00Z");
        assert_eq!(
            repo.save(std::slice::from_ref(&late)).await.unwrap(),
            SaveResult { saved: 1, embedded: 0, failed: 1 }
        );
        embedder.set_down(false);
        assert!(repo.embed_missing_batch(1000).await.unwrap().embedded >= 1);
        assert_eq!(
            repo.search(&["embedder was down".into()], 1, &filter, false).await.unwrap()[0].url.as_deref(),
            Some("https://example.com/d")
        );
    }

    #[tokio::test]
    async fn pg_channel_posts_filter_and_watermark() {
        let Some(pool) = crate::pg::test_pool().await else { return };
        let repo = NewsRepository::new(pool.clone(), crate::embeddings::testing::FakeEmbedder::with_dims(1536));
        let chat = format!("{}", rand::random::<u32>());
        let post = |id: i64, text: &str| crate::userbot::ChannelMessage {
            id,
            date: Utc::now(),
            text: text.into(),
            views: Some(1),
            forwards: None,
        };
        assert_eq!(channel_watermark(&pool, &chat).await.unwrap(), None);
        let items = vec![
            channel_item(&chat, Some("Chan"), Some("chan_user"), &post(7, "first post")),
            channel_item(&chat, Some("Chan"), None, &post(12, "second post")),
        ];
        repo.save(&items).await.unwrap();
        assert_eq!(channel_watermark(&pool, &chat).await.unwrap(), Some(12));
        let by_chat = Filter { source: Some("channel".into()), channel: Some(chat.clone()), ..Filter::default() };
        assert_eq!(repo.list(&by_chat, 10, 0.0).await.unwrap().len(), 2);
    }

    #[test]
    fn classifies_urls_by_host() {
        assert_eq!(source_for_url("https://habr.com/ru/articles/1/"), "habr");
        assert_eq!(source_for_url("https://news.ycombinator.com/item?id=1"), "hackernews");
        assert_eq!(source_for_url("https://example.com"), "external");
    }
}
