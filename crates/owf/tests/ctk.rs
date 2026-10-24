// The official conformance kit (tests/ctk/features, vendored unmodified, see
// tests/ctk/README.md) run with cucumber against the interpreter: 1. the
// test host, durable through an in-memory journal; 2. a local fake of the
// services the scenarios call (the petstore, an OpenAPI document, httpbin),
// reached through `http` and `openapi` functions the way a protocol pack
// would, never over the network; 3. the world and the kit's steps; 4. main.
//
// Every scenario is in scope: protocol calls are host functions, and here
// they are local fakes.
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cucumber::gherkin::Step;
use cucumber::{World, given, then, when};
use owf::durable::{Journal, Record};
use owf::model;
use owf::run::{self, CallInfo, Clock, Event, Functions, Host, Limits, Observer, Status, WorkflowError};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

// ── 1. the host ────────────────────────────────────────────────────────────

#[derive(Default)]
struct Memory(Mutex<BTreeMap<String, Record>>);
impl Journal for Memory {
    fn load(&self) -> anyhow::Result<BTreeMap<String, Record>> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn save(&self, key: &str, record: &Record) -> anyhow::Result<()> {
        self.0.lock().unwrap().insert(key.into(), record.clone());
        Ok(())
    }
}

/// Task names in the order they started.
#[derive(Default)]
struct Starts(Mutex<Vec<String>>);
impl Observer for Starts {
    fn event(&self, event: &Event<'_>) {
        if let Event::TaskStarted { reference, .. } = event {
            let name = reference.rsplit('/').next().unwrap_or_default();
            let name = name.split('[').next().unwrap_or(name);
            self.0.lock().unwrap().push(name.to_owned());
        }
    }
}

struct Now;
impl Clock for Now {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

struct Services;
#[async_trait]
impl Functions for Services {
    fn has(&self, name: &str) -> bool {
        matches!(name, "http" | "openapi")
    }
    fn emits(&self) -> bool {
        true
    }
    async fn emit(&self, _: &Value, _: &CallInfo<'_>) -> Result<(), owf::durable::CallFailure> {
        Ok(())
    }
    async fn call(&self, name: &str, args: Value, info: &CallInfo<'_>) -> Result<Value, WorkflowError> {
        match name {
            "http" => http(&args, info.input),
            _ => openapi(&args),
        }
    }
}

// ── 2. the local services ──────────────────────────────────────────────────

fn pet(id: i64) -> Value {
    json!({ "id": id, "name": format!("pet-{id}"), "status": "available" })
}

/// The fake origin: (status, body) for a method and absolute URL.
fn serve(method: &str, url: &str, auth: Option<(&str, &str)>) -> (u16, Value) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let path = path.trim_start_matches("https://").trim_start_matches("http://");
    let segments: Vec<&str> = path.split('/').collect();
    match (method, segments.as_slice()) {
        ("GET", ["petstore.swagger.io", "v2", "swagger.json"]) => (200, swagger()),
        ("GET", ["petstore.swagger.io", "v2", "pet", "findByStatus"]) => {
            let available = query.split('&').any(|p| p == "status=available");
            (200, if available { json!([pet(1), pet(2)]) } else { json!([]) })
        }
        ("GET", ["petstore.swagger.io", "v2", "pet", id]) => match id.parse::<i64>() {
            Ok(id) => (200, pet(id)),
            Err(_) => (404, json!({ "message": "Pet not found" })),
        },
        ("GET", ["petstore.swagger.io", "v2", "pet", "getPetByName", _]) => {
            (404, json!({ "message": "Pet not found" }))
        }
        ("GET", ["httpbin.org", "basic-auth", user, password]) => match auth {
            Some((u, p)) if u == *user && p == *password => (200, json!({ "authenticated": true, "user": user })),
            _ => (401, json!({ "authenticated": false })),
        },
        _ => (404, json!({ "message": "no such resource" })),
    }
}

fn swagger() -> Value {
    json!({
        "swagger": "2.0",
        "host": "petstore.swagger.io",
        "basePath": "/v2",
        "schemes": ["https"],
        "paths": {
            "/pet/findByStatus": { "get": { "operationId": "findPetsByStatus", "parameters": [ { "name": "status", "in": "query" } ] } },
            "/pet/{petId}": { "get": { "operationId": "getPetById", "parameters": [ { "name": "petId", "in": "path" } ] } }
        }
    })
}

