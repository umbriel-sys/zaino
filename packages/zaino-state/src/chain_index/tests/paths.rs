//! Every served RPC's data path, local or the validator's, read from the
//! validator's call log: with no store, and with the store mid-build (held
//! below the head's window, so a hole lies between them).

use std::time::Duration;

use super::poll::poll_until;
use super::{chain_blocks, index_config, mock_source, regtest, CHAIN_TIP};
use crate::chain_index::NodeBackedChainIndex;
use crate::error::ChainIndexErrorKind;
use crate::indexer::node_backed_indexer::NodeBackedIndexerServiceSubscriber;
use crate::{LightWalletIndexer as _, ZcashIndexer as _};
use zaino_proto::proto::service::{BlockId, TxFilter};
use zaino_source_mock::{Activations, MockChain, MINER_SCRIPT};
use zebra_rpc::client::{GetAddressBalanceRequest, GetAddressTxIdsRequest};

use crate::chain_index::chain_view::BestTip as _;

/// The store stops below the first block that adds notes (height 10).
const STORE_HELD_AT: u32 = 9;

/// A height in the hole between the store and the head's window.
const IN_HOLE: u32 = 150;

/// A height the held store holds.
const IN_STORE: u32 = 5;

struct Rig {
    chain: MockChain,
    rpc: NodeBackedIndexerServiceSubscriber<MockChain>,
    _index: NodeBackedChainIndex<MockChain>,
    _dir: tempfile::TempDir,
}

