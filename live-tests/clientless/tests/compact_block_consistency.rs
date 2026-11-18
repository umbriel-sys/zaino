//! Per-block consistency between served compact-block content and its chain metadata.
//!
//! A compact block's `chainMetadata` commitment-tree sizes are cumulative counts of the
//! note commitments the chain has produced. A scanning wallet advances its trees by the
//! actions/outputs each served block carries, so whenever a served block's tree-size
//! delta disagrees with its served commitment count the wallet observes a tree-size
//! discontinuity and treats it as a chain reorg. This walk pins that invariant per
//! block, per pool, for the request shape real (including pre-Ironwood) light clients
//! send: an empty `poolTypes` filter.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use zaino_testutils::{
    legacy_parser::block::FullBlock, ActivationHeights, Backend, ChainCache, Fixture,
    Validator as _, Zainod, Zebrad, REGTEST_ACTIVATION_HEIGHTS,
};

const READY: Duration = Duration::from_secs(120);

/// The mid-chain NU6.3 (Ironwood) activation height for the transition fixture:
/// an Orchard era `[2, 6)` that flips to Ironwood at height 6.
const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

fn nu6_3_boundary() -> Fixture {
    Fixture::boundary(
        "nu6_3",
        ActivationHeights {
            nu6_3: Some(NU6_3_TRANSITION_BOUNDARY),
            nu7: None,
            ..REGTEST_ACTIVATION_HEIGHTS
        },
        NU6_3_TRANSITION_BOUNDARY,
    )
}

/// The pool a coinbase reward lands in. One per network-upgrade era, and the
/// transaction version that carries it; `Transparent` is a filler block's
/// coinbase, which no shielded pool routes.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum CoinbaseEra {
    Sapling,
    Orchard,
    Ironwood,
    Transparent,
}