/// `{name}` placeholders filled from the task input's top-level properties.
fn expand(template: &str, input: &Value) -> String {
    let mut out = template.to_owned();
    if let Value::Object(m) = input {
        for (k, v) in m {
            let text = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
            out = out.replace(&format!("{{{k}}}"), &text);
        }
    }
    out
}

fn respond(method: &str, url: &str, auth: Option<(&str, &str)>, output: &str) -> Result<Value, WorkflowError> {
    let (status, body) = serve(method, url, auth);
    if status >= 400 {
        return Err(WorkflowError::communication(status, format!("{method} {url}: {body}")));
    }
    Ok(match output {
        "response" => json!({
            "request": { "method": method, "uri": url, "headers": {} },
            "headers": { "Content-Type": "application/json" },
            "statusCode": status,
            "content": body,
        }),
        "raw" => Value::String(body.to_string()),
        _ => body,
    })
}

/// `call: http`, as a protocol pack implements it.
fn http(args: &Value, input: &Value) -> Result<Value, WorkflowError> {
    let method = args["method"].as_str().unwrap_or("get").to_uppercase();
    let endpoint = &args["endpoint"];
    let uri = endpoint.as_str().or_else(|| endpoint["uri"].as_str()).unwrap_or_default();
    let basic = &endpoint["authentication"]["basic"];
    let auth = basic["username"].as_str().zip(basic["password"].as_str());
    respond(&method, &expand(uri, input), auth, args["output"].as_str().unwrap_or("content"))
}

/// `call: openapi`: the document from the fake origin, then the operation.
fn openapi(args: &Value) -> Result<Value, WorkflowError> {
    let document_uri = args["document"]["endpoint"].as_str().unwrap_or_default();
    let (_, document) = serve("GET", document_uri, None);
    let id = args["operationId"].as_str().unwrap_or_default();
    let parameters = args["parameters"].as_object().cloned().unwrap_or_default();
    for (path, operations) in document["paths"].as_object().into_iter().flatten() {
        for (method, operation) in operations.as_object().into_iter().flatten() {
            if operation["operationId"] != id {
                continue;
            }
            let mut url = format!(
                "https://{}{}{}",
                document["host"].as_str().unwrap_or(""),
                document["basePath"].as_str().unwrap_or(""),
                path
            );
            let mut query = Vec::new();
            for p in operation["parameters"].as_array().into_iter().flatten() {
                let name = p["name"].as_str().unwrap_or_default();
                let value = parameters.get(name).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned));
                match (p["in"].as_str(), value) {
                    (Some("path"), Some(v)) => url = url.replace(&format!("{{{name}}}"), &v),
                    (Some("query"), Some(v)) => query.push(format!("{name}={v}")),
                    _ => {}
                }
            }
            if !query.is_empty() {
                url = format!("{url}?{}", query.join("&"));
            }
            return respond(&method.to_uppercase(), &url, None, args["output"].as_str().unwrap_or("content"));
        }
    }
    Err(WorkflowError::communication(404, format!("no operation {id}")))
}

// ── 3. the world and the steps ─────────────────────────────────────────────

#[derive(Debug, Default, World)]
struct Ctk {
    definition: String,
    input: Value,
    status: Option<Status>,
    starts: Vec<String>,
}

/// A step's docstring without its content type line (`"""yaml`).
fn docstring(step: &Step) -> &str {
    let text = step.docstring.as_deref().expect("this step needs a docstring").trim_start_matches('\n');
    match text.split_once('\n') {
        Some((kind, rest)) if matches!(kind.trim(), "yaml" | "json") => rest,
        _ => text,
    }
}

fn yaml(text: &str) -> Value {
    serde_yaml_ng::from_str(text).unwrap_or_else(|e| panic!("not YAML: {e}\n{text}"))
}

impl Ctk {
    fn output(&self) -> &Value {
        match &self.status {
            Some(Status::Completed(v)) => v,
            other => panic!("the workflow did not complete: {other:?}"),
        }
    }
    fn position(&self, task: &str) -> usize {
        self.starts.iter().position(|t| t == task).unwrap_or_else(|| panic!("{task} never ran: {:?}", self.starts))
    }
}

/// `a.b.0` in a value.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |v, key| match v {
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        _ => v.get(key),
    })
}

#[given("a workflow with definition:")]
fn a_workflow(world: &mut Ctk, step: &Step) {
    world.definition = docstring(step).to_owned();
}

#[given("given the workflow input is:")]
fn the_input(world: &mut Ctk, step: &Step) {
    world.input = yaml(docstring(step));
}

