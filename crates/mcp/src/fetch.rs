// fetch_url: GET a page and return its RAW body (HTML / JSON / text) — for
// parsing a <table> in a code_agent step, or a URL the search provider can't
// retrieve. Complements tavily_extract, which returns cleaned prose.
//
// Sections:
//   1. ssrf guard — only public addresses, enforced at DNS resolution
//   2. client     — the guarded HTTP client
//   3. tools      — `fetch` toolset (never handed to third-party clients:
//                   it would make this server their proxy)
//
// The guard is stricter than the TS one it replaces: the check lives in the
// client's resolver, so every connection — including each redirect hop and a
// DNS answer that changed since validation — is checked, not just the URL
// the model typed.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::server::{McpTools, ToolResult, invalid_params, respond};

// ── 1. ssrf guard ────────────────────────────────────────────────────────────

// Loopback, RFC1918, link-local / cloud metadata (169.254.169.254), the
// unspecified address, IPv6 loopback / link-local / unique-local, and
// IPv4-mapped forms of all of those.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 0
                || a == 10
                || a == 127
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback() || v6.is_unspecified() || (first & 0xffc0) == 0xfe80 || (first & 0xfe00) == 0xfc00
        }
    }
}

fn host_ip(url: &reqwest::Url) -> Option<IpAddr> {
    match url.host()? {
        url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        url::Host::Domain(_) => None,
    }
}

pub async fn assert_public_url(raw: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid URL: {raw}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        anyhow::bail!("unsupported scheme \"{}:\" — only http/https", url.scheme());
    }
    let host = url.host_str().unwrap_or_default().to_owned();
    let addrs: Vec<IpAddr> = match host_ip(&url) {
        Some(ip) => vec![ip],
        None => tokio::net::lookup_host((host.as_str(), 0))
            .await
            .map_err(|_| anyhow::anyhow!("could not resolve host \"{host}\""))?
            .map(|a| a.ip())
            .collect(),
    };
    if let Some(ip) = addrs.into_iter().find(|ip| is_private_ip(*ip)) {
        anyhow::bail!("refusing to fetch private/loopback address ({host} → {ip})");
    }
    Ok(url)
}

struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if let Some(bad) = addrs.iter().find(|a| is_private_ip(a.ip())) {
                let err = format!("refusing to fetch private/loopback address ({host} → {})", bad.ip());
                return Err(err.into());
            }
            let iter: Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

// ── 2. client ────────────────────────────────────────────────────────────────

const DEFAULT_MAX_BYTES: usize = 2_000_000;
const HARD_MAX_BYTES: usize = 8_000_000;
const TIMEOUT: Duration = Duration::from_secs(20);
// Some sites 403 a missing/unknown User-Agent.
pub const BROWSER_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

pub fn guarded_client() -> reqwest::Client {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(PublicOnlyResolver))
        // Literal-IP redirect targets skip the resolver; vet them here.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url();
            if url.scheme() != "http" && url.scheme() != "https" {
                return attempt.error("redirect to a non-http(s) URL");
            }
            if host_ip(url).is_some_and(is_private_ip) {
                return attempt.error("redirect to a private/loopback address");
            }
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            attempt.follow()
        }))
        .user_agent(BROWSER_UA)
        .timeout(TIMEOUT)
        .build()
        .expect("static client config is valid")
}

fn is_binary(content_type: &str) -> bool {
    let re =
        regex::Regex::new(r"(?i)^(image|audio|video)/|application/(pdf|zip|octet-stream|x-)").expect("valid regex");
    re.is_match(content_type)
}

// ── 3. tools ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FetchParams {
    /// Absolute http(s) URL.
    url: String,
    /// Cap on bytes returned (default 2000000).
    #[schemars(range(min = 1, max = 8_000_000))]
    max_bytes: Option<usize>,
}

