// Harvesting Telegram channels into the news store: the real provider,
// adapter, Postgres watermark and repository, with Telegram played by a
// stand-in account that answers the history the way the API does (newest
// first; with `reverse`, oldest first from just after min_id; never more than
// `limit`). A port of packages/mcp/test/news-ingest.test.ts. Posts arrive —
// sometimes hundreds between two ticks — and ticks sometimes fail for a
// channel; once the poller has caught up, every text post that arrived after
// the first tick is stored exactly once — and nothing from a group chat:
// Telegram reports a supergroup as a channel too, and its messages are a
// private conversation.

mod support;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use mcp_tools::news::{NewsProvider, NewsRepository, TelegramChannels};
use mcp_tools::pg::{self, PgPool};
use mcp_tools::userbot::{ChannelHandle, ChannelMessage, DialogInfo, DialogKind, TelegramAccount};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use support::{TestDatabase, runtime, word_embedder};

#[derive(Debug, Clone)]
struct Post {
    id: i64,
    date: i64,
    message: String,
}

// Telegram, as far as the harvester can tell.
const GROUP: &str = "900";

#[derive(Default)]
struct FakeTelegram {
    channels: Mutex<BTreeMap<String, Vec<Post>>>,
    failing: Mutex<HashSet<String>>,
}

#[async_trait]
impl TelegramAccount for FakeTelegram {
    async fn has_session(&self) -> bool {
        true
    }

    async fn dialogs(&self, _limit: Option<usize>) -> anyhow::Result<Vec<DialogInfo>> {
        let dialog = |id: &str, kind, title: &str, username: Option<String>| DialogInfo {
            id: id.into(),
            bare_id: Some(id.into()),
            kind,
            title: Some(title.into()),
            username,
            unread_count: 0,
            peer: None,
        };
        let mut out = vec![
            dialog("1", DialogKind::User, "a friend", None),
            dialog("2", DialogKind::BasicGroup, "a basic group", None),
            dialog(GROUP, DialogKind::Supergroup, "a supergroup", None),
        ];
        for id in self.channels.lock().unwrap().keys().filter(|id| *id != GROUP) {
            out.push(dialog(id, DialogKind::Channel, &format!("chan {id}"), Some(format!("u{id}"))));
        }
        Ok(out)
    }

    async fn messages(
        &self,
        channel: &ChannelHandle,
        limit: usize,
        min_id: Option<i64>,
        reverse: bool,
    ) -> anyhow::Result<Vec<ChannelMessage>> {
        anyhow::ensure!(!self.failing.lock().unwrap().remove(&channel.chat_id), "FLOOD_WAIT_7");
        let channels = self.channels.lock().unwrap();
        let mut after: Vec<&Post> = channels
            .get(&channel.chat_id)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter(|p| p.id > min_id.unwrap_or(0))
            .collect();
        if reverse {
            after.sort_by_key(|p| p.id);
        } else {
            after.sort_by_key(|p| std::cmp::Reverse(p.id));
        }
        Ok(after
            .into_iter()
            .take(limit)
            .map(|p| ChannelMessage {
                id: p.id,
                date: Utc.timestamp_opt(p.date, 0).unwrap(),
                text: p.message.clone(),
                views: None,
                forwards: None,
            })
            .collect())
    }
}

#[derive(Debug, Clone)]
enum Step {
    Posts { channel: usize, count: usize, media_every: i64 },
    Tick { fail_channel: Option<usize> },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0usize..=2, prop_oneof![4 => 1usize..=30, 1 => 201usize..=450], proptest::sample::select(vec![0i64, 7]))
            .prop_map(|(channel, count, media_every)| Step::Posts { channel, count, media_every }),
        3 => proptest::option::weighted(5.0 / 6.0, 0usize..=2).prop_map(|fail_channel| Step::Tick { fail_channel }),
    ]
}

struct Env {
    _db: TestDatabase,
    pool: PgPool,
    repo: NewsRepository,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        runtime().block_on(async {
            let db = support::test_database().await;
            let pool = pg::connect(&db.url).expect("pool");
            pg::migrate(&pool).await.expect("migrations apply");
            let repo = NewsRepository::new(pool.clone(), word_embedder());
            Env { _db: db, pool, repo }
        })
    })
}