/// An index over the suites' chain, its store disabled (`ephemeral`) or held
/// at [`STORE_HELD_AT`], its head at the tip.
async fn rig(ephemeral: bool) -> Rig {
    let activations = Activations::default();
    let source = mock_source(&activations, chain_blocks());
    let chain = source.source().clone();
    if !ephemeral {
        chain.fail_ports(&["get_treestate"], None, zaino_source::FailureMode::Auth);
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let config = crate::config::ChainIndexConfig {
        ephemeral,
        ..index_config(&dir, &activations)
    };
    let index = NodeBackedChainIndex::new_with_chain_head_config(
        source,
        config,
        crate::chain_index::chain_head::config(),
    )
    .await
    .expect("the index starts");
    let reader = index.subscriber();
    poll_until(
        "the head at the tip, the store held",
        Duration::from_secs(60),
        Duration::from_millis(25),
        || async {
            let head = u32::from(
                crate::chain_index::ChainIndex::snapshot_nonfinalized_state(&reader)
                    .best_tip()
                    .height,
            );
            let store = index.finalised_tip().map(u32::from);
            let held = match ephemeral {
                true => store.is_none(),
                false => store == Some(STORE_HELD_AT),
            };
            (head == CHAIN_TIP && held).then_some(())
        },
    )
    .await;
    Rig {
        chain,
        rpc: NodeBackedIndexerServiceSubscriber::new_for_test(reader, regtest()),
        _index: index,
        _dir: dir,
    }
}

impl Rig {
    /// Whether `call` asked the validator's `port`.
    async fn asks<T>(&self, port: &str, call: impl std::future::Future<Output = T>) -> (bool, T) {
        let before = self.chain.calls(port);
        let answer = call.await;
        (self.chain.calls(port) > before, answer)
    }

    fn hash_at(&self, height: u32) -> String {
        let mut bytes =
            <[u8; 32]>::from(self.chain.block_at(height).expect("a served height").hash());
        bytes.reverse();
        hex::encode(bytes)
    }

    fn txid_at(&self, height: u32) -> String {
        let block = self.chain.block_at(height).expect("a served height");
        let mut bytes = <[u8; 32]>::from(block.block.transactions()[0].txid);
        bytes.reverse();
        hex::encode(bytes)
    }
}

fn miner() -> String {
    zaino_source_mock::address_of(&MINER_SCRIPT, zaino_primitives::types::Network::Regtest)
        .expect("the miner pays a standard script")
}

/// Asserts `call` asked the validator and answered.
fn asked<T: std::fmt::Debug>(
    (asked, answer): (bool, Result<T, crate::error::NodeBackedIndexerServiceError>),
    what: &str,
) -> T {
    let answer = answer.unwrap_or_else(|error| panic!("{what}: {error}"));
    assert!(asked, "{what} asks the validator, answered {answer:?}");
    answer
}

/// The reads that are the validator's whatever the store holds.
async fn validator_reads(rig: &Rig) {
    let hole = rig.hash_at(IN_HOLE);
    asked(
        rig.asks(
            "get_block_verbose_by_hash",
            rig.rpc.z_get_block(hole.clone(), Some(1)),
        )
        .await,
        "getblock",
    );
    asked(
        rig.asks("get_blockchain_info", rig.rpc.get_blockchain_info())
            .await,
        "getblockchaininfo",
    );
    asked(
        rig.asks("get_difficulty", rig.rpc.get_difficulty()).await,
        "getdifficulty",
    );
    asked(
        rig.asks("get_node_info", rig.rpc.get_info()).await,
        "getinfo",
    );
    asked(
        rig.asks("get_peer_info", rig.rpc.get_peer_info()).await,
        "getpeerinfo",
    );
    asked(
        rig.asks("get_mining_info", rig.rpc.get_mining_info()).await,
        "getmininginfo",
    );
    asked(
        rig.asks("get_network_sol_ps", rig.rpc.get_network_sol_ps(None, None))
            .await,
        "getnetworksolps",
    );
    assert!(
        rig.asks("get_block_subsidy", rig.rpc.get_block_subsidy(IN_HOLE))
            .await
            .0,
        "getblocksubsidy asks the validator"
    );
    asked(
        rig.asks(
            "get_standard_fee",
            crate::ZcashIndexer::get_standard_fee(&rig.rpc),
        )
        .await,
        "getstandardfee",
    );
    asked(
        rig.asks(
            "get_tx_out",
            rig.rpc.get_tx_out(rig.txid_at(IN_HOLE), 0, Some(false)),
        )
        .await,
        "gettxout",
    );
    asked(
        rig.asks(
            "get_transaction",
            rig.rpc.get_raw_transaction(rig.txid_at(IN_HOLE), Some(0)),
        )
        .await,
        "getrawtransaction",
    );
    asked(
        rig.asks(
            "get_transaction",
            rig.rpc.get_transaction(TxFilter {
                block: None,
                index: 0,
                hash: {
                    let mut bytes = hex::decode(rig.txid_at(IN_HOLE)).expect("hex");
                    bytes.reverse();
                    bytes
                },
            }),
        )
        .await,
        "GetTransaction",
    );
}

/// The reads that need the store and the head with nothing between them,
/// answered by the validator over the hole.
async fn reads_over_the_hole(rig: &Rig) {
    let hole = rig.hash_at(IN_HOLE);
    asked(
        rig.asks("get_block_header", rig.rpc.get_block_header(hole.clone()))
            .await,
        "getblockheader",
    );
    asked(
        rig.asks("get_block_deltas", rig.rpc.get_block_deltas(hole.clone()))
            .await,
        "getblockdeltas",
    );
    asked(
        rig.asks(
            "get_address_balance",
            rig.rpc
                .z_get_address_balance(GetAddressBalanceRequest::new(vec![miner()])),
        )
        .await,
        "getaddressbalance",
    );
    asked(
        rig.asks(
            "get_address_txids",
            rig.rpc.get_address_tx_ids(GetAddressTxIdsRequest::new(
                vec![miner()],
                Some(1),
                Some(CHAIN_TIP),
            )),
        )
        .await,
        "getaddresstxids",
    );
    asked(
        rig.asks(
            "get_address_utxos",
            rig.rpc
                .z_get_address_utxos(GetAddressBalanceRequest::new(vec![miner()])),
        )
        .await,
        "getaddressutxos",
    );
    asked(
        rig.asks(
            "get_pre_index_compact_block",
            rig.rpc.get_block(BlockId {
                height: u64::from(IN_HOLE),
                hash: Vec::new(),
            }),
        )
        .await,
        "GetBlock",
    );
}

/// The reads Zaino answers from its own state whatever the store holds.
async fn local_reads(rig: &Rig) {
    let count = rig.rpc.get_block_count().await.expect("getblockcount");
    assert_eq!(count.0, CHAIN_TIP);
    let best = rig
        .rpc
        .get_best_blockhash()
        .await
        .expect("getbestblockhash");
    assert_eq!(best.hash().to_string(), rig.hash_at(CHAIN_TIP));
    let (asked, tips) = rig.asks("get_chain_tips", rig.rpc.get_chain_tips()).await;
    assert!(!asked, "getchaintips is the head's");
    assert!(!tips.expect("getchaintips").is_empty());
    let (asked, _) = rig
        .asks("get_node_info", rig.rpc.validate_address(miner()))
        .await;
    assert!(!asked, "validateaddress is pure");
}

/// With no store every read the store would answer is the validator's.
#[tokio::test(flavor = "multi_thread")]
async fn with_no_store_reads_are_the_validators() {
    let rig = rig(true).await;
    local_reads(&rig).await;
    validator_reads(&rig).await;
    reads_over_the_hole(&rig).await;
    let refused = rig
        .rpc
        .get_tx_out_set_info()
        .await
        .expect_err("no store builds the set");
    assert!(matches!(
        refused,
        crate::error::NodeBackedIndexerServiceError::ChainIndexError(ref error)
            if error.kind() == ChainIndexErrorKind::Unimplemented
    ));
}

/// With the store mid-build, what it holds is local and the hole is the
/// validator's; the txout set is retryable.
#[tokio::test(flavor = "multi_thread")]
async fn with_the_store_mid_build_the_hole_is_the_validators() {
    let rig = rig(false).await;
    local_reads(&rig).await;
    validator_reads(&rig).await;
    reads_over_the_hole(&rig).await;
    let (asked, header) = rig
        .asks(
            "get_block_header",
            rig.rpc.get_block_header(rig.hash_at(IN_STORE)),
        )
        .await;
    assert!(!asked, "a header the store holds is local");
    header.expect("getblockheader");
    let refused = rig
        .rpc
        .get_tx_out_set_info()
        .await
        .expect_err("the set is still building");
    assert!(matches!(
        refused,
        crate::error::NodeBackedIndexerServiceError::ChainIndexError(ref error)
            if error.kind() == ChainIndexErrorKind::Unavailable
    ));
}