#[tokio::test(flavor = "multi_thread")]
async fn unfiltered_compact_blocks_match_chain_metadata_zebrad() -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

    let tip = cache.manifest.fixture.height;
    indexer.wait_for_block(tip, READY).await?;

    // The independent oracle: zebrad's own per-block `(sapling, orchard, ironwood)` tree
    // sizes, computed without reference to what zaino serves. A pool's key is omitted
    // when its tree is empty, so a missing key reads as size 0.
    let vrpc = zebrad.rpc();
    let mut oracle: Vec<(u64, u64, u64)> = Vec::new();
    for height in 0..=u64::from(tip) {
        let block = vrpc
            .call_value("getblock", json!([height.to_string(), 1]))
            .await?;
        let trees = block
            .get("trees")
            .context("verbose getblock must carry a trees field")?;
        let size = |pool: &str| {
            trees
                .get(pool)
                .and_then(|t| t.get("size"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        oracle.push((size("sapling"), size("orchard"), size("ironwood")));
    }

    // The empty pool filter is what unfiltered (pre-Ironwood) clients send; the served
    // stream must include every shielded pool's actions.
    let blocks = indexer.get_block_range(1, tip).await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "the served range must cover every height in [1, {tip}]"
    );

    let (mut prev_sapling, mut prev_orchard, mut prev_ironwood) = oracle[0];
    let mut total_orchard_actions = 0u64;
    let mut total_ironwood_actions = 0u64;
    for (index, block) in blocks.iter().enumerate() {
        assert_eq!(
            block.height,
            index as u64 + 1,
            "served blocks must be contiguous for the walk's running totals"
        );
        let metadata = block
            .chain_metadata
            .as_ref()
            .context("every served compact block carries chain metadata")?;

        let sapling_outputs: u64 = block.vtx.iter().map(|tx| tx.outputs.len() as u64).sum();
        let orchard_actions: u64 = block.vtx.iter().map(|tx| tx.actions.len() as u64).sum();
        let ironwood_actions: u64 = block
            .vtx
            .iter()
            .map(|tx| tx.ironwood_actions.len() as u64)
            .sum();
        total_orchard_actions += orchard_actions;
        total_ironwood_actions += ironwood_actions;

        let sapling_size = u64::from(metadata.sapling_commitment_tree_size);
        let orchard_size = u64::from(metadata.orchard_commitment_tree_size);
        let ironwood_size = u64::from(metadata.ironwood_commitment_tree_size);

        assert_eq!(
            sapling_size,
            prev_sapling + sapling_outputs,
            "sapling tree-size delta must equal the served output count at height {}",
            block.height
        );
        assert_eq!(
            orchard_size,
            prev_orchard + orchard_actions,
            "orchard tree-size delta must equal the served action count at height {}",
            block.height
        );
        // The regression this walk exists for: a served block whose metadata counts
        // commitments from actions the block omits (e.g. ironwood stripped from an
        // unfiltered request) reads to a scanning wallet as a phantom chain reorg.
        assert_eq!(
            ironwood_size,
            prev_ironwood + ironwood_actions,
            "ironwood tree-size delta must equal the served action count at height {}",
            block.height
        );

        // Clientless-exclusive predicate — oracle parity. Package tests cannot express
        // this: their "source of truth" is the object being served, so any such
        // comparison is circular.
        let (oracle_sapling, oracle_orchard, oracle_ironwood) = oracle[block.height as usize];
        assert_eq!(
            sapling_size, oracle_sapling,
            "served sapling tree size must match the validator's own at height {}",
            block.height
        );
        assert_eq!(
            orchard_size, oracle_orchard,
            "served orchard tree size must match the validator's own at height {}",
            block.height
        );
        assert_eq!(
            ironwood_size, oracle_ironwood,
            "served ironwood tree size must match the validator's own at height {}",
            block.height
        );

        prev_sapling = sapling_size;
        prev_orchard = orchard_size;
        prev_ironwood = ironwood_size;
    }

    assert!(
        total_ironwood_actions > 0,
        "the fixture produced no ironwood actions; the walk asserted nothing about ironwood"
    );
    // The counterpart of the guard above: NU6.3 is active from height 2, so consensus
    // requires an empty Orchard coinbase component and every shielded spend lands in
    // Ironwood; the chain carries no Orchard actions at all. Together the two totals
    // distinguish failure modes: pool-swap (orchard > 0, ironwood == 0: ironwood served
    // under the orchard field) vs pool-drop (both zero) vs a broken routing premise.
    assert_eq!(
        total_orchard_actions, 0,
        "an NU6.3-from-2 chain must carry no Orchard actions"
    );

    Ok(())
}

/// Orchard-only era: below the NU6.3 boundary every post-NU5 coinbase is an Orchard
/// coinbase and no ironwood ever appears. The boundary fixture's tip sits two blocks
/// under the activation, so the whole chain is the Orchard era.
#[tokio::test(flavor = "multi_thread")]
async fn orchard_only_coinbase_routing_zebrad() -> Result<()> {
    let cache = ChainCache::ensure_fixture(Fixture::orchard_only()).await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

    let tip = cache.manifest.fixture.height;
    assert_eq!(
        cache.manifest.fixture.heights.nu6_3, None,
        "premise: the orchard-only fixture never activates NU6.3"
    );
    indexer.wait_for_block(tip, READY).await?;

    let blocks = indexer.get_block_range(1, tip).await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "the served range must cover every height in [1, {tip}]"
    );

    // Two independent halves per height, collected across the whole chain so one run
    // separates them: the raw-block predicate is class 1 (zebrad's own consensus
    // routing, zaino uninvolved), the raw-vs-served comparison is class 2 (zaino's
    // pool routing on the wire). <https://github.com/zingolabs/zaino/issues/1368>
    let vrpc = zebrad.rpc();
    let mut violations: Vec<String> = Vec::new();
    for (index, served) in blocks.iter().enumerate() {
        let height = index as u64 + 1;
        assert_eq!(served.height, height, "served blocks must be contiguous");

        let raw = vrpc
            .call_value("getblock", json!([height.to_string(), 0]))
            .await?;
        let raw = zaino_testutils::hex::decode(
            raw.as_str()
                .context("verbosity-0 getblock returns a hex string")?,
            "getblock verbosity 0",
        )?;
        let coinbase = FullBlock::parse_from_hex(&raw, None)?
            .transactions()
            .into_iter()
            .next()
            .context("every block carries a coinbase transaction")?;

        // A coinbase input is the single null prevout (all-zero hash, index u32::MAX).
        let inputs = coinbase.transparent_inputs();
        let is_coinbase = matches!(inputs.as_slice(), [(prevout, u32::MAX, _)] if prevout.iter().all(|b| *b == 0));
        let version = coinbase.version();
        let sapling = coinbase.shielded_outputs().len();
        let orchard = coinbase.orchard_actions().len();
        let ironwood = coinbase.ironwood_actions().len();

        // The reward lands in exactly one pool; anything else is `None` and fails.
        let observed = match (version, sapling, orchard, ironwood) {
            (4, 1.., 0, 0) => Some(CoinbaseEra::Sapling),
            (5, 0, 1.., 0) => Some(CoinbaseEra::Orchard),
            (6, 0, 0, 1..) => Some(CoinbaseEra::Ironwood),
            (5.., 0, 0, 0) => Some(CoinbaseEra::Transparent),
            _ => None,
        };

        let expected = match height {
            h if h >= 2 => CoinbaseEra::Orchard,
            _ => CoinbaseEra::Sapling,
        };

        // The coinbase alone (index 0): the fixture's blocks also carry sends.
        let coinbase = served.vtx.iter().filter(|tx| tx.index == 0);
        let served_orchard: usize = coinbase.clone().map(|tx| tx.actions.len()).sum();
        let served_ironwood: usize = coinbase.map(|tx| tx.ironwood_actions.len()).sum();
        let wire_ok = served_orchard == orchard && served_ironwood == ironwood;

        if !is_coinbase || observed != Some(expected) || !wire_ok {
            violations.push(format!(
                "height {height}: want {expected:?}, raw is {observed:?} \
                 (is_coinbase {is_coinbase}, v{version}, sapling {sapling}, \
                 orchard {orchard}, ironwood {ironwood}); \
                 zaino served orchard {served_orchard}, ironwood {served_ironwood}"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "coinbase routing mismatches ({} of {} heights):\n{}",
        violations.len(),
        blocks.len(),
        violations.join("\n")
    );

    Ok(())
}

/// Ironwood-only era: NU6.3 active from height 2 (with every prior upgrade), so every
/// post-activation shielded coinbase is an Ironwood coinbase and no Orchard coinbase
/// ever appears. Filler blocks pay a transparent coinbase, which routes nowhere; the
/// walk requires at least one Ironwood coinbase so the era predicate is not vacuous.
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_only_coinbase_routing_zebrad() -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

    let tip = cache.manifest.fixture.height;
    indexer.wait_for_block(tip, READY).await?;

    let blocks = indexer.get_block_range(1, tip).await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "the served range must cover every height in [1, {tip}]"
    );

    // Two independent halves per height, collected across the whole chain so one run
    // separates them: the raw-block predicate is class 1 (zebrad's own consensus
    // routing, zaino uninvolved), the raw-vs-served comparison is class 2 (zaino's
    // pool routing on the wire). <https://github.com/zingolabs/zaino/issues/1368>
    let vrpc = zebrad.rpc();
    let mut violations: Vec<String> = Vec::new();
    let mut ironwood_coinbases = 0usize;
    for (index, served) in blocks.iter().enumerate() {
        let height = index as u64 + 1;
        assert_eq!(served.height, height, "served blocks must be contiguous");

        let raw = vrpc
            .call_value("getblock", json!([height.to_string(), 0]))
            .await?;
        let raw = zaino_testutils::hex::decode(
            raw.as_str()
                .context("verbosity-0 getblock returns a hex string")?,
            "getblock verbosity 0",
        )?;
        let coinbase = FullBlock::parse_from_hex(&raw, None)?
            .transactions()
            .into_iter()
            .next()
            .context("every block carries a coinbase transaction")?;

        // A coinbase input is the single null prevout (all-zero hash, index u32::MAX).
        let inputs = coinbase.transparent_inputs();
        let is_coinbase = matches!(inputs.as_slice(), [(prevout, u32::MAX, _)] if prevout.iter().all(|b| *b == 0));
        let version = coinbase.version();
        let sapling = coinbase.shielded_outputs().len();
        let orchard = coinbase.orchard_actions().len();
        let ironwood = coinbase.ironwood_actions().len();

        // The reward lands in exactly one pool; anything else is `None` and fails.
        let observed = match (version, sapling, orchard, ironwood) {
            (4, 1.., 0, 0) => Some(CoinbaseEra::Sapling),
            (5, 0, 1.., 0) => Some(CoinbaseEra::Orchard),
            (6, 0, 0, 1..) => Some(CoinbaseEra::Ironwood),
            (5.., 0, 0, 0) => Some(CoinbaseEra::Transparent),
            _ => None,
        };
        if observed == Some(CoinbaseEra::Ironwood) {
            ironwood_coinbases += 1;
        }

        let era_ok = match height {
            h if h >= 2 => matches!(
                observed,
                Some(CoinbaseEra::Ironwood | CoinbaseEra::Transparent)
            ),
            _ => observed == Some(CoinbaseEra::Sapling),
        };

        // The coinbase alone (index 0): the fixture's blocks also carry sends.
        let coinbase = served.vtx.iter().filter(|tx| tx.index == 0);
        let served_orchard: usize = coinbase.clone().map(|tx| tx.actions.len()).sum();
        let served_ironwood: usize = coinbase.map(|tx| tx.ironwood_actions.len()).sum();
        let wire_ok = served_orchard == orchard && served_ironwood == ironwood;

        if !is_coinbase || !era_ok || !wire_ok {
            violations.push(format!(
                "height {height}: want Ironwood (or a transparent filler coinbase), raw is \
                 {observed:?} (is_coinbase {is_coinbase}, v{version}, sapling {sapling}, \
                 orchard {orchard}, ironwood {ironwood}); \
                 zaino served orchard {served_orchard}, ironwood {served_ironwood}"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "coinbase routing mismatches ({} of {} heights):\n{}",
        violations.len(),
        blocks.len(),
        violations.join("\n")
    );
    assert!(
        ironwood_coinbases > 0,
        "the fixture carries no Ironwood coinbase; the era predicate asserted nothing"
    );

    Ok(())
}

/// The transition: the same unchanged orchard-receiver miner produces Orchard
/// coinbases through NU6.2 and Ironwood coinbases from the NU6.3 activation height —
/// each predicate exactly delimiting its era, so a mis-timed flip fails on both sides
/// of the boundary.
#[tokio::test(flavor = "multi_thread")]
async fn orchard_coinbase_routing_flips_to_ironwood_at_activation_zebrad() -> Result<()> {
    let cache = ChainCache::ensure_fixture(nu6_3_boundary()).await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;

    // Two blocks past the boundary, so both eras carry more than one block.
    let tip = zebrad
        .generate_blocks(NU6_3_TRANSITION_BOUNDARY + 2 - cache.manifest.fixture.height)
        .await?;
    assert_eq!(tip, NU6_3_TRANSITION_BOUNDARY + 2);
    indexer.wait_for_block(tip, READY).await?;

    let blocks = indexer.get_block_range(1, tip).await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "the served range must cover every height in [1, {tip}]"
    );

    // Two independent halves per height, collected across the whole chain so one run
    // separates them: the raw-block predicate is class 1 (zebrad's own consensus
    // routing, zaino uninvolved), the raw-vs-served comparison is class 2 (zaino's
    // pool routing on the wire). <https://github.com/zingolabs/zaino/issues/1368>
    let vrpc = zebrad.rpc();
    let mut violations: Vec<String> = Vec::new();
    for (index, served) in blocks.iter().enumerate() {
        let height = index as u64 + 1;
        assert_eq!(served.height, height, "served blocks must be contiguous");

        let raw = vrpc
            .call_value("getblock", json!([height.to_string(), 0]))
            .await?;
        let raw = zaino_testutils::hex::decode(
            raw.as_str()
                .context("verbosity-0 getblock returns a hex string")?,
            "getblock verbosity 0",
        )?;
        let coinbase = FullBlock::parse_from_hex(&raw, None)?
            .transactions()
            .into_iter()
            .next()
            .context("every block carries a coinbase transaction")?;

        // A coinbase input is the single null prevout (all-zero hash, index u32::MAX).
        let inputs = coinbase.transparent_inputs();
        let is_coinbase = matches!(inputs.as_slice(), [(prevout, u32::MAX, _)] if prevout.iter().all(|b| *b == 0));
        let version = coinbase.version();
        let sapling = coinbase.shielded_outputs().len();
        let orchard = coinbase.orchard_actions().len();
        let ironwood = coinbase.ironwood_actions().len();

        // The reward lands in exactly one pool; anything else is `None` and fails.
        let observed = match (version, sapling, orchard, ironwood) {
            (4, 1.., 0, 0) => Some(CoinbaseEra::Sapling),
            (5, 0, 1.., 0) => Some(CoinbaseEra::Orchard),
            (6, 0, 0, 1..) => Some(CoinbaseEra::Ironwood),
            (5.., 0, 0, 0) => Some(CoinbaseEra::Transparent),
            _ => None,
        };

        let expected = match height {
            h if h >= u64::from(NU6_3_TRANSITION_BOUNDARY) => CoinbaseEra::Ironwood,
            h if h >= 2 => CoinbaseEra::Orchard,
            _ => CoinbaseEra::Sapling,
        };

        // The coinbase alone (index 0): the fixture's blocks also carry sends.
        let coinbase = served.vtx.iter().filter(|tx| tx.index == 0);
        let served_orchard: usize = coinbase.clone().map(|tx| tx.actions.len()).sum();
        let served_ironwood: usize = coinbase.map(|tx| tx.ironwood_actions.len()).sum();
        let wire_ok = served_orchard == orchard && served_ironwood == ironwood;

        if !is_coinbase || observed != Some(expected) || !wire_ok {
            violations.push(format!(
                "height {height}: want {expected:?}, raw is {observed:?} \
                 (is_coinbase {is_coinbase}, v{version}, sapling {sapling}, \
                 orchard {orchard}, ironwood {ironwood}); \
                 zaino served orchard {served_orchard}, ironwood {served_ironwood}"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "coinbase routing mismatches ({} of {} heights):\n{}",
        violations.len(),
        blocks.len(),
        violations.join("\n")
    );

    Ok(())
}
