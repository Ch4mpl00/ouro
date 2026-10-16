// The gateway over real Streamable HTTP upstreams (in-process MCP servers on
// loopback): generated tool sets with clashing and invalid names, upstreams
// that are down at boot, tools that fail, and upstream sessions that get
// evicted between calls. The namespaced surface must be exactly what the
// rules promise, and every call must reach the tool it names, with its
// arguments, or fail as a tool error — never as a broken connection. A port
// of packages/mcp/test/gateway.test.ts; the own tools are the names the
// gateway must not take (McpTools lists them first, see server.rs).

mod support;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use mcp_tools::gateway::{Gateway, Upstream};
use mcp_tools::server::{HttpOptions, serve_http};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};
use support::runtime;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Behaviour {
    Echo,
    Error,
    Throw,
}

#[derive(Debug, Clone)]
struct ToolSpec {
    name: &'static str,
    behaviour: Behaviour,
}

#[derive(Debug, Clone)]
struct UpstreamSpec {
    prefix: String,
    up: bool,
    tools: Vec<ToolSpec>,
}

fn valid_exposed(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// A server whose tools answer with who they are and what they were given.
#[derive(Clone)]
struct Upstreamed {
    source: String,
    tools: Arc<Vec<ToolSpec>>,
}

impl ServerHandler for Upstreamed {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let schema = json!({ "type": "object", "properties": { "q": { "type": "string" } } })
            .as_object()
            .cloned()
            .unwrap_or_default();
        let schema = Arc::new(schema);
        let mut seen = HashSet::new();
        let tools = self
            .tools
            .iter()
            .filter(|t| seen.insert(t.name))
            .map(|t| Tool::new(t.name, format!("{}/{}", self.source, t.name), schema.clone()))
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(t) = self.tools.iter().find(|t| t.name == request.name) else {
            return Err(ErrorData::invalid_params("no such tool", None));
        };
        let args = Value::Object(request.arguments.clone().unwrap_or_default());
        let text = json!({ "source": self.source, "tool": t.name, "args": args }).to_string();
        Ok(match t.behaviour {
            Behaviour::Echo => CallToolResult::success(vec![ContentBlock::text(text)]),
            Behaviour::Error => CallToolResult::error(vec![ContentBlock::text(text)]),
            // What an SDK server makes of a handler that throws.
            Behaviour::Throw => {
                CallToolResult::error(vec![ContentBlock::text(format!("{}/{} blew up", self.source, t.name))])
            }
        }
        .into())
    }
}

fn tool() -> impl Strategy<Value = ToolSpec> {
    (
        proptest::sample::select(vec![
            "search",
            "fetch",
            "x",
            "tavily__x",
            "bad name",
            "dot.name",
            "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy",
        ]),
        proptest::sample::select(vec![Behaviour::Echo, Behaviour::Echo, Behaviour::Error, Behaviour::Throw]),
    )
        .prop_map(|(name, behaviour)| ToolSpec { name, behaviour })
}

fn upstream() -> impl Strategy<Value = UpstreamSpec> {
    (
        proptest::sample::select(vec!["tavily".to_owned(), "web".to_owned(), "a-b".to_owned(), "z".repeat(70)]),
        prop_oneof![3 => Just(true), 1 => Just(false)],
        proptest::collection::vec(tool(), 1..=4),
    )
        .prop_map(|(prefix, up, tools)| UpstreamSpec { prefix, up, tools })
}

// The surface the rules promise: each reachable upstream in order;
// prefix__name; an invalid name, or one already taken (by an own tool or an
// earlier upstream), is dropped.
fn expected_surface(own: &[&str], ups: &[UpstreamSpec]) -> BTreeMap<String, (String, &'static str, Behaviour)> {
    let mut out = BTreeMap::new();
    for (i, u) in ups.iter().enumerate().filter(|(_, u)| u.up) {
        let mut seen = HashSet::new();
        for t in &u.tools {
            if !seen.insert(t.name) {
                continue;
            }
            let exposed = format!("{}__{}", u.prefix, t.name);
            if !valid_exposed(&exposed) || out.contains_key(&exposed) || own.contains(&exposed.as_str()) {
                continue;
            }
            out.insert(exposed, (format!("up{i}"), t.name, t.behaviour));
        }
    }
    out
}

