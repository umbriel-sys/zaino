//! What the service asks of a validator (D6-11).

/// Every validator port the service uses: each tier's, the mempool's, and
/// the passthrough controls.
pub trait ServiceSource:
    zaino_chain::ChainViewSource
    + zaino_chain_head::ChainHeadBlockSource
    + zaino_chain_store::ChainStoreSource
    + zaino_source::OneShotGetMempoolTxids
    + zaino_source::OneShotGetMempoolMetadata
    + zaino_source::OneShotGetRawMempoolTransaction
    + zaino_source::OneShotGetMempoolSourceTip
    + zaino_source::OneShotGetBestBlockHeight
    + zaino_source::OneShotGetBlockDeltas
    + zaino_source::OneShotGetBlockSubsidy
    + zaino_source::OneShotGetStandardFee
    + zaino_source::OneShotGetBlockVerboseByHash
    + zaino_source::OneShotGetBlockchainInfo
    + zaino_source::OneShotGetChainTips
    + zaino_source::OneShotGetDifficulty
    + zaino_source::OneShotGetMiningInfo
    + zaino_source::OneShotGetNetworkInfo
    + zaino_source::OneShotGetNetworkSolPs
    + zaino_source::OneShotGetNodeInfo
    + zaino_source::OneShotGetPeerInfo
    + zaino_source::OneShotGetRawBlockHeader
    + zaino_source::OneShotGetSpentInfo
    + zaino_source::OneShotGetTxOut
    + zaino_source::OneShotPing
    + zaino_source::OneShotSendRawTransaction
    + zaino_source::SourceLifecycle
    + zaino_source::ValidatorSource<NonDomain = zaino_source::NonDomainError>
{
}

impl<T> ServiceSource for T where
    T: zaino_chain::ChainViewSource
        + zaino_chain_head::ChainHeadBlockSource
        + zaino_chain_store::ChainStoreSource
        + zaino_source::OneShotGetMempoolTxids
        + zaino_source::OneShotGetMempoolMetadata
        + zaino_source::OneShotGetRawMempoolTransaction
        + zaino_source::OneShotGetMempoolSourceTip
        + zaino_source::OneShotGetBestBlockHeight
        + zaino_source::OneShotGetBlockDeltas
        + zaino_source::OneShotGetBlockSubsidy
        + zaino_source::OneShotGetStandardFee
        + zaino_source::OneShotGetBlockVerboseByHash
        + zaino_source::OneShotGetBlockchainInfo
        + zaino_source::OneShotGetChainTips
        + zaino_source::OneShotGetDifficulty
        + zaino_source::OneShotGetMiningInfo
        + zaino_source::OneShotGetNetworkInfo
        + zaino_source::OneShotGetNetworkSolPs
        + zaino_source::OneShotGetNodeInfo
        + zaino_source::OneShotGetPeerInfo
        + zaino_source::OneShotGetRawBlockHeader
        + zaino_source::OneShotGetSpentInfo
        + zaino_source::OneShotGetTxOut
        + zaino_source::OneShotPing
        + zaino_source::OneShotSendRawTransaction
        + zaino_source::SourceLifecycle
        + zaino_source::ValidatorSource<NonDomain = zaino_source::NonDomainError>
{
}
