//! Chain and node information: verbose blocks and headers, block deltas,
//! `getblockchaininfo`, difficulty, mining, node and peer info, subsidy, fee.

use zaino_primitives::types::{
    branch_at,
    rpc::{
        BlockDelta, BlockDeltas, BlockHeaderVerbose, BlockSubsidy, InputDelta, MiningInfo,
        NetworkEntry, NetworkInfo, NodeInfo, OutputDelta, PeerInfo, StandardFee,
    },
    AbsoluteChainWork, BlockConfirmations, BlockHash, BlockTransactions, BlockTreeSizes,
    BlockVerbose, BlockVerbosity, BlockchainInfo, ConsensusBranchId, ConsensusBranchIds,
    Difficulty, EquihashSolution, Height, Network, NetworkUpgradeInfo, NetworkUpgradeStatus,
    SignedZatoshis, TransparentAddress, TreeSize, ValuePoolBalance, Zatoshis,
};
use zaino_source::{
    GetBlockDeltasError, GetBlockHeaderError, GetBlockSubsidyError, GetBlockVerboseError,
    GetBlockchainInfoError, GetDifficultyError, GetMiningInfoError, GetNetworkSolPsError,
    GetNodeInfoError, GetPeerInfoError, GetStandardFeeError, QueryError, SolutionRateWindow,
};

use super::ledger::{address_of, pool_values, Ledger, POOLS};
use super::{fault, refused};
use crate::chain::Served;
use crate::{CallArg, MockBlock, MockChain};

/// The difficulty as zebrad's headers and zcashd's `GetDifficulty` report it,
/// against the network's own proof-of-work limit.
pub(crate) fn difficulty(bits: u32, network: Network) -> Difficulty {
    let limit = match network {
        Network::Mainnet => zaino_consensus::POW_LIMIT_BITS,
        Network::PubTestnet => zaino_consensus::TESTNET_POW_LIMIT_BITS,
        Network::Regtest => zaino_consensus::REGTEST_POW_LIMIT_BITS,
    };
    zaino_consensus::difficulty(bits, limit)
}

fn confirmations(served: &Served<'_>, block: &MockBlock) -> BlockConfirmations {
    match (served.is_best(block.hash()), served.tip()) {
        (true, Some(tip)) => BlockConfirmations::of_best_chain_block(block.height(), tip.height()),
        _ => BlockConfirmations::NotInBestChain,
    }
}

/// Cumulative work from genesis through `block`, along its own chain; none
/// from a node that answers none.
fn chainwork(served: &Served<'_>, block: &MockBlock) -> Option<AbsoluteChainWork> {
    if !served.facts().chainwork {
        return None;
    }
    let mut chain = vec![block];
    while let Some(parent) = chain
        .last()
        .filter(|at| u32::from(at.height()) > 0)
        .and_then(|at| served.by_hash(at.block.header.prev_hash))
    {
        chain.push(parent);
    }
    let mut blocks = chain.into_iter().rev();
    let genesis = blocks.next()?;
    blocks.try_fold(
        AbsoluteChainWork::genesis(genesis.block.header.bits.to_work()),
        |work, at| work.accumulate(at.block.header.bits.to_work()).ok(),
    )
}

fn pools<E: std::fmt::Debug + std::fmt::Display>(
    served: &Served<'_>,
    through: Height,
    deltas: bool,
) -> Result<(ValuePoolBalance, Vec<ValuePoolBalance>), QueryError<E>> {
    let (chain, delta) = pool_values(served, through).map_err(fault)?;
    let zatoshis = |value: i128| {
        u64::try_from(value)
            .ok()
            .and_then(|value| Zatoshis::new(value).ok())
            .ok_or_else(|| fault(format!("pool value {value} out of range")))
    };
    let balances = POOLS
        .iter()
        .zip(chain)
        .zip(delta)
        .map(|((id, value), change)| {
            Ok(ValuePoolBalance {
                id: (*id).to_owned(),
                chain_value: zatoshis(value)?,
                monitored: value != 0,
                value_delta: match deltas {
                    true => Some(
                        i64::try_from(change)
                            .ok()
                            .and_then(|change| SignedZatoshis::try_new(change).ok())
                            .ok_or_else(|| fault("pool delta out of range"))?,
                    ),
                    false => None,
                },
            })
        })
        .collect::<Result<Vec<_>, QueryError<E>>>()?;
    let total: i128 = chain.iter().sum();
    let supply = ValuePoolBalance {
        id: String::new(),
        chain_value: zatoshis(total)?,
        monitored: total != 0,
        value_delta: match deltas {
            true => Some(
                i64::try_from(delta.iter().sum::<i128>())
                    .ok()
                    .and_then(|change| SignedZatoshis::try_new(change).ok())
                    .ok_or_else(|| fault("supply delta out of range"))?,
            ),
            false => None,
        },
    };
    Ok((supply, balances))
}

