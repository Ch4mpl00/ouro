// Runtime library for the Rust agent port. Clients and stores are injected
// into the engine; TS judge / improver tooling reads the same agent.db and
// canonical Langfuse trace shapes. Workflow and supervisor wiring are tracked
// in .claude/tasks/rust-agent.md.

pub mod agent;
pub mod codex;
pub mod db;
pub mod decisions;
pub mod errors;
pub mod generation;
pub mod langfuse;
pub mod local_recorder;
pub mod mcp;
pub mod models;
pub mod providers;
pub mod session;
pub mod skills;
pub mod tool_results;
pub mod trace_model;
pub mod tracing;
