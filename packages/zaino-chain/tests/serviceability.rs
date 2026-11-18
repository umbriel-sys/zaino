//! What a deployment offers, and what it refuses because it does not.
//!
//! Distinct from the coverage tests beside them: those ask what the *tiers* can
//! answer, these ask what the deployment has *chosen* to.

mod common;

use common::{chain, height, rig, Rig, DEPTH, TIP};
use zaino_chain::{
    Answerable, ChainCapability, ChainScope, ChainView as _, ChainViewError, SpendRead as _,
    TxOutSetRead as _,
};
use zaino_primitives::types::{Outpoint, TransactionId};

/// Steady state: the store overlaps the window.
async fn steady() -> Rig {
    rig(chain()).store_to(TIP - DEPTH + 50).build().await
}

fn an_outpoint() -> Outpoint {
    Outpoint {
        txid: TransactionId::from([1; 32]),
        index: 0,
    }
}

/// `new` offers everything, which is what every deployment in this workspace
/// wants and what keeps it the obvious constructor.
#[tokio::test]
async fn new_offers_every_capability_the_tiers_can_answer() {
    let manifest = steady().await.view().serviceability();
    for capability in ChainCapability::ALL {
        assert_ne!(
            manifest.get(capability),
            Answerable::Absent,
            "{capability} should be offered by `new` over tiers that can answer it",
        );
    }
}

/// The builder starts at the core set: a deployment that names nothing extra
/// offers nothing extra, even though these tiers could supply all three.
#[tokio::test]
async fn the_builder_withholds_the_optional_capabilities_by_default() {
    let manifest = steady().await.builder().build().serviceability();
    assert_eq!(
        manifest.get(ChainCapability::SpendStatus),
        Answerable::Absent
    );
    assert_eq!(manifest.get(ChainCapability::TxOutSet), Answerable::Absent);
    // The core set is unaffected.
    assert_ne!(manifest.get(ChainCapability::Blocks), Answerable::Absent);
}

/// Naming a capability offers it, and only it.
#[tokio::test]
async fn serving_one_capability_does_not_offer_the_others() {
    let manifest = steady()
        .await
        .builder()
        .serving_spend_status()
        .build()
        .serviceability();
    assert_ne!(
        manifest.get(ChainCapability::SpendStatus),
        Answerable::Absent
    );
    assert_eq!(manifest.get(ChainCapability::TxOutSet), Answerable::Absent);
}

/// A withheld capability is refused by the read, not merely unadvertised.
///
/// A manifest that said `Absent` while the read answered anyway would be worse
/// than no manifest: a consumer that trusted it would route around a
/// capability the view was serving, and one that did not would get data the
/// operator meant to withhold.
#[tokio::test]
async fn a_withheld_capability_is_refused_by_the_read() {
    let refused = steady()
        .await
        .builder()
        .build()
        .snapshot()
        .outpoint_spenders(&[an_outpoint()], ChainScope::FullChain)
        .await;
    assert!(
        matches!(refused, Err(ChainViewError::NotServiceable(_))),
        "a withheld read must refuse, not answer: {refused:?}",
    );
}

/// Offering it makes the same read work, against the same tiers.
#[tokio::test]
async fn offering_it_makes_the_same_read_work() {
    let answered = steady()
        .await
        .builder()
        .serving_spend_status()
        .build()
        .snapshot()
        .outpoint_spenders(&[an_outpoint()], ChainScope::FullChain)
        .await;
    assert!(answered.is_ok(), "{answered:?}");
}

/// The txout set refuses and answers the same way.
#[tokio::test]
async fn the_txout_set_follows_the_same_rule() {
    let rig = steady().await;
    assert!(matches!(
        rig.builder().build().snapshot().txout_set().await,
        Err(ChainViewError::NotServiceable(_)),
    ));
    assert!(rig
        .builder()
        .serving_txout_set()
        .build()
        .snapshot()
        .txout_set()
        .await
        .is_ok());
}

/// Offering a capability is not a claim that it is answerable.
///
/// The builder is a floor, not a ceiling: the manifest still derives the height
/// from live coverage, so a deployment that offers spend status over a store
/// holding only genesis advertises it to genesis rather than to the tip.
#[tokio::test]
async fn offering_a_capability_does_not_override_coverage() {
    let view = rig(chain())
        .store_to(0)
        .build()
        .await
        .builder()
        .serving_spend_status()
        .build();
    // Store at genesis, window from 200: a hole, so spend status — which no
    // validator can fill — is capped below it rather than reaching the tip.
    assert_eq!(
        view.serviceability().get(ChainCapability::SpendStatus),
        Answerable::ToHeight(height(0)),
    );
}
