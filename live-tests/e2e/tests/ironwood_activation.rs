//! Wallet-tier predicates across the Orchard→Ironwood activation boundary.
//!
//! Every test here runs on the `nu6_3` boundary fixture — the hermetic replay
//! of what The Public Testnet did once at height 4,134,000: heights 2 through 5
//! are Orchard era, [`NU6_3_TRANSITION_BOUNDARY`] (6) onward is Ironwood era.
//! The fixture's tip sits at 4 with the faucet holding three Orchard-era
//! coinbase notes and the recipient unfunded. The schedule is pinned in the
//! fixture, and the validator, indexer, and wallet all adopt it (zainod and the
//! wallet both over the validator's `getblockchaininfo`); height drift between
//! them is unrepresentable.
//!
//! # The predicates, and where each era's cell is covered
//!
//! | predicate (wallet-observable)                | Orchard era | Ironwood era |
//! |----------------------------------------------|-------------|--------------|
//! | unified-address receipt lands in Orchard     | here        | false — `wallet_to_validator.rs` `send_to_unified` asserts the Orchard pool stays empty |
//! | unified-address receipt lands in Ironwood    | false (pool inactive) | `wallet_to_validator.rs` `send_to_unified` |
//! | shielded-receiver coinbase pays the era pool | here (wallet view); wire tier in `compact_block_wire.rs` | `wallet_to_validator.rs` `receives_mining_reward`; wire tier in `compact_block_wire.rs` |
//! | an Orchard note spends into an Ironwood receipt (ZIP 318 migration) | n/a (nothing to exit) | here |
//! | `shield` deposits into the era pool             | here        | `wallet_to_validator.rs` `shield_for_validator` |
//! | receipt pool flips between the last Orchard block and the activation block | here (boundary − 1) | here (exactly at the boundary) |
//! | Orchard pool value grows only below the boundary; Ironwood holds value only from it | here (per-height value pools) | here |
//!
//! Era composition of the *served* chain (coinbase routing, compact-block
//! action fields) is covered clientless in
//! `clientless/tests/compact_block_consistency.rs` and over the real gRPC
//! wire in `compact_block_wire.rs`; this file owns the cells that need a
//! wallet on both sides of the boundary.
//!
//! The Public Testnet cannot host the migration cell for us: its pre-NU6.3
//! epoch closed at height 4,134,000, no new value may enter Orchard from
//! there (post-activation Orchard actions permit only same-receiver change
//! or withdrawal — the cross-address restriction,
//! <https://zcash.github.io/ironwood/design/action-circuit.html#the-cross-address-restriction>),
//! and we hold no pre-activation Orchard TAZ — so this hermetic fixture is
//! the only controlled venue for it.

use std::time::Duration;

use anyhow::{Context, Result};
use rstest::rstest;
use zaino_testutils::{
    ActivationHeights, Backend, ChainCache, Fixture, Party, Pool, Validator as _, Wallet as _,
    Zainod, Zebrad, FILLER_ADDRESS, REGTEST_ACTIVATION_HEIGHTS,
};

use e2e::{assert_pool_absent, assert_pool_present};

/// Indexer sync timeout.
const READY: Duration = Duration::from_secs(120);
/// Standard transfer amount (zatoshis).
const SEND_AMOUNT: u64 = 250_000;
/// The ZIP-317 fee for a single-note shield round under regtest.
const SHIELD_FEE: u64 = 15_000;
/// The mid-chain NU6.3 (Ironwood) activation height: heights `[2, 6)` are
/// Orchard era, height 6 onward is Ironwood era.
const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

mod zebrad {
    use super::*;

