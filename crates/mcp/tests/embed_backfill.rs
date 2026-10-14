// `embed-backfill` — the repair path after an embedder outage — over the
// three stores it drains (news, knowledge notes, the memory index), on
// Postgres. A port of packages/mcp/test/embed-backfill.test.ts.
//
// Rows land while the embedder is down, so their vectors are NULL; then the
// backfill runs against an embedder that is up but rejects some inputs for
// good (like OpenAI rejecting one input fails the whole request) and now and
// then has another outage.
//
// Rules, from what was written and what the embedder accepts:
//   - after a backfill with the embedder up, every row whose text it accepts
//     has a vector — a row it never accepts stays NULL and holds up nothing,
//     wherever it sits in the backlog;
//   - a vector is the embedding of that row's own text;
//   - a backfill during an outage changes nothing and ends;
//   - running it again when nothing can be embedded changes nothing.

mod support;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use mcp_tools::embeddings::{Embedder, drain_missing};
use mcp_tools::knowledge::KnowledgeRepository;
use mcp_tools::memory::{MemoryService, PgMemoryStore};
use mcp_tools::news::{NewsItem, NewsRepository};
use mcp_tools::pg::{self, PgPool};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use support::{TestDatabase, property_runs, runtime, word_vectors};

const POISON: &str = "☠";

// An embedder that can be down, and that rejects any request containing a
// poisoned input — one bad input fails the whole request, as OpenAI's does.
#[derive(Default)]
struct ControlledEmbedder {
    down: AtomicBool,
    calls: AtomicUsize,
}

#[async_trait]
impl Embedder for ControlledEmbedder {
    async fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.down.load(Ordering::SeqCst), "embedder unreachable");
        anyhow::ensure!(!texts.iter().any(|t| t.contains(POISON)), "400 invalid input");
        Ok(word_vectors(texts))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Store {
    News,
    Note,
    Fact,
}

#[derive(Debug, Clone)]
struct Row {
    store: Store,
    poison: bool,
}

fn row() -> impl Strategy<Value = Row> {
    (
        proptest::sample::select(vec![Store::News, Store::Note, Store::Fact]),
        prop_oneof![4 => Just(false), 1 => Just(true)],
    )
        .prop_map(|(store, poison)| Row { store, poison })
}

// `table:text` → has a vector, and whether it is that text's own.
#[derive(Debug, Clone, PartialEq)]
struct Vector {
    key: String,
    text: String,
    embedded: bool,
    own: bool,
}

struct Env {
    _db: TestDatabase,
    pool: PgPool,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        runtime().block_on(async {
            let db = support::test_database().await;
            let pool = pg::connect(&db.url).expect("pool");
            pg::migrate(&pool).await.expect("migrations apply");
            Env { _db: db, pool }
        })
    })
}

fn rounded(v: &[f32]) -> Vec<i64> {
    v.iter().map(|x| (f64::from(*x) * 1e4).round() as i64).collect()
}

