//! Embeddable durable workflow host. Register functions, save and activate
//! definitions, admit runs, then drive or recover them with a bounded worker.
mod capabilities;
pub mod control;
mod execution;
mod registry;
mod schedule;
mod telemetry;
mod testing;
pub use capabilities::{EventSink, ProcessRunner, ResourceResolver, SecretResolver};
pub use execution::{Config, InvalidDefinition, ResourceUnavailable, Runtime};
pub use opentelemetry;
#[cfg(feature = "otlp")]
pub use opentelemetry_otlp as otlp;
pub use ouro_durable as durable;
pub use ows;
pub use ows::durable::{CallEntry, CallStatus};
pub use registry::{Delivery, Function, FunctionContext, FunctionError, Handler, MissingFunction, Registry};
pub use testing::{DryRun, Mock, RecordedCall, Replay, mock};
