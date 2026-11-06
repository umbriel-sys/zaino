//! I-08 `chain_metadata`: height → each shielded pool's tree root and
//! cumulative size after the block.
//!
//! A pool before its activation is stored as absent, never as an empty tree.

use core::ops::Range;

use zaino_persistence::{sequence_key, BackendReader, Namespace, WriteOp};
use zaino_primitives::types::{
    BlockRef, Height, ShieldedPool, TreeRoot, TreeRootInfo, TreeRoots, TreeSize,
};

use crate::block::FoldBlock;
use crate::codec::{array, le_u32};
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "chain_metadata";

const METADATA: Namespace = Namespace::new("chain_metadata");

declared! {
    /// Per pool, in [`ShieldedPool::ALL`] order: present (0 or 1), root, size.
    pub const POOLS: Layout = Layout::new(
        "pool_trees",
        1,
        &[
            Field::fixed("sapling_present", 1),
            Field::fixed("sapling_root", 32),
            Field::fixed("sapling_size", 4),
            Field::fixed("orchard_present", 1),
            Field::fixed("orchard_root", 32),
            Field::fixed("orchard_size", 4),
            Field::fixed("ironwood_present", 1),
            Field::fixed("ironwood_root", 32),
            Field::fixed("ironwood_size", 4),
        ],
    );
}
/// Fields per pool: present, root, size.
const PER_POOL: usize = 3;
declared! { const SAPLING_SIZE: Range<usize> = POOLS.fixed_range(2); }
declared! { const ORCHARD_SIZE: Range<usize> = POOLS.fixed_range(5); }
declared! { const IRONWOOD_SIZE: Range<usize> = POOLS.fixed_range(8); }
declared! { const SAPLING_PRESENT: Range<usize> = POOLS.fixed_range(0); }
declared! { const ORCHARD_PRESENT: Range<usize> = POOLS.fixed_range(3); }
declared! { const IRONWOOD_PRESENT: Range<usize> = POOLS.fixed_range(6); }

/// The schema: one fixed record per height.
pub const SCHEMA: Schema = Schema::new(NAME, 1, &[NamespaceLayout::sequence(METADATA, &POOLS)]);

/// I-08.
pub struct ChainMetadata;

/// The last stored block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: BlockRef,
}

impl Index for ChainMetadata {
    const SCHEMA: Schema = SCHEMA;
    const DEPENDENCIES: &'static [&'static str] = &[];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        let Some(tip) = backend.tip()? else {
            return Ok(None);
        };
        if backend.get(METADATA, &key(tip.height))?.is_none() {
            return Err(FoldError::continuity(NAME, "tip without its metadata"));
        }
        Ok(Some(State { tip }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let roots = block.tree_roots();
        let encoded = ShieldedPool::ALL.map(|pool| pool_fields(pool_of(roots, pool)));
        let fields: Vec<&[u8]> = encoded
            .iter()
            .flat_map(|(present, root, size)| [&present[..], &root[..], &size[..]])
            .collect();
        let reference = block.reference();
        Ok(Fold {
            ops: vec![WriteOp::Put {
                namespace: METADATA,
                key: key(reference.height),
                value: POOLS.encode(&fields)?,
            }],
            next: State { tip: reference },
        })
    }
}

/// Typed reads over a backend holding this index.
#[derive(Debug, Clone)]
pub struct Reader<'a, R> {
    backend: &'a R,
}

impl<'a, R: BackendReader> Reader<'a, R> {
    /// Reads through `backend`.
    pub fn new(backend: &'a R) -> Self {
        Self { backend }
    }

    /// Every pool's root and size after the block at `height`.
    pub fn roots(&self, height: Height) -> Result<Option<TreeRoots>, FoldError> {
        let Some(record) = self.backend.get(METADATA, &key(height))? else {
            return Ok(None);
        };
        let pool = |pool: ShieldedPool| -> Result<Option<TreeRootInfo>, FoldError> {
            let first = pool_index(pool).saturating_mul(PER_POOL);
            let field = |offset: usize| POOLS.field(&record, first.saturating_add(offset));
            if !present(field(0)?)? {
                return Ok(None);
            }
            Ok(Some(TreeRootInfo {
                root: TreeRoot::from(array::<32>(field(1)?, "root")?),
                size: TreeSize::from(le_u32(field(2)?, "size")?),
            }))
        };
        Ok(Some(TreeRoots {
            sapling: pool(ShieldedPool::Sapling)?,
            orchard: pool(ShieldedPool::Orchard)?,
            ironwood: pool(ShieldedPool::Ironwood)?,
        }))
    }

