use super::*;
use crate::scripted::{hash, height, Scripted};
use crate::{GetChainTip as _, GetChainTipError, SubscribeBlocks as _, SubscribeChainTip as _};

/// Every transient mode: retried.
const TRANSIENT: [FailureMode; 5] = [
    FailureMode::Connection,
    FailureMode::Timeout,
    FailureMode::HttpStatus(503),
    FailureMode::RpcError(WORK_QUEUE_FULL),
    FailureMode::RpcError(IN_WARMUP),
];

/// Every fatal mode: passed through at once.
const FATAL: [FailureMode; 4] = [
    FailureMode::HttpStatus(404),
    FailureMode::RpcError(-8),
    FailureMode::Parse,
    FailureMode::Auth,
];

fn policy(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        initial_delay: Duration::from_millis(100),
        backoff_factor: 2.0,
        max_delay: Duration::from_millis(300),
    }
}

/// A client over a source with a tip, failing first with `modes`.
fn client(modes: impl IntoIterator<Item = FailureMode>) -> ValidatorClient<Scripted> {
    ValidatorClient::new(
        Scripted::default()
            .with_tip(hash(1), height(5))
            .failing_with(modes),
        policy(5),
    )
}

#[test]
fn classification() {
    for mode in TRANSIENT {
        assert!(is_retryable(&mode), "{mode:?}");
    }
    for mode in FATAL.into_iter().chain([FailureMode::HttpStatus(400)]) {
        assert!(!is_retryable(&mode), "{mode:?}");
    }
    assert!(is_retryable(&FailureMode::HttpStatus(500)));
}

#[test]
fn delay_exponential_backoff() {
    let policy = RetryPolicy {
        max_delay: Duration::from_secs(10),
        ..policy(5)
    };
    assert_eq!(policy.delay_for(1), Duration::from_millis(100));
    assert_eq!(policy.delay_for(2), Duration::from_millis(200));
    assert_eq!(policy.delay_for(3), Duration::from_millis(400));
}

#[test]
fn delay_capped_at_max() {
    assert_eq!(policy(5).delay_for(3), Duration::from_millis(300));
}

/// Each transient mode is retried, and the answer after it returned.
#[tokio::test(start_paused = true)]
async fn every_transient_mode_is_retried() {
    for mode in TRANSIENT {
        let client = client([mode.clone()]);
        let tip = client.get_chain_tip().await;
        assert_eq!(tip.ok(), Some((hash(1), height(5))), "{mode:?}");
        assert_eq!(client.inner().calls(), 2, "{mode:?}");
    }
}

/// Each fatal mode is passed through on the first attempt.
#[tokio::test(start_paused = true)]
async fn every_fatal_mode_is_passed_through() {
    for mode in FATAL {
        let client = client([mode.clone()]);
        match client.get_chain_tip().await {
            Err(SourceError::NonDomain(error)) => assert_eq!(error.mode, mode),
            other => panic!("{mode:?}: expected NonDomain, got {other:?}"),
        }
        assert_eq!(client.inner().calls(), 1, "{mode:?}");
    }
}

