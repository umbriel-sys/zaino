//! A long-lived task's run, with its death reported.

use core::future::Future;

use zaino_async::{CancellationToken, catch_panic};

/// Runs `run`. A panic, or a return before `cancel`, is handed to `failed`
/// with its cause: a task's death is reported, never silent.
pub async fn supervised(
    cancel: CancellationToken,
    run: impl Future<Output = ()>,
    failed: impl FnOnce(String),
) {
    match catch_panic(run).await {
        Ok(()) if cancel.is_cancelled() => {}
        Ok(()) => failed("its task stopped unexpectedly".to_owned()),
        Err(message) => failed(format!("its task panicked: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    async fn outcome(cancelled: bool, run: impl Future<Output = ()>) -> Option<String> {
        let cancel = CancellationToken::new();
        if cancelled {
            cancel.cancel();
        }
        let seen = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&seen);
        supervised(cancel, run, move |reason| {
            *slot.lock().expect("slot") = Some(reason);
        })
        .await;
        let seen = seen.lock().expect("slot");
        seen.clone()
    }

    #[tokio::test]
    async fn a_panic_is_reported_with_its_message() {
        let reason = outcome(false, async { panic!("boom") }).await;
        assert_eq!(reason.as_deref(), Some("its task panicked: boom"));
    }

    #[tokio::test]
    async fn a_return_before_cancellation_is_reported() {
        let reason = outcome(false, async {}).await;
        assert_eq!(reason.as_deref(), Some("its task stopped unexpectedly"));
    }

    #[tokio::test]
    async fn a_return_after_cancellation_is_not_a_failure() {
        assert_eq!(outcome(true, async {}).await, None);
    }
}
