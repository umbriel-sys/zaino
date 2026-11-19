//! The engine over the shared fake validator and an in-memory filesystem:
//! each store mode spawns, serves, reports who answers what, and stops.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{all_local, chain, config, disk, eventually, validator_backed};
use zaino_chain::ChainCapability;
use zaino_chain_store_service::NamedSet;
use zaino_indexes::IndexId;
use zaino_persistence_lsm_tree::fs::SimFs;
use zaino_service::{Capability, Mode, Provision, Service, ServiceError};

fn chain_capability(capability: ChainCapability) -> Capability {
    Capability::Chain(capability)
}

/// No store: what the validator answers is the validator's, what only an
/// index can answer is unavailable and names its index, the window and the
/// mempool are local.
#[tokio::test]
async fn a_validator_backed_service_serves_from_the_validator() {
    let service = Service::spawn(Arc::new(chain()), config(validator_backed()))
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
    assert_eq!(service.statuses().all().count(), 4, "no sync");
    assert!(service.is_caught_up(), "no store to catch up");
    service.shutdown().await.expect("stops");
}

/// A primary store is built in the background: the service serves from the
/// start and every capability becomes local as its index catches up.
#[tokio::test]
async fn a_primary_store_builds_in_the_background_until_every_read_is_local() {
    let service = common::local(&chain()).await;
    assert!(service
        .manifest()
        .require(&Capability::all().collect::<Vec<_>>())
        .is_ok());
    assert!(service.statuses().sync.is_some(), "a primary store syncs");
    service
        .caught_up()
        .await
        .expect("the sync reaches the floor");
    assert!(service.is_caught_up());
    assert!(service.store_tip().is_some());
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

/// The zcash RPC set answers its indexes locally and compact blocks from the
/// validator; an empty set is a store that answers nothing itself.
#[tokio::test]
async fn the_zcash_rpc_and_empty_sets_serve_what_they_build() {
    let fs = SimFs::new();
    let service = Service::spawn(
        Arc::new(chain()),
        config(disk(&fs, NamedSet::ZcashRpc, Mode::Primary)),
    )
    .await
    .expect("spawns");
    let local = |capability| {
        matches!(
            service.manifest().get(chain_capability(capability)),
            Provision::Local(Some(_))
        )
    };
    assert!(
        eventually(|| [
            ChainCapability::TxOutSet,
            ChainCapability::AddressHistory,
            ChainCapability::SpendStatus,
            ChainCapability::Treestate,
        ]
        .into_iter()
        .all(local))
        .await,
        "{:?}",
        service.manifest()
    );
    assert_eq!(
        service
            .manifest()
            .get(chain_capability(ChainCapability::CompactBlocks)),
        Provision::Validator
    );
    service.shutdown().await.expect("stops");

    let fs = SimFs::new();
    let empty = Service::spawn(
        Arc::new(chain()),
        config(disk(&fs, NamedSet::Empty, Mode::Primary)),
    )
    .await
    .expect("spawns");
    let manifest = empty.manifest();
    assert_eq!(
        manifest.get(chain_capability(ChainCapability::CompactBlocks)),
        Provision::Validator
    );
    assert_eq!(
        manifest.get(chain_capability(ChainCapability::TxOutSet)),
        Provision::Unavailable(Some(IndexId::TxOutSet))
    );
    empty.shutdown().await.expect("stops");
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
    assert_eq!(linked.statuses().sync, None, "a linked store does not sync");
    linked.shutdown().await.expect("stops");
    primary.shutdown().await.expect("stops");
}
