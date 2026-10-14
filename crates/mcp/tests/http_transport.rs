// The Streamable HTTP transport under generated session lifecycles: clients
// connect, call, end their session or just go away, in both modes. A probe
// handler counts how many live handler instances exist, so leaks show. A port
// of packages/mcp/test/http-transport.test.ts.
//
// One difference of shape, not of rule: the TS transport shares one server
// object between the sessions of a single-session instance, rmcp builds a
// handler per session — so "one shared instance" reads here as "at most one
// live instance": older sessions are evicted and let go of theirs.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mcp_tools::server::{HttpOptions, serve_http};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt};
use support::runtime;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Counts {
    created: AtomicUsize,
    closed: AtomicUsize,
}

impl Counts {
    fn live(&self) -> usize {
        self.created.load(Ordering::SeqCst) - self.closed.load(Ordering::SeqCst)
    }
}

// One handler instance; the last clone dropped is the instance closed.
struct Instance {
    id: usize,
    counts: Arc<Counts>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.counts.closed.fetch_add(1, Ordering::SeqCst);
    }
}

// A handler with one tool that answers which instance it is.
#[derive(Clone)]
struct Probe(Arc<Instance>);

impl ServerHandler for Probe {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let schema = serde_json::json!({ "type": "object" }).as_object().cloned().unwrap_or_default();
        Ok(ListToolsResult::with_all_items(vec![Tool::new("whoami", "instance id", Arc::new(schema))]))
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(self.0.id.to_string())]).into())
    }
}

async fn start(multi_session: bool) -> (String, Arc<Counts>, CancellationToken) {
    let counts = Arc::new(Counts::default());
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cancel = CancellationToken::new();
    let factory_counts = counts.clone();
    tokio::spawn(serve_http(
        move || {
            let id = factory_counts.created.fetch_add(1, Ordering::SeqCst) + 1;
            Probe(Arc::new(Instance { id, counts: factory_counts.clone() }))
        },
        HttpOptions { port, allowed_hosts: Vec::new(), multi_session },
        cancel.clone(),
    ));
    let url = format!("http://127.0.0.1:{port}/mcp");
    for _ in 0..100 {
        if reqwest::Client::new().post(&url).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (url, counts, cancel)
}

type Client = RunningService<RoleClient, ()>;

// rmcp's client re-initializes a session the server dropped (404) on its own;
// the TS SDK client the supervisor runs doesn't, and an evicted session must
// stay evicted for the rule to be observable.
async fn open(url: &str) -> Client {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    config.reinit_on_expired_session = false;
    let transport = StreamableHttpClientTransport::with_client(reqwest::Client::new(), config);
    ().serve(transport).await.expect("session opens")
}

async fn works(client: &Client) -> bool {
    client.call_tool(CallToolRequestParams::new("whoami")).await.is_ok()
}

// rmcp lets go of a session's handler on a task of its own; give it a moment.
async fn settle(counts: &Counts, want: usize) -> usize {
    for _ in 0..100 {
        if counts.live() == want {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    counts.live()
}

#[derive(Debug, Clone)]
enum Op {
    Open,
    Call(usize),
    End(usize),
    Abandon(usize),
}

fn op() -> impl Strategy<Value = Op> {
    let nat = || 0usize..(i32::MAX as usize);
    prop_oneof![3 => Just(Op::Open), 4 => nat().prop_map(Op::Call), 2 => nat().prop_map(Op::End), 1 => nat().prop_map(Op::Abandon)]
}

#[derive(PartialEq)]
enum State {
    Open,
    Ended,
    Abandoned,
}

#[test]
fn multi_session_every_open_session_works_an_ended_one_stops_working_and_lets_go_of_its_handler() {
    TestRunner::new(Config::with_cases(60))
        .run(&proptest::collection::vec(op(), 1..=15), |ops| {
            runtime().block_on(async {
                let (url, counts, cancel) = start(true).await;
                let mut sessions: Vec<(Option<Client>, State)> = Vec::new();
                for o in &ops {
                    let live: Vec<usize> =
                        sessions.iter().enumerate().filter(|(_, s)| s.1 == State::Open).map(|(i, _)| i).collect();
                    match o {
                        Op::Open => sessions.push((Some(open(&url).await), State::Open)),
                        Op::Call(pick) | Op::End(pick) | Op::Abandon(pick) if !live.is_empty() => {
                            let i = live[pick % live.len()];
                            match o {
                                Op::Call(_) => {
                                    assert!(works(sessions[i].0.as_ref().unwrap()).await, "an open session answers")
                                }
                                Op::End(_) => {
                                    // Ending the session: the client closes it (DELETE).
                                    let client = sessions[i].0.take().unwrap();
                                    let _ = client.cancel().await;
                                    sessions[i].1 = State::Ended;
                                }
                                _ => {
                                    // Going away without a word: the connection
                                    // is dropped, the session is not ended.
                                    drop(sessions[i].0.take());
                                    sessions[i].1 = State::Abandoned;
                                }
                            }
                        }
                        _ => {}
                    }
                    // Every session that is still open keeps working.
                    for (client, state) in &sessions {
                        if *state == State::Open {
                            assert!(works(client.as_ref().unwrap()).await, "other sessions are unaffected");
                        }
                    }
                    // An ended session holds no handler instance; an abandoned
                    // one may still (until the server times it out).
                    let open_now = sessions.iter().filter(|s| s.1 == State::Open).count();
                    let held = sessions.iter().filter(|s| s.1 != State::Ended).count();
                    let live = settle(&counts, held).await;
                    assert!(
                        live >= open_now && live <= held,
                        "live handler instances after {o:?}: {live}, open {open_now}, held {held}"
                    );
                }
                for (client, _) in &mut sessions {
                    if let Some(c) = client.take() {
                        let _ = c.cancel().await;
                    }
                }
                cancel.cancel();
            });
            Ok(())
        })
        .unwrap();
}

#[test]
fn single_session_the_newest_session_works_older_ones_are_evicted_and_let_go_of_their_handler() {
    TestRunner::new(Config::with_cases(15))
        .run(&(1usize..=5), |n| {
            runtime().block_on(async {
                let (url, counts, cancel) = start(false).await;
                let mut clients: Vec<Client> = Vec::new();
                for _ in 0..n {
                    clients.push(open(&url).await);
                    assert!(works(clients.last().unwrap()).await, "the newest session works");
                    for old in &clients[..clients.len() - 1] {
                        assert!(!works(old).await, "an older session is evicted");
                    }
                    assert_eq!(settle(&counts, 1).await, 1, "one live handler instance");
                }
                for c in clients {
                    let _ = c.cancel().await;
                }
                cancel.cancel();
            });
            Ok(())
        })
        .unwrap();
}
