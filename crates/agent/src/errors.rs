// Cancellation. Where the TS agent threads an AbortSignal, this one threads a
// CancellationToken; an operation cut short fails with `Aborted`, which the
// supervisor tells apart from a real failure (no recovery report for it).

use std::future::Future;

use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Aborted(pub String);

pub fn aborted() -> anyhow::Error {
    Aborted("This operation was aborted".into()).into()
}

pub fn is_aborted(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<Aborted>())
}

// Fails with `Aborted` if the token is already cancelled.
pub fn check(cancel: &CancellationToken) -> anyhow::Result<()> {
    if cancel.is_cancelled() { Err(aborted()) } else { Ok(()) }
}

// Runs `fut` until it completes or `cancel` fires; a cancelled future is
// dropped, which cancels whatever it was waiting on.
pub async fn or_cancel<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(aborted()),
        out = fut => out,
    }
}

// The message of an error as the model and the traces see it: the outermost
// context, like a JS Error's message.
pub fn message(err: &anyhow::Error) -> String {
    err.to_string()
}
