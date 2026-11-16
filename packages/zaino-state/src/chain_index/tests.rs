//! Zaino-State ChainIndex unit tests, on the shared mock validator (D-87).

use crate::chain_index::chain_view::BestTip as _;
mod chain_head;
mod eras;
mod mockchain_tests;
mod poll;
mod sync_loop;

pub(crate) fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(true)
        .try_init()
        .unwrap();
}

use crate::legacy::config::{DatabaseConfig, StorageConfig};
use tempfile::TempDir;
use tokio::time::Duration;
use zaino_source_mock::{Activations, BlockSpec, ChainBuilder, MockBlock, MockChain, NodeFacts};

use crate::{
    chain_index::{
        finalized_height_floor, validator_source::ValidatorSource, ChainIndex,
        NodeBackedChainIndex, NodeBackedChainIndexSubscriber,
    },
    ChainIndexConfig,
};
use zaino_chain_head::ChainHeadConfig;
use zaino_primitives::types::{rpc::StandardFee, ShieldedPool, Zatoshis};

/// The mock as ChainIndex consumes it: through the same `ValidatorSource`
/// conversion production runs.
pub(crate) type MockSource = ValidatorSource<MockChain>;

/// The suites' chain tip: genesis plus 300 blocks, so the finalised seam
/// (depth 100 under `cfg(test)`) sits at 200, above the first spends (101:
/// coinbases mature at 100 blocks).
const CHAIN_TIP: u32 = 300;

/// The tip [`MockchainMode::Active`] serves first; `mine` serves the rest.
const ACTIVE_TIP: u32 = 250;

/// Which tip the suites' source serves at start.
///
/// - `Active`: [`ACTIVE_TIP`]; tests advance it with `mine`.
/// - `Static`: [`CHAIN_TIP`]; the tip doesn't move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MockchainMode {
    Active,
    Static,
}

/// The suites' chain: every block spends three of its parent's coinbase
/// outputs, and every tenth adds two Sapling and two Orchard notes.
fn chain_blocks() -> Vec<MockBlock> {
    (1..=CHAIN_TIP)
        .fold(ChainBuilder::new(), |builder, height| {
            let spec = BlockSpec::spends(3);
            builder.block(if height % 10 == 0 {
                spec.notes(ShieldedPool::Sapling, 2)
                    .notes(ShieldedPool::Orchard, 2)
            } else {
                spec
            })
        })
        .into_blocks()
}

/// The fee the suites' validator answers `getstandardfee` with: ZIP 317's
/// marginal fee.
const STANDARD_FEE: u64 = 5_000;

/// `blocks` on `activations`, served through `ValidatorSource`.
fn mock_source(activations: &Activations, blocks: Vec<MockBlock>) -> MockSource {
    let facts = NodeFacts {
        upgrades: activations.upgrades(),
        standard_fee: Some(StandardFee {
            fee_per_action: Zatoshis::new(STANDARD_FEE).expect("the fee is in range"),
            version: 0,
        }),
        ..NodeFacts::regtest()
    };
    ValidatorSource::new(
        MockChain::new(facts, blocks),
        activations.activation_heights().to_regtest_network(),
        // No zebra state service, so no `ChainTipChange` stream, as over RPC.
        None,
    )
}

/// The suites' network: regtest on the default schedule.
fn regtest() -> zebra_chain::parameters::Network {
    Activations::default()
        .activation_heights()
        .to_regtest_network()
}

/// The served tip's height.
fn served_tip(source: &MockSource) -> u32 {
    source
        .source()
        .tip()
        .map_or(0, |block| u32::from(block.height()))
}

/// An index config over a fresh database in `dir`, on `activations`.
fn index_config(dir: &TempDir, activations: &Activations) -> ChainIndexConfig {
    ChainIndexConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: dir.path().to_path_buf(),
                ..Default::default()
            },
            ..Default::default()
        },
        ephemeral: false,
        mempool: Default::default(),
        db_version: 1,
        network: activations.activation_heights().to_regtest_network(),
    }
}

/// A synced index and what it was built from.
struct Synced {
    blocks: Vec<MockBlock>,
    indexer: NodeBackedChainIndex<MockSource>,
    reader: NodeBackedChainIndexSubscriber<MockSource>,
    source: MockSource,
    _dir: TempDir,
}

/// An index over the suites' chain, synced to the source's tip.
async fn synced(mode: MockchainMode) -> Synced {
    synced_with(mode, crate::chain_index::chain_head::config()).await
}

/// [`synced`] with the chain head on `chain_head_config`.
async fn synced_with(mode: MockchainMode, chain_head_config: ChainHeadConfig) -> Synced {
    let served = match mode {
        MockchainMode::Active => Some(ACTIVE_TIP),
        MockchainMode::Static => None,
    };
    synced_on(
        &Activations::default(),
        chain_blocks(),
        served,
        chain_head_config,
    )
    .await
}

/// An index over `blocks` on `activations`, the source serving up to
/// `served` (`None` = all), synced to the source's tip.
async fn synced_on(
    activations: &Activations,
    blocks: Vec<MockBlock>,
    served: Option<u32>,
    chain_head_config: ChainHeadConfig,
) -> Synced {
    init_tracing();
    let source = mock_source(activations, blocks.clone());
    if let Some(tip) = served {
        source.source().serve_to(tip);
    }

    let dir = tempfile::tempdir().expect("a temp dir");
    let indexer = NodeBackedChainIndex::new_with_chain_head_config(
        source.clone(),
        index_config(&dir, activations),
        chain_head_config,
    )
    .await
    .expect("the index starts");
    let reader = indexer.subscriber();
    wait_until_synced(&indexer, &reader, served_tip(&source)).await;

    Synced {
        blocks,
        indexer,
        reader,
        source,
        _dir: dir,
    }
}

/// Waits until the chain head serves `tip` and the finalised state holds
/// everything below the seam.
async fn wait_until_synced(
    indexer: &NodeBackedChainIndex<MockSource>,
    reader: &NodeBackedChainIndexSubscriber<MockSource>,
    tip: u32,
) {
    let floor = finalized_height_floor(tip).0;
    poll::poll_until(
        "the index to serve the tip with the finalised state at its floor",
        Duration::from_secs(60),
        Duration::from_millis(25),
        || async {
            let head = u32::from(reader.snapshot_nonfinalized_state().best_tip().height);
            let finalised = indexer
                .finalised_watermark()
                .tip
                .map(|tip| u32::from(tip.height));
            (head == tip && finalised.is_some_and(|height| height >= floor)).then_some(())
        },
    )
    .await;
}
