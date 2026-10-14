// Text → vector, and the vector math every retrieval path shares.
//
// Sections:
//   1. embedder  — the `Embedder` port + the OpenAI implementation
//   2. retrieval — cosine distance and near-duplicate filtering
//
// Generic infrastructure: nothing here knows a table. Domains (news,
// knowledge, memory) compose the text they embed and store the vectors.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

// ── 1. embedder ──────────────────────────────────────────────────────────────

#[async_trait]
pub trait Embedder: Send + Sync {
    // One vector per input, in order.
    async fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}

pub type SharedEmbedder = Arc<dyn Embedder>;

pub async fn embed_one(embedder: &dyn Embedder, text: &str) -> anyhow::Result<Vec<f32>> {
    embedder
        .embed_batch(&[text.to_owned()])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("OpenAI embeddings returned no vector for input"))
}

// 8000 chars sits under the ~8191-token limit of text-embedding-3-small at
// ~4 chars/token. Only the head of a long text is embedded; multi-chunk
// documents are chunked by their own domain (memory) before they get here.
pub const DEFAULT_MAX_CHARS: usize = 8000;
// OpenAI accepts 2048 inputs per request; 100 keeps the payload modest.
const BATCH_SIZE: usize = 100;
// The openai SDK's default: two retries on 429 / 5xx / connection errors.
const MAX_RETRIES: u32 = 2;

pub struct OpenAiEmbedder {
    http: reqwest::Client,
    api_key: String,
    model: String,
    dimensions: u32,
    max_chars: usize,
}

impl OpenAiEmbedder {
    pub fn new(http: reqwest::Client, api_key: String) -> Self {
        Self::with_model(http, api_key, "text-embedding-3-small", 1536, DEFAULT_MAX_CHARS)
    }

    pub fn with_model(http: reqwest::Client, api_key: String, model: &str, dimensions: u32, max_chars: usize) -> Self {
        Self { http, api_key, model: model.to_owned(), dimensions, max_chars }
    }

    pub fn from_env(http: reqwest::Client) -> anyhow::Result<Self> {
        let key = std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty());
        let key =
            key.ok_or_else(|| anyhow::anyhow!("OPENAI_API_KEY is not set. Embeddings need it; see .env.mcp.example."))?;
        Ok(Self::new(http, key))
    }

    async fn request(&self, input: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        #[derive(Deserialize)]
        struct Item {
            index: usize,
            embedding: Vec<f32>,
        }
        #[derive(Deserialize)]
        struct Response {
            data: Vec<Item>,
        }

        let body = serde_json::json!({ "model": self.model, "input": input, "dimensions": self.dimensions });
        let mut attempt = 0;
        loop {
            let sent = self
                .http
                .post("https://api.openai.com/v1/embeddings")
                .bearer_auth(&self.api_key)
                .timeout(Duration::from_secs(60))
                .json(&body)
                .send()
                .await;
            let retryable = match &sent {
                Ok(res) => res.status().as_u16() == 429 || res.status().is_server_error(),
                Err(err) => err.is_connect() || err.is_timeout() || err.is_request(),
            };
            if retryable && attempt < MAX_RETRIES {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
                continue;
            }
            let res = sent?;
            if !res.status().is_success() {
                let status = res.status();
                let detail = res.text().await.unwrap_or_default();
                anyhow::bail!("OpenAI embeddings failed ({status}): {detail}");
            }
            let mut parsed: Response = res.json().await?;
            parsed.data.sort_by_key(|item| item.index);
            return Ok(parsed.data.into_iter().map(|item| item.embedding).collect());
        }
    }
}

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => text[..at].to_owned(),
        None => text.to_owned(),
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    async fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let heads: Vec<String> = texts.iter().map(|t| truncate_chars(t, self.max_chars)).collect();
        let batches = heads.chunks(BATCH_SIZE).map(|batch| self.request(batch));
        let results = futures::future::try_join_all(batches).await?;
        Ok(results.into_iter().flatten().collect())
    }
}

// ── 2. retrieval ─────────────────────────────────────────────────────────────

// text-embedding-3 and pgvector vectors are unit-normalised, so cosine
// distance is 1 - dot. No re-normalisation.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    1.0 - a.iter().zip(b).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum::<f64>()
}

// Catches exact and near-exact copies (a headline reposted to several
// feeds, channel-noise repeated by one source) without merging distinct
// posts on one topic. Tuned on the RAG eval golden set
// (eval configs/baseline-dedup-003.json).
pub const DEFAULT_DEDUP_THRESHOLD: f64 = 0.03;

