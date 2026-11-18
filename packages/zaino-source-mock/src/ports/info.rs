//! Chain and node information: verbose blocks and headers, block deltas,
//! `getblockchaininfo`, difficulty, mining, node and peer info, subsidy, fee.

use zaino_primitives::types::{
    branch_at,
    rpc::{
        BlockDelta, BlockDeltas, BlockHeaderVerbose, BlockSubsidy, InputDelta, MiningInfo,
        NodeInfo, OutputDelta, PeerInfo, StandardFee,
    },
    AbsoluteChainWork, BlockConfirmations, BlockHash, BlockTreeSizes, BlockVerbose, BlockchainInfo,
    ConsensusBranchId, ConsensusBranchIds, Difficulty, EquihashSolution, Height, Network,
    NetworkUpgradeInfo, NetworkUpgradeStatus, SignedZatoshis, TransparentAddress, TreeSize,
    ValuePoolBalance, Zatoshis,
};
use zaino_source::{
    GetBlockDeltasError, GetBlockHeaderError, GetBlockSubsidyError, GetBlockVerboseError,
    GetBlockchainInfoError, GetDifficultyError, GetMiningInfoError, GetNetworkSolPsError,
    GetNodeInfoError, GetPeerInfoError, GetStandardFeeError, QueryError,
};

use super::ledger::{address_of, pool_values, Ledger, POOLS};
use super::{fault, refused};
use crate::chain::Served;
use crate::{CallArg, MockBlock, MockChain};

/// zcashd's `GetDifficulty`: the network's minimum-difficulty target over
/// `bits`'s, both from their compact forms (zebra divides by its exact
/// 256-bit limit instead, which differs in the eighth significant digit).
pub(crate) fn difficulty(bits: u32, network: Network) -> Difficulty {
    let limit = match network {
        Network::Mainnet | Network::PubTestnet => 0x1f07_ffff,
        Network::Regtest => 0x200f_0f0f,
    };
    let target = |bits: u32| {
        let mantissa = f64::from(bits & 0x00ff_ffff);
        let exponent = i32::try_from(bits >> 24).unwrap_or(0);
        mantissa * 256f64.powi(exponent - 3)
    };
    target(limit) / target(bits)
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

fn verbose<E: std::fmt::Debug + std::fmt::Display>(
    served: &Served<'_>,
    block: &MockBlock,
) -> Result<BlockVerbose, QueryError<E>> {
    // value pools along a side chain would need its own ledger: best chain only
    let (supply, balances) = match served.is_best(block.hash()) {
        true => {
            let (supply, balances) = pools(served, block.height(), true)?;
            (Some(supply), balances)
        }
        false => (None, Vec::new()),
    };
    Ok(BlockVerbose {
        confirmations: confirmations(served, block),
        difficulty: difficulty(block.block.header.bits.as_bits(), served.facts().network),
        chainwork: chainwork(served, block),
        chain_supply: supply,
        value_pools: balances,
        tree_sizes: tree_sizes(block),
        next_block_hash: served.next_of(block).map(MockBlock::hash),
    })
}

impl zaino_source::OneShotGetBlockVerbose for MockChain {
    async fn get_block_verbose(
        &self,
        height: Height,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        let served = self
            .enter("get_block_verbose", CallArg::Height(height))
            .await?;
        match served.at(height) {
            Some(block) => verbose(&served, block),
            None => refused(GetBlockVerboseError::HeightNotFound(height)),
        }
    }
}

impl zaino_source::OneShotGetBlockVerboseByHash for MockChain {
    async fn get_block_verbose_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<BlockVerbose, QueryError<GetBlockVerboseError>> {
        let served = self
            .enter("get_block_verbose_by_hash", CallArg::Hash(hash))
            .await?;
        match served.by_hash(hash) {
            Some(block) => verbose(&served, block),
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
        let genesis = u32::from(block.height()) == 0;
        Ok(BlockHeaderVerbose {
            hash,
            confirmations: confirmations(&served, block),
            height: block.height(),
            version: header.version,
            merkle_root: header.merkle_root,
            time: header.time,
            nonce: header.nonce,
            solution: match &header.solution {
                EquihashSolution::Standard(bytes) => bytes.to_vec(),
                EquihashSolution::Regtest(bytes) => bytes.to_vec(),
            },
            bits: header.bits,
            difficulty: difficulty(header.bits.as_bits(), served.facts().network),
            block_commitments: Some(header.block_commitments),
            final_sapling_root: block.tree_roots.sapling.as_ref().map(|info| info.root),
            chainwork: chainwork(&served, block),
            previous_block_hash: (!genesis).then_some(header.prev_hash),
            next_block_hash: served.next_of(block).map(MockBlock::hash),
        })
    }
}

/// zcashd's `GetMedianTimePast`: the median of the last eleven block times.
fn median_time(served: &Served<'_>, block: &MockBlock) -> u32 {
    let mut times = Vec::new();
    let mut at = Some(block);
    while let Some(current) = at.filter(|_| times.len() < 11) {
        times.push(current.block.header.time);
        at = (u32::from(current.height()) > 0)
            .then(|| served.by_hash(current.block.header.prev_hash))
            .flatten();
    }
    times.sort_unstable();
    times
        .get(times.len() / 2)
        .copied()
        .unwrap_or(block.block.header.time)
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
            size: block.size,
            height: block.height(),
            version: header.version,
            merkle_root: header.merkle_root,
            deltas,
            time: header.time,
            median_time: median_time(&served, block),
            nonce: header.nonce,
            bits: header.bits,
            difficulty: difficulty(header.bits.as_bits(), network),
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

impl zaino_source::OneShotGetMiningInfo for MockChain {
    async fn get_mining_info(&self) -> Result<MiningInfo, QueryError<GetMiningInfoError>> {
        let served = self.enter("get_mining_info", CallArg::None).await?;
        let Some(tip) = served.tip() else {
            return refused(GetMiningInfoError::NotReady);
        };
        let facts = served.facts();
        Ok(MiningInfo {
            tip_height: tip.height(),
            chain: chain_name(facts.network).to_owned(),
            testnet: facts.network != Network::Mainnet,
            current_block_size: None,
            current_block_tx: None,
            network_solution_rate: Some(facts.network_sol_ps),
            network_hash_rate: Some(facts.network_sol_ps),
            difficulty: Some(difficulty(tip.block.header.bits.as_bits(), facts.network)),
            errors: None,
        })
    }
}

impl zaino_source::OneShotGetNetworkSolPs for MockChain {
    async fn get_network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> Result<u64, QueryError<GetNetworkSolPsError>> {
        let served = self
            .enter("get_network_sol_ps", CallArg::SolPs { blocks, height })
            .await?;
        match served.tip() {
            Some(_) => Ok(served.facts().network_sol_ps),
            None => refused(GetNetworkSolPsError::NotReady),
        }
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
            errors_timestamp: None,
        })
    }
}

impl zaino_source::OneShotGetPeerInfo for MockChain {
    async fn get_peer_info(&self) -> Result<Vec<PeerInfo>, QueryError<GetPeerInfoError>> {
        let served = self.enter("get_peer_info", CallArg::None).await?;
        Ok(served.facts().peers.clone())
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
