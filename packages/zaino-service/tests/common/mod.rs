//! The suites' chain, configs and waits.

#![allow(dead_code)]

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use zaino_chain::{ChainCapability, ChainViewConfig};
use zaino_chain_head::ChainHeadConfig;
use zaino_chain_store_service::{
    ChainStoreServiceConfig, IndexesConfig, IngestConfig, NamedSet, OnCorruption,
};
use zaino_consensus::FAST_TEST_MAX_NONFINALISED_DEPTH;
use zaino_mempool::MempoolConfig;
use zaino_persistence_lsm_tree::{fs::SimFs, LsmConfig, LsmTreeStorage};
use zaino_primitives::types::{Network, ShieldedPool};
use zaino_service::{Capability, DiskStore, Mode, Provision, Service, ServiceConfig, Store};
use zaino_source_mock::{BlockSpec, ChainBuilder, MockBlock, MockChain, NodeFacts};

/// The chain's tip.
pub const TIP: u32 = 300;

/// A fresh fake validator: every block spends two outputs once coinbases
/// mature, every tenth adds Sapling and Orchard notes.
pub fn chain() -> MockChain {
    chain_with(NodeFacts::regtest())
}

/// The chain, served by a node reporting `facts`.
pub fn chain_with(facts: NodeFacts) -> MockChain {
    static BLOCKS: OnceLock<Vec<MockBlock>> = OnceLock::new();
    let blocks = BLOCKS.get_or_init(|| {
        (1..=TIP)
            .fold(ChainBuilder::new(), |builder, height| {
                let spec = BlockSpec::spends(2);
                builder.block(match height % 10 {
                    0 => spec
                        .notes(ShieldedPool::Sapling, 2)
                        .notes(ShieldedPool::Orchard, 2),
                    _ => spec,
                })
            })
            .into_blocks()
    });
    MockChain::new(facts, blocks.clone())
}

/// The service's config over `store`, its head polling every 2 ms.
pub fn config(store: Store) -> ServiceConfig {
    let mut chain_head = ChainHeadConfig::with_max_depth(
        NonZeroU32::new(FAST_TEST_MAX_NONFINALISED_DEPTH).expect("non-zero"),
    );
    chain_head.set_poll_interval_ms(NonZeroU64::new(2).expect("non-zero"));
    ServiceConfig {
        store,
        chain_view: ChainViewConfig::default(),
        chain_head,
        mempool: MempoolConfig::default(),
    }
}

/// A disk store with `set` on `fs`.
pub fn disk(fs: &Arc<SimFs>, set: NamedSet, mode: Mode) -> Store {
    let fs: Arc<dyn zaino_persistence_lsm_tree::fs::Fs> = fs.clone();
    Store::DiskAccelerated(Box::new(DiskStore {
        store: ChainStoreServiceConfig {
            root: PathBuf::from("/zaino-service-tests"),
            network: Network::Regtest,
            indexes: IndexesConfig {
                set: Some(set),
                ..IndexesConfig::default()
            },
            on_corruption: OnCorruption::default(),
            ingest: IngestConfig::default(),
        },
        storage: LsmTreeStorage {
            fs,
            lsm: LsmConfig::default(),
            write_buffer: NonZeroUsize::new(1 << 20).expect("non-zero"),
            threads: NonZeroUsize::new(1),
            strict_manifest_read: false,
        },
        mode,
    }))
}

/// No store.
pub fn validator_backed() -> Store {
    Store::ValidatorBacked {
        network: Network::Regtest,
    }
}

/// Waits up to 30 s for `condition`.
pub async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..3_000 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

/// Every chain capability but transactions' bytes answered from its index.
pub fn all_local(service: &Service<MockChain>) -> bool {
    let manifest = service.manifest();
    ChainCapability::ALL.into_iter().all(|capability| {
        match (capability, manifest.get(Capability::Chain(capability))) {
            (ChainCapability::Transactions, provision) => provision == Provision::Validator,
            (_, Provision::Local(_)) => true,
            _ => false,
        }
    })
}

/// A primary service over every index on `chain`, built until every read is
/// local.
pub async fn local(chain: &MockChain) -> Service<MockChain> {
    let service = Service::spawn(
        Arc::new(chain.clone()),
        config(disk(&SimFs::new(), NamedSet::Full, Mode::Primary)),
    )
    .await
    .expect("spawns");
    assert!(
        eventually(|| all_local(&service)).await,
        "{:?}",
        service.manifest()
    );
    service
}

/// A service with no store on `chain`, its head at the tip.
pub async fn remote(chain: &MockChain) -> Service<MockChain> {
    let service = Service::spawn(Arc::new(chain.clone()), config(validator_backed()))
        .await
        .expect("spawns");
    assert!(eventually(|| u32::from(service.snapshot().tip().height) == TIP).await);
    service
}