#[tool_router(router = fetch_tools, vis = "pub(crate)")]
impl McpTools {
    #[tool(
        name = "fetch_url",
        title = "Fetch a URL's raw content",
        description = "GET a URL and return its RAW body (HTML / JSON / text). Use when you \
            already have a link and need the raw content — e.g. the raw HTML to \
            parse a <table> in a code_agent step, or a page the search/extract \
            provider can't retrieve. Complements tavily_extract (which returns \
            cleaned prose). Follows redirects; binary content (PDF/image/zip) is \
            not returned — use read_pdf for PDFs."
    )]
    async fn fetch_url(&self, Parameters(p): Parameters<FetchParams>) -> ToolResult {
        if p.max_bytes.is_some_and(|m| m == 0 || m > HARD_MAX_BYTES) {
            return Err(invalid_params("maxBytes must be between 1 and 8000000"));
        }
        let cap = p.max_bytes.unwrap_or(DEFAULT_MAX_BYTES).min(HARD_MAX_BYTES);
        let client = self.deps.fetcher.clone();
        respond(
            async {
                let target = assert_public_url(&p.url).await?;
                let res = client.get(target).header("accept", "*/*").send().await.map_err(|err| {
                    let reason = if err.is_timeout() {
                        format!("timed out after {}ms", TIMEOUT.as_millis())
                    } else {
                        format!("{err:#}")
                    };
                    anyhow::anyhow!("fetch failed: {reason}")
                })?;
                let final_url = res.url().to_string();
                let status = res.status().as_u16();
                let content_type = res
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                if is_binary(&content_type) {
                    return Ok(json!({
                        "url": final_url,
                        "status": status,
                        "contentType": content_type,
                        "binary": true,
                        "note": "binary content not returned; for a PDF use read_pdf",
                    }));
                }
                let mut body = Vec::new();
                let mut total = 0usize;
                let mut truncated = false;
                let mut stream = res.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    total += chunk.len();
                    body.extend_from_slice(&chunk);
                    if total >= cap {
                        truncated = true;
                        break;
                    }
                }
                body.truncate(cap);
                Ok(json!({
                    "url": final_url,
                    "status": status,
                    "contentType": content_type,
                    "bytes": total,
                    "truncated": truncated,
                    "content": String::from_utf8_lossy(&body),
                }))
            }
            .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private(ip: &str) -> bool {
        is_private_ip(ip.parse().unwrap())
    }

    #[test]
    fn flags_loopback_rfc1918_link_local_and_metadata() {
        for ip in ["127.0.0.1", "10.0.0.5", "192.168.1.1", "172.16.0.1", "172.31.255.255", "169.254.169.254", "0.0.0.0"]
        {
            assert!(private(ip), "{ip}");
        }
        for ip in ["::1", "fe80::1", "fc00::1", "fd12:3456::1", "::ffff:10.0.0.1"] {
            assert!(private(ip), "{ip}");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for ip in ["8.8.8.8", "1.1.1.1", "172.15.0.1", "172.32.0.1", "2606:4700::1"] {
            assert!(!private(ip), "{ip}");
        }
    }

    #[tokio::test]
    async fn rejects_bad_schemes_private_hosts_and_garbage() {
        let msg = |r: anyhow::Result<reqwest::Url>| r.unwrap_err().to_string();
        assert!(msg(assert_public_url("file:///etc/passwd").await).contains("scheme"));
        assert!(msg(assert_public_url("ftp://example.com").await).contains("scheme"));
        assert!(msg(assert_public_url("http://169.254.169.254/latest/meta-data/").await).contains("private/loopback"));
        assert!(msg(assert_public_url("http://127.0.0.1:8080/").await).contains("private/loopback"));
        assert!(msg(assert_public_url("http://localhost/admin").await).contains("private/loopback"));
        assert!(msg(assert_public_url("not a url").await).contains("invalid URL"));
    }

    #[test]
    fn binary_content_types_match_like_the_ts_regex() {
        assert!(is_binary("application/pdf"));
        assert!(is_binary("IMAGE/png"));
        assert!(is_binary("application/x-tar"));
        assert!(!is_binary("text/html; charset=utf-8"));
        assert!(!is_binary("application/json"));
    }
}