/// A transient failure on every attempt is `Unavailable` after exactly
/// `max_attempts`, carrying the last failure.
#[tokio::test(start_paused = true)]
async fn exhausted_retries_are_unavailable() {
    let mut modes = vec![FailureMode::Timeout; 4];
    modes.push(FailureMode::Connection);
    let client = client(modes);
    match client.get_chain_tip().await {
        Err(SourceError::Unavailable(unavailable)) => {
            assert_eq!(unavailable.attempts, 5);
            assert_eq!(unavailable.last_error.mode, FailureMode::Connection);
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert_eq!(client.inner().calls(), 5);
}

/// A domain answer is the answer: never retried.
#[tokio::test(start_paused = true)]
async fn a_domain_error_is_not_retried() {
    let client = ValidatorClient::new(Scripted::default(), policy(5));
    assert!(matches!(
        client.get_chain_tip().await,
        Err(SourceError::Domain(GetChainTipError::NotReady))
    ));
    assert_eq!(client.inner().calls(), 1);
}

/// The waits between attempts follow the policy: 100, 200, then capped at
/// 300 ms.
#[tokio::test(start_paused = true)]
async fn retries_wait_as_the_policy_says() {
    let client = client(vec![FailureMode::Timeout; 4]);
    let start = tokio::time::Instant::now();
    client
        .get_chain_tip()
        .await
        .expect("answers on the fifth attempt");
    assert_eq!(
        start.elapsed(),
        Duration::from_millis(100 + 200 + 300 + 300)
    );
}

/// Subscriptions are the adapter's own, handed through unretried.
#[tokio::test]
async fn subscriptions_are_forwarded() {
    let client = client([]);
    let mut blocks = client
        .subscribe_to_blocks_received()
        .expect("the adapter has a push path");
    client.inner().wake_blocks();
    blocks.changed().await.expect("the adapter's wake arrives");
    assert!(client.subscribe_to_chain_tip().is_none());
}

/// Every generated port: the scripted transient failures are retried, then
/// the call reaches the adapter's own method with the caller's arguments,
/// and its fatal failure comes back unchanged.
mod generated {
    use super::*;
    use crate::scripted::call;
    use zaino_primitives::types::{
        rpc::SpentOutpoint, AddressBalance, AddressDelta, Block, BlockHash, BlockVerbose,
        BlockchainInfo, Difficulty, Height, OutputIndex, PreIndexCompactBlock, ShieldedPool,
        SubtreeRoot, TransactionId, TreeRoots, Treestate, Utxo,
    };

    /// The client after its adapter failed twice, transiently, then fatally.
    fn transient_twice() -> ValidatorClient<Scripted> {
        ValidatorClient::new(
            Scripted::default().failing_with([FailureMode::Timeout, FailureMode::Connection]),
            policy(5),
        )
    }

    fn assert_reached<E: core::fmt::Debug + core::fmt::Display>(
        client: &ValidatorClient<Scripted>,
        error: SourceError<E>,
        expected: &str,
    ) {
        match error {
            SourceError::NonDomain(error) => {
                assert_eq!(
                    (error.mode, error.message.as_str()),
                    (FailureMode::Auth, expected)
                );
            }
            other => panic!("{expected}: expected the adapter's fatal failure, got {other:?}"),
        }
        assert_eq!(client.inner().calls(), 3, "{expected}");
    }

    fn txid() -> TransactionId {
        TransactionId::from([9; 32])
    }

    /// Implements each port on [`Scripted`] and tests its generated twin on
    /// [`ValidatorClient`].
    macro_rules! ports {
        ($($twin:ident / $one_shot:ident :: $method:ident
            ($($arg:ident : $ty:ty = $value:expr),*) -> $ok:ty, $err:ty;)*) => {$(
            impl crate::$one_shot for Scripted {
                async fn $method(&self $(, $arg: $ty)*) -> Result<$ok, QueryError<$err>> {
                    let failure = self
                        .attempt()
                        .unwrap_or_else(|| Scripted::fatal(call(stringify!($method), &($($arg,)*))));
                    Err(QueryError::NonDomain(failure))
                }
            }

            #[tokio::test(start_paused = true)]
            async fn $method() {
                $(let $arg: $ty = $value;)*
                let expected = call(stringify!($method), &($(&$arg,)*));
                let client = transient_twice();
                let error = <ValidatorClient<Scripted> as crate::$twin>::$method(&client $(, $arg)*)
                    .await
                    .expect_err("the script ends in a fatal failure");
                assert_reached(&client, error, &expected);
            }
        )*};
    }

    ports! {
        GetAddressBalance / OneShotGetAddressBalance :: get_address_balance(
            addresses: Vec<String> = vec!["t-addr".to_owned()]
        ) -> AddressBalance, crate::GetAddressBalanceError;
        GetAddressDeltas / OneShotGetAddressDeltas :: get_address_deltas(
            addresses: Vec<String> = vec!["t-addr".to_owned()],
            start: Height = height(3),
            end: Height = height(9)
        ) -> Vec<AddressDelta>, crate::GetAddressDeltasError;
        GetAddressTxids / OneShotGetAddressTxids :: get_address_txids(
            addresses: Vec<String> = vec!["t-addr".to_owned()],
            start: Height = height(3),
            end: Height = height(9)
        ) -> Vec<TransactionId>, crate::GetAddressTxidsError;
        GetAddressUtxos / OneShotGetAddressUtxos :: get_address_utxos(
            addresses: Vec<String> = vec!["t-addr".to_owned()]
        ) -> Vec<Utxo>, crate::GetAddressUtxosError;
        GetBestBlockHeight / OneShotGetBestBlockHeight :: get_best_block_height()
            -> Height, crate::GetBestBlockHeightError;
        GetBlock / OneShotGetBlock :: get_block(height: Height = height(7))
            -> Block, crate::GetBlockError;
        GetBlockByHash / OneShotGetBlockByHash :: get_block_by_hash(hash: BlockHash = hash(7))
            -> Block, crate::GetBlockByHashError;
        GetBlockchainInfo / OneShotGetBlockchainInfo :: get_blockchain_info()
            -> BlockchainInfo, crate::GetBlockchainInfoError;
        GetBlockDeltas / OneShotGetBlockDeltas :: get_block_deltas(hash: BlockHash = hash(7))
            -> zaino_primitives::types::rpc::BlockDeltas, crate::GetBlockDeltasError;
        GetBlockHeader / OneShotGetBlockHeader :: get_block_header(hash: BlockHash = hash(7))
            -> zaino_primitives::types::rpc::BlockHeaderVerbose, crate::GetBlockHeaderError;
        GetBlockSubsidy / OneShotGetBlockSubsidy :: get_block_subsidy(height: Height = height(7))
            -> zaino_primitives::types::rpc::BlockSubsidy, crate::GetBlockSubsidyError;
        GetBlockVerbose / OneShotGetBlockVerbose :: get_block_verbose(height: Height = height(7))
            -> BlockVerbose, crate::get_block_verbose::GetBlockVerboseError;
        GetBlockVerboseByHash / OneShotGetBlockVerboseByHash :: get_block_verbose_by_hash(
            hash: BlockHash = hash(7)
        ) -> BlockVerbose, crate::get_block_verbose::GetBlockVerboseError;
        GetChainTips / OneShotGetChainTips :: get_chain_tips()
            -> Vec<zaino_primitives::types::rpc::ChainTip>, crate::GetChainTipsError;
        GetCommitmentTreeRoots / OneShotGetCommitmentTreeRoots :: get_commitment_tree_roots(
            block: BlockHash = hash(7)
        ) -> TreeRoots, crate::GetCommitmentTreeRootsError;
        GetCommitmentTreeRootsByHeight / OneShotGetCommitmentTreeRootsByHeight
            :: get_commitment_tree_roots_by_height(height: Height = height(7))
            -> (BlockHash, TreeRoots),
            crate::get_commitment_tree_roots_by_height::GetCommitmentTreeRootsByHeightError;
        GetPreIndexCompactBlock / OneShotGetPreIndexCompactBlock :: get_pre_index_compact_block(
            height: Height = height(7)
        ) -> PreIndexCompactBlock, crate::GetBlockError;
        GetDifficulty / OneShotGetDifficulty :: get_difficulty()
            -> Difficulty, crate::GetDifficultyError;
        GetMempoolMetadata / OneShotGetMempoolMetadata :: get_mempool_metadata()
            -> Vec<crate::MempoolTxMeta>, crate::GetMempoolMetadataError;
        GetMempoolSourceTip / OneShotGetMempoolSourceTip :: get_mempool_source_tip()
            -> (BlockHash, Height), std::convert::Infallible;
        GetMempoolTxids / OneShotGetMempoolTxids :: get_mempool_txids()
            -> Vec<TransactionId>, crate::GetMempoolTxidsError;
        GetMiningInfo / OneShotGetMiningInfo :: get_mining_info()
            -> zaino_primitives::types::rpc::MiningInfo, crate::GetMiningInfoError;
        GetNetworkSolPs / OneShotGetNetworkSolPs :: get_network_sol_ps(
            blocks: Option<u32> = Some(120),
            height: Option<Height> = Some(height(7))
        ) -> u64, crate::GetNetworkSolPsError;
        GetNodeInfo / OneShotGetNodeInfo :: get_node_info()
            -> zaino_primitives::types::rpc::NodeInfo, crate::GetNodeInfoError;
        GetPeerInfo / OneShotGetPeerInfo :: get_peer_info()
            -> Vec<zaino_primitives::types::rpc::PeerInfo>, crate::GetPeerInfoError;
        GetRawBlockHeader / OneShotGetRawBlockHeader :: get_raw_block_header(
            hash: BlockHash = hash(7)
        ) -> Vec<u8>, crate::GetBlockHeaderError;
        GetRawBlock / OneShotGetRawBlock :: get_raw_block(height: Height = height(7))
            -> Vec<u8>, crate::GetBlockError;
        GetRawBlockByHash / OneShotGetRawBlockByHash :: get_raw_block_by_hash(
            hash: BlockHash = hash(7)
        ) -> Vec<u8>, crate::GetBlockByHashError;
        GetRawMempoolTransaction / OneShotGetRawMempoolTransaction :: get_raw_mempool_transaction(
            txid: TransactionId = txid()
        ) -> Vec<u8>, crate::get_raw_mempool_transaction::GetRawMempoolTransactionError;
        GetSpentInfo / OneShotGetSpentInfo :: get_spent_info(
            outpoint: SpentOutpoint = SpentOutpoint { txid: txid(), index: 2 }
        ) -> zaino_primitives::types::rpc::SpentInfo, crate::GetSpentInfoError;
        GetStandardFee / OneShotGetStandardFee :: get_standard_fee()
            -> zaino_primitives::types::rpc::StandardFee, crate::GetStandardFeeError;
        GetSubtreeRoots / OneShotGetSubtreeRoots :: get_subtree_roots(
            pool: ShieldedPool = ShieldedPool::Orchard,
            start_index: u16 = 4,
            limit: Option<u16> = Some(2)
        ) -> Vec<SubtreeRoot>, crate::GetSubtreeRootsError;
        GetTransaction / OneShotGetTransaction :: get_transaction(txid: TransactionId = txid())
            -> crate::TransactionResponse, crate::GetTransactionError;
        GetTreestate / OneShotGetTreestate :: get_treestate(height: Height = height(7))
            -> Treestate, crate::GetTreestateError;
        GetTreestateByHash / OneShotGetTreestateByHash :: get_treestate_by_hash(
            hash: BlockHash = hash(7)
        ) -> Treestate, crate::GetTreestateByHashError;
        GetTxOut / OneShotGetTxOut :: get_tx_out(
            txid: TransactionId = txid(),
            index: OutputIndex = 2,
            include_mempool: bool = true
        ) -> Option<zaino_primitives::types::rpc::TxOut>, crate::GetTxOutError;
    }

    /// The chain tip's twin, hand-implemented on [`Scripted`]: retried, then
    /// answered.
    #[tokio::test(start_paused = true)]
    async fn get_chain_tip() {
        let client = client([FailureMode::Timeout, FailureMode::Connection]);
        assert_eq!(
            client.get_chain_tip().await.ok(),
            Some((hash(1), height(5)))
        );
        assert_eq!(client.inner().calls(), 3);
    }
}
