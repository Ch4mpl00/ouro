//! The repository README's quickstart, compiled and run by the checks.
use ouro_runtime::{
    Config, Function, FunctionContext, Registry, Runtime,
    durable::{self, memory::memory},
    ows::run::Status,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const YAML: &str = r#"
document: { dsl: '1.0.4', namespace: example, name: greet, version: '1.0.0' }
do:
  - hello: { call: greet, with: { name: '${ .name }' } }
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut registry = Registry::new();
    registry.register(Function::new(
        "greet",
        json!({ "type": "object", "required": ["name"], "properties": { "name": { "type": "string" } } }),
        json!({ "type": "object", "required": ["text"] }),
        |args: Value, _cx: FunctionContext| async move {
            Ok(json!({ "text": format!("Hello, {}!", args["name"].as_str().unwrap_or("world")) }))
        },
    ))?;

    let runtime = Runtime::new(
        Arc::new(memory()), // or durable::sqlite::open("runs.sqlite")?
        Arc::new(registry),
        Config::default(),
        Arc::new(durable::SystemClock),
        ouro_runtime::opentelemetry::global::tracer("quickstart"),
    )?;
    let definition = runtime.save("greet", YAML).await?;
    runtime.activate(&definition.name, definition.version).await?;

    runtime.start("run-1", "greet", json!({ "name": "ouro" })).await?;
    let status = runtime.drive("run-1", &CancellationToken::new()).await?;
    assert_eq!(status, Some(Status::Completed(json!({ "text": "Hello, ouro!" }))));
    Ok(())
}
