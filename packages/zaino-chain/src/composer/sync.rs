//! Driving the finalised store toward the chain head's floor (D-72).
//!
//! The one place in this crate that *writes*. Everything else composes reads
//! from providers someone else advances; this advances one of them.
//!
//! # Why the composer drives it, and not the store
//!
//! A chain store is built to where it is told to be built to. It has a
//! validator of its own and could follow the chain unaided, but a store that
//! decided its own target would be a second writer racing whatever else in the
//! deployment thought it owned that decision — and the two would contend on one
//! database, multiplying memory rather than throughput.
//!
//! The composer is the party that knows. It already holds both tiers, already
//! knows where the seam between them is, and is the thing that suffers if they
//! drift apart. So the store stays caller-driven and this is the caller.
//!
//! # The floor is the target
//!
//! The chain head's floor is its lowest retained canonical block, below the
//! consensus seam, so everything up to it is settled. On launch and on every
//! chain-head update the loop asks the store to build to the floor; the store
//! fetches those blocks from its own validator through `build_to`, the store's
//! only write path. A floor the store already holds costs a call that finds
//! nothing to do.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use zaino_chain_head::{ChainHeadBlockService, ChainHeadSnapshot as _};
use zaino_chain_store::{ChainStoreIngest, ChainStoreService, ChainStoreSourceError};
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, StatusSource, StatusWatch,
};
use zaino_primitives::types::{ChainStateEpoch, Height};

use crate::composer::ChainViewComposer;
use crate::source::ChainViewSource;

/// How this subsystem names itself in status reports.
const COMPONENT: ComponentName = ComponentName("chain-view-sync");

/// This loop's status, as the two axes a component reports.
///
/// A build it cannot complete leaves it `Syncing` and `Recoverable` — still
/// doing its job, not yet succeeding — which one axis cannot say.
fn report(lifecycle: Lifecycle, health: Health) -> ComponentStatus {
    ComponentStatus::new(COMPONENT, lifecycle, health)
}

/// A running sync loop.
///
/// Returned rather than kept on the composer because the composer is `Clone`
/// and a read handle: a loop reachable from every clone would be a loop any
/// clone could stop. One owner, one lifetime.
#[derive(Debug)]
pub struct ChainViewSync {
    status: watch::Sender<ComponentStatus>,
    cancel: CancellationToken,
    /// `None` once [`shutdown`](Self::shutdown) has taken it.
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ChainViewSync {
    /// How the loop is faring.
    ///
    /// `Syncing` until the store holds the floor, `Ready` once it does,
    /// `Recoverable` while a build fails, `Offline` once the loop has stopped.
    /// The store's own status is separate and says whether the *database* is
    /// healthy; this says whether anything is still feeding it.
    pub fn status(&self) -> ComponentStatus {
        self.status.borrow().clone()
    }

    /// Stops the loop.
    ///
    /// The token passed to [`spawn_sync`](ChainViewComposer::spawn_sync) also
    /// stops it; this additionally publishes `Closing` and releases the
    /// handle, so shutdown is observable rather than merely effective.
    ///
    /// Synchronous, and does not wait: it cancels, then aborts. A build cut
    /// short leaves the store at a block boundary, since the store commits per
    /// block.
    pub fn shutdown(&self) {
        self.status
            .send_replace(report(Lifecycle::Closing, Health::Healthy));
        self.cancel.cancel();
        // a poisoned lock still holds the handle: abort it all the same
        if let Some(handle) = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            handle.abort();
        }
    }
}

impl StatusSource for ChainViewSync {
    fn status(&self) -> ComponentStatus {
        ChainViewSync::status(self)
    }
}

/// So a supervisor reacts to a transition rather than polling for one.
impl StatusWatch for ChainViewSync {
    fn subscribe(&self) -> watch::Receiver<ComponentStatus> {
        self.status.subscribe()
    }
}