fn tree_sizes(block: &MockBlock) -> BlockTreeSizes {
    let size = |info: Option<&zaino_primitives::types::TreeRootInfo>| {
        info.map_or(TreeSize::ZERO, |info| info.size)
    };
    BlockTreeSizes {
        sapling: size(block.tree_roots.sapling.as_ref()),
        orchard: size(block.tree_roots.orchard.as_ref()),
        ironwood: size(block.tree_roots.ironwood.as_ref()),
    }
}

/// zebrad's `getblock` at `verbosity`.
fn verbose<E: std::fmt::Debug + std::fmt::Display>(
    served: &Served<'_>,
    block: &MockBlock,
    verbosity: BlockVerbosity,
) -> Result<BlockVerbose, QueryError<E>> {
    // value pools along a side chain would need its own ledger: best chain only
    let (supply, balances) = match served.is_best(block.hash()) {
        true => {
            let (supply, balances) = pools(served, block.height(), true)?;
            (Some(supply), balances)
        }
        false => (None, Vec::new()),
    };
    let confirmations = confirmations(served, block);
    let header = &block.block.header;
    let transactions = match verbosity {
        BlockVerbosity::TransactionIds => BlockTransactions::Ids(
            block
                .block
                .transactions()
                .iter()
                .map(|tx| tx.txid)
                .collect(),
        ),
        BlockVerbosity::Transactions | BlockVerbosity::TransactionsWithPrevouts => {
            BlockTransactions::Objects(super::tx_objects::transaction_objects(
                served,
                block,
                verbosity,
                confirmations.to_rpc_i64(),
            )?)
        }
    };
    Ok(BlockVerbose {
        hash: block.hash(),
        confirmations,
        size: block
            .raw
            .as_ref()
            .and_then(|raw| u64::try_from(raw.block.len()).ok()),
        height: block.height(),
        version: header.version,
        merkle_root: header.merkle_root,
        block_commitments: Some(header.block_commitments),
        final_sapling_root: block.tree_roots.sapling.as_ref().map(|info| info.root),
        final_orchard_root: block.tree_roots.orchard.as_ref().map(|info| info.root),
        transactions,
        time: header.time,
        nonce: header.nonce,
        solution: solution(&header.solution),
        bits: header.bits,
        difficulty: difficulty(header.bits.as_bits(), served.facts().network),
        chainwork: chainwork(served, block),
        chain_supply: supply,
        value_pools: balances,
        tree_sizes: tree_sizes(block),
        previous_block_hash: Some(header.prev_hash),
        next_block_hash: served.next_of(block).map(MockBlock::hash),
    })
}

fn solution(solution: &EquihashSolution) -> Vec<u8> {
    match solution {
        EquihashSolution::Standard(bytes) => bytes.to_vec(),
        EquihashSolution::Regtest(bytes) => bytes.to_vec(),
    }
}

impl zaino_source::OneShotGetBlockVerbose for MockChain {
    async fn get_block_verbose(
        &self,
        height: Height,
        verbosity: BlockVerbosity,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        let served = self
            .enter("get_block_verbose", CallArg::Height(height))
            .await?;
        match served.at(height) {
            Some(block) => verbose(&served, block, verbosity),
            None => refused(GetBlockVerboseError::HeightNotFound(height)),
        }
    }
}

impl zaino_source::OneShotGetBlockVerboseByHash for MockChain {
    async fn get_block_verbose_by_hash(
        &self,
        hash: BlockHash,
        verbosity: BlockVerbosity,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        let served = self
            .enter("get_block_verbose_by_hash", CallArg::Hash(hash))
            .await?;
        match served.by_hash(hash) {
            Some(block) => verbose(&served, block, verbosity),
            None => refused(GetBlockVerboseError::BlockNotFound(hash)),
        }
    }
}

