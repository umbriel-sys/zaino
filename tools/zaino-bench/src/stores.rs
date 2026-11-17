//! Opening the source and the store, each behind the ports the commands are
//! written against.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zaino_chain_store_service::{
    ChainStore, ChainStoreServiceConfig, IndexesConfig, IngestConfig, NamedSet, OnCorruption,
};
use zaino_persistence_lsm_tree::fs::RealFs;
use zaino_persistence_lsm_tree::{LsmConfig, LsmTreeStorage};
use zaino_primitives::types::{Network, NetworkUpgradeInfo};
use zaino_source::OneShotGetBlockchainInfo as _;
use zaino_source_zebra::ZebraReadStateAdapter;

/// The validator: zebra's finalized state, read-only.
pub type Source = ZebraReadStateAdapter;
/// The store over the LSM engine.
pub type Store = ChainStore<LsmTreeStorage, Source>;

/// The store's name in records and under `--store-dir`.
pub const NAME: &str = "lsm";

pub fn open_source(cache: &Path) -> Result<Source, String> {
    ZebraReadStateAdapter::open(cache, &zebra_chain::parameters::Network::Mainnet)
}

/// The upgrade schedule the validator reports (the store needs it for C6).
pub async fn schedule(source: &Source) -> Result<Arc<[NetworkUpgradeInfo]>, String> {
    let info = source
        .get_blockchain_info()
        .await
        .map_err(|error| format!("blockchain info: {error}"))?;
    Ok(Arc::from(info.upgrades))
}

/// `<store_dir>/lsm/`, emptied when `fresh`.
pub fn store_path(store_dir: &Path, fresh: bool) -> Result<PathBuf, String> {
    let dir = store_dir.join(NAME);
    if fresh && dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|error| format!("emptying {}: {error}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("creating {}: {error}", dir.display()))?;
    Ok(dir)
}

fn named_set(set: &str) -> Result<NamedSet, String> {
    match set {
        "full" => Ok(NamedSet::Full),
        "lightclient" => Ok(NamedSet::Lightclient),
        "zcash_rpc" => Ok(NamedSet::ZcashRpc),
        other => Err(format!("unknown index set {other}")),
    }
}

pub async fn open(
    dir: &Path,
    set: &str,
    source: &Arc<Source>,
    schedule: &Arc<[NetworkUpgradeInfo]>,
    threads: Option<NonZeroUsize>,
    write_buffer_mib: usize,
) -> Result<Store, String> {
    let config = ChainStoreServiceConfig {
        root: dir.to_path_buf(),
        network: Network::Mainnet,
        indexes: IndexesConfig {
            set: Some(named_set(set)?),
            ..IndexesConfig::default()
        },
        on_corruption: OnCorruption::default(),
        ingest: IngestConfig::default(),
    };
    let storage = LsmTreeStorage {
        fs: RealFs::shared(),
        lsm: LsmConfig::default(),
        write_buffer: NonZeroUsize::new(write_buffer_mib.saturating_mul(1 << 20))
            .ok_or("--lsm-write-buffer-mib must be at least 1")?,
        threads,
        strict_manifest_read: false,
    };
    ChainStore::boot(&config, storage, Arc::clone(source), Arc::clone(schedule))
        .await
        .map_err(|error| format!("booting the store: {error}"))
}
