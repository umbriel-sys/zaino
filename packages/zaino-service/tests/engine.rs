//! The engine over the shared fake validator and an in-memory filesystem:
//! each store mode spawns, serves, reports who answers what, and stops.

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
use zaino_indexes::IndexId;
use zaino_mempool::MempoolConfig;
use zaino_persistence_lsm_tree::{fs::SimFs, LsmConfig, LsmTreeStorage};
use zaino_primitives::types::{Network, ShieldedPool};
use zaino_service::{
    Capability, DiskStore, Mode, Provision, Service, ServiceConfig, ServiceError, Store,
};
use zaino_source_mock::{BlockSpec, ChainBuilder, MockBlock, MockChain, NodeFacts};

const TIP: u32 = 300;

fn chain() -> MockChain {
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
    MockChain::new(NodeFacts::regtest(), blocks.clone())
}

fn config(store: Store) -> ServiceConfig {
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

fn disk(fs: &Arc<SimFs>, set: NamedSet, mode: Mode) -> Store {
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

/// Waits up to 30 s for `condition`.
async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..3_000 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

fn chain_capability(capability: ChainCapability) -> Capability {
    Capability::Chain(capability)
}

/// Every chain capability but transactions' bytes and the window's tips is
/// answered from its own index.
fn all_local(service: &Service<MockChain>) -> bool {
    let manifest = service.manifest();
    ChainCapability::ALL.into_iter().all(|capability| {
        match (capability, manifest.get(chain_capability(capability))) {
            (ChainCapability::Transactions, provision) => provision == Provision::Validator,
            (_, Provision::Local(_)) => true,
            _ => false,
        }
    })
}

/// No store: what the validator answers is the validator's, what only an
/// index can answer is unavailable and names its index, the window and the
/// mempool are local.
#[tokio::test]
async fn a_validator_backed_service_serves_from_the_validator() {
    let service = Service::spawn(
        Arc::new(chain()),
        config(Store::ValidatorBacked {
            network: Network::Regtest,
        }),
    )
    .await
    .expect("spawns");
    let manifest = service.manifest();
    for capability in [
        ChainCapability::Blocks,
        ChainCapability::CompactBlocks,
        ChainCapability::Transactions,
        ChainCapability::Treestate,
        ChainCapability::AddressHistory,
    ] {
        assert_eq!(
            manifest.get(chain_capability(capability)),
            Provision::Validator,
            "{capability}"
        );
    }
    assert_eq!(
        manifest.get(chain_capability(ChainCapability::ChainTips)),
        Provision::Local(None)
    );
    assert_eq!(
        manifest.get(chain_capability(ChainCapability::TxOutSet)),
        Provision::Unavailable(Some(IndexId::TxOutSet))
    );
    assert_eq!(manifest.get(Capability::Mempool), Provision::Local(None));
    assert!(matches!(
        manifest.require(&[chain_capability(ChainCapability::TxOutSet)]),
        Err(ServiceError::NotServed {
            index: Some(IndexId::TxOutSet),
            ..
        })
    ));
    assert_eq!(service.statuses().len(), 2, "store and head");
    service.shutdown().await.expect("stops");
}

/// A primary store is built in the background: the service serves from the
/// start and every capability becomes local as its index catches up.
#[tokio::test]
async fn a_primary_store_builds_in_the_background_until_every_read_is_local() {
    let fs = SimFs::new();
    let service = Service::spawn(
        Arc::new(chain()),
        config(disk(&fs, NamedSet::Full, Mode::Primary)),
    )
    .await
    .expect("spawns");
    assert!(
        eventually(|| all_local(&service)).await,
        "{:?}",
        service.manifest()
    );
    assert!(service
        .manifest()
        .require(&Capability::all().collect::<Vec<_>>())
        .is_ok());
    assert_eq!(service.statuses().len(), 3, "store, head and sync");
    service.shutdown().await.expect("stops");
}

/// A smaller set serves what it builds locally, falls to the validator for
/// what the validator can answer, and names the index for the rest.
#[tokio::test]
async fn the_lightclient_set_names_what_it_lacks() {
    let fs = SimFs::new();
    let service = Service::spawn(
        Arc::new(chain()),
        config(disk(&fs, NamedSet::Lightclient, Mode::Primary)),
    )
    .await
    .expect("spawns");
    let compact = chain_capability(ChainCapability::CompactBlocks);
    assert!(
        eventually(|| matches!(service.manifest().get(compact), Provision::Local(Some(_)))).await,
        "{:?}",
        service.manifest()
    );
    let manifest = service.manifest();
    assert_eq!(
        manifest.get(chain_capability(ChainCapability::TxOutSet)),
        Provision::Unavailable(Some(IndexId::TxOutSet))
    );
    assert_eq!(
        manifest.get(chain_capability(ChainCapability::AddressHistory)),
        Provision::Validator
    );
    service.shutdown().await.expect("stops");
}

/// A linked service reads the store another process builds, with its own
/// head and mempool.
#[tokio::test]
async fn a_linked_service_reads_the_primary_store() {
    let fs = SimFs::new();
    let primary = Service::spawn(
        Arc::new(chain()),
        config(disk(&fs, NamedSet::Full, Mode::Primary)),
    )
    .await
    .expect("spawns");
    assert!(eventually(|| all_local(&primary)).await);
    let linked = Service::spawn(
        Arc::new(chain()),
        config(disk(
            &fs,
            NamedSet::Full,
            Mode::Linked {
                refresh_interval: Duration::from_millis(5),
            },
        )),
    )
    .await
    .expect("spawns");
    assert!(
        eventually(|| all_local(&linked)).await,
        "{:?}",
        linked.manifest()
    );
    assert_eq!(linked.statuses().len(), 2, "store and head: no sync");
    linked.shutdown().await.expect("stops");
    primary.shutdown().await.expect("stops");
}
