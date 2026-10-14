// The SSRF guard and fetch_url, judged from the outside. A port of
// packages/mcp/test/fetch-guard.test.ts.
//
// 1. Address classification, over every spelling a URL can carry: an
//    independent 128-bit oracle (below — not the guard's own code) says
//    which addresses must be refused, which must be allowed, and which may
//    go either way; the guard's URL check must agree.
// 2. fetch_url over MCP with the production guard: no spelling of a loopback
//    address ever reaches a local server.
// 3. fetch_url over MCP with a guard that lets a local server play the
//    public internet: byte caps, truncation and binary detection.

mod support;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use mcp_tools::fetch::GuardedFetch;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{Value, json};
use support::{
    Client, Outcome, Server, TestDatabase, call, connect, property_runs, runtime, start_test_server,
    start_test_server_with_fetch,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── the oracle: 128-bit addresses, IPv4 as ::ffff:a.b.c.d ─────────────────────

const V4_MAPPED: u128 = 0xffff << 32;
const NAT64: u128 = 0x0064_ff9b << 96;

#[derive(Debug, Clone, Copy)]
struct Range {
    base: u128,
    prefix: u32,
    label: &'static str,
}

fn v4_bits(text: &str) -> u128 {
    text.split('.').fold(0u128, |acc, p| (acc << 8) | p.parse::<u128>().unwrap())
}

// The plain (mapped) form and the NAT64 form reach the same IPv4 host.
fn v4(cidr: &str, label: &'static str) -> [Range; 2] {
    let (addr, len) = cidr.split_once('/').unwrap();
    let bits = v4_bits(addr);
    let prefix = 96 + len.parse::<u32>().unwrap();
    [Range { base: V4_MAPPED | bits, prefix, label }, Range { base: NAT64 | bits, prefix, label }]
}

fn v6(hex: &str, prefix: u32, label: &'static str) -> Range {
    Range { base: u128::from_str_radix(hex, 16).unwrap(), prefix, label }
}

// Must never be fetched: the box itself, its private networks, cloud
// metadata, carrier-grade NAT (Tailscale lives there), and IPv6 equivalents.
fn must_refuse() -> Vec<Range> {
    let mut out = Vec::new();
    for (cidr, label) in [
        ("0.0.0.0/8", "this network"),
        ("10.0.0.0/8", "RFC1918"),
        ("100.64.0.0/10", "CGNAT"),
        ("127.0.0.0/8", "loopback"),
        ("169.254.0.0/16", "link-local / metadata"),
        ("172.16.0.0/12", "RFC1918"),
        ("192.168.0.0/16", "RFC1918"),
    ] {
        out.extend(v4(cidr, label));
    }
    out.push(v6("0", 96, "unspecified / loopback / IPv4-compatible"));
    out.push(v6("fc000000000000000000000000000000", 7, "unique-local"));
    out.push(v6("fe800000000000000000000000000000", 10, "link-local"));
    out.push(v6("fec00000000000000000000000000000", 10, "site-local"));
    out.push(v6("0064ff9b000100000000000000000000", 48, "local-use NAT64"));
    out
}

// Not reachable on the public internet either way; refusing is fine,
// allowing is harmless.
fn either() -> Vec<Range> {
    let mut out = Vec::new();
    for (cidr, label) in [
        ("192.0.0.0/24", "IETF"),
        ("192.0.2.0/24", "documentation"),
        ("198.18.0.0/15", "benchmarking"),
        ("198.51.100.0/24", "documentation"),
        ("203.0.113.0/24", "documentation"),
        ("224.0.0.0/4", "multicast"),
        ("240.0.0.0/4", "reserved / broadcast"),
    ] {
        out.extend(v4(cidr, label));
    }
    out.push(v6("0064ff9b000000000000000000000000", 96, "NAT64 of a public address"));
    out.push(v6("ff000000000000000000000000000000", 8, "multicast"));
    out.push(v6("20010db8000000000000000000000000", 32, "documentation"));
    out.push(v6("20020000000000000000000000000000", 16, "6to4"));
    out.push(v6("20010000000000000000000000000000", 32, "Teredo"));
    out.push(v6("01000000000000000000000000000000", 64, "discard"));
    out
}

fn in_range(addr: u128, r: &Range) -> bool {
    r.prefix == 0 || addr >> (128 - r.prefix) == r.base >> (128 - r.prefix)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    Allow,
    Either,
    Refuse,
}

fn verdict(addr: u128) -> Verdict {
    if must_refuse().iter().any(|r| in_range(addr, r)) {
        Verdict::Refuse
    } else if either().iter().any(|r| in_range(addr, r)) {
        Verdict::Either
    } else {
        Verdict::Allow
    }
}

// ── spellings ────────────────────────────────────────────────────────────────

fn hextets(addr: u128) -> Vec<String> {
    (0..8).map(|i| format!("{:x}", (addr >> (112 - 16 * i)) & 0xffff)).collect()
}

fn dotted(bits: u128) -> String {
    [24, 16, 8, 0].iter().map(|s| ((bits >> s) & 0xff).to_string()).collect::<Vec<_>>().join(".")
}

// Every way a URL host can name the address. The WHATWG parser normalises
// most of them (decimal, hex and short IPv4; IPv4-mapped IPv6 to hex), which
// is exactly why each must be tried.
fn spellings(addr: u128) -> Vec<String> {
    let h = hextets(addr);
    let mut out = vec![
        format!("[{}]", h.join(":")),
        format!("[{}]", h.join(":").to_uppercase()),
        format!("[{}:{}]", h[..6].join(":"), dotted(addr & 0xffff_ffff)),
    ];
    if addr >> 32 == 0xffff {
        let bits = addr & 0xffff_ffff;
        out.push(dotted(bits));
        out.push(bits.to_string());
        out.push(format!("0x{bits:x}"));
        out.push(format!("[::ffff:{}]", dotted(bits)));
        out.push(format!("[::ffff:{}:{}]", h[6], h[7]));
        let parts: Vec<u128> = dotted(bits).split('.').map(|p| p.parse().unwrap()).collect();
        out.push(format!("{}.{}", parts[0], (parts[1] << 16) | (parts[2] << 8) | parts[3]));
        out.push(format!("0{:o}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3]));
    }
    out
}

fn inside(r: Range) -> impl Strategy<Value = u128> {
    let span = if r.prefix == 0 { u128::MAX } else { (1u128 << (128 - r.prefix)) - 1 };
    (0..=span).prop_map(move |offset| (r.base & !span) | offset)
}

fn address() -> impl Strategy<Value = u128> {
    let refuse = proptest::sample::select(must_refuse()).prop_flat_map(inside);
    let maybe = proptest::sample::select(either()).prop_flat_map(inside);
    prop_oneof![
        4 => refuse,
        2 => maybe,
        2 => (0u128..=0xffff_ffff).prop_map(|bits| V4_MAPPED | bits),
        2 => (0x2u128 << 124)..(0x4u128 << 124), // 2000::/3, global unicast
        1 => any::<u128>(),
    ]
}

fn refused_as_private(message: &str) -> bool {
    message.contains("private/loopback")
}

#[test]
fn refuses_every_spelling_of_a_non_public_address_and_allows_public_ones() {
    let guard = GuardedFetch::new();
    let seen = Mutex::new(BTreeSet::new());
    TestRunner::new(Config::with_cases(property_runs(400)))
        .run(&address(), |addr| {
            let want = verdict(addr);
            seen.lock().unwrap().insert(want);
            for host in spellings(addr) {
                let url = format!("http://{host}:8080/x");
                let outcome = runtime().block_on(guard.assert_public_url(&url));
                let refused = match &outcome {
                    Ok(_) => false,
                    Err(err) if refused_as_private(&err.to_string()) => true,
                    Err(err) => panic!("{url}: unexpected failure {err}"),
                };
                let label =
                    must_refuse().into_iter().chain(either()).find(|r| in_range(addr, r)).map_or("public", |r| r.label);
                if want == Verdict::Refuse && !refused {
                    panic!("{url} ({label}) must be refused, was allowed");
                }
                if want == Verdict::Allow && refused {
                    panic!("{url} ({label}) must be allowed, was refused");
                }
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        seen.into_inner().unwrap().into_iter().collect::<Vec<_>>(),
        [Verdict::Allow, Verdict::Either, Verdict::Refuse]
    );
}

#[test]
fn refuses_names_that_are_not_http_s() {
    let guard = GuardedFetch::new();
    for url in
        ["file:///etc/passwd", "gopher://127.0.0.1/", "ftp://example.com/", "data:text/plain,hi", "javascript:alert(1)"]
    {
        let err = runtime().block_on(guard.assert_public_url(url)).unwrap_err().to_string();
        assert!(err.contains("scheme") || err.contains("invalid URL"), "{url}: {err}");
    }
}

// ── over MCP ─────────────────────────────────────────────────────────────────

// `cuts`: byte offsets where the body is split into separately flushed chunks
// (chunked encoding), so a chunk can end exactly at a cap.
#[derive(Clone)]
struct Route {
    content_type: String,
    body: Vec<u8>,
    cuts: Option<Vec<usize>>,
}

type Routes = Arc<Mutex<HashMap<String, Route>>>;

struct Local {
    port: u16,
    hits: Arc<Mutex<Vec<String>>>,
    routes: Routes,
}

// One server on the dual-stack wildcard, so a request through 127.0.0.1, ::1
// or ::ffff:127.0.0.1 all land here and are counted. A minimal HTTP/1.1:
// one request per connection.
async fn local_server() -> Local {
    let socket = tokio::net::TcpSocket::new_v6().unwrap();
    // Linux and macOS default to dual-stack for a v6 wildcard socket.
    socket.bind("[::]:0".parse().unwrap()).unwrap();
    let listener = socket.listen(64).unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits: Arc<Mutex<Vec<String>>> = Arc::default();
    let routes: Routes = Arc::default();
    let (h, r) = (hits.clone(), routes.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let (hits, routes) = (h.clone(), r.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut read = 0;
                while !buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = stream.read(&mut buf[read..]).await else { return };
                    if n == 0 {
                        return;
                    }
                    read += n;
                }
                let head = String::from_utf8_lossy(&buf[..read]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let local = stream.local_addr().map(|a| a.ip().to_string()).unwrap_or_default();
                hits.lock().unwrap().push(format!("{local} {path}"));
                let route = routes.lock().unwrap().get(&path).cloned();
                let Some(route) = route else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .await;
                    return;
                };
                match route.cuts {
                    None => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            route.content_type,
                            route.body.len()
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(&route.body).await;
                    }
                    Some(cuts) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                            route.content_type
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let mut from = 0;
                        for cut in cuts.into_iter().chain([route.body.len()]) {
                            if cut <= from || cut > route.body.len() {
                                continue;
                            }
                            let chunk = &route.body[from..cut];
                            let _ = stream.write_all(format!("{:x}\r\n", chunk.len()).as_bytes()).await;
                            let _ = stream.write_all(chunk).await;
                            let _ = stream.write_all(b"\r\n").await;
                            let _ = stream.flush().await;
                            from = cut;
                            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                        }
                        let _ = stream.write_all(b"0\r\n\r\n").await;
                    }
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    Local { port, hits, routes }
}

struct Env {
    _db: TestDatabase,
    local: Local,
    _guarded: Server,
    _open: Server,
    guarded_client: Client,
    open_client: Client,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        runtime().block_on(async {
            let db = support::test_database().await;
            let local = local_server().await;
            let guarded = start_test_server(&db, Some("fetch")).await.expect("server starts");
            // Only 127.0.0.1 counts as public: the local server plays the internet.
            let open_rule = Arc::new(GuardedFetch::with_rule(Arc::new(|ip| ip.to_string() != "127.0.0.1")));
            let open = start_test_server_with_fetch(&db, Some("fetch"), open_rule).await.expect("server starts");
            let guarded_client = connect(&guarded.url).await;
            let open_client = connect(&open.url).await;
            Env { _db: db, local, _guarded: guarded, _open: open, guarded_client, open_client }
        })
    })
}

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn no_spelling_of_the_boxs_own_address_reaches_it() {
    let _serial = serial();
    let env = env();
    env.local
        .routes
        .lock()
        .unwrap()
        .insert("/secret".into(), Route { content_type: "text/plain".into(), body: b"internal".to_vec(), cuts: None });
    let loopback = prop_oneof![
        (0x7f00_0000u128..=0x7fff_ffff).prop_map(|bits| V4_MAPPED | bits),
        proptest::sample::select(vec![1u128, 0, V4_MAPPED]),
    ];
    TestRunner::new(Config::with_cases(60))
        .run(&loopback, |addr| {
            for host in spellings(addr) {
                let url = format!("http://{host}:{}/secret", env.local.port);
                let outcome = runtime().block_on(call(&env.guarded_client, "fetch_url", json!({ "url": url })));
                match outcome {
                    Outcome::ToolError(message) if refused_as_private(&message) => {}
                    other => panic!("{host}: expected a private-address refusal, got {other:?}"),
                }
            }
            Ok(())
        })
        .unwrap();
    // Only this test asks for /secret; the others share the server, in any order.
    let reached: Vec<String> =
        env.local.hits.lock().unwrap().iter().filter(|h| h.ends_with(" /secret")).cloned().collect();
    assert_eq!(reached, Vec::<String>::new());
}

// The cap is on bytes; the content is the longest whole-character prefix of
// the body within it, and `truncated` says whether anything was cut.
#[test]
fn returns_a_body_whole_or_its_longest_prefix_within_max_bytes() {
    let _serial = serial();
    let env = env();
    // fast-check's grapheme strings: any characters, multi-byte ones included.
    let text = "\\PC{0,300}";
    TestRunner::new(Config::with_cases(150))
        .run(&(text, 1usize..=1500, any::<bool>()), |(text, cap, chunked)| {
            let body = text.as_bytes().to_vec();
            // Chunked: one chunk boundary exactly at the cap, one just before.
            let cuts = chunked.then(|| vec![cap - 1, cap]);
            env.local.routes.lock().unwrap().insert(
                "/page".into(),
                Route { content_type: "text/plain; charset=utf-8".into(), body: body.clone(), cuts },
            );
            let url = format!("http://127.0.0.1:{}/page", env.local.port);
            let outcome =
                runtime().block_on(call(&env.open_client, "fetch_url", json!({ "url": url, "maxBytes": cap })));
            let Outcome::Ok(reply) = outcome else { panic!("fetch_url failed: {outcome:?}") };
            assert!(reply.get("binary").is_none(), "text came back as binary");
            let content = reply["content"].as_str().expect("content");
            let got = content.len();
            if body.len() <= cap {
                assert_eq!(content, text, "fits: whole body");
                assert_eq!(reply["truncated"], false, "fits: not truncated");
                assert_eq!(reply["bytes"], json!(body.len()));
                assert_eq!(reply["status"], 200);
            } else {
                assert_eq!(reply["truncated"], true, "longer than the cap: truncated");
                assert!(
                    text.starts_with(content),
                    "a prefix of the body: {:?}",
                    &content[content.len().saturating_sub(5)..]
                );
                assert!(got <= cap, "within the cap");
                assert!(got + 4 > cap, "the longest such prefix");
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn returns_text_types_as_text_and_binary_types_as_a_note() {
    let _serial = serial();
    let env = env();
    let text = [
        "text/html; charset=utf-8",
        "text/csv",
        "application/json",
        "application/xml",
        "application/ld+json",
        "application/javascript",
    ];
    let binary = [
        "image/png",
        "audio/mpeg",
        "video/mp4",
        "application/pdf",
        "application/zip",
        "application/octet-stream",
        "application/x-gzip",
    ];
    for content_type in text.iter().chain(&binary) {
        env.local.routes.lock().unwrap().insert(
            "/typed".into(),
            Route { content_type: (*content_type).into(), body: b"payload".to_vec(), cuts: None },
        );
        let url = format!("http://127.0.0.1:{}/typed", env.local.port);
        let outcome = runtime().block_on(call(&env.open_client, "fetch_url", json!({ "url": url })));
        let Outcome::Ok(reply) = outcome else { panic!("{content_type}: {outcome:?}") };
        assert_eq!(reply.get("binary") == Some(&Value::Bool(true)), binary.contains(content_type), "{content_type}");
    }
}
