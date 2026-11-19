//! Reads pinned to one snapshot: ChainView's (`chain`), those composed from
//! them, and the validator's passthrough controls.

use std::sync::Arc;

use zaino_chain::{
    AddressRead as _, BlockId, BlockRead as _, ChainScope, ChainViewError, ChainViewSnapshot as _,
    ComposerSnapshot, PrevoutRead as _, RawTransaction, SpendRead as _, SpendStatus,
    TransactionRead as _, TxOutSetRead as _,
};
use zaino_chain_head_service::PersistentSnapshot;
use zaino_chain_store_service::StoreReader;
use zaino_indexes::IndexId;
use zaino_persistence_lsm_tree::LsmTreeReader;
use zaino_primitives::types::rpc::{
    AddressDeltas, AddressDeltasRequest, BlockDelta, BlockDeltas, BlockHeaderVerbose, BlockSubsidy,
    InputDelta, MiningInfo, NetworkInfo, NodeInfo, OutputDelta, PeerInfo, SpentInfo, SpentOutpoint,
    StandardFee, TxOut,
};
use zaino_primitives::types::{
    BlockConfirmations, BlockHash, BlockRef, BlockVerbose, BlockchainInfo, Difficulty,
    EquihashSolution, Height, Network, Outpoint, OutputIndex, SignedZatoshis, TransactionId,
    TransparentAddress, TxOutSetInfo, Utxo, Zatoshis,
};
use zaino_source::{
    GetBlockDeltasError, GetBlockHeaderError, GetBlockVerboseError, GetSpentInfoError, QueryError,
    UtxoPage,
};

use crate::capability::Capability;
use crate::engine::Service;
use crate::error::ServiceError;
use crate::source::ServiceSource;

/// The chain view one service snapshot reads through.
pub type ChainSnapshot<S> = ComposerSnapshot<StoreReader<LsmTreeReader>, PersistentSnapshot, S>;

type Result<T> = core::result::Result<T, ServiceError>;

/// Every read the service answers, against one chain state.
pub struct ServiceSnapshot<S: ServiceSource> {
    chain: ChainSnapshot<S>,
    source: Arc<S>,
    pub(crate) network: Network,
    /// The tip-agnostic mempool set.
    pub(crate) mempool: zaino_mempool_service::MempoolSubscriber,
    /// The mempool coherent with a chain tip.
    pub(crate) coherence: zaino_mempool_service::CoherentSubscriber,
    /// The view, for what the chain holds now rather than at this snapshot.
    pub(crate) view: Arc<crate::engine::View<S>>,
}

impl<S: ServiceSource> Service<S> {
    /// A snapshot every read in a request is answered from.
    pub fn snapshot(&self) -> ServiceSnapshot<S> {
        self.with_chain(self.view.snapshot())
    }

    /// A snapshot answering from `chain`, a chain snapshot this service took.
    pub fn with_chain(&self, chain: ChainSnapshot<S>) -> ServiceSnapshot<S> {
        ServiceSnapshot {
            chain,
            source: Arc::clone(&self.source),
            network: self.network(),
            mempool: self.mempool.subscriber(),
            coherence: self.coherence.subscriber(),
            view: Arc::clone(&self.view),
        }
    }
}

impl<S: ServiceSource> ServiceSnapshot<S> {
    /// ChainView's reads: blocks, compact blocks and frames, transactions,
    /// treestates, subtrees, chain tips, address history, spends, the txout
    /// set and the history tree, each routed by ChainView.
    pub fn chain(&self) -> &ChainSnapshot<S> {
        &self.chain
    }

    /// The tip this snapshot answers for.
    pub fn tip(&self) -> BlockRef {
        self.chain.tip()
    }

    /// The verbose header of the block at `at` (`getblockheader`): composed
    /// from the indexed block, wherever ChainView finds it.
    pub async fn block_header(&self, at: BlockId) -> Result<Option<BlockHeaderVerbose>> {
        let Some(block) = self.chain.block(at).await? else {
            return Ok(None);
        };
        let header = &block.header;
        let (confirmations, next_block_hash) = self.placement(header.hash, header.height).await?;
        Ok(Some(BlockHeaderVerbose {
            hash: header.hash,
            confirmations,
            height: header.height,
            version: header.version,
            merkle_root: header.merkle_root,
            time: header.time,
            nonce: header.nonce,
            solution: match &header.solution {
                EquihashSolution::Standard(bytes) => bytes.to_vec(),
                EquihashSolution::Regtest(bytes) => bytes.to_vec(),
            },
            bits: header.bits,
            difficulty: self.difficulty(header.bits.as_bits()),
            block_commitments: Some(header.block_commitments),
            final_sapling_root: block.tree_roots.sapling.as_ref().map(|info| info.root),
            chainwork: block.chainwork,
            previous_block_hash: previous(header.height, header.prev_hash),
            next_block_hash,
        }))
    }

