// The RAG eval harness: a frozen corpus snapshot + labelled queries, scored
// under a config (embedding model, text composition, dedup). It re-embeds the
// corpus from text, so configs compare on one yardstick. Fixtures and their
// labelling rules: eval/fixtures/README.md.
//
// Sections:
//   1. types   — corpus rows, queries, config, results
//   2. config  — loading/validating a config + the corpus-cache key
//   3. metrics — recall, precision, MRR, source diversity
//   4. cache   — corpus vectors per config hash (JSONL)
//   5. run     — embed, retrieve, dedup, score
//   6. report  — the markdown report
//   7. inspect — per-query top-k listing for debugging labels

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::embeddings::{Embedder, cosine_distance, dedup_by_pairwise_cosine};

// ── 1. types ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusRow {
    pub id: i64,
    pub source: String,
    pub title: Option<String>,
    pub body: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

impl CorpusRow {
    // One "source" for diversity counting: the channel for Telegram posts,
    // the feed name otherwise.
    pub fn bucket(&self) -> String {
        self.metadata.get("chat_title").and_then(Value::as_str).map_or_else(|| self.source.clone(), str::to_owned)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueryRow {
    pub id: String,
    pub query: String,
    pub reformulation: String,
    pub gold: Vec<i64>,
    pub acceptable: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BuildText {
    #[serde(rename = "title+body")]
    TitleBody,
    #[serde(rename = "body-only")]
    BodyOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryField {
    Query,
    Reformulation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedConfig {
    pub model: String,
    pub dimensions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DedupConfig {
    pub threshold: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalConfig {
    pub embed: EmbedConfig,
    pub build_text: BuildText,
    pub top_k: usize,
    pub dedup: Option<DedupConfig>,
    // Reserved; always null today.
    #[serde(default)]
    pub rerank: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryConfig {
    pub field: QueryField,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringConfig {
    // Only "binary" is supported.
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalConfig {
    pub name: String,
    pub retrieval: RetrievalConfig,
    pub query: QueryConfig,
    pub scoring: ScoringConfig,
}

#[derive(Debug, Clone, Copy)]
pub struct Retrieved {
    pub id: i64,
    pub distance: f64,
}

#[derive(Debug)]
pub struct PerQuery {
    pub qid: String,
    pub query: String,
    pub gold_count: usize,
    pub hit_at5: usize,
    pub hit_at10: usize,
    pub hit_at30: usize,
    pub precision_at5: f64,
    pub precision_at10: f64,
    pub unique_sources_at5: usize,
    pub unique_sources_at10: usize,
    pub first_gold_rank: Option<usize>,
    pub dist_to_first_gold: Option<f64>,
}

#[derive(Debug)]
pub struct NegativeTest {
    pub qid: String,
    pub query: String,
    pub min_distance: f64,
    pub top1_id: i64,
}

#[derive(Debug)]
pub struct Aggregate {
    pub scored_queries: usize,
    pub recall_at5: f64,
    pub recall_at10: f64,
    pub recall_at30: f64,
    pub precision_at5: f64,
    pub precision_at10: f64,
    pub mean_unique_sources_at5: f64,
    pub mean_unique_sources_at10: f64,
    pub mrr: f64,
    pub mean_dist_to_first_gold: Option<f64>,
}

pub struct EvalResult {
    pub config: EvalConfig,
    pub config_hash: String,
    pub per_query: Vec<PerQuery>,
    pub negative_tests: Vec<NegativeTest>,
    pub aggregate: Aggregate,
    pub cache_hit: bool,
}

// ── 2. config ────────────────────────────────────────────────────────────────

pub fn load_config(path: &Path) -> anyhow::Result<EvalConfig> {
    let config: EvalConfig = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    anyhow::ensure!(!config.name.is_empty(), "config.name must be a non-empty string");
    anyhow::ensure!(config.retrieval.top_k > 0, "config.retrieval.topK must be a positive number");
    anyhow::ensure!(
        config.retrieval.dedup.as_ref().is_none_or(|d| d.threshold >= 0.0),
        "config.retrieval.dedup.threshold must be a non-negative number"
    );
    anyhow::ensure!(
        config.scoring.mode == "binary",
        "config.scoring.mode must be 'binary' (only mode supported today)"
    );
    Ok(config)
}

// Only what changes the corpus vectors — model, dimensions, text
// composition — so swapping the query field or scoring keeps the cache.
pub fn hash_corpus_inputs(config: &EvalConfig) -> String {
    let key = serde_json::json!({
        "model": config.retrieval.embed.model,
        "dimensions": config.retrieval.embed.dimensions,
        "buildText": config.retrieval.build_text,
    });
    hex::encode(Sha256::digest(key.to_string().as_bytes()))[..16].to_owned()
}

pub fn build_text(row: &CorpusRow, mode: BuildText) -> String {
    let title = row.title.as_deref().unwrap_or("").trim();
    let body = row.body.trim();
    match mode {
        BuildText::BodyOnly => body.to_owned(),
        BuildText::TitleBody if !title.is_empty() => format!("{title}\n\n{body}"),
        BuildText::TitleBody => body.to_owned(),
    }
}

// ── 3. metrics ───────────────────────────────────────────────────────────────

fn count_hits(gold: &HashSet<i64>, items: &[Retrieved]) -> usize {
    items.iter().filter(|i| gold.contains(&i.id)).count()
}

pub fn first_gold_rank(gold: &[i64], top: &[Retrieved]) -> Option<usize> {
    top.iter().position(|i| gold.contains(&i.id)).map(|p| p + 1)
}

pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() { f64::NAN } else { values.iter().sum::<f64>() / values.len() as f64 }
}

fn unique_sources(items: &[Retrieved], corpus: &HashMap<i64, CorpusRow>) -> usize {
    items.iter().filter_map(|i| corpus.get(&i.id)).map(CorpusRow::bucket).collect::<HashSet<_>>().len()
}

fn score_query(q: &QueryRow, top: &[Retrieved], corpus: &HashMap<i64, CorpusRow>) -> PerQuery {
    let gold: HashSet<i64> = q.gold.iter().copied().collect();
    let at = |k: usize| &top[..top.len().min(k)];
    let (hit5, hit10, hit30) = (count_hits(&gold, at(5)), count_hits(&gold, at(10)), count_hits(&gold, at(30)));
    PerQuery {
        qid: q.id.clone(),
        query: q.query.clone(),
        gold_count: q.gold.len(),
        hit_at5: hit5,
        hit_at10: hit10,
        hit_at30: hit30,
        precision_at5: hit5 as f64 / 5.0,
        precision_at10: hit10 as f64 / 10.0,
        unique_sources_at5: unique_sources(at(5), corpus),
        unique_sources_at10: unique_sources(at(10), corpus),
        first_gold_rank: first_gold_rank(&q.gold, top),
        dist_to_first_gold: top.iter().find(|i| gold.contains(&i.id)).map(|i| i.distance),
    }
}

fn aggregate(per_query: &[PerQuery]) -> Aggregate {
    let scored: Vec<&PerQuery> = per_query.iter().filter(|p| p.gold_count > 0).collect();
    let m = |f: &dyn Fn(&PerQuery) -> f64| mean(&scored.iter().map(|p| f(p)).collect::<Vec<_>>());
    let distances: Vec<f64> = scored.iter().filter_map(|p| p.dist_to_first_gold).collect();
    Aggregate {
        scored_queries: scored.len(),
        recall_at5: m(&|p| p.hit_at5 as f64 / p.gold_count as f64),
        recall_at10: m(&|p| p.hit_at10 as f64 / p.gold_count as f64),
        recall_at30: m(&|p| p.hit_at30 as f64 / p.gold_count as f64),
        precision_at5: m(&|p| p.precision_at5),
        precision_at10: m(&|p| p.precision_at10),
        mean_unique_sources_at5: m(&|p| p.unique_sources_at5 as f64),
        mean_unique_sources_at10: m(&|p| p.unique_sources_at10 as f64),
        mrr: m(&|p| p.first_gold_rank.map_or(0.0, |r| 1.0 / r as f64)),
        mean_dist_to_first_gold: (!distances.is_empty()).then(|| mean(&distances)),
    }
}

// ── 4. cache ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct CachedVector {
    id: i64,
    embedding: Vec<f32>,
}

pub fn read_cache(dir: &Path, hash: &str) -> anyhow::Result<Option<HashMap<i64, Vec<f32>>>> {
    let path = dir.join(format!("{hash}.jsonl"));
    if !path.exists() {
        return Ok(None);
    }
    let mut map = HashMap::new();
    for line in std::fs::read_to_string(path)?.lines().filter(|l| !l.trim().is_empty()) {
        let v: CachedVector = serde_json::from_str(line)?;
        map.insert(v.id, v.embedding);
    }
    Ok(Some(map))
}

pub fn write_cache(dir: &Path, hash: &str, vectors: &HashMap<i64, Vec<f32>>) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut out = String::new();
    for (id, embedding) in vectors {
        out.push_str(&serde_json::to_string(&CachedVector { id: *id, embedding: embedding.clone() })?);
        out.push('\n');
    }
    std::fs::write(dir.join(format!("{hash}.jsonl")), out)?;
    Ok(())
}

// ── 5. run ───────────────────────────────────────────────────────────────────

// Always retrieve this many so R@30 is computable whatever topK says
// (topK stays a hint of what a skill would actually show).
const RETRIEVAL_SIZE: usize = 30;
// Cyrillic runs ~6 bytes/token, so the prod 8000-char cap overflows the
// model's 8192-token limit on some posts; 6000 stays inside it.
pub const EVAL_MAX_CHARS: usize = 6000;

pub fn load_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    std::fs::read_to_string(path)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| Ok(serde_json::from_str(l)?))
        .collect()
}

pub struct EvalPaths {
    pub corpus: PathBuf,
    pub queries: PathBuf,
    pub cache_dir: PathBuf,
}

impl EvalPaths {
    pub fn under(eval_dir: &Path) -> Self {
        Self {
            corpus: eval_dir.join("fixtures/corpus.jsonl"),
            queries: eval_dir.join("fixtures/queries.jsonl"),
            cache_dir: eval_dir.join("cache"),
        }
    }
}

pub async fn corpus_vectors(
    corpus: &[CorpusRow],
    config: &EvalConfig,
    embedder: &dyn Embedder,
    cache_dir: &Path,
) -> anyhow::Result<(HashMap<i64, Vec<f32>>, bool)> {
    let hash = hash_corpus_inputs(config);
    if let Some(cached) = read_cache(cache_dir, &hash)?
        && cached.len() == corpus.len()
    {
        return Ok((cached, true));
    }
    let texts: Vec<String> = corpus.iter().map(|r| build_text(r, config.retrieval.build_text)).collect();
    let vectors = embedder.embed_batch(&texts).await?;
    anyhow::ensure!(vectors.len() == corpus.len(), "missing embeddings for corpus rows");
    let map: HashMap<i64, Vec<f32>> = corpus.iter().map(|r| r.id).zip(vectors).collect();
    write_cache(cache_dir, &hash, &map)?;
    Ok((map, false))
}

// Cosine top-k over the corpus, then dedup over a 2× pool so k survive.
pub fn retrieve(
    query: &[f32],
    corpus: &HashMap<i64, Vec<f32>>,
    ids: &[i64],
    k: usize,
    dedup: Option<f64>,
) -> Vec<Retrieved> {
    let pool_size = if dedup.is_some() { (k * 2).max(30) } else { k };
    let mut scored: Vec<Retrieved> = ids
        .iter()
        .filter_map(|id| corpus.get(id).map(|v| Retrieved { id: *id, distance: cosine_distance(query, v) }))
        .collect();
    scored.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    scored.truncate(pool_size);
    let kept = match dedup {
        Some(threshold) => {
            let with_vectors: Vec<(Retrieved, Option<&Vec<f32>>)> =
                scored.into_iter().map(|r| (r, corpus.get(&r.id))).collect();
            dedup_by_pairwise_cosine(with_vectors, |(_, v)| v.map(|v| v.as_slice()), threshold, false)
                .into_iter()
                .map(|(r, _)| r)
                .collect()
        }
        None => scored,
    };
    kept.into_iter().take(k).collect()
}

fn query_text(q: &QueryRow, field: QueryField) -> String {
    match field {
        QueryField::Query => q.query.clone(),
        QueryField::Reformulation => q.reformulation.clone(),
    }
}

pub async fn run(config: EvalConfig, paths: &EvalPaths, embedder: &dyn Embedder) -> anyhow::Result<EvalResult> {
    let corpus: Vec<CorpusRow> = load_jsonl(&paths.corpus)?;
    let queries: Vec<QueryRow> = load_jsonl(&paths.queries)?;
    let (vectors, cache_hit) = corpus_vectors(&corpus, &config, embedder, &paths.cache_dir).await?;
    let ids: Vec<i64> = corpus.iter().map(|r| r.id).collect();
    let by_id: HashMap<i64, CorpusRow> = corpus.into_iter().map(|r| (r.id, r)).collect();
    let texts: Vec<String> = queries.iter().map(|q| query_text(q, config.query.field)).collect();
    let query_vectors = embedder.embed_batch(&texts).await?;
    let dedup = config.retrieval.dedup.as_ref().map(|d| d.threshold);

    let mut per_query = Vec::new();
    let mut negative_tests = Vec::new();
    for (q, vector) in queries.iter().zip(&query_vectors) {
        let top = retrieve(vector, &vectors, &ids, RETRIEVAL_SIZE, dedup);
        if q.gold.is_empty() && q.acceptable.is_empty() {
            negative_tests.push(NegativeTest {
                qid: q.id.clone(),
                query: q.query.clone(),
                min_distance: top.first().map_or(f64::NAN, |r| r.distance),
                top1_id: top.first().map_or(-1, |r| r.id),
            });
            continue;
        }
        per_query.push(score_query(q, &top, &by_id));
    }
    Ok(EvalResult {
        config_hash: hash_corpus_inputs(&config),
        aggregate: aggregate(&per_query),
        config,
        per_query,
        negative_tests,
        cache_hit,
    })
}

// ── 6. report ────────────────────────────────────────────────────────────────

fn fmt(n: f64) -> String {
    if n.is_nan() { "—".into() } else { format!("{n:.3}") }
}

fn escape_pipe(s: &str) -> String {
    s.replace('|', "\\|")
}

pub fn render_markdown(r: &EvalResult) -> String {
    let a = &r.aggregate;
    let mut lines = vec![
        format!("# Eval: {}", r.config.name),
        String::new(),
        format!("Config hash: `{}` {}", r.config_hash, if r.cache_hit { "(corpus cache hit)" } else { "(fresh embed)" }),
        String::new(),
        "## Config".into(),
        "```json".into(),
        serde_json::to_string_pretty(&r.config).unwrap_or_default(),
        "```".into(),
        String::new(),
        "## Aggregate".into(),
        String::new(),
        "| Metric | Value |".into(),
        "|---|---|".into(),
        format!("| Scored queries (gold > 0) | {} |", a.scored_queries),
        format!("| Precision@5 | {} |", fmt(a.precision_at5)),
        format!("| Precision@10 | {} |", fmt(a.precision_at10)),
        format!("| Recall@5 | {} |", fmt(a.recall_at5)),
        format!("| Recall@10 | {} |", fmt(a.recall_at10)),
        format!("| Recall@30 | {} |", fmt(a.recall_at30)),
        format!("| MRR | {} |", fmt(a.mrr)),
        format!("| Mean unique sources @5 | {} |", fmt(a.mean_unique_sources_at5)),
        format!("| Mean unique sources @10 | {} |", fmt(a.mean_unique_sources_at10)),
        format!("| Mean distance to first gold | {} |", a.mean_dist_to_first_gold.map_or_else(|| "—".into(), fmt)),
        String::new(),
        "## Per-query".into(),
        String::new(),
        "| qid | query | gold | hit@5 | hit@10 | hit@30 | P@5 | P@10 | uniq@5 | uniq@10 | first-gold rank | dist to gold |".into(),
        "|---|---|---|---|---|---|---|---|---|---|---|---|".into(),
    ];
    for q in &r.per_query {
        let hits = |h: usize| if q.gold_count > 0 { format!("{h}/{}", q.gold_count) } else { "—".into() };
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            q.qid,
            escape_pipe(&q.query),
            q.gold_count,
            hits(q.hit_at5),
            hits(q.hit_at10),
            hits(q.hit_at30),
            fmt(q.precision_at5),
            fmt(q.precision_at10),
            q.unique_sources_at5,
            q.unique_sources_at10,
            q.first_gold_rank.map_or_else(|| "—".into(), |r| r.to_string()),
            q.dist_to_first_gold.map_or_else(|| "—".into(), fmt),
        ));
    }
    lines.push(String::new());
    if !r.negative_tests.is_empty() {
        lines.extend([
            "## Negative tests (gold = 0)".into(),
            String::new(),
            "Queries where the corpus has nothing relevant. Min distance is the closest".into(),
            "result the retriever surfaced — if it's low (< ~0.5), the retriever is".into(),
            "confidently wrong; high distance is correct \"I don't know\" behaviour.".into(),
            String::new(),
            "| qid | query | min distance | top-1 id |".into(),
            "|---|---|---|---|".into(),
        ]);
        for n in &r.negative_tests {
            lines.push(format!("| {} | {} | {} | {} |", n.qid, escape_pipe(&n.query), fmt(n.min_distance), n.top1_id));
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

// ── 7. inspect ───────────────────────────────────────────────────────────────

fn short_title(row: &CorpusRow, max: usize) -> String {
    let raw = row.title.clone().unwrap_or_else(|| row.body.chars().take(200).collect());
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > max {
        format!("{}…", one_line.chars().take(max - 1).collect::<String>())
    } else {
        one_line
    }
}

pub async fn inspect(
    config: &EvalConfig,
    paths: &EvalPaths,
    embedder: &dyn Embedder,
    qids: &[String],
    k: usize,
) -> anyhow::Result<String> {
    let corpus: Vec<CorpusRow> = load_jsonl(&paths.corpus)?;
    let all: Vec<QueryRow> = load_jsonl(&paths.queries)?;
    let queries: Vec<QueryRow> = qids
        .iter()
        .map(|qid| all.iter().find(|q| &q.id == qid).cloned().ok_or_else(|| anyhow::anyhow!("unknown qid: {qid}")))
        .collect::<anyhow::Result<_>>()?;
    let (vectors, cache_hit) = corpus_vectors(&corpus, config, embedder, &paths.cache_dir).await?;
    let ids: Vec<i64> = corpus.iter().map(|r| r.id).collect();
    let by_id: HashMap<i64, CorpusRow> = corpus.into_iter().map(|r| (r.id, r)).collect();
    let texts: Vec<String> = queries.iter().map(|q| query_text(q, config.query.field)).collect();
    let query_vectors = embedder.embed_batch(&texts).await?;

    let mut out =
        vec![if cache_hit { format!("[inspect] corpus cache hit ({} rows)\n", vectors.len()) } else { String::new() }];
    let src = |id: i64| by_id.get(&id).map_or_else(|| "?".into(), |r| r.bucket().chars().take(30).collect::<String>());
    for (q, vector) in queries.iter().zip(&query_vectors) {
        let top = retrieve(vector, &vectors, &ids, k, config.retrieval.dedup.as_ref().map(|d| d.threshold));
        let (gold, acceptable): (HashSet<i64>, HashSet<i64>) =
            (q.gold.iter().copied().collect(), q.acceptable.iter().copied().collect());
        let hits = |n: usize| count_hits(&gold, &top[..top.len().min(n)]);
        out.push(format!("━━━ {} ━━━", q.id));
        out.push(format!("query:         {}", q.query));
        out.push(format!("reformulation: {}", q.reformulation));
        out.push(format!("gold: {} · acceptable: {}\n", q.gold.len(), q.acceptable.len()));
        out.push(format!(
            "gold hits  top-5: {}/{}   top-10: {}/{}   top-{k}: {}/{}\n",
            hits(5),
            q.gold.len(),
            hits(10),
            q.gold.len(),
            hits(k),
            q.gold.len()
        ));
        out.push("rank  dist   mark  source                          title".into());
        out.push("────  ─────  ────  ──────                          ─────".into());
        for (rank, item) in top.iter().enumerate() {
            let mark = if gold.contains(&item.id) {
                "GOLD"
            } else if acceptable.contains(&item.id) {
                "okay"
            } else {
                "    "
            };
            let title = by_id.get(&item.id).map_or_else(|| "<row not found>".into(), |r| short_title(r, 120));
            out.push(format!("{:>4}  {:.3}  {mark}  {:<30}  {title}", rank + 1, item.distance, src(item.id)));
        }
        out.push(String::new());
        let retrieved: HashSet<i64> = top.iter().map(|r| r.id).collect();
        let missed: Vec<i64> = q.gold.iter().copied().filter(|id| !retrieved.contains(id)).collect();
        if !missed.is_empty() {
            out.push(format!("missed gold (not in top-{k}): {}", missed.len()));
            for id in missed.iter().take(8) {
                let title = by_id.get(id).map_or_else(|| "<not in corpus>".into(), |r| short_title(r, 120));
                out.push(format!("        id={id:>4}              {:<30}  {title}", src(*id)));
            }
            if missed.len() > 8 {
                out.push(format!("        …and {} more", missed.len() - 8));
            }
        }
        out.push(String::new());
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(id: i64) -> Retrieved {
        Retrieved { id, distance: 0.1 * id as f64 }
    }

    #[test]
    fn first_gold_rank_is_one_indexed() {
        assert_eq!(first_gold_rank(&[3], &[r(1), r(2), r(3)]), Some(3));
        assert_eq!(first_gold_rank(&[9], &[r(1)]), None);
        assert!(mean(&[]).is_nan());
        assert_eq!(mean(&[1.0, 3.0]), 2.0);
    }

    #[test]
    fn aggregates_only_queries_with_gold() {
        let corpus: HashMap<i64, CorpusRow> = HashMap::new();
        let q = |id: &str, gold: Vec<i64>| QueryRow {
            id: id.into(),
            query: id.into(),
            reformulation: id.into(),
            gold,
            acceptable: vec![],
        };
        let top = vec![r(1), r(2), r(3)];
        let per = vec![score_query(&q("a", vec![2]), &top, &corpus), score_query(&q("b", vec![]), &top, &corpus)];
        let agg = aggregate(&per);
        assert_eq!(agg.scored_queries, 1);
        assert_eq!(agg.recall_at5, 1.0);
        assert_eq!(agg.mrr, 0.5);
    }

    #[test]
    fn corpus_cache_key_matches_the_ts_harness() {
        // sha256('{"model":"text-embedding-3-small","dimensions":1536,"buildText":"title+body"}')[..16]
        let config: EvalConfig = serde_json::from_str(
            r#"{"name":"x","retrieval":{"embed":{"model":"text-embedding-3-small","dimensions":1536},"buildText":"title+body","topK":10,"dedup":null,"rerank":null},"query":{"field":"query"},"scoring":{"mode":"binary"}}"#,
        )
        .unwrap();
        let expected = hex::encode(Sha256::digest(
            br#"{"model":"text-embedding-3-small","dimensions":1536,"buildText":"title+body"}"#,
        ))[..16]
            .to_owned();
        assert_eq!(hash_corpus_inputs(&config), expected);
    }
}