impl zaino_source::OneShotGetBlockHeader for MockChain {
    async fn get_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<BlockHeaderVerbose, QueryError<GetBlockHeaderError>> {
        let served = self.enter("get_block_header", CallArg::Hash(hash)).await?;
        let Some(block) = served.by_hash(hash) else {
            return refused(GetBlockHeaderError::BlockNotFound(hash));
        };
        let header = &block.block.header;
        Ok(BlockHeaderVerbose {
            hash,
            confirmations: confirmations(&served, block),
            height: block.height(),
            version: header.version,
            merkle_root: header.merkle_root,
            time: header.time,
            nonce: header.nonce,
            solution: solution(&header.solution),
            bits: header.bits,
            difficulty: difficulty(header.bits.as_bits(), served.facts().network),
            block_commitments: Some(header.block_commitments),
            final_sapling_root: block.tree_roots.sapling.as_ref().map(|info| info.root),
            chainwork: chainwork(&served, block),
            // as zebrad: every block's parent, genesis's all-zero one included
            previous_block_hash: Some(header.prev_hash),
            next_block_hash: served.next_of(block).map(MockBlock::hash),
        })
    }
}

/// zcashd's `GetMedianTimePast`: the median of the block's time and its ten
/// predecessors'.
fn median_time(served: &Served<'_>, block: &MockBlock) -> u32 {
    let mut times = Vec::new();
    let mut at = Some(block);
    while let Some(current) = at.filter(|_| times.len() < zaino_consensus::MEDIAN_TIME_SPAN) {
        times.push(current.block.header.time);
        at = (u32::from(current.height()) > 0)
            .then(|| served.by_hash(current.block.header.prev_hash))
            .flatten();
    }
    times.reverse();
    zaino_consensus::median_time_past(&times).unwrap_or(block.block.header.time)
}

impl zaino_source::OneShotGetBlockDeltas for MockChain {
    /// Per transaction: each input spending a standard output (a debit) and
    /// each output paying a standard script (a credit).
    async fn get_block_deltas(
        &self,
        hash: BlockHash,
    ) -> Result<BlockDeltas, QueryError<GetBlockDeltasError>> {
        let served = self.enter("get_block_deltas", CallArg::Hash(hash)).await?;
        let Some(block) = served.by_hash(hash) else {
            return refused(GetBlockDeltasError::BlockNotFound(hash));
        };
        let network = served.facts().network;
        let ledger = Ledger::of(&served);
        let address = |script: &[u8]| -> Option<TransparentAddress> {
            TransparentAddress::try_new(address_of(script, network)?).ok()
        };
        let mut deltas = Vec::new();
        for (index, tx) in (0u32..).zip(block.block.transactions()) {
            let mut inputs = Vec::new();
            for (input_index, input) in (0u32..).zip(&tx.transparent.inputs) {
                let Some(created) = ledger.outputs.get(&(input.prev_txid, input.prev_index)) else {
                    continue;
                };
                let Some(address) = address(created.output.script.as_bytes()) else {
                    continue;
                };
                let value = i64::try_from(u64::from(created.output.value))
                    .map_err(|_| fault("value past i64"))?;
                inputs.push(InputDelta {
                    address,
                    satoshis: SignedZatoshis::try_new(-value)
                        .map_err(|why| fault(why.to_string()))?,
                    index: input_index,
                    prev_txid: input.prev_txid,
                    prev_output: input.prev_index,
                });
            }
            let outputs = (0u32..)
                .zip(&tx.transparent.outputs)
                .filter_map(|(output_index, output)| {
                    Some(OutputDelta {
                        address: address(output.script.as_bytes())?,
                        satoshis: output.value,
                        index: output_index,
                    })
                })
                .collect();
            deltas.push(BlockDelta {
                txid: tx.txid,
                index,
                inputs,
                outputs,
            });
        }
        let header = &block.block.header;
        Ok(BlockDeltas {
            hash,
            confirmations: confirmations(&served, block),
            size: u64::from(block.block.size()),
            height: block.height(),
            version: header.version,
            merkle_root: header.merkle_root,
            deltas,
            time: header.time,
            median_time: median_time(&served, block),
            nonce: header.nonce,
            bits: header.bits,
            difficulty: difficulty(header.bits.as_bits(), network),
            chainwork: chainwork(&served, block),
            previous_block_hash: (u32::from(block.height()) > 0).then_some(header.prev_hash),
            next_block_hash: served.next_of(block).map(MockBlock::hash),
        })
    }
}