    /// The serialized header of the block at `at`, from the validator by the
    /// hash this snapshot holds there.
    pub async fn raw_block_header(&self, at: BlockId) -> Result<Option<Vec<u8>>> {
        let Some(hash) = self.pinned(at).await? else {
            return Ok(None);
        };
        found(self.source.get_raw_block_header(hash).await, |error| {
            matches!(error, GetBlockHeaderError::BlockNotFound(_))
        })
    }

    /// The block's chain facts (`getblock` verbosity 1: value pools, supply,
    /// tree sizes), from the validator by the hash this snapshot holds.
    pub async fn block_verbose(&self, at: BlockId) -> Result<Option<BlockVerbose>> {
        let Some(hash) = self.pinned(at).await? else {
            return Ok(None);
        };
        found(self.source.get_block_verbose_by_hash(hash).await, |error| {
            matches!(
                error,
                GetBlockVerboseError::BlockNotFound(_) | GetBlockVerboseError::HeightNotFound(_)
            )
        })
    }

    /// `getblockdeltas` for a best-chain block: composed from the indexed
    /// block and its prevouts where Zaino holds them, else the validator's.
    pub async fn block_deltas(&self, hash: BlockHash) -> Result<Option<BlockDeltas>> {
        match self.local_block_deltas(hash).await {
            Err(ServiceError::Chain(ChainViewError::NotServiceable(_))) => {}
            answer => return answer,
        }
        found(self.source.get_block_deltas(hash).await, |error| {
            matches!(error, GetBlockDeltasError::BlockNotFound(_))
        })
    }

    async fn local_block_deltas(&self, hash: BlockHash) -> Result<Option<BlockDeltas>> {
        if self.chain.block_height(hash).await?.is_none() {
            return Ok(None);
        }
        let Some(block) = self.chain.block(BlockId::Hash(hash)).await? else {
            return Ok(None);
        };
        let outpoints: Vec<Outpoint> = block
            .transactions
            .iter()
            .flat_map(|tx| &tx.transparent_inputs)
            .map(|input| Outpoint {
                txid: input.prev_txid,
                index: input.prev_index,
            })
            .collect();
        let prevouts = self.chain.previous_outputs(&outpoints).await?;
        let mut prevouts = outpoints.iter().zip(prevouts);
        let address =
            |script: &[u8]| TransparentAddress::from_standard_script(script, self.network);
        let mut deltas = Vec::with_capacity(block.transactions.len());
        for (tx_index, tx) in (0u32..).zip(&block.transactions) {
            let mut inputs = Vec::new();
            for (index, input) in (0u32..).zip(&tx.transparent_inputs) {
                let output = match prevouts.next() {
                    Some((_, Some(output))) => output,
                    _ => {
                        return Err(ServiceError::Inconsistent(format!(
                            "the input {}:{} of {} spends no output Zaino holds",
                            input.prev_txid, input.prev_index, tx.txid
                        )))
                    }
                };
                let Some(address) = address(output.script.as_bytes()) else {
                    continue;
                };
                inputs.push(InputDelta {
                    address,
                    satoshis: debit(output.value)?,
                    index,
                    prev_txid: input.prev_txid,
                    prev_output: input.prev_index,
                });
            }
            let outputs = (0u32..)
                .zip(&tx.transparent_outputs)
                .filter_map(|(index, output)| {
                    Some(OutputDelta {
                        address: address(output.script.as_bytes())?,
                        satoshis: output.value,
                        index,
                    })
                })
                .collect();
            deltas.push(BlockDelta {
                txid: tx.txid,
                index: tx_index,
                inputs,
                outputs,
            });
        }
        let header = &block.header;
        let (confirmations, next_block_hash) = self.placement(hash, header.height).await?;
        Ok(Some(BlockDeltas {
            hash,
            confirmations,
            size: u64::from(block.size),
            height: header.height,
            version: header.version,
            merkle_root: header.merkle_root,
            deltas,
            time: header.time,
            median_time: self.median_time(header.height).await?,
            nonce: header.nonce,
            bits: header.bits,
            difficulty: self.difficulty(header.bits.as_bits()),
            previous_block_hash: previous(header.height, header.prev_hash),
            next_block_hash,
        }))
    }

