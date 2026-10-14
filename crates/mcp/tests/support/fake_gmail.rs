// A stand-in for the Gmail API on a loopback port, for the real client
// pointed at it with `GmailModule::with_api_root`. A port of
// packages/mcp/test/support/fake-gmail.ts.
//
// Behaves like Gmail where the poller cares:
//   - messages.list answers the mails matching the query — a mail says
//     whether the subscription query matches it — honours `after:<seconds>`
//     non-strictly, newest first, `maxResults` per page with a
//     nextPageToken;
//   - messages.get answers metadata headers or the full payload, with one
//     part per attachment;
//   - `fail_get(id, times)` makes the next `times` gets of a mail fail with a
//     500;
//   - attachments.get answers an attachment's bytes (`content`, or a default
//     derived from its id) — or, with `lenient`, any attachment path at all,
//     as an API that doesn't reject a malformed id would.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{StatusCode, Uri};
use base64::Engine;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct FakeAttachment {
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub content: Option<String>,
    // Inside a multipart/mixed part of its own, as forwarded and client-built
    // mails put it.
    pub nested: bool,
}

#[derive(Debug, Clone)]
pub struct FakeMail {
    pub id: String,
    pub internal_date: i64,
    pub subject: String,
    pub matches: bool,
    pub attachments: Vec<FakeAttachment>,
}

pub fn attachment_bytes(attachment_id: &str, content: Option<&str>) -> String {
    content.map_or_else(|| format!("%PDF bytes of {attachment_id}"), str::to_owned)
}

#[derive(Default)]
pub struct FakeGmailState {
    pub mails: Vec<FakeMail>,
    // Every list call's query, in order.
    pub queries: Vec<String>,
    failures: HashMap<String, usize>,
    // Answer attachments.get for any path, ids unchecked.
    pub lenient: bool,
}

#[derive(Clone)]
pub struct FakeGmail {
    pub root_url: String,
    pub state: Arc<Mutex<FakeGmailState>>,
}

impl FakeGmail {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, FakeGmailState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    // An empty Gmail: no mails, no pending failures. (Ids repeat across
    // generated cases, so a failure left pending by one must not hit the
    // same id in the next.)
    pub fn reset(&self) {
        let mut s = self.lock();
        s.mails.clear();
        s.queries.clear();
        s.failures.clear();
        s.lenient = false;
    }

    pub fn fail_get(&self, id: &str, times: usize) {
        *self.lock().failures.entry(id.to_owned()).or_default() += times;
    }
}

fn reply(status: u16, body: Value) -> (StatusCode, Json<Value>) {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(body))
}

fn not_found(message: &str) -> (StatusCode, Json<Value>) {
    reply(404, json!({ "error": { "code": 404, "message": message } }))
}

fn decode(segment: &str) -> String {
    url::form_urlencoded::parse(format!("x={segment}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

async fn handle(
    State(state): State<Arc<Mutex<FakeGmailState>>>,
    uri: Uri,
    Query(params): Query<Vec<(String, String)>>,
) -> (StatusCode, Json<Value>) {
    let param = |name: &str| params.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    let path = uri.path().to_owned();
    let Some(rest) = path.strip_prefix("/gmail/v1/users/me/") else { return not_found(&format!("no route {path}")) };
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["messages"] => {
            let q = param("q").unwrap_or_default();
            s.queries.push(q.clone());
            let after_ms = regex::Regex::new(r"after:(\d+)")
                .unwrap()
                .captures(&q)
                .map_or(i64::MIN, |c| c[1].parse::<i64>().unwrap() * 1000);
            let max: usize = param("maxResults").and_then(|v| v.parse().ok()).unwrap_or(100);
            let offset: usize = param("pageToken").and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut hits: Vec<&FakeMail> =
                s.mails.iter().filter(|m| m.matches && m.internal_date >= after_ms).collect();
            hits.sort_by_key(|m| std::cmp::Reverse(m.internal_date));
            let page: Vec<Value> = hits
                .iter()
                .skip(offset)
                .take(max)
                .map(|m| json!({ "id": m.id, "threadId": format!("t-{}", m.id) }))
                .collect();
            let mut body = json!({ "messages": page, "resultSizeEstimate": hits.len() });
            if offset + max < hits.len() {
                body["nextPageToken"] = json!((offset + max).to_string());
            }
            reply(200, body)
        }
        ["messages", mid, "attachments", aid] => {
            let (mid, aid) = (decode(mid), decode(aid));
            let found = s
                .mails
                .iter()
                .find(|m| m.id == mid)
                .and_then(|m| m.attachments.iter().find(|a| a.attachment_id == aid))
                .map(|a| attachment_bytes(&a.attachment_id, a.content.as_deref()));
            let bytes = found.or_else(|| s.lenient.then(|| format!("%PDF lenient {mid}/{aid}")));
            let Some(bytes) = bytes else { return not_found("Requested entity was not found.") };
            reply(
                200,
                json!({ "size": bytes.len(), "data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes) }),
            )
        }
        ["messages", id] => {
            let id = decode(id);
            let Some(mail) = s.mails.iter().find(|m| m.id == id).cloned() else {
                return not_found("Requested entity was not found.");
            };
            let left = s.failures.get(&id).copied().unwrap_or(0);
            if left > 0 {
                s.failures.insert(id, left - 1);
                return reply(500, json!({ "error": { "code": 500, "message": "Backend Error" } }));
            }
            let date = chrono::DateTime::from_timestamp_millis(mail.internal_date).unwrap().to_rfc2822();
            let mut parts = vec![
                json!({ "partId": "0", "mimeType": "text/plain", "filename": "", "body": { "size": 5, "data": "aGVsbG8" } }),
            ];
            for (i, a) in mail.attachments.iter().enumerate() {
                let part = json!({ "partId": (i + 1).to_string(), "mimeType": a.mime_type, "filename": a.filename, "body": { "attachmentId": a.attachment_id, "size": a.size } });
                parts.push(if a.nested {
                    let mut inner = part.clone();
                    inner["partId"] = json!(format!("{}.1", i + 1));
                    json!({ "partId": (i + 1).to_string(), "mimeType": "multipart/mixed", "filename": "", "body": { "size": 0 }, "parts": [inner] })
                } else {
                    part
                });
            }
            reply(
                200,
                json!({
                    "id": mail.id,
                    "threadId": format!("t-{}", mail.id),
                    "snippet": format!("snippet of {}", mail.subject),
                    "internalDate": mail.internal_date.to_string(),
                    "labelIds": ["INBOX"],
                    "payload": {
                        "mimeType": "multipart/mixed",
                        "headers": [
                            { "name": "From", "value": "NashDom <bills@nashdom.example>" },
                            { "name": "To", "value": "me@example.com" },
                            { "name": "Subject", "value": mail.subject },
                            { "name": "Date", "value": date },
                        ],
                        "parts": parts,
                    },
                }),
            )
        }
        _ => not_found(&format!("no route GET {path}")),
    }
}

pub async fn start() -> FakeGmail {
    let state: Arc<Mutex<FakeGmailState>> = Arc::default();
    let app = axum::Router::new().fallback(axum::routing::get(handle)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    FakeGmail { root_url: format!("http://127.0.0.1:{port}/"), state }
}