    /// Orchard-era receipt: with the tip still below the boundary, the
    /// faucet's coinbase notes are Orchard notes (not Ironwood), and a
    /// unified-address send received before the boundary lands in the
    /// recipient's Orchard pool with the Ironwood pool exactly empty — the
    /// era-mirror of `wallet_to_validator.rs::send_to_unified`.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn unified_receipt_lands_in_orchard_before_boundary(
        #[case] backend: Backend,
    ) -> Result<()> {
        let cache = ChainCache::ensure_fixture(Fixture::boundary(
            "nu6_3",
            ActivationHeights {
                nu6_3: Some(NU6_3_TRANSITION_BOUNDARY),
                nu7: None,
                ..REGTEST_ACTIVATION_HEIGHTS
            },
            NU6_3_TRANSITION_BOUNDARY,
        ))
        .await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let faucet_balance = faucet.balances().await?;
        assert!(
            faucet_balance.get(Pool::Orchard) > 0,
            "pre-boundary coinbase should be an orchard note, got {faucet_balance:?}"
        );
        assert_eq!(
            faucet_balance.get(Pool::Ironwood),
            0,
            "no ironwood note can exist below the boundary, got {faucet_balance:?}"
        );

        let before = recipient.balances().await?;
        let ua = recipient.address(Pool::Orchard).await?;
        faucet.send(&ua, SEND_AMOUNT).await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        assert_eq!(tip, NU6_3_TRANSITION_BOUNDARY - 1);
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;

        let balance = recipient.balances().await?;
        assert_eq!(
            balance.get(Pool::Orchard) - before.get(Pool::Orchard),
            SEND_AMOUNT
        );
        assert_eq!(balance.get(Pool::Ironwood), 0);
        Ok(())
    }

    /// The ZIP 318 migration shape: an Orchard note minted before the
    /// boundary is spent after it, to a unified address, and the receipt
    /// lands in the Ironwood pool with the recipient's Orchard pool exactly
    /// empty. The faucet's Orchard balance must shrink: from the boundary
    /// the cross-address restriction limits each Orchard action to
    /// same-receiver change or withdrawal, so a genuine Orchard spend nets
    /// sent-amount-plus-fee out of the pool. The served per-height
    /// `valuePools` totals witness the Orchard pool growing only below the
    /// boundary and the Ironwood pool taking value only from it.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn orchard_note_spends_to_ironwood_across_boundary(
        #[case] backend: Backend,
    ) -> Result<()> {
        let cache = ChainCache::ensure_fixture(Fixture::boundary(
            "nu6_3",
            ActivationHeights {
                nu6_3: Some(NU6_3_TRANSITION_BOUNDARY),
                nu7: None,
                ..REGTEST_ACTIVATION_HEIGHTS
            },
            NU6_3_TRANSITION_BOUNDARY,
        ))
        .await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        let fixture_tip = usize::try_from(cache.manifest.fixture.height)?;
        let boundary = NU6_3_TRANSITION_BOUNDARY as usize;
        let migration_height = boundary + 1;
        // Per-height served value-pool totals, indexed by height; slots below
        // the fixture tip are never read.
        let mut orchard = vec![0u64; migration_height + 1];
        let mut ironwood = vec![0u64; migration_height + 1];
        // A missing pool or field is a failure, never a zero: the
        // below-boundary ironwood assertions would pass vacuously.
        let pool_zats = |info: &serde_json::Value, pool_id: &str| -> Result<u64> {
            info.get("valuePools")
                .and_then(serde_json::Value::as_array)
                .context("getblockchaininfo.valuePools")?
                .iter()
                .find(|pool| pool.get("id").and_then(serde_json::Value::as_str) == Some(pool_id))
                .with_context(|| format!("valuePools has no {pool_id} entry"))?
                .get("chainValueZat")
                .and_then(serde_json::Value::as_u64)
                .with_context(|| format!("valuePools[{pool_id}].chainValueZat"))
        };

        let irpc = indexer.json_rpc();
        let info = irpc
            .call_value("getblockchaininfo", serde_json::json!([]))
            .await?;
        orchard[fixture_tip] = pool_zats(&info, "orchard")?;
        ironwood[fixture_tip] = pool_zats(&info, "ironwood")?;

        let pre_boundary_balance = faucet.balances().await?;
        assert!(
            pre_boundary_balance.get(Pool::Orchard) > 0,
            "pre-boundary coinbase should be an orchard note, got {pre_boundary_balance:?}"
        );

        // Mine to the boundary one block at a time, coinbase to the faucet,
        // recording each height's served value pools.
        for height in (fixture_tip + 1)..=boundary {
            let tip = zebrad.generate_blocks(1).await?;
            assert_eq!(usize::try_from(tip)?, height);
            indexer.wait_for_block(tip, READY).await?;
            let info = irpc
                .call_value("getblockchaininfo", serde_json::json!([]))
                .await?;
            orchard[height] = pool_zats(&info, "orchard")?;
            ironwood[height] = pool_zats(&info, "ironwood")?;
        }

        faucet.sync().await?;
        let crossed_balance = faucet.balances().await?;
        let orchard_before_send = crossed_balance.get(Pool::Orchard);
        assert!(
            crossed_balance.get(Pool::Ironwood) > 0,
            "the boundary coinbase should be an ironwood note, got {crossed_balance:?}"
        );

        // The migration send: built at the boundary tip, it spends an
        // Orchard note into a unified-address (Ironwood) receipt.
        let before = recipient.balances().await?;
        let ua = recipient.address(Pool::Orchard).await?;
        faucet.send_from(&[Pool::Orchard], &ua, SEND_AMOUNT).await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        assert_eq!(usize::try_from(tip)?, migration_height);
        indexer.wait_for_block(tip, READY).await?;
        let info = irpc
            .call_value("getblockchaininfo", serde_json::json!([]))
            .await?;
        orchard[migration_height] = pool_zats(&info, "orchard")?;
        ironwood[migration_height] = pool_zats(&info, "ironwood")?;
        faucet.sync().await?;
        recipient.sync().await?;

        let balance = recipient.balances().await?;
        assert_eq!(
            balance.get(Pool::Ironwood) - before.get(Pool::Ironwood),
            SEND_AMOUNT
        );
        assert_eq!(balance.get(Pool::Orchard), 0);
        assert!(
            faucet.balances().await?.get(Pool::Orchard) < orchard_before_send,
            "the migration send must spend an orchard note"
        );

        // Per-height served value-pool assertions, over the heights actually read.
        for (height, &value) in ironwood.iter().enumerate().take(boundary).skip(fixture_tip) {
            assert_eq!(
                value, 0,
                "the ironwood pool must hold no value at height {height}, below the boundary"
            );
        }
        assert!(
            orchard[fixture_tip] > 0,
            "the fixture's pre-boundary coinbase must give the orchard pool value"
        );
        for height in (fixture_tip + 1)..boundary {
            assert!(
                orchard[height] > orchard[height - 1],
                "each pre-boundary coinbase must grow the orchard pool (height {height}: {} after {})",
                orchard[height],
                orchard[height - 1]
            );
        }
        assert_eq!(
            orchard[boundary],
            orchard[boundary - 1],
            "the orchard pool must hold exactly steady across the boundary edge"
        );
        assert!(
            ironwood[boundary] > 0,
            "the activation block's coinbase must give the ironwood pool its first value"
        );
        assert!(
            orchard[migration_height] < orchard[boundary],
            "the migration block must shrink the orchard pool ({} at the boundary, {} at the migration block)",
            orchard[boundary],
            orchard[migration_height]
        );
        Ok(())
    }

    /// The receipt pool flips between adjacent blocks: a send confirmed in
    /// the last Orchard-era block (boundary − 1) lands in Orchard, and a send
    /// built one block below the boundary but confirmed in the activation
    /// block itself lands in Ironwood. The second send is itself
    /// migration-shaped: constructed while the tip is still Orchard-era, it
    /// spends an Orchard note (the faucet's only spendable kind at that tip),
    /// so the activation block carries the Orchard spend's data alongside the
    /// Ironwood receipt.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn receipts_flip_pools_exactly_at_the_boundary(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure_fixture(Fixture::boundary(
            "nu6_3",
            ActivationHeights {
                nu6_3: Some(NU6_3_TRANSITION_BOUNDARY),
                nu7: None,
                ..REGTEST_ACTIVATION_HEIGHTS
            },
            NU6_3_TRANSITION_BOUNDARY,
        ))
        .await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        // The fixture's tip is boundary − 2, so the first send (built here)
        // confirms in the last Orchard-era block (boundary − 1).
        let before = recipient.balances().await?;
        let ua = recipient.address(Pool::Orchard).await?;
        let last_orchard_txid = faucet.send(&ua, SEND_AMOUNT).await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        assert_eq!(tip, NU6_3_TRANSITION_BOUNDARY - 1);
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;

        let balance = recipient.balances().await?;
        assert_eq!(
            balance.get(Pool::Orchard) - before.get(Pool::Orchard),
            SEND_AMOUNT,
            "receipt confirmed at boundary - 1 must be orchard, got {balance:?}"
        );
        assert_eq!(
            balance.get(Pool::Ironwood),
            0,
            "no ironwood receipt below the boundary, got {balance:?}"
        );

        // The second send is built at boundary − 1 (still Orchard era,
        // spending an Orchard note) but confirmed in the activation block.
        faucet.sync().await?;
        let first_ironwood_txid = faucet.send_from(&[Pool::Orchard], &ua, SEND_AMOUNT).await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        assert_eq!(tip, NU6_3_TRANSITION_BOUNDARY);
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;

        let balance = recipient.balances().await?;
        assert_eq!(
            balance.get(Pool::Ironwood) - before.get(Pool::Ironwood),
            SEND_AMOUNT
        );
        assert_eq!(
            balance.get(Pool::Orchard) - before.get(Pool::Orchard),
            SEND_AMOUNT,
            "the pre-boundary receipt must survive the flip unchanged"
        );

        // Served edge-block pool-presence: the last Orchard block and the
        // activation block.
        let blocks = indexer.get_block_range(1, tip).await?;
        let last_orchard_block = blocks
            .iter()
            .find(|b| b.height == u64::from(NU6_3_TRANSITION_BOUNDARY - 1))
            .expect("boundary-1 block served");
        let activation_block = blocks
            .iter()
            .find(|b| b.height == u64::from(NU6_3_TRANSITION_BOUNDARY))
            .expect("boundary block served");
        assert_eq!(
            last_orchard_block.height,
            u64::from(NU6_3_TRANSITION_BOUNDARY - 1)
        );
        assert_eq!(
            activation_block.height,
            u64::from(NU6_3_TRANSITION_BOUNDARY)
        );
        assert_pool_present(last_orchard_block, &last_orchard_txid, Pool::Orchard);
        assert_pool_absent(last_orchard_block, &last_orchard_txid, Pool::Ironwood);
        assert_pool_present(activation_block, &first_ironwood_txid, Pool::Ironwood);
        // The second send is migration-shaped: it spends an Orchard note, so
        // its compact form carries the Orchard spend's data too.
        assert_pool_present(activation_block, &first_ironwood_txid, Pool::Orchard);
        Ok(())
    }

    /// Below the boundary, `shield` deposits into the Orchard pool: the
    /// faucet funds the recipient's transparent address, the recipient
    /// shields, and the shielded balance (net of the ZIP-317 fee, mirroring
    /// `wallet_to_validator.rs::shield_for_validator`) lands in Orchard with the Ironwood
    /// pool exactly empty — the era-mirror of the Ironwood-era shield cell.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[tokio::test(flavor = "multi_thread")]
    async fn shield_deposits_to_orchard_before_boundary(#[case] backend: Backend) -> Result<()> {
        let cache = ChainCache::ensure_fixture(Fixture::boundary(
            "nu6_3",
            ActivationHeights {
                nu6_3: Some(NU6_3_TRANSITION_BOUNDARY),
                nu7: None,
                ..REGTEST_ACTIVATION_HEIGHTS
            },
            NU6_3_TRANSITION_BOUNDARY,
        ))
        .await?;
        let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
        let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
        let recipient = cache.wallet(Party::Recipient, &zebrad, &indexer).await?;

        // The fixture funds the recipient's transparent address in its tip
        // block, so the shield (targeting tip + 1) still lands in an
        // Orchard-era block.
        let before = recipient.balances().await?;
        let tip = cache.manifest.fixture.height;
        indexer.wait_for_block(tip, READY).await?;
        assert_eq!(
            before.get(Pool::Transparent),
            SEND_AMOUNT,
            "the boundary fixture's recipient holds one transparent receipt"
        );
        assert!(
            tip + 1 < NU6_3_TRANSITION_BOUNDARY,
            "the shield must target an Orchard-era block"
        );
        recipient.shield().await?;
        let tip = zebrad.generate_blocks_to(1, FILLER_ADDRESS).await?;
        assert!(tip < NU6_3_TRANSITION_BOUNDARY);
        indexer.wait_for_block(tip, READY).await?;
        recipient.sync().await?;

        let balance = recipient.balances().await?;
        assert_eq!(
            balance.get(Pool::Orchard) - before.get(Pool::Orchard),
            SEND_AMOUNT - SHIELD_FEE,
            "shielded balance must be the send net of the ZIP-317 fee \
             (below NU6.3 the shield deposits into the Orchard pool)"
        );
        assert_eq!(balance.get(Pool::Ironwood), 0);
        Ok(())
    }
}
