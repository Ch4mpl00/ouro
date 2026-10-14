// list_userbot_dialogs over MCP, with Telegram's dialog list played by a
// stand-in that answers the way the API does: most recent first, at most
// `limit` when one is given. A port of packages/mcp/test/userbot.test.ts.
//
// Rules: each dialog is typed by what it is (a supergroup is a group); a type
// filter returns the first `limit` dialogs of that type, in Telegram's order
// — not whatever survives filtering the first `limit` of all.

mod support;

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use mcp_tools::userbot::{ChannelHandle, ChannelMessage, DialogInfo, DialogKind, TelegramAccount};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{Value, json};
use support::{Client, Outcome, Server, TestDatabase, call, connect, runtime, start_test_server_with_account};

#[derive(Default)]
struct Dialogs(Mutex<Vec<DialogKind>>);

#[async_trait]
impl TelegramAccount for Dialogs {
    async fn has_session(&self) -> bool {
        true
    }

    async fn dialogs(&self, limit: Option<usize>) -> anyhow::Result<Vec<DialogInfo>> {
        let kinds = self.0.lock().unwrap().clone();
        Ok(kinds
            .iter()
            .enumerate()
            .take(limit.unwrap_or(kinds.len()))
            .map(|(i, kind)| DialogInfo {
                id: (1000 + i).to_string(),
                bare_id: Some((1000 + i).to_string()),
                kind: *kind,
                title: Some(format!("{kind:?} {i}")),
                username: (*kind == DialogKind::Channel).then(|| format!("chan{i}")),
                unread_count: (i % 4) as i32,
                peer: None,
            })
            .collect())
    }

    async fn messages(
        &self,
        _: &ChannelHandle,
        _: usize,
        _: Option<i64>,
        _: bool,
    ) -> anyhow::Result<Vec<ChannelMessage>> {
        Ok(Vec::new())
    }
}

struct Env {
    _db: TestDatabase,
    _server: Server,
    client: Client,
    dialogs: Arc<Dialogs>,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        runtime().block_on(async {
            let db = support::test_database().await;
            let dialogs = Arc::new(Dialogs::default());
            let server =
                start_test_server_with_account(&db, Some("userbot"), dialogs.clone()).await.expect("server starts");
            let client = connect(&server.url).await;
            Env { _db: db, _server: server, client, dialogs }
        })
    })
}

fn type_of(kind: DialogKind) -> &'static str {
    match kind {
        DialogKind::User => "user",
        DialogKind::BasicGroup | DialogKind::Supergroup => "group",
        DialogKind::Channel => "channel",
    }
}

#[test]
fn types_each_dialog_by_what_it_is_and_filters_before_limiting() {
    let env = env();
    let kinds = proptest::collection::vec(
        proptest::sample::select(vec![
            DialogKind::User,
            DialogKind::BasicGroup,
            DialogKind::Supergroup,
            DialogKind::Channel,
        ]),
        0..=60,
    );
    let filter =
        proptest::option::weighted(5.0 / 6.0, proptest::sample::select(vec!["channel", "group", "user", "all"]));
    let limit = proptest::option::weighted(5.0 / 6.0, 1usize..=30);
    TestRunner::new(Config::with_cases(300))
        .run(&(kinds, filter, limit), |(kinds, filter, limit)| {
            *env.dialogs.0.lock().unwrap() = kinds.clone();
            let mut args = json!({});
            if let Some(f) = filter {
                args["type"] = json!(f);
            }
            if let Some(l) = limit {
                args["limit"] = json!(l);
            }
            let out = runtime().block_on(call(&env.client, "list_userbot_dialogs", args));
            let Outcome::Ok(value) = out else { panic!("{out:?}") };
            let got: Vec<(String, String)> = value["dialogs"]
                .as_array()
                .expect("dialogs")
                .iter()
                .map(|d| {
                    (d["id"].as_str().unwrap_or_default().to_owned(), d["type"].as_str().unwrap_or_default().to_owned())
                })
                .collect();
            let want: Vec<(String, String)> = kinds
                .iter()
                .enumerate()
                .map(|(i, kind)| ((1000 + i).to_string(), type_of(*kind).to_owned()))
                .filter(|(_, t)| filter.is_none_or(|f| f == "all" || f == t))
                .take(limit.unwrap_or(100))
                .collect();
            let at = format!("{kinds:?} {filter:?} {limit:?}");
            assert_eq!(got, want, "{at}");
            assert_eq!(value["count"], Value::from(want.len()), "{at}");
            Ok(())
        })
        .unwrap();
}