/// The branch in force at `height` (0 before any upgrade).
fn branch(upgrades: &[NetworkUpgradeInfo], height: Option<Height>) -> ConsensusBranchId {
    height
        .and_then(|height| branch_at(upgrades, height))
        .unwrap_or(ConsensusBranchId::new(0))
}

fn chain_name(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "main",
        Network::PubTestnet => "test",
        Network::Regtest => "regtest",
    }
}

impl zaino_source::OneShotGetBlockchainInfo for MockChain {
    /// The served tip, its difficulty and work, the value pools, and the
    /// schedule's status against the tip. A local chain is fully verified:
    /// headers = blocks = the estimate, progress 1.
    async fn get_blockchain_info(
        &self,
    ) -> Result<BlockchainInfo, QueryError<GetBlockchainInfoError>> {
        let served = self.enter("get_blockchain_info", CallArg::None).await?;
        let Some(tip) = served.tip() else {
            return refused(GetBlockchainInfoError::NotReady);
        };
        let facts = served.facts();
        let height = tip.height();
        let (supply, balances) = pools(&served, height, false)?;
        let upgrades = facts
            .upgrades
            .iter()
            .map(|upgrade| NetworkUpgradeInfo {
                status: match upgrade.activation_height <= height {
                    true => NetworkUpgradeStatus::Active,
                    false => NetworkUpgradeStatus::Pending,
                },
                ..upgrade.clone()
            })
            .collect();
        Ok(BlockchainInfo {
            chain: chain_name(facts.network).to_owned(),
            blocks: height,
            headers: height,
            estimated_height: height,
            best_block_hash: tip.hash(),
            difficulty: difficulty(tip.block.header.bits.as_bits(), facts.network),
            verification_progress: 1.0,
            chain_work: chainwork(&served, tip),
            pruned: false,
            size_on_disk: facts.size_on_disk,
            commitments: 0,
            chain_supply: supply,
            value_pools: balances,
            upgrades,
            consensus: ConsensusBranchIds {
                chain_tip: branch(&facts.upgrades, Some(height)),
                next_block: branch(&facts.upgrades, height.checked_add(1)),
            },
        })
    }
}

impl zaino_source::OneShotGetDifficulty for MockChain {
    async fn get_difficulty(&self) -> Result<Difficulty, QueryError<GetDifficultyError>> {
        let served = self.enter("get_difficulty", CallArg::None).await?;
        match served.tip() {
            Some(tip) => Ok(difficulty(
                tip.block.header.bits.as_bits(),
                served.facts().network,
            )),
            None => refused(GetDifficultyError::NotReady),
        }
    }
}

/// zebrad's solution rate (zebra-state `solution_rate`): the work of the
/// `window` blocks ending at `height` (the tip when absent or above it) over
/// the seconds they and the block below them span; 0 with no tip or no span.
fn solution_rate(served: &Served<'_>, window: SolutionRateWindow, height: Option<Height>) -> u64 {
    let Some((start, blocks)) = measured_blocks(served, window, height) else {
        return 0;
    };
    // newest first, with one block below the window for when it began
    let samples: Vec<(u32, u128)> = (0..=blocks)
        .map_while(|back| {
            let height = Height::try_from(u32::from(start).checked_sub(back)?).ok()?;
            let header = &served.at(height)?.block.header;
            let work = std::num::NonZeroU128::from(header.bits.to_work()).get();
            Some((header.time, work))
        })
        .collect();
    rate_over(&samples)
}

/// The block a solution rate is measured at and how many it averages:
/// `height` clamped to the tip (the tip when absent), and the window, the
/// difficulty averaging one being 17 blocks, 102 from NU7. `None` with no tip.
pub(crate) fn measured_blocks(
    served: &Served<'_>,
    window: SolutionRateWindow,
    height: Option<Height>,
) -> Option<(Height, u32)> {
    let tip = served.tip()?.height();
    let start = height.map_or(tip, |height| height.min(tip));
    let blocks = match window {
        SolutionRateWindow::Default => 120,
        SolutionRateWindow::DifficultyAveraging => {
            let nu7 = ConsensusBranchId::new(crate::Upgrade::Nu7.branch_id());
            match branch_at(&served.facts().upgrades, start) == Some(nu7) {
                true => 102,
                false => 17,
            }
        }
        SolutionRateWindow::Blocks(blocks) => blocks.get(),
    };
    Some((start, blocks))
}