/// Stopping the loop when its handle goes, so a dropped handle cannot leave a
/// task writing to a store nobody is reading.
impl Drop for ChainViewSync {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl<Store, Head, Source> ChainViewComposer<Store, Head, Source>
where
    Store: ChainStoreService + ChainStoreIngest + 'static,
    Head: ChainHeadBlockService,
    Source: ChainViewSource,
{
    /// Starts building this view's finalised store to its chain head's floor,
    /// and keeps it there as the floor rises.
    ///
    /// Asked for, never assumed: a deployment that drives its own store on its
    /// own schedule must not also get this, or two writers contend on one
    /// database. The method exists only where `Store: ChainStoreIngest`, so a
    /// composer over a store that cannot be built (a linked, read-only one)
    /// does not have it.
    ///
    /// Cancelling `cancel` stops the loop, as does dropping the returned
    /// handle. Both are the same mechanism; the handle also makes the stop
    /// observable through [`ChainViewSync::status`].
    pub fn spawn_sync(self: &Arc<Self>, cancel: CancellationToken) -> Arc<ChainViewSync> {
        let status = watch::Sender::new(report(Lifecycle::Syncing, Health::Healthy));
        let updates = self.head.subscribe_updates();
        let failed = status.clone();
        let task = tokio::spawn(zaino_component::supervised(
            cancel.clone(),
            run(Arc::clone(self), updates, status.clone(), cancel.clone()),
            move |reason| fail(&failed, reason),
        ));
        Arc::new(ChainViewSync {
            status,
            cancel,
            task: Mutex::new(Some(task)),
        })
    }

    /// The chain head's floor: its lowest retained canonical block.
    fn floor(&self) -> Option<Height> {
        self.head
            .current()
            .best_chain()
            .next()
            .map(|block| block.height())
    }

    /// The store's tip height.
    fn store_tip(&self) -> Option<Height> {
        self.store
            .subscribe_watermark()
            .borrow()
            .tip
            .map(|tip| tip.height)
    }
}

/// The wait before a failed build's first retry; doubled each failure.
const RETRY_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between retries.
const RETRY_MAX: Duration = Duration::from_secs(30);
/// Failed builds in a row, the store not advancing, before the loop fails.
const MAX_FAILURES: u32 = 20;

/// The loop has failed for good, for a fault of Zaino's own.
fn fail(status: &watch::Sender<ComponentStatus>, reason: String) {
    status.send_if_modified(|current| current.fail(reason, false));
}

/// The loop has failed for good: the validator did not answer its retries.
fn exhausted(status: &watch::Sender<ComponentStatus>, reason: String) {
    status.send_if_modified(|current| current.fail(reason, true));
}

/// Whether a later attempt can succeed: the validator did not answer.
fn transient(error: &ChainStoreSourceError) -> bool {
    match error {
        ChainStoreSourceError::Unavailable { .. } | ChainStoreSourceError::NotReady { .. } => true,
        ChainStoreSourceError::InconsistentData { .. } | ChainStoreSourceError::Commit { .. } => {
            false
        }
    }
}

/// The loop: build to the floor, then again whenever the chain head moves
/// it. A build the validator did not answer is retried, each wait longer, up
/// to [`MAX_FAILURES`] times in a row without the store advancing; then, as
/// at once for any other failure, the loop fails with the cause.
async fn run<Store, Head, Source>(
    composer: Arc<ChainViewComposer<Store, Head, Source>>,
    mut updates: watch::Receiver<ChainStateEpoch>,
    status: watch::Sender<ComponentStatus>,
    cancel: CancellationToken,
) where
    Store: ChainStoreService + ChainStoreIngest + 'static,
    Head: ChainHeadBlockService,
    Source: ChainViewSource,
{
    let mut built_to: Option<Height> = None;
    let mut failures = 0u32;
    let mut backoff = RETRY_FIRST;
    // set while a failed build waits for its retry
    let mut retry: Option<Duration> = None;
    loop {
        if let Some(floor) = composer.floor() {
            // a floor already built to is not asked for again; a failed build is
            if built_to.is_none_or(|built| built < floor) {
                let before = composer.store_tip();
                let built = tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    built = composer.store.build_to(floor) => built,
                };
                match built {
                    Ok(()) => {
                        status.send_replace(report(Lifecycle::Ready, Health::Healthy));
                        built_to = Some(floor);
                        failures = 0;
                        backoff = RETRY_FIRST;
                        retry = None;
                    }
                    Err(error) => {
                        // a build that moved the store is progress, not a repeat
                        if composer.store_tip() > before {
                            failures = 0;
                            backoff = RETRY_FIRST;
                        }
                        failures = failures.saturating_add(1);
                        if !transient(&error) || failures >= MAX_FAILURES {
                            warn!(%error, %floor, attempts = failures, "building the store failed; the sync has stopped");
                            let reason = format!(
                                "building the store to {floor} failed after {failures} attempts: {error}"
                            );
                            match transient(&error) {
                                true => exhausted(&status, reason),
                                false => fail(&status, reason),
                            }
                            return;
                        }
                        warn!(%error, %floor, attempt = failures, retry_in = ?backoff, "building the store to the chain head's floor failed; retrying");
                        let mut retrying = report(Lifecycle::Syncing, Health::Recoverable);
                        retrying.reason = Some(error.to_string());
                        status.send_replace(retrying);
                        retry = Some(backoff);
                        backoff = backoff.saturating_mul(2).min(RETRY_MAX);
                    }
                }
            }
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            changed = updates.changed() => {
                if changed.is_err() {
                    // nothing will move the floor again
                    fail(&status, "the chain head stopped publishing".to_owned());
                    return;
                }
            }
            () = async {
                match retry {
                    Some(wait) => tokio::time::sleep(wait).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
    }
    status.send_replace(report(Lifecycle::Offline, Health::Offline));
}