// Every row, with whether its vector is the embedding of the text it was
// embedded from (news: title + body; the others: their text).
async fn observe(pool: &PgPool) -> Vec<Vector> {
    let client = pool.get().await.unwrap();
    let mut out = Vec::new();
    for (sql, table) in [
        ("SELECT coalesce(title || E'\\n\\n', '') || body AS text, embedding::text FROM news_items", "news"),
        ("SELECT body AS text, embedding::text FROM knowledge_base_notes", "note"),
        ("SELECT text, embedding::text FROM memory_index", "index"),
    ] {
        for r in client.query(sql, &[]).await.unwrap() {
            let text: String = r.get(0);
            let embedding: Option<String> = r.get(1);
            let want = rounded(&word_vectors(std::slice::from_ref(&text))[0]);
            let got = embedding.as_deref().and_then(pg::parse_vector).map(|v| rounded(&v));
            out.push(Vector { key: format!("{table}:{text}"), embedded: got.is_some(), own: got == Some(want), text });
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

#[test]
fn embeds_every_row_the_embedder_accepts_whatever_rejected_rows_sit_in_the_backlog() {
    let env = env();
    let witnessed = Mutex::new(HashSet::new());
    let strategy = (
        proptest::collection::vec(row(), 1..=25),
        1i64..=6,
        any::<bool>(),
        proptest::collection::vec(0usize..(i32::MAX as usize), 0..=8),
    );
    let mut runner = TestRunner::new(Config::with_cases(property_runs(40)));
    runner
        .run(&strategy, |(rows, batch_size, outage_first, touched)| {
            runtime().block_on(async {
                let pool = &env.pool;
                pool.get()
                    .await
                    .unwrap()
                    .batch_execute(
                        "TRUNCATE news_items, knowledge_base_notes, memory_facts, memory_index RESTART IDENTITY",
                    )
                    .await
                    .unwrap();
                let embedder = Arc::new(ControlledEmbedder::default());
                let news = NewsRepository::new(pool.clone(), embedder.clone());
                let knowledge = KnowledgeRepository::new(pool.clone(), embedder.clone());
                let memory = MemoryService::new(Arc::new(PgMemoryStore::new(pool.clone())), embedder.clone());

                // Everything lands during an outage.
                embedder.down.store(true, Ordering::SeqCst);
                for (i, r) in rows.iter().enumerate() {
                    let store = format!("{:?}", r.store).to_lowercase();
                    let text =
                        format!("entry {i} {store}{}", if r.poison { format!(" {POISON}") } else { String::new() });
                    match r.store {
                        Store::News => {
                            news.save(&[NewsItem {
                                source: "hn".into(),
                                external_id: format!("e{i}"),
                                title: None,
                                url: None,
                                body: text,
                                metadata: Default::default(),
                                posted_at: None,
                            }])
                            .await
                            .unwrap();
                        }
                        Store::Note => {
                            knowledge.add_note(&text, None, None).await.unwrap();
                        }
                        Store::Fact => {
                            memory.remember(&text, None, None).await.unwrap();
                        }
                    }
                }
                // Rows updated since they landed (a re-fetched article, a
                // re-tagged note) sit elsewhere on disk than their id
                // suggests: a no-op update moves a row to the end of the table.
                for pick in &touched {
                    for table in ["news_items", "knowledge_base_notes", "memory_index"] {
                        let id = (pick % rows.len()) as i64 + 1;
                        pool.get()
                            .await
                            .unwrap()
                            .execute(&format!("UPDATE {table} SET id = id WHERE id = $1"), &[&id])
                            .await
                            .unwrap();
                    }
                }
                let quiet = |_: &str| {};
                let backfill = || async {
                    let news = drain_missing("news", batch_size, &quiet, |b, a| news.embed_missing_batch(b, a));
                    let knowledge =
                        drain_missing("knowledge", batch_size, &quiet, |b, a| knowledge.embed_missing_batch(b, a));
                    let memory =
                        drain_missing("memory", batch_size, &quiet, |b, a| memory.indexer().embed_missing_batch(b, a));
                    let (n, k, m) = futures::join!(news, knowledge, memory);
                    n.unwrap();
                    k.unwrap();
                    m.unwrap();
                };
                let landed = observe(pool).await;
                assert!(landed.iter().all(|r| !r.embedded), "nothing embedded during the outage");

                if outage_first {
                    backfill().await;
                    assert_eq!(observe(pool).await, landed, "a backfill during an outage changes nothing");
                }

                embedder.down.store(false, Ordering::SeqCst);
                backfill().await;
                let after = observe(pool).await;
                let at = format!("{rows:?} batch={batch_size}");
                for r in &after {
                    let poisoned = r.text.contains(POISON);
                    assert_eq!(
                        r.embedded,
                        !poisoned,
                        "{at}: {} {}",
                        r.key,
                        if poisoned { "is rejected, so stays NULL" } else { "is accepted, so is embedded" }
                    );
                    if r.embedded {
                        assert!(r.own, "{at}: {} holds its own text's vector", r.key);
                    }
                }
                if rows.iter().any(|r| r.poison) && rows.iter().any(|r| !r.poison) {
                    witnessed.lock().unwrap().insert("accepted and rejected rows in one backlog");
                }

                let calls = embedder.calls.load(Ordering::SeqCst);
                backfill().await;
                assert_eq!(observe(pool).await, after, "{at}: running again changes nothing");
                if !rows.iter().any(|r| r.poison) {
                    assert_eq!(embedder.calls.load(Ordering::SeqCst), calls, "{at}: nothing left, nothing asked");
                }
            });
            Ok(())
        })
        .unwrap();
    let seen: Vec<&str> = witnessed.into_inner().unwrap().into_iter().collect();
    assert_eq!(seen, ["accepted and rejected rows in one backlog"]);
}