/// `(time, work)` samples, the last one the block below the window: the
/// window's work (the last sample's left out) over the seconds every sample
/// spans; 0 with no span.
pub(crate) fn rate_over(samples: &[(u32, u128)]) -> u64 {
    let times = || samples.iter().map(|(time, _)| *time);
    let seconds = match (times().min(), times().max()) {
        (Some(first), Some(last)) => u128::from(last.saturating_sub(first)),
        _ => 0,
    };
    let Some((_below, measured)) = samples.split_last() else {
        return 0;
    };
    let work = measured
        .iter()
        .fold(0u128, |total, (_, work)| total.saturating_add(*work));
    match work.checked_div(seconds) {
        Some(rate) => u64::try_from(rate).unwrap_or(u64::MAX),
        None => 0,
    }
}

impl zaino_source::OneShotGetMiningInfo for MockChain {
    /// As zebrad: the last block's size and non-coinbase transaction count
    /// above genesis, the default-window solution rate twice, no difficulty
    /// or errors; `blocks` 0 with no tip.
    async fn get_mining_info(&self) -> Result<MiningInfo, QueryError<GetMiningInfoError>> {
        let served = self.enter("get_mining_info", CallArg::None).await?;
        let facts = served.facts();
        let mined = served
            .tip()
            .filter(|tip| tip.height() != Height::GENESIS)
            .map(|tip| &tip.block);
        let rate = solution_rate(&served, SolutionRateWindow::Default, None);
        Ok(MiningInfo {
            tip_height: served.tip().map_or(Height::GENESIS, MockBlock::height),
            chain: chain_name(facts.network).to_owned(),
            testnet: facts.network != Network::Mainnet,
            current_block_size: mined.map(|block| u64::from(block.size())),
            current_block_tx: mined
                .and_then(|block| u64::try_from(block.transactions().len()).ok())
                .map(|transactions| transactions.saturating_sub(1)),
            network_solution_rate: Some(rate),
            network_hash_rate: Some(rate),
            difficulty: None,
            errors: None,
        })
    }
}

impl zaino_source::OneShotGetNetworkSolPs for MockChain {
    async fn get_network_sol_ps(
        &self,
        window: SolutionRateWindow,
        height: Option<Height>,
    ) -> Result<u64, QueryError<GetNetworkSolPsError>> {
        let served = self
            .enter("get_network_sol_ps", CallArg::SolPs { window, height })
            .await?;
        Ok(solution_rate(&served, window, height))
    }
}

impl zaino_source::OneShotGetNodeInfo for MockChain {
    async fn get_node_info(&self) -> Result<NodeInfo, QueryError<GetNodeInfoError>> {
        let served = self.enter("get_node_info", CallArg::None).await?;
        let Some(tip) = served.tip() else {
            return refused(GetNodeInfoError::NotReady);
        };
        let facts = served.facts();
        Ok(NodeInfo {
            version: facts.version,
            build: facts.build.clone(),
            subversion: facts.subversion.clone(),
            protocol_version: facts.protocol_version,
            blocks: tip.height(),
            connections: u64::try_from(facts.peers.len()).unwrap_or(u64::MAX),
            difficulty: difficulty(tip.block.header.bits.as_bits(), facts.network),
            testnet: facts.network != Network::Mainnet,
            proxy: None,
            pay_tx_fee: facts.pay_tx_fee,
            relay_fee: facts.relay_fee,
            errors: None,
            // zebrad always sends it: with no error, the time of the reply,
            // which the mock takes from its tip
            errors_timestamp: Some(i64::from(tip.block.header.time)),
        })
    }
}

impl zaino_source::OneShotGetPeerInfo for MockChain {
    async fn get_peer_info(&self) -> Result<Vec<PeerInfo>, QueryError<GetPeerInfoError>> {
        let served = self.enter("get_peer_info", CallArg::None).await?;
        Ok(served.facts().peers.clone())
    }
}

