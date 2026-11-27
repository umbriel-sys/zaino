//! The wallet port.

use anyhow::Result;
use zcash_protocol::TxId;

use crate::validator::Pool;

/// Spendable balance per pool, in zatoshis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PoolBalances {
    /// Transparent.
    pub transparent: u64,
    /// Sapling.
    pub sapling: u64,
    /// Orchard.
    pub orchard: u64,
    /// Ironwood.
    pub ironwood: u64,
}

impl PoolBalances {
    /// The balance in `pool`.
    pub fn get(&self, pool: Pool) -> u64 {
        match pool {
            Pool::Transparent => self.transparent,
            Pool::Sapling => self.sapling,
            Pool::Orchard => self.orchard,
            Pool::Ironwood => self.ironwood,
        }
    }

    /// Every pool summed.
    pub fn total(&self) -> u64 {
        self.transparent + self.sapling + self.orchard + self.ironwood
    }
}

/// A wallet synced through the indexer under test.
pub trait Wallet: Send + Sync {
    /// The default address that routes funds into `pool`: a bare `tm…` or
    /// `zregtestsapling…`, or a unified address for Orchard and Ironwood.
    async fn address(&self, pool: Pool) -> Result<String>;
    /// Scans to the indexer's tip.
    async fn sync(&self) -> Result<()>;
    /// Spendable balances at one confirmation (sync first).
    async fn balances(&self) -> Result<PoolBalances>;
    /// Sends `zats` to `to`; broadcasts through the indexer and returns the txid.
    async fn send(&self, to: &str, zats: u64) -> Result<TxId>;
    /// Shields every transparent UTXO into the active shielded pool.
    async fn shield(&self) -> Result<TxId>;
}