// Walks items in input order (callers sort by ascending distance first) and
// keeps one only if it is at least `threshold` away from everything kept.
// Items without a vector are dropped, or kept in place with `keep_null`.
pub fn dedup_by_pairwise_cosine<T>(
    items: Vec<T>,
    vector: impl Fn(&T) -> Option<&[f32]>,
    threshold: f64,
    keep_null: bool,
) -> Vec<T> {
    if threshold <= 0.0 {
        return items;
    }
    let mut kept_vectors: Vec<Vec<f32>> = Vec::new();
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        let Some(candidate) = vector(&item) else {
            if keep_null {
                kept.push(item);
            }
            continue;
        };
        if kept_vectors.iter().all(|k| cosine_distance(candidate, k) >= threshold) {
            kept_vectors.push(candidate.to_vec());
            kept.push(item);
        }
    }
    kept
}

#[cfg(test)]
pub mod testing {
    use super::*;

    // Deterministic stand-in for text-embedding-3-small: a bag-of-words
    // vector, so distance tracks word overlap. Normalised like the real one.
    pub struct FakeEmbedder {
        pub down: std::sync::atomic::AtomicBool,
        dims: usize,
    }

    impl FakeEmbedder {
        pub fn new() -> Arc<Self> {
            Self::with_dims(64)
        }

        // 1536 to fit the real vector(1536) columns in Postgres tests.
        pub fn with_dims(dims: usize) -> Arc<Self> {
            Arc::new(Self { down: std::sync::atomic::AtomicBool::new(false), dims })
        }

        pub fn set_down(&self, down: bool) {
            self.down.store(down, std::sync::atomic::Ordering::SeqCst);
        }
    }

    pub fn fake_embed(text: &str) -> Vec<f32> {
        fake_embed_dims(text, 64)
    }

    pub fn fake_embed_dims(text: &str, dims: usize) -> Vec<f32> {
        let mut v = vec![0f32; dims];
        let lower = text.to_lowercase();
        for token in lower.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()) {
            let mut hash: u64 = 0;
            for ch in token.chars() {
                hash = (hash * 31 + u64::from(ch)) % dims as u64;
            }
            v[hash as usize] += 1.0;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }

    #[async_trait]
    impl Embedder for FakeEmbedder {
        async fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            if self.down.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("provider unreachable");
            }
            Ok(texts.iter().map(|t| fake_embed_dims(t, self.dims)).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(items: &[(u32, Option<Vec<f32>>)]) -> Vec<u32> {
        items.iter().map(|(id, _)| *id).collect()
    }

    fn dedup(items: Vec<(u32, Option<Vec<f32>>)>, keep_null: bool) -> Vec<(u32, Option<Vec<f32>>)> {
        dedup_by_pairwise_cosine(items, |(_, v)| v.as_deref(), 0.05, keep_null)
    }

    #[test]
    fn drops_exact_and_near_duplicates_keeping_input_order() {
        let theta = std::f32::consts::PI / 12.0;
        let out = dedup(
            vec![
                (1, Some(vec![1.0, 0.0, 0.0])),
                (2, Some(vec![theta.cos(), theta.sin(), 0.0])),
                (3, Some(vec![(2.0 * theta).cos(), (2.0 * theta).sin(), 0.0])),
                (4, Some(vec![1.0, 0.0, 0.0])),
            ],
            false,
        );
        // 15° apart collapses, 30° apart survives — a transitive chain keeps
        // its endpoints.
        assert_eq!(ids(&out), [1, 3]);
    }

    #[test]
    fn null_vectors_are_dropped_or_kept_in_place() {
        let items = || vec![(1, Some(vec![1.0, 0.0])), (2, None), (3, Some(vec![1.0, 0.0])), (4, Some(vec![0.0, 1.0]))];
        assert_eq!(ids(&dedup(items(), false)), [1, 4]);
        assert_eq!(ids(&dedup(items(), true)), [1, 2, 4]);
    }

    #[test]
    fn threshold_zero_is_a_passthrough() {
        let items = vec![(1, Some(vec![1.0, 0.0])), (2, Some(vec![1.0, 0.0]))];
        assert_eq!(ids(&dedup_by_pairwise_cosine(items, |(_, v)| v.as_deref(), 0.0, false)), [1, 2]);
    }

    #[test]
    fn truncates_on_characters_not_bytes() {
        assert_eq!(truncate_chars("привет", 3), "при");
        assert_eq!(truncate_chars("hi", 10), "hi");
    }
}
