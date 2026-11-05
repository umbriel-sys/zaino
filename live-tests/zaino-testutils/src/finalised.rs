//! Gating on zaino's finalised index.

use std::time::Duration;

use anyhow::{ensure, Result};

use crate::indexer::Zainod;

/// Polls the finalised store's committed tip until it reaches `target`.
///
/// No served height answers this: above the seam reads come from the chain
/// head, below it an unfilled store passes through to the validator, so both
/// read correct at heights the store has never written.
///
/// The suite runs on `fast-test-seam`, so the writer targets `tip - 100` and
/// saturates to genesis on a shorter chain.
pub async fn wait_for_finalised(indexer: &Zainod, target: u32, timeout: Duration) -> Result<u32> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let frontier = indexer.finalised_tip_height();
        if let Some(frontier) = frontier.filter(|f| *f >= target) {
            return Ok(frontier);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "finalised index reached {frontier:?}, never {target}, within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
