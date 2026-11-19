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
    assert_eq!(service.statuses().len(), 2, "store and head");
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
