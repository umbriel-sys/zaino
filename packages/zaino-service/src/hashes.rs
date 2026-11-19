//! `getblockhash` and `getblockhashes`, from Zaino's headers (D6-03).

use zaino_chain::{BlockId, BlockRead as _};
use zaino_primitives::types::{BlockHash, BlockTime, Height};

use crate::block_time::{CandidateSearch, Drift, SearchStep};
use crate::error::ServiceError;
use crate::reads::ServiceSnapshot;
use crate::source::ServiceSource;

type Result<T> = core::result::Result<T, ServiceError>;

/// A block `getblockhashes` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHashAt {
    /// Its height.
    pub height: Height,
    /// Its hash.
    pub hash: BlockHash,
    /// Its own time (`nTime`).
    pub time: BlockTime,
}

impl<S: ServiceSource> ServiceSnapshot<S> {
    /// The best-chain hash at `index`, as zebrad reads it: a height from
    /// genesis, or from the tip when negative (`-1` is the tip).
    pub async fn block_hash(&self, index: i32) -> Result<BlockHash> {
        let tip = i64::from(u32::from(self.tip().height));
        let height = match index >= 0 {
            true => i64::from(index),
            false => tip.saturating_add(1).saturating_add(i64::from(index)),
        };
        let height = u32::try_from(height)
            .ok()
            .filter(|height| i64::from(*height) <= tip)
            .and_then(|height| Height::try_from(height).ok())
            .ok_or_else(|| {
                ServiceError::InvalidParameter(format!("block height {index} out of range"))
            })?;
        self.chain().block_hash(height).await?.ok_or_else(|| {
            ServiceError::Inconsistent(format!("no block at {height} below the tip"))
        })
    }

    /// Every best-chain block whose own time lies in `[low, high)`, by time
    /// then hash, as zcashd's timestamp index lists them; empty past the tip
    /// or before genesis. Selected by `nTime`: zcashd's logical times are not
    /// indexed.
    pub async fn block_hashes(&self, low: BlockTime, high: BlockTime) -> Result<Vec<BlockHashAt>> {
        let tip = self.tip().height;
        let mut search = CandidateSearch::new(tip, low, high, Drift::of(self.network));
        let bracket = loop {
            match search.poll() {
                SearchStep::Need(height) => {
                    let header = self.chain().block_header(BlockId::Height(height)).await?;
                    search.supply(height, header.map(|header| header.time));
                }
                SearchStep::Done(bracket) => break bracket,
                SearchStep::Failed(missing) => return Err(missing_header(missing.height)),
            }
        };
        let Some(bracket) = bracket else {
            return Ok(Vec::new());
        };
        let mut hits = Vec::new();
        for height in u32::from(bracket.start)..=u32::from(bracket.end) {
            let height = Height::try_from(height).map_err(|_| missing_header(bracket.end))?;
            let header = self
                .chain()
                .block_header(BlockId::Height(height))
                .await?
                .ok_or_else(|| missing_header(height))?;
            if low <= header.time && header.time < high {
                hits.push(BlockHashAt {
                    height,
                    hash: header.hash,
                    time: header.time,
                });
            }
        }
        hits.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.hash.cmp(&b.hash)));
        Ok(hits)
    }
}

fn missing_header(height: Height) -> ServiceError {
    ServiceError::Inconsistent(format!("no header at {height} below the tip"))
}
