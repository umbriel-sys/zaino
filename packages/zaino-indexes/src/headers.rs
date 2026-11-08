//! I-01 `headers`: height → header and cumulative chain work; hash → height.

use core::ops::Range;

use zaino_persistence::{sequence_key, BackendReader, Namespace, WriteOp};
use zaino_primitives::types::{
    AbsoluteChainWork, BlockCommitments, BlockHash, BlockHeader, BlockRef, CompactDifficulty,
    EquihashSolution, Height, MerkleRoot,
};

use crate::block::FoldBlock;
use crate::codec::{self, array, le_u32};
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "headers";

const HEADERS: Namespace = Namespace::new("headers");
const HEIGHTS: Namespace = Namespace::new("heights");

declared! {
    /// A header at its height, with the chain work through it.
    pub const HEADER: Layout = Layout::new(
        "header",
        1,
        &[
            Field::fixed("hash", 32),
            Field::fixed("version", 4),
            Field::fixed("prev_hash", 32),
            Field::fixed("time", 4),
            Field::fixed("merkle_root", 32),
            Field::fixed("block_commitments", 32),
            Field::fixed("bits", 4),
            Field::fixed("nonce", 32),
            Field::fixed("chainwork", 32),
            Field::var("solution"),
        ],
    );
}
const HASH: usize = 0;
const VERSION: usize = 1;
const PREV_HASH: usize = 2;
const TIME: usize = 3;
const MERKLE_ROOT: usize = 4;
const BLOCK_COMMITMENTS: usize = 5;
const BITS: usize = 6;
const NONCE: usize = 7;
const CHAINWORK: usize = 8;
const SOLUTION: usize = 9;
declared! { const HASH_RANGE: Range<usize> = HEADER.fixed_range(HASH); }
declared! { const CHAINWORK_RANGE: Range<usize> = HEADER.fixed_range(CHAINWORK); }

declared! {
    /// A block hash as a map key.
    pub const BLOCK_HASH: Layout = Layout::new("block_hash", 1, &[Field::fixed("hash", 32)]);
}
declared! {
    /// A height as a map value.
    pub const HEIGHT: Layout = Layout::new("height", 1, &[Field::fixed("height", 4)]);
}
declared! {
    const HEIGHTS_BY_HASH: NamespaceLayout =
        NamespaceLayout::map(HEIGHTS, &BLOCK_HASH, &HEIGHT, false);
}

/// The schema: `headers` by height, `heights` by hash.
pub const SCHEMA: Schema = Schema::new(
    NAME,
    1,
    &[NamespaceLayout::sequence(HEADERS, &HEADER), HEIGHTS_BY_HASH],
);

/// I-01.
pub struct Headers;

/// The last stored block and the chain work through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: BlockRef,
    /// Cumulative work through it.
    pub chainwork: AbsoluteChainWork,
}

