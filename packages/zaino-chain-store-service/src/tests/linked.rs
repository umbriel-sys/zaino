//! A linked store following its primary through the primary's record.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use zaino_chain_store::StoreWatermark;
use zaino_indexes::IndexId;
use zaino_persistence::{in_memory::InMemoryStorage, Storage};
use zaino_persistence_lsm_tree::{fs::RealFs, LsmConfig, LsmTreeStorage};
use zaino_source::FailureMode;

use super::{boot, built_then_behind, chain, config, height, schedule, tip_of, PLAIN};
use crate::config::ChainStoreServiceConfig;
use crate::linked::{LinkedChainStore, LinkedError, PrimaryStatus};
use crate::record::PrimaryState;
use crate::set::SetError;
use crate::store::{ChainStore, Serviceability};

fn linked<St: Storage + Clone>(
    config: &ChainStoreServiceConfig,
    storage: &St,
) -> LinkedChainStore<St> {
    LinkedChainStore::open(config, storage.clone()).expect("linked")
}

fn state<St: Storage>(store: &LinkedChainStore<St>) -> Option<PrimaryState> {
    store.primary().state
}

/// Nothing is read before the first refresh; then each index follows the
/// primary's announced durable tips, as far as the primary serves.
#[tokio::test]
async fn a_linked_store_serves_what_the_primary_made_durable() {
    let (storage, chain) = (InMemoryStorage::new(), chain());
    let config = config(&PLAIN);
    let linked = linked(&config, &storage);
    assert_eq!(linked.primary(), PrimaryStatus::default());
    linked.refresh().expect("refreshed: no primary yet");
    assert_eq!(linked.primary(), PrimaryStatus::default());
    assert_eq!(
        linked.serviceability(IndexId::Headers),
        Serviceability::Absent
    );

    let primary = boot(&config, &storage, &chain).await.expect("boots");
    linked.refresh().expect("refreshed");
    assert_eq!(
        linked.primary(),
        PrimaryStatus {
            running: true,
            state: Some(PrimaryState::Ready),
            target: None,
        }
    );
    assert_eq!(
        linked.serviceability(IndexId::Headers),
        Serviceability::Building(None),
        "announced, empty"
    );

    let tip = tip_of(&chain);
    primary.build_to(tip.height).await.expect("builds");
    linked.refresh().expect("refreshed");
    assert_eq!(linked.subscribe_watermark().borrow().tip, Some(tip));
    assert_eq!(
        *linked.subscribe_serviceability().borrow(),
        *primary.subscribe_serviceability().borrow()
    );
    assert_eq!(linked.capabilities(), primary.capabilities());
    assert_eq!(linked.primary().target, Some(tip.height));

    drop(primary);
    linked.refresh().expect("refreshed");
    assert_eq!(
        linked.primary(),
        PrimaryStatus {
            running: false,
            state: Some(PrimaryState::Stopped),
            target: Some(tip.height),
        }
    );
    assert_eq!(
        linked.subscribe_watermark().borrow().tip,
        Some(tip),
        "a stopped primary's durable state is still served"
    );
}

#[tokio::test]
async fn the_record_reports_a_stalled_build_and_its_recovery() {
    let (storage, chain) = (InMemoryStorage::new(), chain());
    let config = config(&PLAIN);
    let primary = boot(&config, &storage, &chain).await.expect("boots");
    let linked = linked(&config, &storage);
    primary.build_to(height(2)).await.expect("builds");
    chain.fail_ports(&["get_block"], Some(1), FailureMode::Connection);
    let tip = tip_of(&chain);
    assert!(primary.build_to(tip.height).await.is_err());
    linked.refresh().expect("refreshed");
    assert!(
        matches!(state(&linked), Some(PrimaryState::Stalled(message)) if message.contains("get_block")),
        "{:?}",
        state(&linked)
    );
    assert_eq!(
        linked
            .subscribe_watermark()
            .borrow()
            .tip
            .map(|tip| tip.height),
        Some(height(2)),
        "the durable state before the failure"
    );
    primary.build_to(tip.height).await.expect("retry");
    linked.refresh().expect("refreshed");
    assert_eq!(state(&linked), Some(PrimaryState::Ready));
    assert_eq!(linked.subscribe_watermark().borrow().tip, Some(tip));
}