impl zaino_source::OneShotGetNetworkInfo for MockChain {
    /// As zebrad 7 answers: the node's identity, fee and peers from its
    /// facts, and zebrad's fixed services, offset, networks and empty
    /// addresses and warnings.
    async fn get_network_info(&self) -> Result<NetworkInfo, QueryError<std::convert::Infallible>> {
        let served = self.enter("get_network_info", CallArg::None).await?;
        let facts = served.facts();
        let network = |name: &str, reachable| NetworkEntry {
            name: name.to_owned(),
            limited: false,
            reachable,
            proxy: String::new(),
            proxy_randomize_credentials: false,
        };
        Ok(NetworkInfo {
            version: facts.version,
            subversion: facts.subversion.clone(),
            protocol_version: facts.protocol_version,
            local_services: "0000000000000001".to_owned(),
            time_offset: 0,
            connections: u64::try_from(facts.peers.len()).unwrap_or(u64::MAX),
            networks: vec![
                network("ipv4", true),
                network("ipv6", true),
                network("onion", false),
            ],
            relay_fee: facts.relay_fee,
            local_addresses: Vec::new(),
            warnings: String::new(),
        })
    }
}

impl zaino_source::OneShotPing for MockChain {
    async fn ping(&self) -> Result<(), QueryError<std::convert::Infallible>> {
        self.enter("ping", CallArg::None).await?;
        Ok(())
    }
}

impl zaino_source::OneShotGetBlockSubsidy for MockChain {
    /// The configured subsidy for a height up to the tip; past it,
    /// `HeightNotReached`.
    async fn get_block_subsidy(
        &self,
        height: Height,
    ) -> Result<BlockSubsidy, QueryError<GetBlockSubsidyError>> {
        let served = self
            .enter("get_block_subsidy", CallArg::Height(height))
            .await?;
        if served
            .tip_height()
            .is_none_or(|tip| u32::from(height) > tip)
        {
            return refused(GetBlockSubsidyError::HeightNotReached(height));
        }
        served
            .facts()
            .subsidy
            .clone()
            .ok_or_else(|| fault("no subsidy configured in NodeFacts"))
    }
}

impl zaino_source::OneShotGetStandardFee for MockChain {
    async fn get_standard_fee(&self) -> Result<StandardFee, QueryError<GetStandardFeeError>> {
        let served = self.enter("get_standard_fee", CallArg::None).await?;
        match served.facts().standard_fee {
            Some(fee) => Ok(fee),
            None => refused(GetStandardFeeError::Unsupported),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{measured_blocks, rate_over, GetNetworkSolPsError, SolutionRateWindow};
    use crate::{Activations, CallArg, ChainBuilder, NodeFacts, Upgrade};
    use zaino_primitives::types::Height;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("a valid height")
    }

    /// The window's work, the block below it left out, over the seconds
    /// every sample spans; nothing to measure is 0.
    #[test]
    fn the_rate_is_the_windows_work_over_its_span() {
        assert_eq!(rate_over(&[(130, 600), (120, 400), (100, 9_999)]), 33);
        assert_eq!(rate_over(&[(100, 5), (100, 5)]), 0, "no span");
        assert_eq!(rate_over(&[(100, 5)]), 0, "one block");
        assert_eq!(rate_over(&[]), 0);
    }

    /// The height asked, clamped to the tip, and the window: 120 by default,
    /// the difficulty averaging window 17 before NU7 and 102 from it.
    #[tokio::test]
    async fn the_window_and_the_height_choose_the_blocks() {
        let chain = ChainBuilder::new().blocks(40).build();
        let served = chain
            .enter::<GetNetworkSolPsError>("test", CallArg::None)
            .await
            .expect("served");
        let five = SolutionRateWindow::from_rpc(Some(5));
        let chosen = |window, at: Option<u32>| measured_blocks(&served, window, at.map(height));
        assert_eq!(
            chosen(SolutionRateWindow::Default, None),
            Some((height(40), 120))
        );
        assert_eq!(chosen(five, Some(20)), Some((height(20), 5)));
        assert_eq!(chosen(five, Some(4_000)), Some((height(40), 5)), "the tip");
        assert_eq!(
            chosen(SolutionRateWindow::DifficultyAveraging, None),
            Some((height(40), 17))
        );

        let nu7 = ChainBuilder::with(
            NodeFacts::regtest(),
            Activations::default().at(Upgrade::Nu7, 10),
        )
        .blocks(20)
        .build();
        let served = nu7
            .enter::<GetNetworkSolPsError>("test", CallArg::None)
            .await
            .expect("served");
        let averaging = |at| {
            measured_blocks(
                &served,
                SolutionRateWindow::DifficultyAveraging,
                Some(height(at)),
            )
        };
        assert_eq!(averaging(20), Some((height(20), 102)));
        assert_eq!(averaging(5), Some((height(5), 17)));
    }
}