    /// Where an outpoint was spent (`getspentinfo`): from Zaino's spend index
    /// where it answers, else the validator's; `None` when unspent.
    pub async fn spent_info(&self, outpoint: SpentOutpoint) -> Result<Option<SpentInfo>> {
        let local = self
            .chain
            .outpoint_spenders(
                &[Outpoint {
                    txid: outpoint.txid,
                    index: outpoint.index,
                }],
                ChainScope::FullChain,
            )
            .await;
        match local.as_deref() {
            Ok(
                [SpendStatus::SpentBy {
                    txid,
                    input_index,
                    height,
                }],
            ) => {
                return Ok(Some(SpentInfo {
                    txid: *txid,
                    index: *input_index,
                    height: *height,
                }))
            }
            Ok([SpendStatus::Unspent]) => return Ok(None),
            Ok(_) | Err(ChainViewError::NotServiceable(_)) => {}
            Err(_) => return local.map(|_| None).map_err(ServiceError::from),
        }
        match self.source.get_spent_info(outpoint).await {
            Ok(info) => Ok(Some(info)),
            Err(QueryError::Domain(GetSpentInfoError::NotSpent)) => Ok(None),
            Err(QueryError::Domain(GetSpentInfoError::Unsupported)) => {
                Err(ServiceError::NotServed {
                    capability: Capability::Chain(zaino_chain::ChainCapability::SpendStatus),
                    index: Some(IndexId::Spent),
                })
            }
            Err(error) => Err(ServiceError::from_query(error)),
        }
    }

    /// `gettxoutsetinfo` at the tip, from I-11 and the window; refused where
    /// it is not built.
    pub async fn txout_set_info(&self) -> Result<TxOutSetInfo> {
        let set = match self.chain.txout_set().await {
            Ok(set) => set,
            Err(ChainViewError::NotServiceable(_)) => {
                return Err(ServiceError::NotServed {
                    capability: Capability::Chain(zaino_chain::ChainCapability::TxOutSet),
                    index: Some(IndexId::TxOutSet),
                })
            }
            Err(error) => return Err(error.into()),
        };
        let tip = self.chain.tip();
        Ok(TxOutSetInfo {
            height: tip.height,
            best_block: tip.hash,
            transactions: set.transactions,
            tx_outs: set.transaction_outputs,
            bytes_serialized: set.bytes_serialized,
            hash_serialized: hex::encode(set.hash_serialized),
            total_amount: Zatoshis::new(set.total_zatoshis).map_err(|error| {
                ServiceError::Inconsistent(format!("the txout set's total: {error}"))
            })?,
        })
    }

    /// A transaction's bytes and where it was mined; with `block`, only if
    /// mined in that block (zebrad's `getrawtransaction` `block_hash`).
    pub async fn raw_transaction(
        &self,
        txid: TransactionId,
        block: Option<BlockHash>,
    ) -> Result<Option<RawTransaction>> {
        if let Some(block) = block {
            let locations = self.chain.transaction_locations(txid).await?;
            let in_block = locations
                .best_chain
                .iter()
                .chain(&locations.non_best_chain)
                .any(|position| position.block.hash == block);
            if !in_block {
                return Ok(None);
            }
        }
        Ok(self.chain.raw_transaction(txid).await?)
    }

    /// A page of the addresses' unspent outputs and the tip it describes
    /// (zebrad's `getaddressutxos` with `chainInfo`).
    pub async fn address_utxos(
        &self,
        addresses: &[TransparentAddress],
        page: UtxoPage,
    ) -> Result<(Vec<Utxo>, BlockRef)> {
        let utxos = self.chain.address_utxos(addresses, page).await?;
        Ok((utxos, self.chain.tip()))
    }

