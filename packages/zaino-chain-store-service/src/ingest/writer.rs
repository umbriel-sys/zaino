//! One index's writer (D-67, D-96): on its own blocking thread, it folds and
//! commits each queued block above its tip (the engine writes its buffer
//! when full), announces each applied block, and flushes when the queue
//! ends.
//!
//! A panic (ours or a dependency's) is caught here and reported: the index
//! stays in its slot, which the thread only borrows, and is marked to be
//! reopened from disk, since a panic mid-commit leaves its backend's
//! in-memory state unknown.

use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::runtime::Handle;
use tokio::sync::{watch, Semaphore};
use zaino_chain_store::ChainStoreSourceError;
use zaino_persistence::{Backend, BackendWriter};
use zaino_primitives::types::BlockRef;

use super::queue::Receiver;
use crate::store::{commit_error, fold_error, Open};

/// An index while its writer runs: the writer borrows it, so a panic never
/// takes it; the pipeline takes it back when the writer ends.
pub(crate) type Slot<B> = Arc<Mutex<Option<Open<B>>>>;

/// How a writer ended.
pub(crate) struct Written {
    pub(crate) result: Result<(), ChainStoreSourceError>,
    /// It panicked: its index must be reopened from disk.
    pub(crate) panicked: bool,
}

/// Runs the writer of the index in `slot` over `queue`; at most `writers`
/// fold or commit at once.
pub(super) fn write<B: Backend>(
    slot: &Slot<B>,
    mut queue: Receiver,
    applied: watch::Sender<Option<BlockRef>>,
    writers: Arc<Semaphore>,
    runtime: Handle,
) -> Written {
    let mut held = slot.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(open) = held.as_mut() else {
        return Written {
            result: Err(ChainStoreSourceError::Commit {
                message: "an index writer found its slot empty".to_owned(),
                cause: None,
            }),
            panicked: false,
        };
    };
    let id = open.id;
    let run = panic::catch_unwind(AssertUnwindSafe(|| {
        while let Some(item) = queue.next() {
            let block = Arc::clone(&item.block);
            let height = block.header().height;
            if open.tip.is_none_or(|tip| tip.height < height) {
                let _turn = runtime
                    .block_on(writers.acquire())
                    .map_err(|error| commit_error(id, error))?;
                let fold = open
                    .id
                    .fold(open.state.as_ref(), &block)
                    .map_err(|error| fold_error(id, error))?;
                open.writer
                    .commit(block.reference(), fold.ops)
                    .map_err(|error| commit_error(id, error))?;
                open.state = Some(fold.next);
                open.tip = Some(block.reference());
                applied.send_replace(open.tip);
            }
            queue.done(item);
        }
        open.backend
            .flush()
            .map_err(|error| commit_error(id, error))
    }));
    // a stopped writer closes its queue, so the pipeline stops sending
    drop(queue);
    match run {
        Ok(result) => Written {
            result,
            panicked: false,
        },
        Err(payload) => {
            let message = panic_message(&*payload);
            tracing::error!(index = id.name(), %message, "index writer panicked: reopening the index");
            Written {
                result: Err(panicked(id.name(), &message)),
                panicked: true,
            }
        }
    }
}

/// The error a caught panic in `what` becomes.
pub(crate) fn panicked(what: &str, message: &str) -> ChainStoreSourceError {
    ChainStoreSourceError::Commit {
        message: format!("{what} panicked: {message}"),
        cause: None,
    }
}

/// A panic's message, where it carries one.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string payload".to_owned())
}