#[when("the workflow is executed")]
async fn executed(world: &mut Ctk) {
    let wf = model::parse_yaml(&world.definition).unwrap_or_else(|e| panic!("{e}"));
    let starts = Arc::new(Starts::default());
    let host = Host {
        functions: Arc::new(Services),
        observer: starts.clone(),
        clock: Arc::new(Now),
        limits: Limits::default(),
        runtime: json!({ "name": "owf-ctk", "version": env!("CARGO_PKG_VERSION"), "metadata": {} }),
    };
    run::validate(&wf, &*host.functions).unwrap_or_else(|e| panic!("{e}"));
    let journal = Memory::default();
    let input = if world.input.is_null() { Value::Object(Map::new()) } else { world.input.clone() };
    let mut status = None;
    for _ in 0..10 {
        let outcome = run::resume(&host, &wf, "ctk", input.clone(), &CancellationToken::new(), &journal).await;
        if !matches!(outcome.status, Status::Waiting { .. } | Status::Interrupted(_)) {
            status = Some(outcome.status);
            break;
        }
    }
    world.status = status;
    world.starts = starts.0.lock().unwrap().clone();
}

#[then("the workflow should complete")]
fn completes(world: &mut Ctk) {
    world.output();
}

#[then("the workflow should complete with output:")]
fn completes_with(world: &mut Ctk, step: &Step) {
    assert_eq!(world.output(), &yaml(docstring(step)));
}

#[then("the workflow should fault")]
fn faults(world: &mut Ctk) {
    assert!(matches!(world.status, Some(Status::Faulted(_))), "{:?}", world.status);
}

#[then("the workflow should fault with error:")]
fn faults_with(world: &mut Ctk, step: &Step) {
    let Some(Status::Faulted(e)) = &world.status else { panic!("did not fault: {:?}", world.status) };
    let expected = yaml(docstring(step));
    let actual = serde_json::to_value(e).unwrap();
    for (k, v) in expected.as_object().expect("an error is an object") {
        assert_eq!(actual.get(k), Some(v), "{k} of {actual}");
    }
}

#[then("the workflow should cancel")]
fn cancels(world: &mut Ctk) {
    assert!(matches!(world.status, Some(Status::Cancelled)), "{:?}", world.status);
}

#[then(regex = r"^the workflow output should have properties (.+)$")]
fn has_properties(world: &mut Ctk, list: String) {
    for path in list.split(',').map(|p| p.trim().trim_matches('\'')) {
        assert!(at(world.output(), path).is_some(), "no {path} in {}", world.output());
    }
}

#[then(regex = r"^the workflow output should have a '([^']+)' property with value:$")]
fn has_value(world: &mut Ctk, path: String, step: &Step) {
    assert_eq!(at(world.output(), &path), Some(&yaml(docstring(step))), "{path} in {}", world.output());
}

#[then(regex = r"^the workflow output should have a '([^']+)' property containing (\d+) items$")]
fn has_items(world: &mut Ctk, path: String, count: usize) {
    let items = at(world.output(), &path).and_then(Value::as_array).map(Vec::len);
    assert_eq!(items, Some(count), "{path} in {}", world.output());
}

#[then(regex = r"^(\w+) should run first$")]
fn runs_first(world: &mut Ctk, task: String) {
    assert_eq!(world.starts.first(), Some(&task), "{:?}", world.starts);
}

#[then(regex = r"^(\w+) should run last$")]
fn runs_last(world: &mut Ctk, task: String) {
    assert_eq!(world.starts.last(), Some(&task), "{:?}", world.starts);
}

#[then(regex = r"^(\w+) should run after (\w+)$")]
fn runs_after(world: &mut Ctk, later: String, earlier: String) {
    assert!(world.position(&later) > world.position(&earlier), "{:?}", world.starts);
}

#[then(regex = r"^(\w+) should run before (\w+)$")]
fn runs_before(world: &mut Ctk, earlier: String, later: String) {
    assert!(world.position(&earlier) < world.position(&later), "{:?}", world.starts);
}

// ── 4. main ────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    use cucumber::writer::Stats as _;
    let result = Ctk::cucumber()
        // A step nobody taught the harness fails: the kit never passes on
        // sentences that check nothing.
        .fail_on_skipped()
        .run(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ctk/features"))
        .await;
    if result.execution_has_failed() || result.parsing_errors() > 0 {
        std::process::exit(1);
    }
}