impl Index for Headers {
    const SCHEMA: Schema = SCHEMA;
    const READS: &'static [&'static str] = &[];
    const BUILDS_FROM: &'static [&'static str] = &[];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        let reader = Reader::new(backend);
        let Some(tip) = backend.tip()? else {
            return Ok(None);
        };
        let chainwork = reader
            .chainwork(tip.height)?
            .ok_or_else(|| FoldError::continuity(NAME, "tip without its header"))?;
        Ok(Some(State { tip, chainwork }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let header = block.header();
        let work = header.bits.to_work();
        let chainwork = match state {
            Some(state) => state.chainwork.accumulate(work)?,
            None => AbsoluteChainWork::genesis(work),
        };
        let record = HEADER.encode(&[
            &<[u8; 32]>::from(header.hash),
            &header.version.to_le_bytes(),
            &<[u8; 32]>::from(header.prev_hash),
            &header.time.to_le_bytes(),
            &<[u8; 32]>::from(header.merkle_root),
            &<[u8; 32]>::from(header.block_commitments),
            &header.bits.as_bits().to_le_bytes(),
            &header.nonce,
            &chainwork.to_be_bytes(),
            header.solution.as_bytes(),
        ])?;
        let ops = vec![
            WriteOp::Put {
                namespace: HEADERS,
                key: sequence_key(u64::from(header.height)),
                value: record,
            },
            WriteOp::Put {
                namespace: HEIGHTS,
                key: <[u8; 32]>::from(header.hash).to_vec(),
                value: u32::from(header.height).to_le_bytes().to_vec(),
            },
        ];
        Ok(Fold {
            ops,
            next: State {
                tip: BlockRef {
                    hash: header.hash,
                    height: header.height,
                },
                chainwork,
            },
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

    /// The header at `height`.
    pub fn header(&self, height: Height) -> Result<Option<BlockHeader>, FoldError> {
        self.backend
            .with_value(HEADERS, &sequence_key(u64::from(height)), |record| {
                decode_header(record, height)
            })?
            .transpose()
    }

    /// The hash at `height`: one field, not the record.
    pub fn hash(&self, height: Height) -> Result<Option<BlockHash>, FoldError> {
        let Some(bytes) =
            self.backend
                .get_bytes(HEADERS, &sequence_key(u64::from(height)), HASH_RANGE)?
        else {
            return Ok(None);
        };
        Ok(Some(BlockHash::from(array::<32>(&bytes, "hash")?)))
    }

    /// Cumulative chain work through `height`.
    pub fn chainwork(&self, height: Height) -> Result<Option<AbsoluteChainWork>, FoldError> {
        let Some(bytes) =
            self.backend
                .get_bytes(HEADERS, &sequence_key(u64::from(height)), CHAINWORK_RANGE)?
        else {
            return Ok(None);
        };
        AbsoluteChainWork::from_be_bytes(array::<32>(&bytes, "chainwork")?)
            .map(Some)
            .map_err(|_| FoldError::continuity(NAME, "stored chain work of zero"))
    }

    /// The height of the block with `hash`.
    pub fn height(&self, hash: BlockHash) -> Result<Option<Height>, FoldError> {
        self.backend
            .with_value(HEIGHTS, &<[u8; 32]>::from(hash), |bytes| {
                codec::height(NAME, le_u32(bytes, "height")?)
            })?
            .transpose()
    }
}

/// The header a stored record holds at `height`.
fn decode_header(record: &[u8], height: Height) -> Result<BlockHeader, FoldError> {
    let field = |index: usize| HEADER.field(record, index);
    let bits = le_u32(field(BITS)?, "bits")?;
    let solution = field(SOLUTION)?;
    let solution = match (
        <[u8; 1344]>::try_from(solution),
        <[u8; 36]>::try_from(solution),
    ) {
        (Ok(standard), _) => EquihashSolution::Standard(standard),
        (_, Ok(regtest)) => EquihashSolution::Regtest(regtest),
        _ => {
            return Err(FoldError::continuity(
                NAME,
                format!("a {}-byte equihash solution", solution.len()),
            ))
        }
    };
    Ok(BlockHeader {
        hash: BlockHash::from(array::<32>(field(HASH)?, "hash")?),
        version: le_u32(field(VERSION)?, "version")?,
        prev_hash: BlockHash::from(array::<32>(field(PREV_HASH)?, "prev_hash")?),
        height,
        time: le_u32(field(TIME)?, "time")?,
        merkle_root: MerkleRoot::from(array::<32>(field(MERKLE_ROOT)?, "merkle_root")?),
        block_commitments: BlockCommitments::from(array::<32>(
            field(BLOCK_COMMITMENTS)?,
            "block_commitments",
        )?),
        bits: CompactDifficulty::try_from_bits(bits)
            .map_err(|_| FoldError::continuity(NAME, format!("stored bits {bits:#x}")))?,
        nonce: array::<32>(field(NONCE)?, "nonce")?,
        solution,
    })
}

#[cfg(test)]
mod tests {
    use zaino_persistence::{Backend as _, SchemaDigest};
    use zaino_primitives::types::Height;

    use super::*;
    use crate::testing::{backend, block, block_of, fold_all, hash, header, REGTEST_BITS};

    /// Every header folded reads back whole and by field, the hash map
    /// answers the height, and the chain work accumulates block by block.
    #[test]
    fn headers_read_back_by_height_hash_and_field() {
        let backend = backend(&SCHEMA);
        let chain: Vec<_> = (0..3).map(block).collect();
        let state = fold_all::<Headers>(&backend, &chain).expect("three blocks");
        let reader = backend.reader().expect("reader");
        let reader = Reader::new(&reader);

        let work = CompactDifficulty::try_from_bits(REGTEST_BITS)
            .expect("bits")
            .to_work();
        let mut expected = AbsoluteChainWork::genesis(work);
        for block in &chain {
            let header = block.header();
            let height = header.height;
            assert_eq!(
                reader.header(height).expect("header").as_ref(),
                Some(header)
            );
            assert_eq!(reader.hash(height).expect("hash"), Some(header.hash));
            assert_eq!(reader.height(header.hash).expect("height"), Some(height));
            assert_eq!(reader.chainwork(height).expect("chainwork"), Some(expected));
            expected = expected.accumulate(work).expect("fits");
        }
        assert_eq!(state.tip.height, Height::try_from(2).expect("2"));
        assert_eq!(
            state.chainwork,
            reader
                .chainwork(state.tip.height)
                .expect("work")
                .expect("held")
        );
        assert_eq!(
            reader
                .header(Height::try_from(3).expect("3"))
                .expect("absent"),
            None
        );
        assert_eq!(reader.height(hash(9)).expect("absent"), None);
    }

    /// A block that does not extend the stored tip is refused by name, and
    /// so is a first block that is not genesis.
    #[test]
    fn a_block_off_the_chain_is_refused() {
        let backend = backend(&SCHEMA);
        let state = fold_all::<Headers>(&backend, &[block(0), block(1)]).expect("two blocks");

        let gap = Headers::fold(Some(&state), &block(3));
        assert!(
            matches!(&gap, Err(FoldError::Continuity { index: NAME, what }) if what == "height 3 after 1"),
            "{gap:?}"
        );
        let mut forked = header(2);
        forked.prev_hash = hash(7);
        let forked = Headers::fold(Some(&state), &block_of(forked));
        assert!(
            matches!(&forked, Err(FoldError::Continuity { index: NAME, what }) if what.contains("does not extend")),
            "{forked:?}"
        );
        let late = Headers::fold(None, &block(1));
        assert!(
            matches!(&late, Err(FoldError::Continuity { index: NAME, what }) if what == "height 1 into an empty index"),
            "{late:?}"
        );
    }

    /// Reopening resumes at the stored tip with its chain work, and an empty
    /// directory resumes at nothing.
    #[test]
    fn resume_rebuilds_the_state_from_the_tip() {
        let backend = backend(&SCHEMA);
        assert_eq!(
            Headers::resume(&backend.reader().expect("reader")).expect("empty"),
            None
        );
        let folded = fold_all::<Headers>(&backend, &[block(0), block(1)]);
        let resumed = Headers::resume(&backend.reader().expect("reader")).expect("resume");
        assert_eq!(resumed, folded);
    }

    /// The schema digest every `headers_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0x1b, 0xe3, 0x13, 0x90, 0xa4, 0x20, 0x66, 0xd4, 0x07, 0x54, 0xaf, 0xb7, 0x7c, 0x1e,
            0x19, 0x49, 0xe3, 0x04, 0x78, 0xb1, 0x4b, 0x61, 0xdc, 0x12, 0x3e, 0x82, 0xd2, 0x3f,
            0xd0, 0x01, 0x7b, 0x92,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "headers_1");
    }
}
