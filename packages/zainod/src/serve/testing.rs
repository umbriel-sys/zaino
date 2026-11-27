//! The fake chain the served handlers' tests share.

use std::sync::OnceLock;

use zaino_primitives::types::ShieldedPool;
use zaino_source_mock::{BlockSpec, ChainBuilder, MockBlock};

/// The blocks through `tip` of one chain, 130 long: every block spends two
/// outputs once coinbases mature, every tenth adds Sapling and Orchard notes.
pub(crate) fn blocks(tip: u32) -> Vec<MockBlock> {
    const LENGTH: u32 = 130;
    static BLOCKS: OnceLock<Vec<MockBlock>> = OnceLock::new();
    assert!(tip <= LENGTH, "the shared chain is {LENGTH} blocks");
    let blocks = BLOCKS.get_or_init(|| {
        (1..=LENGTH)
            .fold(ChainBuilder::new(), |builder, height| {
                let spec = BlockSpec::spends(2);
                builder.block(match height % 10 {
                    0 => spec
                        .notes(ShieldedPool::Sapling, 2)
                        .notes(ShieldedPool::Orchard, 2),
                    _ => spec,
                })
            })
            .into_blocks()
    });
    // genesis and `tip` blocks
    blocks.iter().take(tip as usize + 1).cloned().collect()
}