    /// `pool`'s cumulative size after `height`: `Some(None)` before the
    /// pool's activation, `None` above the tip. Two field reads.
    pub fn size(
        &self,
        height: Height,
        pool: ShieldedPool,
    ) -> Result<Option<Option<TreeSize>>, FoldError> {
        let (present_at, size_at) = match pool {
            ShieldedPool::Sapling => (SAPLING_PRESENT, SAPLING_SIZE),
            ShieldedPool::Orchard => (ORCHARD_PRESENT, ORCHARD_SIZE),
            ShieldedPool::Ironwood => (IRONWOOD_PRESENT, IRONWOOD_SIZE),
        };
        let key = key(height);
        let Some(flag) = self.backend.get_range(METADATA, &key, present_at)? else {
            return Ok(None);
        };
        if !present(&flag)? {
            return Ok(Some(None));
        }
        let Some(size) = self.backend.get_range(METADATA, &key, size_at)? else {
            return Err(FoldError::continuity(NAME, "record vanished between reads"));
        };
        Ok(Some(Some(TreeSize::from(le_u32(&size, "size")?))))
    }
}

fn key(height: Height) -> Vec<u8> {
    sequence_key(u64::from(height))
}

fn pool_index(pool: ShieldedPool) -> usize {
    match pool {
        ShieldedPool::Sapling => 0,
        ShieldedPool::Orchard => 1,
        ShieldedPool::Ironwood => 2,
    }
}

fn pool_of(roots: &TreeRoots, pool: ShieldedPool) -> Option<&TreeRootInfo> {
    match pool {
        ShieldedPool::Sapling => roots.sapling.as_ref(),
        ShieldedPool::Orchard => roots.orchard.as_ref(),
        ShieldedPool::Ironwood => roots.ironwood.as_ref(),
    }
}

/// A pool's three fields; an absent pool is all zeros with its flag clear.
fn pool_fields(info: Option<&TreeRootInfo>) -> ([u8; 1], [u8; 32], [u8; 4]) {
    match info {
        Some(info) => (
            [1],
            <[u8; 32]>::from(info.root),
            info.size.get().to_le_bytes(),
        ),
        None => ([0], [0; 32], [0; 4]),
    }
}

fn present(flag: &[u8]) -> Result<bool, FoldError> {
    match array::<1>(flag, "present")? {
        [0] => Ok(false),
        [1] => Ok(true),
        [other] => Err(FoldError::continuity(NAME, format!("present flag {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use zaino_persistence::{Backend as _, SchemaDigest};

    use super::*;
    use crate::testing::{block_with, fold_all, tx};

    fn info(byte: u8, size: u32) -> TreeRootInfo {
        TreeRootInfo {
            root: TreeRoot::from([byte; 32]),
            size: TreeSize::from(size),
        }
    }

    fn with_roots(n: u32, roots: TreeRoots) -> FoldBlock {
        let block = block_with(n, vec![tx(n)]);
        FoldBlock::try_new(block.header().clone(), block.transactions().to_vec(), roots)
            .expect("fixture")
    }

    /// Roots and sizes read back whole and by pool; a pool before its
    /// activation reads as absent, distinct from above the tip.
    #[test]
    fn roots_and_sizes_read_back_with_absent_pools() {
        let chain = [
            with_roots(
                0,
                TreeRoots {
                    sapling: None,
                    orchard: None,
                    ironwood: None,
                },
            ),
            with_roots(
                1,
                TreeRoots {
                    sapling: Some(info(1, 7)),
                    orchard: None,
                    ironwood: None,
                },
            ),
            with_roots(
                2,
                TreeRoots {
                    sapling: Some(info(2, 9)),
                    orchard: Some(info(3, 0)),
                    ironwood: Some(info(4, u32::MAX)),
                },
            ),
        ];
        let backend = crate::testing::backend(&SCHEMA);
        fold_all::<ChainMetadata>(&backend, &chain).expect("folded");
        let reader = backend.reader().expect("reader");
        let reader = Reader::new(&reader);
        for block in &chain {
            let height = block.header().height;
            let read = reader.roots(height).expect("read").expect("present");
            for pool in ShieldedPool::ALL {
                let expected = pool_of(block.tree_roots(), pool);
                assert_eq!(pool_of(&read, pool), expected, "{pool} at {height}");
                assert_eq!(
                    reader.size(height, pool).expect("size"),
                    Some(expected.map(|info| info.size)),
                );
            }
        }
        let above = Height::try_from(3).expect("3");
        assert_eq!(
            reader.size(above, ShieldedPool::Sapling).expect("read"),
            None
        );
        assert!(reader.roots(above).expect("read").is_none());
    }

    /// The schema digest every `chain_metadata_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0x17, 0xcf, 0x0c, 0x68, 0x45, 0xa6, 0x0b, 0xd7, 0x37, 0x4f, 0xe3, 0x79, 0xbf, 0xdb,
            0x45, 0x6e, 0x94, 0x69, 0x60, 0x88, 0x9f, 0x4f, 0x04, 0x7f, 0x27, 0x60, 0x7b, 0x7e,
            0xfc, 0xce, 0x94, 0x1c,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "chain_metadata_1");
    }
}
