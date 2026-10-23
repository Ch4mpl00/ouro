//! Embeddable durable workflow host. Register functions, save and activate
//! definitions, admit runs, then drive or recover them with a bounded worker.
mod execution;
mod registry;
mod telemetry;
mod testing;
pub use execution::{Config, Runtime};
pub use opentelemetry;
#[cfg(feature = "otlp")]
pub use opentelemetry_otlp as otlp;
pub use ouro_durable as durable;
pub use owf;
pub use registry::{Effect, Function, FunctionContext, FunctionError, Handler, Registry};
pub use testing::{DryRun, Mock, RecordedCall, Replay};