async fn serve_upstream(source: String, tools: Vec<ToolSpec>) -> (String, CancellationToken) {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cancel = CancellationToken::new();
    let tools = Arc::new(tools);
    // Single-session, like a strict upstream: a second client evicts the
    // gateway's session, which the gateway must survive.
    tokio::spawn(serve_http(
        move || Upstreamed { source: source.clone(), tools: tools.clone() },
        HttpOptions {
            bind_address: "127.0.0.1".parse().unwrap(),
            port,
            allowed_hosts: Vec::new(),
            multi_session: false,
        },
        cancel.clone(),
    ));
    let url = format!("http://127.0.0.1:{port}/mcp");
    for _ in 0..100 {
        if reqwest::Client::new().post(&url).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (url, cancel)
}

fn text_of(result: &CallToolResult) -> String {
    result.content.first().and_then(|c| c.as_text()).map(|t| t.text.clone()).unwrap_or_default()
}

#[test]
fn serves_exactly_the_namespaced_union_and_routes_every_call_to_its_tool() {
    let own_names = proptest::collection::vec(
        proptest::sample::select(vec!["search", "fetch", "x", "tavily__x", "web__search"]),
        1..=3,
    );
    TestRunner::new(Config::with_cases(60))
        .run(
            &(
                own_names,
                proptest::collection::vec(upstream(), 0..=3),
                proptest::collection::vec(0usize..(i32::MAX as usize), 1..=4),
            ),
            |(own, ups, kicks)| {
                runtime().block_on(async {
                    let mut running: Vec<(String, CancellationToken)> = Vec::new();
                    let mut upstreams = Vec::new();
                    for (i, u) in ups.iter().enumerate() {
                        let url = if u.up {
                            let served = serve_upstream(format!("up{i}"), u.tools.clone()).await;
                            let url = served.0.clone();
                            running.push(served);
                            url
                        } else {
                            // Nothing listens on the discard port.
                            "http://127.0.0.1:9/mcp".to_owned()
                        };
                        upstreams.push(Upstream {
                            name: format!("up{i}"),
                            url,
                            prefix: u.prefix.clone(),
                            headers: Vec::new(),
                        });
                    }
                    let own_tools: Vec<String> = own.iter().map(|s| (*s).to_owned()).collect();
                    let gateway = Gateway::connect(upstreams, &own_tools).await;
                    let at = format!("{own:?} {ups:?}");
                    let want = expected_surface(&own, &ups);
                    let mut listed: Vec<String> = gateway.tools().iter().map(|t| t.name.to_string()).collect();
                    listed.sort();
                    assert_eq!(listed, want.keys().cloned().collect::<Vec<_>>(), "{at}: the namespaced surface");

                    let names: Vec<&String> = want.keys().collect();
                    for (i, pick) in kicks.iter().enumerate() {
                        // Evict the gateway's session at one upstream now and then.
                        if i % 2 == 1 && !running.is_empty() {
                            let (url, _) = &running[pick % running.len()];
                            let transport = StreamableHttpClientTransport::with_client(
                                reqwest::Client::new(),
                                StreamableHttpClientTransportConfig::with_uri(url.as_str()),
                            );
                            let intruder = ().serve(transport).await.expect("intruder connects");
                            let _ = intruder.cancel().await;
                        }
                        if names.is_empty() {
                            break;
                        }
                        let exposed = names[pick % names.len()];
                        let (source, original, behaviour) = &want[exposed];
                        let q = format!("call-{i}");
                        let mut args = serde_json::Map::new();
                        args.insert("q".into(), json!(q));
                        let result =
                            gateway.call(CallToolRequestParams::new(exposed.clone()).with_arguments(args)).await;
                        let text = text_of(&result);
                        if *behaviour == Behaviour::Throw {
                            assert_eq!(result.is_error, Some(true), "{at}: {exposed} threw → a tool error");
                            assert!(text.contains("blew up"), "{at}: {exposed} error says what failed: {text}");
                            continue;
                        }
                        assert_eq!(
                            result.is_error == Some(true),
                            *behaviour == Behaviour::Error,
                            "{at}: {exposed} error flag passes through ({text})"
                        );
                        let got: Value =
                            serde_json::from_str(&text).unwrap_or_else(|_| panic!("{at}: {exposed}: {text}"));
                        assert_eq!(
                            got,
                            json!({ "source": source, "tool": original, "args": { "q": q } }),
                            "{at}: {exposed} reached its tool with its arguments"
                        );
                    }

                    let unknown = gateway.call(CallToolRequestParams::new("nope__nothing")).await;
                    assert_eq!(unknown.is_error, Some(true), "an unknown tool is a tool error");
                    for (_, cancel) in running {
                        cancel.cancel();
                    }
                });
                Ok(())
            },
        )
        .unwrap();
}
