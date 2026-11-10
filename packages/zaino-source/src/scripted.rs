//! A scripted single-attempt source for this crate's tests.
//!
//! Every call first takes the next scripted failure, or the standing one.
//! Past those, `get_chain_tip` answers the set tip (`NotReady` without one),
//! and every other port fails fatally (`Auth`) with a message naming the call
//! and its arguments, so a test sees which method was reached, with what.

use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;
use zaino_primitives::types::{BlockHash, Height};

use crate::{FailureMode, GetChainTipError, NonDomainError, QueryError};

#[derive(Default)]
struct State {
    failures: VecDeque<FailureMode>,
    failing: Option<FailureMode>,
    tip: Option<(BlockHash, Height)>,
    calls: usize,
}

/// The scripted source; clones share one script.
#[derive(Clone)]
pub(crate) struct Scripted {
    state: Arc<Mutex<State>>,
    blocks: Arc<watch::Sender<()>>,
}

impl Default for Scripted {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            blocks: Arc::new(watch::channel(()).0),
        }
    }
}

impl Scripted {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next calls fail with `modes`, in order.
    pub(crate) fn failing_with(self, modes: impl IntoIterator<Item = FailureMode>) -> Self {
        self.lock().failures.extend(modes);
        self
    }

    /// With `tip` as the chain tip.
    pub(crate) fn with_tip(self, hash: BlockHash, height: Height) -> Self {
        self.set_tip(hash, height);
        self
    }

    /// Moves the chain tip.
    pub(crate) fn set_tip(&self, hash: BlockHash, height: Height) {
        self.lock().tip = Some((hash, height));
    }

    /// Every call past the scripted failures fails with `mode` (`None` = stop).
    pub(crate) fn set_failing(&self, mode: Option<FailureMode>) {
        self.lock().failing = mode;
    }

    /// Calls made so far.
    pub(crate) fn calls(&self) -> usize {
        self.lock().calls
    }

    /// Wakes block subscribers.
    pub(crate) fn wake_blocks(&self) {
        self.blocks.send_replace(());
    }

    /// Counts a call and takes its scripted failure, if any.
    pub(crate) fn attempt(&self) -> Option<NonDomainError> {
        let mut state = self.lock();
        state.calls = state.calls.saturating_add(1);
        let mode = state
            .failures
            .pop_front()
            .or_else(|| state.failing.clone())?;
        Some(NonDomainError::new(mode, "scripted"))
    }

    /// The fatal failure ending every call but `get_chain_tip`.
    pub(crate) fn fatal(call: String) -> NonDomainError {
        NonDomainError::new(FailureMode::Auth, call)
    }
}

/// A call as the scripted ports name it: method and arguments.
pub(crate) fn call(method: &str, args: &impl Debug) -> String {
    format!("{method}{args:?}")
}

pub(crate) fn height(height: u32) -> Height {
    Height::try_from(height).expect("test height is in range")
}

pub(crate) fn hash(byte: u8) -> BlockHash {
    BlockHash::from([byte; 32])
}

impl crate::ValidatorSource for Scripted {
    type NonDomain = NonDomainError;
}

impl crate::OneShotGetChainTip for Scripted {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        if let Some(failure) = self.attempt() {
            return Err(QueryError::NonDomain(failure));
        }
        self.lock()
            .tip
            .ok_or(QueryError::Domain(GetChainTipError::NotReady))
    }
}

impl crate::SubscribeBlocks for Scripted {
    fn subscribe_to_blocks_received(&self) -> Option<watch::Receiver<()>> {
        Some(self.blocks.subscribe())
    }
}

/// No push path for tips.
impl crate::SubscribeChainTip for Scripted {}
