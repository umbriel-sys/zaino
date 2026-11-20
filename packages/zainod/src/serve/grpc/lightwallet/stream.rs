//! Served streams: a bounded channel fed by one task, which stops when the
//! client stops reading it.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// A stream of items read from the task feeding it. Dropping the stream
/// aborts the task.
#[derive(Debug)]
pub struct ChannelStream<T> {
    items: mpsc::Receiver<Result<T, tonic::Status>>,
    feeder: JoinHandle<()>,
}

impl<T> Stream for ChannelStream<T> {
    type Item = Result<T, tonic::Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.items.poll_recv(cx)
    }
}

impl<T> Drop for ChannelStream<T> {
    fn drop(&mut self) {
        self.feeder.abort();
    }
}

/// The server's shutdown, as its streams see it.
#[derive(Debug, Clone, Default)]
pub(super) struct Shutdown(Option<watch::Receiver<()>>);

impl Shutdown {
    /// Shutdown when `signal` changes or its sender is gone.
    pub(super) fn on(signal: watch::Receiver<()>) -> Self {
        Self(Some(signal))
    }

    /// Resolves at shutdown; never, for a service no server owns.
    async fn signalled(self) {
        match self.0 {
            Some(mut signal) => {
                if signal.changed().await.is_err() {
                    // the owner is gone: shut down as well
                }
            }
            None => std::future::pending().await,
        }
    }
}

/// The bounds a stream is served under.
#[derive(Debug, Clone)]
pub(super) struct StreamBounds {
    /// How long the feeding task may run: a client that stops reading holds
    /// it no longer.
    pub(super) limit: Duration,
    /// Items buffered ahead of the client.
    pub(super) buffer: usize,
    /// Ends the stream when the server shuts down.
    pub(super) shutdown: Shutdown,
}

/// Runs `body` in a task feeding the returned stream, sending `on_timeout`
/// if it outlives the bound and ending the stream at shutdown.
pub(super) fn spawn_timed_stream<T, F>(
    bounds: StreamBounds,
    on_timeout: tonic::Status,
    body: impl FnOnce(mpsc::Sender<Result<T, tonic::Status>>) -> F,
) -> ChannelStream<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = ()> + Send + 'static,
{
    // a channel holds at least one item
    let (sender, items) = mpsc::channel(bounds.buffer.max(1));
    let work = body(sender.clone());
    let feeder = tokio::spawn(async move {
        tokio::select! {
            outcome = tokio::time::timeout(bounds.limit, work) => {
                if outcome.is_err() && sender.send(Err(on_timeout)).await.is_err() {
                    // the client went away: nobody to tell
                }
            }
            // the stream ends here, so a graceful shutdown is not held open
            () = bounds.shutdown.signalled() => {}
        }
    });
    ChannelStream { items, feeder }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;

    use super::*;

    fn bounds(limit: Duration) -> StreamBounds {
        StreamBounds {
            limit,
            buffer: 1,
            shutdown: Shutdown::default(),
        }
    }

    /// A stream ends, without an error, when the server shuts down.
    #[tokio::test]
    async fn shutdown_ends_the_stream() {
        let (shutdown, signal) = watch::channel(());
        let mut stream = spawn_timed_stream::<u32, _>(
            StreamBounds {
                shutdown: Shutdown::on(signal),
                ..bounds(Duration::from_secs(60))
            },
            tonic::Status::internal("late"),
            |sender| async move {
                if sender.send(Ok(1)).await.is_err() {
                    return;
                }
                std::future::pending::<()>().await;
            },
        );
        assert_eq!(stream.next().await.expect("an item").expect("sent"), 1);
        shutdown.send_replace(());
        assert!(stream.next().await.is_none());
    }

    /// Items arrive in order and the stream ends when the task does.
    #[tokio::test]
    async fn a_stream_yields_what_its_task_sends() {
        let stream = spawn_timed_stream(
            bounds(Duration::from_secs(5)),
            tonic::Status::internal("late"),
            |sender| async move {
                for item in 0..3u32 {
                    if sender.send(Ok(item)).await.is_err() {
                        return;
                    }
                }
            },
        );
        let items: Vec<_> = stream.map(|item| item.expect("sent")).collect().await;
        assert_eq!(items, vec![0, 1, 2]);
    }

    /// A task that outlives its bound ends the stream with the timeout.
    #[tokio::test(start_paused = true)]
    async fn a_task_past_its_bound_ends_with_the_timeout() {
        let mut stream = spawn_timed_stream::<u32, _>(
            bounds(Duration::from_secs(1)),
            tonic::Status::deadline_exceeded("late"),
            |_sender| std::future::pending(),
        );
        let late = stream.next().await.expect("an item").expect_err("late");
        assert_eq!(late.code(), tonic::Code::DeadlineExceeded);
        assert!(stream.next().await.is_none());
    }

    /// Dropping the stream stops the task feeding it.
    #[tokio::test]
    async fn dropping_the_stream_aborts_its_task() {
        let (alive, mut ended) = mpsc::channel::<()>(1);
        let stream = spawn_timed_stream::<u32, _>(
            bounds(Duration::from_secs(60)),
            tonic::Status::internal("late"),
            |_sender| async move {
                let _held = alive;
                std::future::pending::<()>().await;
            },
        );
        drop(stream);
        assert!(ended.recv().await.is_none(), "the task's state is dropped");
    }
}