    /// The addresses' deltas over the request's range, clamped to this
    /// snapshot's tip (`end` 0, or the single-address form, reads to it);
    /// with `chain_info`, the range's bounding blocks too.
    pub async fn address_deltas(&self, request: AddressDeltasRequest) -> Result<AddressDeltas> {
        let (addresses, start, end, chain_info) = match request {
            AddressDeltasRequest::Filtered {
                addresses,
                start,
                end,
                chain_info,
            } => (addresses, start, end, chain_info),
            AddressDeltasRequest::Address(address) => (vec![address], 0, 0, false),
        };
        let (start, end) = deltas_range(self.chain.tip().height, start, end);
        let deltas = self.chain.address_deltas(&addresses, start, end).await?;
        if !chain_info {
            return Ok(AddressDeltas::Simple(deltas));
        }
        Ok(AddressDeltas::WithChainInfo {
            deltas,
            start: self.block_at(start).await?,
            end: self.block_at(end).await?,
        })
    }

    /// The block at a height at or below this snapshot's tip.
    async fn block_at(&self, height: Height) -> Result<BlockRef> {
        match self.chain.block_hash(height).await? {
            Some(hash) => Ok(BlockRef { hash, height }),
            None => Err(ServiceError::Inconsistent(format!(
                "no block at {height}, at or below the tip"
            ))),
        }
    }

    /// The block subsidy at `height`, the tip's when `None`.
    pub async fn block_subsidy(&self, height: Option<Height>) -> Result<BlockSubsidy> {
        let height = height.unwrap_or(self.chain.tip().height);
        self.source
            .get_block_subsidy(height)
            .await
            .map_err(ServiceError::from_query)
    }

    /// The validator's chain info.
    pub async fn blockchain_info(&self) -> Result<BlockchainInfo> {
        passthrough(self.source.get_blockchain_info().await)
    }

    /// The validator's node info.
    pub async fn node_info(&self) -> Result<NodeInfo> {
        passthrough(self.source.get_node_info().await)
    }

    /// The validator's peers.
    pub async fn peer_info(&self) -> Result<Vec<PeerInfo>> {
        passthrough(self.source.get_peer_info().await)
    }

    /// The validator's network view.
    pub async fn network_info(&self) -> Result<NetworkInfo> {
        passthrough(self.source.get_network_info().await)
    }

    /// Whether the validator answers.
    pub async fn ping(&self) -> Result<()> {
        passthrough(self.source.ping().await)
    }

    /// The validator's mining info.
    pub async fn mining_info(&self) -> Result<MiningInfo> {
        passthrough(self.source.get_mining_info().await)
    }

    /// The validator's tip difficulty.
    pub async fn difficulty_at_tip(&self) -> Result<Difficulty> {
        passthrough(self.source.get_difficulty().await)
    }

    /// The validator's network solutions per second.
    pub async fn network_sol_ps(&self, blocks: Option<u32>, height: Option<Height>) -> Result<u64> {
        passthrough(self.source.get_network_sol_ps(blocks, height).await)
    }

    /// The validator's standard fee.
    pub async fn standard_fee(&self) -> Result<StandardFee> {
        passthrough(self.source.get_standard_fee().await)
    }

    /// An unspent output, as the validator holds it.
    pub async fn tx_out(
        &self,
        txid: TransactionId,
        index: OutputIndex,
        include_mempool: bool,
    ) -> Result<Option<TxOut>> {
        passthrough(self.source.get_tx_out(txid, index, include_mempool).await)
    }

    /// Submits a transaction, refused here if larger than a block.
    pub async fn send_raw_transaction(&self, transaction: Vec<u8>) -> Result<TransactionId> {
        zaino_consensus::validate_raw_transaction_bytes(&transaction)
            .map_err(|error| ServiceError::InvalidParameter(error.to_string()))?;
        passthrough(self.source.send_raw_transaction(transaction).await)
    }

    /// The hash this snapshot holds at `at`, if any.
    async fn pinned(&self, at: BlockId) -> Result<Option<BlockHash>> {
        Ok(match at {
            BlockId::Hash(hash) => Some(hash),
            BlockId::Height(height) => self.chain.block_hash(height).await?,
        })
    }

    /// A block's confirmations and next block: best-chain or not.
    async fn placement(
        &self,
        hash: BlockHash,
        height: Height,
    ) -> Result<(BlockConfirmations, Option<BlockHash>)> {
        if self.chain.block_height(hash).await? != Some(height) {
            return Ok((BlockConfirmations::NotInBestChain, None));
        }
        let next = match height.checked_add(1) {
            Some(next) => self.chain.block_hash(next).await?,
            None => None,
        };
        Ok((
            BlockConfirmations::of_best_chain_block(height, self.chain.tip().height),
            next,
        ))
    }