#[test]
fn stores_every_text_post_after_the_first_tick_exactly_once_bursts_and_failures_included() {
    let env = env();
    let witnessed = Mutex::new(BTreeSet::new());
    TestRunner::new(Config::with_cases(20))
        .run(&proptest::collection::vec(step(), 1..=12), |steps| {
            runtime().block_on(async {
                env.pool.get().await.unwrap().batch_execute("TRUNCATE news_items RESTART IDENTITY").await.unwrap();
                let telegram = Arc::new(FakeTelegram::default());
                let ids = ["100", "200", "300"];
                for c in ids.iter().chain(&[GROUP]) {
                    telegram.channels.lock().unwrap().insert((*c).into(), Vec::new());
                }
                let mut next_id: BTreeMap<&str, i64> = ids.iter().chain(&[GROUP]).map(|c| (*c, 1)).collect();
                let mut clock = 1_790_000_000i64;
                let mut post = |channel: &'static str, count: usize, media_every: i64| {
                    let mut channels = telegram.channels.lock().unwrap();
                    let list = channels.get_mut(channel).unwrap();
                    for _ in 0..count {
                        let id = next_id[channel];
                        next_id.insert(channel, id + 1);
                        let message = if media_every > 0 && id % media_every == 0 {
                            String::new()
                        } else {
                            format!("post {channel}/{id}")
                        };
                        list.push(Post { id, date: clock, message });
                        clock += 1;
                    }
                };
                // History before the harvester ever runs: only the newest 50
                // of it is wanted (the bootstrap).
                for c in ids {
                    post(c, 80, 0);
                }
                // The supergroup chats all along.
                post(GROUP, 5, 0);
                let bootstrap_cut: BTreeMap<&str, i64> = ids.iter().map(|c| (*c, 81 - 50)).collect();

                let provider = TelegramChannels::new(telegram.clone(), env.pool.clone()).with_delay(Duration::ZERO);
                let tick = || async {
                    let items = provider.fetch().await.unwrap();
                    if !items.is_empty() {
                        env.repo.save(&items).await.unwrap();
                    }
                };
                tick().await; // the first tick bootstraps every channel

                for s in &steps {
                    match s {
                        Step::Posts { channel, count, media_every } => {
                            if *count > 200 {
                                witnessed.lock().unwrap().insert("a burst of more than one tick's worth");
                            }
                            post(ids[*channel], *count, *media_every);
                            post(GROUP, 5, 0);
                        }
                        Step::Tick { fail_channel } => {
                            if let Some(c) = fail_channel {
                                telegram.failing.lock().unwrap().insert(ids[*c].into());
                                witnessed.lock().unwrap().insert("a channel fails a tick");
                            }
                            tick().await;
                        }
                    }
                }
                for _ in 0..6 {
                    tick().await; // catch up
                }

                let rows = env
                    .pool
                    .get()
                    .await
                    .unwrap()
                    .query("SELECT external_id, body FROM news_items WHERE source = 'channel'", &[])
                    .await
                    .unwrap();
                let mut stored: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
                assert_eq!(stored.iter().collect::<HashSet<_>>().len(), stored.len(), "no post stored twice");
                assert!(!stored.iter().any(|id| id.starts_with(&format!("{GROUP}:"))), "nothing from a group chat");
                for r in &rows {
                    assert!(!r.get::<_, String>(1).trim().is_empty(), "only text posts are stored");
                }
                let channels = telegram.channels.lock().unwrap();
                let mut want: Vec<String> = ids
                    .iter()
                    .flat_map(|c| {
                        channels[*c]
                            .iter()
                            .filter(|p| !p.message.is_empty() && p.id >= bootstrap_cut[c])
                            .map(move |p| format!("{c}:{}", p.id))
                    })
                    .collect();
                stored.sort();
                want.sort();
                assert_eq!(stored, want, "every text post since the bootstrap, and nothing older");
            });
            Ok(())
        })
        .unwrap();
    assert_eq!(
        witnessed.into_inner().unwrap().into_iter().collect::<Vec<_>>(),
        ["a burst of more than one tick's worth", "a channel fails a tick"]
    );
}