#[tokio::test]
async fn a_primary_that_fails_to_boot_is_halted_and_not_running() {
    let (storage, chain) = (InMemoryStorage::new(), chain());
    let config = config(&PLAIN);
    {
        let store = boot(&config, &storage, &chain).await.expect("boots");
        store.build_to(height(2)).await.expect("builds");
    }
    // the reboot's tip check cannot ask the validator
    chain.fail_ports(&["get_best_block_height"], None, FailureMode::Connection);
    assert!(boot(&config, &storage, &chain).await.is_err());
    let linked = linked(&config, &storage);
    linked.refresh().expect("refreshed");
    let status = linked.primary();
    assert!(!status.running);
    assert!(
        matches!(&status.state, Some(PrimaryState::Halted(message)) if message.contains("best height")),
        "{status:?}"
    );
}

/// An index the primary holds back (its tip above the validator's) is not
/// announced, so not served here either.
#[tokio::test]
async fn an_unverified_index_is_not_announced() {
    let (storage, chain, _) = built_then_behind().await;
    chain.serve_to(4);
    let config = config(&PLAIN);
    let _primary = boot(&config, &storage, &chain).await.expect("boots behind");
    let linked = linked(&config, &storage);
    linked.refresh().expect("refreshed");
    assert_eq!(
        linked.serviceability(IndexId::Headers),
        Serviceability::Absent
    );
    assert_eq!(
        *linked.subscribe_watermark().borrow(),
        StoreWatermark::empty()
    );
}

#[tokio::test]
async fn a_malformed_set_is_refused() {
    let mut config = config(&[]);
    config.indexes.enable = vec!["no_such_index".to_owned()];
    assert!(matches!(
        LinkedChainStore::open(&config, InMemoryStorage::new()),
        Err(LinkedError::Set(SetError::UnknownIndex(_)))
    ));
}

/// The poll task refreshes on its own; dropping its handle stops it.
#[tokio::test(start_paused = true)]
async fn polling_follows_the_primary() {
    let (storage, chain) = (InMemoryStorage::new(), chain());
    let config = config(&PLAIN);
    let primary = boot(&config, &storage, &chain).await.expect("boots");
    let linked = linked(&config, &storage);
    let polling = linked.poll(Duration::from_millis(10));
    let tip = tip_of(&chain);
    primary.build_to(tip.height).await.expect("builds");
    let mut watermark = linked.subscribe_watermark();
    watermark
        .wait_for(|watermark| watermark.tip == Some(tip))
        .await
        .expect("followed");
    drop(polling);
}

/// On disk: the primary's record file, the running marker and the read-only
/// directories, all through the real filesystem.
#[tokio::test]
async fn a_linked_store_follows_a_primary_on_a_real_filesystem() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = LsmTreeStorage {
        fs: RealFs::shared(),
        lsm: LsmConfig::default(),
        write_buffer: NonZeroUsize::MAX,
        threads: NonZeroUsize::new(2),
        strict_manifest_read: false,
    };
    let config = ChainStoreServiceConfig {
        root: dir.path().to_path_buf(),
        ..config(&PLAIN)
    };
    let chain = chain();
    let primary = ChainStore::boot(
        &config,
        storage.clone(),
        Arc::new(chain.clone()),
        schedule(),
    )
    .await
    .expect("boots");
    let linked = linked(&config, &storage);
    let tip = tip_of(&chain);
    primary.build_to(height(3)).await.expect("builds");
    linked.refresh().expect("refreshed");
    assert_eq!(
        linked
            .subscribe_watermark()
            .borrow()
            .tip
            .map(|tip| tip.height),
        Some(height(3))
    );
    assert!(linked.primary().running);

    primary.build_to(tip.height).await.expect("builds");
    linked.refresh().expect("refreshed");
    assert_eq!(linked.subscribe_watermark().borrow().tip, Some(tip));
    drop(primary);
    linked.refresh().expect("refreshed");
    assert_eq!(
        linked.primary(),
        PrimaryStatus {
            running: false,
            state: Some(PrimaryState::Stopped),
            target: Some(tip.height),
        }
    );
    assert_eq!(linked.subscribe_watermark().borrow().tip, Some(tip));
}