    /// The median time past at `height`: of it and up to ten blocks below.
    async fn median_time(&self, height: Height) -> Result<u32> {
        let mut times = Vec::with_capacity(zaino_consensus::MEDIAN_TIME_SPAN);
        let mut at = Some(height);
        while let Some(current) = at.filter(|_| times.len() < zaino_consensus::MEDIAN_TIME_SPAN) {
            let header = self
                .chain
                .block_header(BlockId::Height(current))
                .await?
                .ok_or_else(|| {
                    ServiceError::Inconsistent(format!("no header at {current} below the tip"))
                })?;
            times.push(header.time);
            at = current.checked_sub(1);
        }
        // oldest first, as the window is taken
        times.reverse();
        zaino_consensus::median_time_past(&times)
            .ok_or_else(|| ServiceError::Inconsistent(format!("no block times at {height}")))
    }

    fn difficulty(&self, bits: u32) -> Difficulty {
        let limit = match self.network {
            Network::Mainnet => zaino_consensus::POW_LIMIT_BITS,
            Network::PubTestnet => zaino_consensus::TESTNET_POW_LIMIT_BITS,
            Network::Regtest => zaino_consensus::REGTEST_POW_LIMIT_BITS,
        };
        zaino_consensus::difficulty(bits, limit)
    }
}

fn previous(height: Height, prev_hash: BlockHash) -> Option<BlockHash> {
    (height != Height::GENESIS).then_some(prev_hash)
}

/// The value a spend takes from its address.
fn debit(value: Zatoshis) -> Result<SignedZatoshis> {
    i64::try_from(value.as_u64())
        .ok()
        .and_then(i64::checked_neg)
        .and_then(|value| SignedZatoshis::try_new(value).ok())
        .ok_or_else(|| ServiceError::Inconsistent(format!("a prevout of {value:?}")))
}

/// The validator's answer; a refusal `absent` names is a miss.
fn found<T, E>(
    answer: core::result::Result<T, QueryError<E>>,
    absent: impl Fn(&E) -> bool,
) -> Result<Option<T>>
where
    E: std::error::Error + Send + Sync + 'static,
{
    match answer {
        Ok(answer) => Ok(Some(answer)),
        Err(QueryError::Domain(error)) if absent(&error) => Ok(None),
        Err(error) => Err(ServiceError::from_query(error)),
    }
}

/// A `getaddressdeltas` range clamped to `tip`: `end` 0 reads to it, and a
/// bound past it (or past the protocol maximum) is the tip.
fn deltas_range(tip: Height, start: u32, end: u32) -> (Height, Height) {
    let clamp = |raw: u32| Height::try_from(raw).map_or(tip, |height| height.min(tip));
    (clamp(start), if end == 0 { tip } else { clamp(end) })
}

fn passthrough<T, E>(answer: core::result::Result<T, QueryError<E>>) -> Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    answer.map_err(ServiceError::from_query)
}

#[cfg(test)]
mod tests {
    use super::{deltas_range, Height};

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    /// `end` 0 reads to the tip, and both bounds clamp down to it.
    #[test]
    fn bounds_clamp_to_tip() {
        let tip = height(100);
        assert_eq!(deltas_range(tip, 5, 0), (height(5), tip));
        assert_eq!(deltas_range(tip, 5, 400), (height(5), tip));
        assert_eq!(deltas_range(tip, 300, 50), (tip, height(50)));
    }

    /// A bound past the protocol maximum is the tip, not a failure.
    #[test]
    fn a_bound_past_the_protocol_maximum_clamps() {
        let tip = height(100);
        assert_eq!(deltas_range(tip, u32::MAX, u32::MAX), (tip, tip));
    }

    /// The single-address form's `0..=0` is the whole chain, not genesis.
    #[test]
    fn absent_range_covers_the_whole_chain() {
        assert_eq!(deltas_range(height(100), 0, 0), (height(0), height(100)));
    }

    /// A range inside the chain is untouched.
    #[test]
    fn in_range_bounds_pass_through() {
        assert_eq!(deltas_range(height(100), 10, 20), (height(10), height(20)));
    }
}
