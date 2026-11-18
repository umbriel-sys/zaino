//! Synthesise a tip subscription for sources that have no native stream.

use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::{GetChainTipError, OneShotGetChainTip, QueryError, SubscribeChainTip, TipObservation};

/// A tip subscription built by polling a source that has no native stream.
///
/// A decorator rather than something baked into each adapter, matching
/// [`ValidatorClient`](crate::ValidatorClient): the capability is synthesised on top of any
/// source that can answer [`GetChainTip`], so one implementation serves every
/// pollable source rather than each adapter growing its own poll loop.
///
/// Deliberately not a wrapper that re-exposes the source's other traits. It
/// owns only the polling, so a composite holds it *alongside* its adapters and
/// delegates [`SubscribeChainTip`] to it — no forwarding boilerplate for the
/// thirty other queries it has nothing to say about.
///
/// # What it is not
///
/// Polling is not a push stream, and this type does not pretend otherwise.
/// Readings arrive at the configured cadence rather than when a block lands, so
/// a subscriber sees a tip change up to one interval late. What it does
/// guarantee is that every published reading is stamped with when it was taken,
/// so a subscriber can tell a quiet chain from a source that has stopped
/// answering — see [`TipObservation::age`].
///
/// The poll task stops when this value is dropped.
pub struct PolledChainTip {
    tip: watch::Receiver<TipObservation>,
    task: JoinHandle<()>,
}

impl PolledChainTip {
    /// Start polling `source` every `interval`.
    ///
    /// Takes one reading before returning, so the subscription always has a
    /// real tip to hand out and never a placeholder. That makes construction
    /// fallible: a source that cannot answer once cannot seed a subscription,
    /// and failing here is better than handing back a channel that may never
    /// carry anything.
    ///
    /// Later failures do not stop the poller. It keeps the last good reading
    /// and keeps trying, letting the reading's age carry the bad news — a
    /// transient outage should not tear down a subscription that will recover.
    pub async fn spawn<S>(
        source: S,
        interval: Duration,
    ) -> Result<Self, QueryError<GetChainTipError, S::NonDomain>>
    where
        S: OneShotGetChainTip + Send + 'static,
    {
        let (hash, height) = source.get_chain_tip().await?;
        let (tx, tip) = watch::channel(TipObservation::now(hash, height));

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // The first tick completes immediately, and the constructor has
            // just read the tip; skip it so the source is not asked twice in
            // quick succession at startup.
            ticker.tick().await;

            loop {
                ticker.tick().await;

                // Every receiver is gone, so nothing would observe further
                // readings. Stop rather than keep querying the validator.
                if tx.is_closed() {
                    return;
                }

                if let Ok((hash, height)) = source.get_chain_tip().await {
                    // Published on every success, including an unchanged tip:
                    // that is what keeps `observed_at` a liveness signal rather
                    // than a record of when the chain last moved.
                    tx.send_replace(TipObservation::now(hash, height));
                }
            }
        });

        Ok(Self { tip, task })
    }

    /// The most recent reading, without waiting for the next one.
    pub fn current(&self) -> TipObservation {
        *self.tip.borrow()
    }
}

impl SubscribeChainTip for PolledChainTip {
    fn subscribe_to_chain_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        Some(self.tip.clone())
    }
}

impl Drop for PolledChainTip {
    fn drop(&mut self) {
        // The task holds the sender and would otherwise poll the validator for
        // the life of the process.
        self.task.abort();
    }
}

// Time is paused in every test here: the poll interval elapses only as the
// runtime advances it, so each test sees an exact number of polls.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripted::{hash, height, Scripted};
    use crate::FailureMode;

    const INTERVAL: Duration = Duration::from_millis(10);

    /// A poller over a source at tip `(1, 0)`, and the source.
    async fn polled() -> (PolledChainTip, Scripted) {
        let source = Scripted::default().with_tip(hash(1), height(0));
        let polled = PolledChainTip::spawn(source.clone(), INTERVAL)
            .await
            .expect("seeded from the initial read");
        (polled, source)
    }

    /// The constructor must not hand back a subscription seeded with a
    /// placeholder, so it reads once and fails if that read fails.
    #[tokio::test(start_paused = true)]
    async fn spawn_fails_when_the_source_has_no_tip() {
        let result = PolledChainTip::spawn(Scripted::default(), INTERVAL).await;
        assert!(result.is_err(), "no tip must not seed a subscription");
    }

    #[tokio::test(start_paused = true)]
    async fn current_reading_is_available_immediately() {
        let (polled, source) = polled().await;
        let observation = polled.current();
        assert_eq!((observation.hash, observation.height), (hash(1), height(0)));
        assert_eq!(source.calls(), 1, "read once, at construction");
    }

    /// Readings keep arriving while the tip is unchanged: that is what tells
    /// a quiet chain from a dead poller.
    #[tokio::test(start_paused = true)]
    async fn republishes_an_unchanged_tip() {
        let (polled, source) = polled().await;
        let mut subscriber = polled
            .subscribe_to_chain_tip()
            .expect("always subscribable");

        subscriber
            .changed()
            .await
            .expect("the poller publishes again");
        let observation = *subscriber.borrow_and_update();
        assert_eq!((observation.hash, observation.height), (hash(1), height(0)));
        assert_eq!(source.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_tip_reaches_subscribers() {
        let (polled, source) = polled().await;
        let mut subscriber = polled
            .subscribe_to_chain_tip()
            .expect("always subscribable");

        source.set_tip(hash(2), height(1));
        subscriber.changed().await.expect("the poller publishes");
        let observation = *subscriber.borrow_and_update();
        assert_eq!((observation.hash, observation.height), (hash(2), height(1)));
    }

    /// A failing source publishes nothing, so the last reading stands and
    /// ages; polling goes on, and the first success publishes again.
    #[tokio::test(start_paused = true)]
    async fn a_failing_source_keeps_the_last_reading_and_recovers() {
        let (polled, source) = polled().await;
        let mut subscriber = polled
            .subscribe_to_chain_tip()
            .expect("always subscribable");
        let seeded = polled.current();

        source.set_failing(Some(FailureMode::Timeout));
        tokio::time::sleep(INTERVAL * 5 + INTERVAL / 2).await;
        assert!(!subscriber.has_changed().expect("the poller lives"));
        assert_eq!(polled.current(), seeded, "the last reading stands");
        assert_eq!(source.calls(), 6, "polling went on through the failures");

        source.set_failing(None);
        subscriber
            .changed()
            .await
            .expect("the poller publishes again");
        assert_ne!(polled.current(), seeded);
    }

    /// Dropping the subscription stops the background poll, or a short-lived
    /// subscriber would query the validator forever: the aborted task drops
    /// the sender, which closes every receiver.
    #[tokio::test(start_paused = true)]
    async fn dropping_stops_the_poll_task() {
        let (polled, _source) = polled().await;
        let mut subscriber = polled
            .subscribe_to_chain_tip()
            .expect("always subscribable");

        drop(polled);
        assert!(
            subscriber.changed().await.is_err(),
            "the poll task outlived its subscription"
        );
    }
}
