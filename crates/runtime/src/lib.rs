//! Embeddable durable workflow host. Register functions, save and activate
//! definitions, admit runs, then drive or recover them with a bounded worker.
mod capabilities;
mod execution;
mod registry;
mod schedule;
mod telemetry;
mod testing;
pub use capabilities::{EventSink, ProcessRunner, ResourceResolver, SecretResolver};
pub use execution::{Config, Runtime};
pub use opentelemetry;
#[cfg(feature = "otlp")]
pub use opentelemetry_otlp as otlp;
pub use ouro_durable as durable;
pub use owf;
pub use owf::durable::{CallEntry, CallStatus};
pub use registry::{Delivery, Function, FunctionContext, FunctionError, Handler, Registry};
pub use testing::{DryRun, Mock, RecordedCall, Replay, mock};
