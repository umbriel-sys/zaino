//! The chain history tree (C6) through ChainView (D-51): the store's to its
//! tip and the chain head's above it, equal to a store built over the whole
//! chain.

mod common;

use common::{chain, height, rig, TIP};
use zaino_chain::{ChainViewConfig, ChainViewError, HistoryTreeRead as _};
use zaino_chain_store::{ChainStoreService as _, HistoryTreeIndex as _};

/// The store's tip: below it the store answers, above it the head.
const STORE_TOP: u32 = TIP - 50;

#[tokio::test]
async fn the_history_tree_matches_a_store_over_the_whole_chain() {
    let local = rig(chain())
        .store_to(STORE_TOP)
        .extended_head()
        .build()
        .await;
    let reference = rig(chain()).store_to(TIP).build().await.store.reader();
    let snapshot = local.view().snapshot();

    for h in 1..=TIP {
        let at = height(h);
        assert_eq!(
            snapshot.history_root(at).await.expect("answered"),
            reference.history_root(at).await.expect("read"),
            "root at {h}"
        );
        let epoch = snapshot
            .history_epoch(at)
            .await
            .expect("answered")
            .expect("Heartwood is active");
        assert_eq!(
            Some(epoch),
            reference.history_epoch(at).await.expect("read"),
            "epoch at {h}"
        );
        // the first, a middle, the last and one past the end
        let positions = [0, epoch.length / 2, epoch.length - 1, epoch.length];
        assert_eq!(
            snapshot
                .history_entries(at, &positions)
                .await
                .expect("answered"),
            reference
                .history_entries(at, &positions)
                .await
                .expect("read"),
            "entries at {h}"
        );
    }
    assert_eq!(
        snapshot
            .history_root(height(TIP + 1))
            .await
            .expect("answered"),
        None,
        "above the tip"
    );
}

/// Without the head's history tree there is nothing to answer with: no
/// validator holds it.
#[tokio::test]
async fn without_both_halves_the_history_tree_is_not_serviceable() {
    let plain = rig(chain()).store_to(STORE_TOP).build().await;
    assert!(matches!(
        plain
            .view_with(ChainViewConfig::default())
            .snapshot()
            .history_root(height(1))
            .await,
        Err(ChainViewError::NotServiceable(_))
    ));
}
