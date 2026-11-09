//! Blocks, their encodings and the chain tips.

use zaino_primitives::types::{
    rpc::{ChainTip, ChainTipStatus},
    Block, BlockHash, Height, PreIndexCompactBlock,
};
use zaino_source::{
    GetBestBlockHeightError, GetBlockByHashError, GetBlockError, GetBlockHeaderError,
    GetChainTipError, GetChainTipsError, QueryError,
};

use super::{fault, refused};
use crate::chain::Served;
use crate::{MockBlock, MockChain};

impl zaino_source::OneShotGetBlock for MockChain {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        let served = self.enter("get_block")?;
        match served.at(height) {
            Some(block) => Ok(block.block.clone()),
            None => refused(GetBlockError::HeightNotFound(height)),
        }
    }
}

impl zaino_source::OneShotGetBlockByHash for MockChain {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        let served = self.enter("get_block_by_hash")?;
        match served.by_hash(hash) {
            Some(block) => Ok(block.block.clone()),
            None => refused(GetBlockByHashError::NotFound(hash)),
        }
    }
}

fn raw<E: std::fmt::Debug + std::fmt::Display>(
    block: &MockBlock,
    part: impl FnOnce(&crate::block::RawBlock) -> Vec<u8>,
) -> Result<Vec<u8>, QueryError<E>> {
    block.raw.as_ref().map(part).ok_or_else(|| {
        fault(format!(
            "block {} was built without its bytes",
            block.height()
        ))
    })
}

impl zaino_source::OneShotGetRawBlock for MockChain {
    async fn get_raw_block(&self, height: Height) -> Result<Vec<u8>, QueryError<GetBlockError>> {
        let served = self.enter("get_raw_block")?;
        match served.at(height) {
            Some(block) => raw(block, |raw| raw.block.clone()),
            None => refused(GetBlockError::HeightNotFound(height)),
        }
    }
}

impl zaino_source::OneShotGetRawBlockByHash for MockChain {
    async fn get_raw_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockByHashError>> {
        let served = self.enter("get_raw_block_by_hash")?;
        match served.by_hash(hash) {
            Some(block) => raw(block, |raw| raw.block.clone()),
            None => refused(GetBlockByHashError::NotFound(hash)),
        }
    }
}

impl zaino_source::OneShotGetRawBlockHeader for MockChain {
    async fn get_raw_block_header(
        &self,
        hash: BlockHash,
    ) -> Result<Vec<u8>, QueryError<GetBlockHeaderError>> {
        let served = self.enter("get_raw_block_header")?;
        match served.by_hash(hash) {
            Some(block) => raw(block, |raw| raw.header.clone()),
            None => refused(GetBlockHeaderError::BlockNotFound(hash)),
        }
    }
}

impl zaino_source::OneShotGetPreIndexCompactBlock for MockChain {
    async fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> Result<PreIndexCompactBlock, QueryError<GetBlockError>> {
        let served = self.enter("get_pre_index_compact_block")?;
        match served.at(height) {
            Some(block) => Ok(PreIndexCompactBlock::from(&block.block)),
            None => refused(GetBlockError::HeightNotFound(height)),
        }
    }
}

impl zaino_source::OneShotGetBestBlockHeight for MockChain {
    async fn get_best_block_height(&self) -> Result<Height, QueryError<GetBestBlockHeightError>> {
        let served = self.enter("get_best_block_height")?;
        match served.tip() {
            Some(tip) => Ok(tip.height()),
            None => refused(GetBestBlockHeightError::NotReady),
        }
    }
}

impl zaino_source::OneShotGetChainTip for MockChain {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        let served = self.enter("get_chain_tip")?;
        match served.tip() {
            Some(tip) => Ok((tip.hash(), tip.height())),
            None => refused(GetChainTipError::NotReady),
        }
    }
}

impl zaino_source::OneShotGetChainTips for MockChain {
    /// The active tip, and each side branch's tip with its length back to
    /// the best chain.
    async fn get_chain_tips(&self) -> Result<Vec<ChainTip>, QueryError<GetChainTipsError>> {
        let served = self.enter("get_chain_tips")?;
        let Some(tip) = served.tip() else {
            return refused(GetChainTipsError::NotReady);
        };
        let mut tips = vec![ChainTip {
            height: tip.height(),
            hash: tip.hash(),
            branch_len: 0,
            status: ChainTipStatus::Active,
        }];
        let side: Vec<&MockBlock> = served.side_blocks().collect();
        for block in &side {
            let has_child = side
                .iter()
                .any(|other| other.block.header.prev_hash == block.hash());
            if has_child {
                continue;
            }
            tips.push(ChainTip {
                height: block.height(),
                hash: block.hash(),
                branch_len: branch_len(&served, block),
                status: ChainTipStatus::ValidFork,
            });
        }
        tips.sort_by_key(|tip| {
            (
                tip.status != ChainTipStatus::Active,
                std::cmp::Reverse(tip.height),
            )
        });
        Ok(tips)
    }
}

/// Blocks from `block` back to the best chain.
fn branch_len(served: &Served<'_>, block: &MockBlock) -> u32 {
    let mut len = 0u32;
    let mut at = block;
    while !served.is_best(at.hash()) {
        len = len.saturating_add(1);
        match served.by_hash(at.block.header.prev_hash) {
            Some(parent) => at = parent,
            None => break,
        }
    }
    len
}
