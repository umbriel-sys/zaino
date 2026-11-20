//! ZIP 221 history proofs: each proof recomputes the root the chain holds
//! for the tip's header, from the leaf up.

mod common;

use common::{chain, local, remote, TIP};
use zaino_chain::{BlockId, BlockRead as _, HistoryTreeRead as _};
use zaino_indexes::history_tree::{root_of, Epoch};
use zaino_primitives::types::Height;
use zaino_service::{HistoryNode, HistoryProof, ServiceError};
use zcash_history::{Entry, Version as _, V2};

fn height(h: u32) -> Height {
    Height::try_from(h).expect("small")
}

/// The node a proof's leaf and path combine to, and its position.
fn combined(proof: &HistoryProof) -> (u32, [u8; 32]) {
    let branch = u32::from(proof.branch);
    let data = |node: &HistoryNode| {
        Entry::<V2>::from_bytes(branch, &node.node)
            .expect("decodes")
            .data()
            .clone()
    };
    let (mut at, mut node) = (proof.leaf.position, data(&proof.leaf));
    for sibling in &proof.path {
        // the parent follows its right child
        (at, node) = match sibling.position < at {
            true => (at + 1, V2::combine(&data(sibling), &node)),
            false => (sibling.position + 1, V2::combine(&node, &data(sibling))),
        };
    }
    (at, V2::hash(&node))
}

/// For blocks across the store and the chain head, against the latest tip
/// and an earlier one: the leaf and its path combine to one of the peaks,
/// and the peaks give the root the chain holds for the tip's header.
#[tokio::test]
async fn a_proof_recomputes_the_root_the_tips_header_commits_to() {
    let service = local(&chain()).await;
    let snapshot = service.snapshot();
    for (block, tip) in [
        (1, None),
        (2, None),
        (150, None),
        (TIP - 1, None),
        (1, Some(2)),
        (99, Some(200)),
        (199, Some(200)),
    ] {
        let proof = snapshot
            .history_proof(
                BlockId::Height(height(block)),
                tip.map(|tip| BlockId::Height(height(tip))),
            )
            .await
            .expect("answers")
            .expect("on the best chain");
        let tip = tip.unwrap_or(TIP);
        assert_eq!(u32::from(proof.tip.height), tip, "{block} at {tip}");
        assert_eq!(
            Some(proof.tip.hash),
            snapshot
                .chain()
                .block_hash(height(tip))
                .await
                .expect("answers")
        );

        let (peak, hash) = combined(&proof);
        let reached = proof
            .peaks
            .iter()
            .find(|node| node.position == peak)
            .unwrap_or_else(|| panic!("{block} at {tip}: the path ends at {peak}, not a peak"));
        let stored =
            Entry::<V2>::from_bytes(u32::from(proof.branch), &reached.node).expect("decodes");
        assert_eq!(hash, V2::hash(stored.data()), "{block} at {tip}");

        let epoch = Epoch {
            branch: u32::from(proof.branch),
            start: 0,
            length: proof.tree_length,
            peaks: proof
                .peaks
                .iter()
                .map(|node| (node.position, node.node.clone()))
                .collect(),
        };
        assert_eq!(
            Some(root_of(&epoch).expect("a root")),
            snapshot
                .chain()
                .history_root(height(tip - 1))
                .await
                .expect("answers"),
            "{block} at {tip}"
        );
    }
    service.shutdown().await.expect("stops");
}

/// By hash as by height; a block or tip off the best chain is `None`; a
/// block at or above the tip, one before the tree's epoch, and a tip whose
/// header commits to no tree are refused.
#[tokio::test]
async fn a_proof_is_refused_where_none_exists() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let at = |h: u32| BlockId::Height(height(h));
    let hash = |h: u32| BlockId::Hash(chain.block_at(h).expect("built").hash());

    assert_eq!(
        snapshot
            .history_proof(hash(50), Some(hash(60)))
            .await
            .expect("answers"),
        snapshot
            .history_proof(at(50), Some(at(60)))
            .await
            .expect("answers")
    );
    let unknown = BlockId::Hash([9; 32].into());
    for (block, tip) in [
        (unknown, None),
        (at(50), Some(unknown)),
        (at(TIP + 1), None),
    ] {
        assert_eq!(
            snapshot.history_proof(block, tip).await.expect("answers"),
            None
        );
    }
    for (block, tip) in [
        (at(TIP), None),
        (at(60), Some(at(60))),
        (at(61), Some(at(60))),
        // genesis is before the first upgrade: in no tree
        (at(0), None),
        // block 1's header commits to the tree before it: none
        (at(0), Some(at(1))),
        (at(0), Some(at(0))),
    ] {
        assert!(
            matches!(
                snapshot.history_proof(block, tip).await,
                Err(ServiceError::InvalidParameter(_))
            ),
            "{block:?} at {tip:?}"
        );
    }
    service.shutdown().await.expect("stops");
}

/// Without the history tree nobody answers: the validator has no such read.
#[tokio::test]
async fn without_the_history_tree_a_proof_is_not_served() {
    let service = remote(&chain()).await;
    let refused = service
        .snapshot()
        .history_proof(BlockId::Height(height(50)), None)
        .await
        .expect_err("no history tree");
    assert!(matches!(refused, ServiceError::Chain(_)), "{refused:?}");
    service.shutdown().await.expect("stops");
}
