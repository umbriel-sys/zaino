//! Non-validating raw block parser: zcash wire bytes → domain [`Block`].
//!
//! Fields are sliced off the wire and curve points stay bytes: nothing is
//! decompressed or checked against a curve (D-66). Structure is enforced —
//! lengths, canonical CompactSize, known versions, value ranges, the BIP 34
//! coinbase height — so malformed input is a typed error, never a panic.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::let_underscore_must_use
    )
)]

mod reader;
#[cfg(test)]
mod tests;
mod txid;

use sha2::{Digest as _, Sha256};
use zaino_primitives::types::{
    Block, BlockCommitments, BlockError, BlockHash, BlockHeader, ChainMetadata, CompactCiphertext,
    CompactDifficulty, CompactDifficultyError, EphemeralKey, EquihashSolution, Height,
    HeightOverflow, MerkleRoot, NoteCommitment, Nullifier, OrchardAction, OrchardData, SaplingData,
    SaplingOutput, SaplingSpend, Script, ShieldedPool, SignedZatoshis, SignedZatoshisOverflow,
    Transaction, TransactionId, TransparentData, TransparentInput, TransparentOutput, Zatoshis,
    ZatoshisOverflow,
};

use reader::Reader;

const V3_GROUP_ID: u32 = 0x03C4_8270;
const V4_GROUP_ID: u32 = 0x892F_2085;
const V5_GROUP_ID: u32 = 0x26A7_270A;
const V6_GROUP_ID: u32 = 0xD884_B698;

const GROTH_PROOF_LEN: usize = 192;
const BCTV14_PROOF_LEN: usize = 296;
const SIGNATURE_LEN: usize = 64;

/// Smallest encodings, bounding CompactSize counts against the bytes left.
const MIN_TX_LEN: usize = 10;
const MIN_INPUT_LEN: usize = 41;
const MIN_OUTPUT_LEN: usize = 9;
/// v4 spend: cv, anchor, nullifier, rk, proof, spendAuthSig.
const SPEND_V4_LEN: usize = 384;
/// v5 spend prefix: cv, nullifier, rk.
const SPEND_V5_LEN: usize = 96;
/// v5 output prefix: cv, cmu, epk, encCiphertext, outCiphertext.
const OUTPUT_V5_LEN: usize = 756;
/// v4 output: the v5 prefix and its proof.
const OUTPUT_V4_LEN: usize = 948;
/// Orchard-shaped action: cv, nullifier, rk, cmx, epk, encCiphertext, outCiphertext.
const ACTION_LEN: usize = 820;
/// JoinSplit after its two values, less the proof: anchor, nullifiers,
/// commitments, ephemeral key, random seed, MACs, ciphertexts.
const JOINSPLIT_REST_LEN: usize = 1490;
/// JoinSplit without its proof.
const JOINSPLIT_FIXED_LEN: usize = 1506;
/// `joinSplitPubKey` and `joinSplitSig`.
const JOINSPLIT_AUTH_LEN: usize = 96;
/// v4 spend after rk: proof and spendAuthSig.
const SPEND_V4_AUTH_LEN: usize = 256;

/// Sprout's branch id, which `zcash_protocol` also accepts on the wire.
const SPROUT_BRANCH_ID: u32 = 0;

/// Orchard-shaped flags: spends and outputs enabled.
const FLAGS_SPENDS_OUTPUTS: u8 = 0b0000_0011;
/// Ironwood's `enableCrossAddress`; reserved in Orchard.
const FLAG_CROSS_ADDRESS: u8 = 0b0000_0100;

const MIN_COINBASE_SCRIPT_LEN: usize = 2;
const MAX_COINBASE_SCRIPT_LEN: usize = 100;

/// The genesis coinbase script, which predates BIP 34 and carries no height.
const GENESIS_COINBASE_SCRIPT_SIG: [u8; 77] = [
    4, 255, 255, 7, 31, 1, 4, 69, 90, 99, 97, 115, 104, 48, 98, 57, 99, 52, 101, 101, 102, 56, 98,
    55, 99, 99, 52, 49, 55, 101, 101, 53, 48, 48, 49, 101, 51, 53, 48, 48, 57, 56, 52, 98, 54, 102,
    101, 97, 51, 53, 54, 56, 51, 97, 55, 99, 97, 99, 49, 52, 49, 97, 48, 52, 51, 99, 52, 50, 48,
    54, 52, 56, 51, 53, 100, 51, 52,
];

/// Errors from [`parse_block`].
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum RawBlockError {
    /// Input ended inside a field.
    #[error("truncated: {needed} bytes needed at offset {offset}")]
    Truncated {
        /// Where the field starts.
        offset: usize,
        /// The field's length.
        needed: usize,
    },
    /// A CompactSize encoded in more bytes than its value needs.
    #[error("non-canonical CompactSize at offset {0}")]
    NonCanonicalCompactSize(usize),
    /// A CompactSize above the protocol maximum.
    #[error("CompactSize {0} exceeds the protocol maximum")]
    CompactSizeTooLarge(u64),
    /// A count whose items cannot fit in the remaining bytes.
    #[error("count {count} at offset {offset} exceeds the {remaining} bytes left")]
    CountExceedsData {
        /// Where the count starts.
        offset: usize,
        /// The count read.
        count: u64,
        /// Bytes left after it.
        remaining: usize,
    },
    /// Bytes after the last transaction.
    #[error("{0} trailing bytes after the block")]
    TrailingBytes(usize),
    /// Input larger than the maximum block size.
    #[error("{0} bytes exceeds the maximum block size")]
    BlockTooLarge(usize),
    /// A block version below 4 or with the high bit set.
    #[error("invalid block version {0}")]
    BlockVersion(u32),
    /// An Equihash solution of neither defined length.
    #[error("Equihash solution of {0} bytes")]
    SolutionLength(u64),
    /// A transaction version no format defines.
    #[error("unknown transaction version {0}")]
    TxVersion(u32),
    /// An overwintered version whose version group does not match it.
    #[error("version group {group:#010x} does not match transaction version {version}")]
    VersionGroup {
        /// The version read.
        version: u32,
        /// The version group read.
        group: u32,
    },
    /// A coinbase script outside BIP 34's encoding.
    #[error("coinbase script: {0}")]
    CoinbaseScript(&'static str),
    /// The first transaction's first input is not a coinbase.
    #[error("block has no coinbase height")]
    NoCoinbaseHeight,
    /// A coinbase transaction with Sapling spends.
    #[error("coinbase transaction has Sapling spends")]
    CoinbaseSaplingSpend,
    /// A v4 Sapling value balance with no spends or outputs to carry it.
    #[error("non-zero Sapling value balance without Sapling spends or outputs")]
    UnbalancedEmptySapling,
    /// A transparent or JoinSplit value above the money supply.
    #[error("value: {0}")]
    Value(#[from] ZatoshisOverflow),
    /// A value balance whose magnitude exceeds the money supply.
    #[error("value balance: {0}")]
    ValueBalance(#[from] SignedZatoshisOverflow),
    /// A coinbase height above the protocol maximum.
    #[error("height: {0}")]
    Height(#[from] HeightOverflow),
    /// Header nBits outside the compact difficulty acceptance set.
    #[error("nBits: {0}")]
    Difficulty(#[from] CompactDifficultyError),
    /// A v5/v6 consensus branch id no network upgrade defines.
    #[error("unknown consensus branch id {0:#010x}")]
    BranchId(u32),
    /// A v6 transaction under a branch before NU6.3.
    #[error("v6 transaction under pre-NU6.3 branch id {0:#010x}")]
    V6BeforeNu6_3(u32),
    /// A non-empty bundle under a branch before its pool activates.
    #[error("{pool} bundle under branch id {branch:#010x}, before the pool activates")]
    PoolInactive {
        /// The bundle's pool.
        pool: ShieldedPool,
        /// The transaction's branch id.
        branch: u32,
    },
    /// Reserved flag bits set.
    #[error("{pool} flags {flags:#04x} set reserved bits")]
    Flags {
        /// The bundle's pool.
        pool: ShieldedPool,
        /// The flag byte read.
        flags: u8,
    },
    /// The parts did not form a block.
    #[error("block: {0}")]
    Block(#[from] BlockError),
    /// The header's merkle root is not the root of the block's txids.
    #[error("merkle root does not commit to the block's txids")]
    MerkleRoot,
    /// A field element encoded at or above its field's modulus.
    #[error("non-canonical {0}")]
    NonCanonical(&'static str),
    /// zebra's parse refused the block.
    #[error("zebra refused the block: {0}")]
    Upstream(String),
    /// zebra's parse gave a different block.
    #[error("zebra parsed a different block")]
    UpstreamMismatch,
}

/// How much of a raw block to check beyond its structure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BlockVerification {
    /// Structure only: the validator has verified the block.
    #[default]
    Off,
    /// Also the header's merkle root over the txids, and every anchor, note
    /// commitment and Orchard-shaped nullifier as a canonical field element.
    Light,
    /// Light, then zebra's own parse (curve points included), which must
    /// give the same block. Parse-level validity: no proofs or signatures.
    Full,
}

/// Parse a zcash-serialized block.
///
/// `chain_metadata` is indexed state, not block data, so the caller supplies it.
pub(crate) fn parse_block(
    bytes: &[u8],
    chain_metadata: ChainMetadata,
    verification: BlockVerification,
) -> Result<Block, RawBlockError> {
    let block = parse_checked(bytes, chain_metadata.clone(), verification)?;
    if verification == BlockVerification::Full && upstream(bytes, chain_metadata)? != block {
        return Err(RawBlockError::UpstreamMismatch);
    }
    Ok(block)
}

/// zebra's block from `bytes`, which it must consume entirely.
fn upstream(bytes: &[u8], chain_metadata: ChainMetadata) -> Result<Block, RawBlockError> {
    use zebra_chain::serialization::ZcashDeserialize as _;
    let mut cursor = std::io::Cursor::new(bytes);
    let block = zebra_chain::block::Block::zcash_deserialize(&mut cursor)
        .map_err(|e| RawBlockError::Upstream(e.to_string()))?;
    if usize::try_from(cursor.position()).ok() != Some(bytes.len()) {
        return Err(RawBlockError::Upstream("trailing bytes".into()));
    }
    zaino_primitives::zebra::block_from_zebra(&block, chain_metadata)
        .map_err(|e| RawBlockError::Upstream(e.to_string()))
}

/// The bitcoin-style merkle root of `txids`: pairs hashed with SHA-256d, an
/// odd level's last node paired with itself.
fn merkle_root(txids: Vec<[u8; 32]>) -> [u8; 32] {
    let mut level = txids;
    while level.len() > 1 {
        level = level
            .chunks(2)
            .map(|pair| {
                let mut h = Sha256::new();
                for node in pair.iter().chain(pair.last()).take(2) {
                    h.update(node);
                }
                Sha256::digest(h.finalize()).into()
            })
            .collect();
    }
    level.first().copied().unwrap_or_default()
}

/// Every anchor, note commitment and Orchard-shaped nullifier in `raw` is a
/// canonical field element (no point is decompressed).
fn check_fields(raw: &RawTx<'_>) -> Result<(), RawBlockError> {
    fn canonical(is_some: bool, field: &'static str) -> Result<(), RawBlockError> {
        match is_some {
            true => Ok(()),
            false => Err(RawBlockError::NonCanonical(field)),
        }
    }
    use orchard::note::{ExtractedNoteCommitment as Cmx, Nullifier};
    use sapling_crypto::note::ExtractedNoteCommitment as Cmu;
    let sapling = &raw.sapling;
    let anchors = sapling.spends.iter().filter_map(|spend| spend.anchor);
    for anchor in anchors.chain(sapling.anchor) {
        let anchor = sapling_crypto::Anchor::from_bytes(anchor);
        canonical(anchor.is_some().into(), "Sapling anchor")?;
    }
    for output in &sapling.outputs {
        let cmu = Cmu::from_bytes(&output.note.cm);
        canonical(cmu.is_some().into(), "Sapling note commitment")?;
    }
    for bundle in raw.orchard.iter().chain(raw.ironwood.iter()) {
        let anchor = orchard::Anchor::from_bytes(bundle.anchor);
        canonical(anchor.is_some().into(), "Orchard anchor")?;
        for action in &bundle.actions {
            let nullifier = Nullifier::from_bytes(&action.nullifier);
            canonical(nullifier.is_some().into(), "Orchard nullifier")?;
            let cmx = Cmx::from_bytes(&action.note.cm);
            canonical(cmx.is_some().into(), "Orchard note commitment")?;
        }
    }
    Ok(())
}

/// The block, structure checked, plus Light's checks from `Light`.
fn parse_checked(
    bytes: &[u8],
    chain_metadata: ChainMetadata,
    verification: BlockVerification,
) -> Result<Block, RawBlockError> {
    let light = verification != BlockVerification::Off;
    if u64::try_from(bytes.len()).map_or(true, |len| len > zaino_consensus::MAX_BLOCK_BYTES) {
        return Err(RawBlockError::BlockTooLarge(bytes.len()));
    }
    let mut r = Reader::new(bytes);
    let header = read_header(&mut r)?;
    let count = r.count(MIN_TX_LEN)?;
    let mut transactions = Vec::with_capacity(count);
    let mut height = None;
    for index in 0..count {
        let parsed = read_transaction(&mut r, light)?;
        if index == 0 {
            height = parsed.coinbase_height;
        }
        transactions.push(parsed.transaction);
    }
    r.finish()?;
    let height = height.ok_or(RawBlockError::NoCoinbaseHeight)?;
    if light {
        let txids = transactions.iter().map(|tx| tx.txid.into()).collect();
        if merkle_root(txids) != header.merkle_root {
            return Err(RawBlockError::MerkleRoot);
        }
    }
    Ok(Block::try_new(
        header.into_header(height),
        transactions,
        chain_metadata,
    )?)
}

/// Header fields, before the coinbase supplies the height.
struct RawHeader {
    hash: [u8; 32],
    version: u32,
    prev_hash: [u8; 32],
    merkle_root: [u8; 32],
    block_commitments: [u8; 32],
    time: u32,
    bits: CompactDifficulty,
    nonce: [u8; 32],
    solution: EquihashSolution,
}

impl RawHeader {
    fn into_header(self, height: Height) -> BlockHeader {
        BlockHeader {
            hash: BlockHash::from(self.hash),
            version: self.version,
            prev_hash: BlockHash::from(self.prev_hash),
            height,
            time: self.time,
            merkle_root: MerkleRoot::from(self.merkle_root),
            block_commitments: BlockCommitments::from(self.block_commitments),
            bits: self.bits,
            nonce: self.nonce,
            solution: self.solution,
        }
    }
}

fn read_header(r: &mut Reader<'_>) -> Result<RawHeader, RawBlockError> {
    let start = r.pos();
    let version = r.u32()?;
    if version & 0x8000_0000 != 0 || version < 4 {
        return Err(RawBlockError::BlockVersion(version));
    }
    let prev_hash = r.array()?;
    let merkle_root = r.array()?;
    let block_commitments = r.array()?;
    let time = r.u32()?;
    let bits = CompactDifficulty::try_from_bits(r.u32()?)?;
    let nonce = r.array()?;
    let solution = match r.compact_size()? {
        1344 => EquihashSolution::Standard(r.array()?),
        36 => EquihashSolution::Regtest(r.array()?),
        other => return Err(RawBlockError::SolutionLength(other)),
    };
    Ok(RawHeader {
        hash: sha256d(r.since(start)?),
        version,
        prev_hash,
        merkle_root,
        block_commitments,
        time,
        bits,
        nonce,
        solution,
    })
}

fn sha256d(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(bytes)).into()
}

/// Transaction formats, keyed by version and version group.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TxVersion {
    /// Pre-Overwinter; JoinSplits from version 2.
    Sprout {
        joinsplits: bool,
    },
    V3,
    V4,
    V5,
    V6,
}

struct RawInput<'a> {
    prev_txid: [u8; 32],
    prev_index: u32,
    script: &'a [u8],
    sequence: u32,
}

impl RawInput<'_> {
    fn is_coinbase(&self) -> bool {
        self.prev_txid == [0; 32] && self.prev_index == u32::MAX
    }
}

struct RawOutput<'a> {
    /// The whole encoding: value and script.
    raw: &'a [u8],
    value: u64,
    script: &'a [u8],
}

struct RawSpend<'a> {
    cv: &'a [u8],
    /// v4's per-spend anchor; v5/v6 share one.
    anchor: Option<[u8; 32]>,
    nullifier: [u8; 32],
    rk: &'a [u8],
}

/// Note commitment, ephemeral key, and ciphertexts, as Sapling outputs and
/// Orchard-shaped actions share them.
struct RawNote<'a> {
    cm: [u8; 32],
    epk: [u8; 32],
    /// `encCiphertext[..52]`.
    enc_compact: [u8; 52],
    /// `encCiphertext[52..564]`.
    enc_memo: &'a [u8],
    /// `encCiphertext[564..]`.
    enc_tail: &'a [u8],
    out: &'a [u8],
}

struct RawSaplingOutput<'a> {
    cv: &'a [u8],
    note: RawNote<'a>,
}

#[derive(Default)]
struct RawSapling<'a> {
    spends: Vec<RawSpend<'a>>,
    outputs: Vec<RawSaplingOutput<'a>>,
    /// Zero when there are no spends or outputs.
    value_balance: i64,
    /// The v5/v6 shared anchor, present with spends.
    anchor: Option<[u8; 32]>,
}

impl RawSapling<'_> {
    fn is_empty(&self) -> bool {
        self.spends.is_empty() && self.outputs.is_empty()
    }
}

struct RawAction<'a> {
    cv: &'a [u8],
    nullifier: [u8; 32],
    rk: &'a [u8],
    note: RawNote<'a>,
}

/// A non-empty Orchard or Ironwood bundle.
struct RawOrchard<'a> {
    actions: Vec<RawAction<'a>>,
    flags: u8,
    value_balance: i64,
    anchor: [u8; 32],
}

/// The ZIP 244 header fields of a v5/v6 transaction.
struct Zip244Header<'a> {
    /// Version, version group, branch id, lock time, expiry height.
    fields: &'a [u8],
    branch_id: [u8; 4],
}

struct RawTx<'a> {
    version: TxVersion,
    zip244: Option<Zip244Header<'a>>,
    inputs: Vec<RawInput<'a>>,
    outputs: Vec<RawOutput<'a>>,
    sapling: RawSapling<'a>,
    orchard: Option<RawOrchard<'a>>,
    ironwood: Option<RawOrchard<'a>>,
}

struct ParsedTransaction {
    transaction: Transaction,
    /// The height a leading coinbase input encodes.
    coinbase_height: Option<Height>,
}

/// One transaction; `check_fields` also runs [`check_fields`] on it.
fn read_transaction(
    r: &mut Reader<'_>,
    check_fields: bool,
) -> Result<ParsedTransaction, RawBlockError> {
    let start = r.pos();
    let header = r.u32()?;
    let number = header & 0x7FFF_FFFF;
    let version = if header & 0x8000_0000 != 0 {
        let group = r.u32()?;
        match (number, group) {
            (3, V3_GROUP_ID) => TxVersion::V3,
            (4, V4_GROUP_ID) => TxVersion::V4,
            (5, V5_GROUP_ID) => TxVersion::V5,
            (6, V6_GROUP_ID) => TxVersion::V6,
            _ => {
                return Err(RawBlockError::VersionGroup {
                    version: number,
                    group,
                })
            }
        }
    } else if number >= 1 {
        TxVersion::Sprout {
            joinsplits: number >= 2,
        }
    } else {
        return Err(RawBlockError::TxVersion(number));
    };
    let raw = match version {
        TxVersion::Sprout { .. } | TxVersion::V3 | TxVersion::V4 => read_pre_v5(r, version)?,
        TxVersion::V5 | TxVersion::V6 => read_v5_family(r, start, version)?,
    };
    if check_fields {
        self::check_fields(&raw)?;
    }
    let txid = match &raw.zip244 {
        Some(header) => txid::zip244(&raw, header),
        None => sha256d(r.since(start)?),
    };
    Ok(ParsedTransaction {
        coinbase_height: check_coinbase(&raw)?,
        transaction: to_domain(&raw, txid)?,
    })
}

/// v1–v4: transparent, Sapling (v4), JoinSplits (v2–v4).
fn read_pre_v5<'a>(r: &mut Reader<'a>, version: TxVersion) -> Result<RawTx<'a>, RawBlockError> {
    let inputs = r.list(MIN_INPUT_LEN, read_input)?;
    let outputs = r.list(MIN_OUTPUT_LEN, read_output)?;
    r.skip(4)?; // lock time
    if !matches!(version, TxVersion::Sprout { .. }) {
        r.skip(4)?; // expiry height
    }
    let mut sapling = RawSapling::default();
    if version == TxVersion::V4 {
        let value_balance = r.i64()?;
        sapling.spends = r.list(SPEND_V4_LEN, read_spend_v4)?;
        sapling.outputs = r.list(OUTPUT_V4_LEN, read_output_v4)?;
        if sapling.is_empty() {
            if value_balance != 0 {
                return Err(RawBlockError::UnbalancedEmptySapling);
            }
        } else {
            sapling.value_balance = value_balance;
        }
    }
    let joinsplit_proof_len = match version {
        TxVersion::Sprout { joinsplits: true } | TxVersion::V3 => Some(BCTV14_PROOF_LEN),
        TxVersion::V4 => Some(GROTH_PROOF_LEN),
        TxVersion::Sprout { joinsplits: false } | TxVersion::V5 | TxVersion::V6 => None,
    };
    if let Some(proof_len) = joinsplit_proof_len {
        let joinsplit_len = JOINSPLIT_FIXED_LEN.saturating_add(proof_len);
        let joinsplits = r.list(joinsplit_len, |r| skip_joinsplit(r, proof_len))?;
        if !joinsplits.is_empty() {
            r.skip(JOINSPLIT_AUTH_LEN)?;
        }
    }
    if !sapling.is_empty() {
        r.skip(SIGNATURE_LEN)?; // bindingSigSapling
    }
    Ok(RawTx {
        version,
        zip244: None,
        inputs,
        outputs,
        sapling,
        orchard: None,
        ironwood: None,
    })
}

/// v5 and v6 (ZIP 225 / ZIP 229): v6 appends an Ironwood bundle.
fn read_v5_family<'a>(
    r: &mut Reader<'a>,
    start: usize,
    version: TxVersion,
) -> Result<RawTx<'a>, RawBlockError> {
    let branch_id = r.array()?;
    let branch = u32::from_le_bytes(branch_id);
    let pools = branch_pools(branch).ok_or(RawBlockError::BranchId(branch))?;
    if version == TxVersion::V6 && !pools.ironwood {
        return Err(RawBlockError::V6BeforeNu6_3(branch));
    }
    r.skip(8)?; // lock time, expiry height
    let zip244 = Zip244Header {
        fields: r.since(start)?,
        branch_id,
    };
    let inputs = r.list(MIN_INPUT_LEN, read_input)?;
    let outputs = r.list(MIN_OUTPUT_LEN, read_output)?;
    let mut sapling = RawSapling {
        spends: r.list(SPEND_V5_LEN, read_spend_v5)?,
        outputs: r.list(OUTPUT_V5_LEN, read_output_v5)?,
        ..RawSapling::default()
    };
    if !sapling.is_empty() {
        sapling.value_balance = r.i64()?;
    }
    if !sapling.spends.is_empty() {
        sapling.anchor = Some(r.array()?);
    }
    let spends = sapling.spends.len();
    r.skip_each(spends, GROTH_PROOF_LEN)?;
    r.skip_each(spends, SIGNATURE_LEN)?;
    r.skip_each(sapling.outputs.len(), GROTH_PROOF_LEN)?;
    if !sapling.is_empty() {
        r.skip(SIGNATURE_LEN)?; // bindingSigSapling
    }
    let orchard = read_orchard_bundle(r, ShieldedPool::Orchard)?;
    if orchard.is_some() && !pools.orchard {
        return Err(RawBlockError::PoolInactive {
            pool: ShieldedPool::Orchard,
            branch,
        });
    }
    // v6 implies NU6.3, so Ironwood is active.
    let ironwood = match version {
        TxVersion::V6 => read_orchard_bundle(r, ShieldedPool::Ironwood)?,
        _ => None,
    };
    Ok(RawTx {
        version,
        zip244: Some(zip244),
        inputs,
        outputs,
        sapling,
        orchard,
        ironwood,
    })
}

fn read_input<'a>(r: &mut Reader<'a>) -> Result<RawInput<'a>, RawBlockError> {
    Ok(RawInput {
        prev_txid: r.array()?,
        prev_index: r.u32()?,
        script: r.var_bytes()?,
        sequence: r.u32()?,
    })
}

fn read_output<'a>(r: &mut Reader<'a>) -> Result<RawOutput<'a>, RawBlockError> {
    let start = r.pos();
    let value = r.u64()?;
    let script = r.var_bytes()?;
    Ok(RawOutput {
        raw: r.since(start)?,
        value,
        script,
    })
}

fn skip_joinsplit(r: &mut Reader<'_>, proof_len: usize) -> Result<(), RawBlockError> {
    Zatoshis::new(r.u64()?)?; // vpub_old
    Zatoshis::new(r.u64()?)?; // vpub_new
    r.skip(JOINSPLIT_REST_LEN)?;
    r.skip(proof_len)
}

fn read_spend_v4<'a>(r: &mut Reader<'a>) -> Result<RawSpend<'a>, RawBlockError> {
    let cv = r.take(32)?;
    let anchor = Some(r.array()?);
    let nullifier = r.array()?;
    let rk = r.take(32)?;
    r.skip(SPEND_V4_AUTH_LEN)?;
    Ok(RawSpend {
        cv,
        anchor,
        nullifier,
        rk,
    })
}

fn read_spend_v5<'a>(r: &mut Reader<'a>) -> Result<RawSpend<'a>, RawBlockError> {
    Ok(RawSpend {
        cv: r.take(32)?,
        anchor: None,
        nullifier: r.array()?,
        rk: r.take(32)?,
    })
}

fn read_output_v4<'a>(r: &mut Reader<'a>) -> Result<RawSaplingOutput<'a>, RawBlockError> {
    let output = read_output_v5(r)?;
    r.skip(GROTH_PROOF_LEN)?;
    Ok(output)
}

fn read_output_v5<'a>(r: &mut Reader<'a>) -> Result<RawSaplingOutput<'a>, RawBlockError> {
    Ok(RawSaplingOutput {
        cv: r.take(32)?,
        note: read_note(r)?,
    })
}

fn read_note<'a>(r: &mut Reader<'a>) -> Result<RawNote<'a>, RawBlockError> {
    Ok(RawNote {
        cm: r.array()?,
        epk: r.array()?,
        enc_compact: r.array()?,
        enc_memo: r.take(512)?,
        enc_tail: r.take(16)?,
        out: r.take(80)?,
    })
}

fn read_action<'a>(r: &mut Reader<'a>) -> Result<RawAction<'a>, RawBlockError> {
    Ok(RawAction {
        cv: r.take(32)?,
        nullifier: r.array()?,
        rk: r.take(32)?,
        note: read_note(r)?,
    })
}

/// The Orchard-shaped pools a v5/v6 branch admits.
struct BranchPools {
    orchard: bool,
    ironwood: bool,
}

/// `None` for a branch id `zcash_protocol` does not define; pools per its
/// `orchard_protocol_revision` (Orchard from NU5, Ironwood from NU6.3).
fn branch_pools(branch: u32) -> Option<BranchPools> {
    use zaino_consensus::branch_id::{
        BLOSSOM, CANOPY, HEARTWOOD, NU5, NU6, NU6_1, NU6_2, NU6_3, NU7, OVERWINTER, SAPLING,
    };
    let (orchard, ironwood) = match branch {
        SPROUT_BRANCH_ID | OVERWINTER | SAPLING | BLOSSOM | HEARTWOOD | CANOPY => (false, false),
        NU5 | NU6 | NU6_1 | NU6_2 => (true, false),
        NU6_3 | NU7 => (true, true),
        _ => return None,
    };
    Some(BranchPools { orchard, ironwood })
}

/// An Orchard or Ironwood bundle; `None` when it has no actions.
fn read_orchard_bundle<'a>(
    r: &mut Reader<'a>,
    pool: ShieldedPool,
) -> Result<Option<RawOrchard<'a>>, RawBlockError> {
    let actions = r.list(ACTION_LEN, read_action)?;
    if actions.is_empty() {
        return Ok(None);
    }
    let flags = r.u8()?;
    let allowed = match pool {
        ShieldedPool::Ironwood => FLAGS_SPENDS_OUTPUTS | FLAG_CROSS_ADDRESS,
        ShieldedPool::Sapling | ShieldedPool::Orchard => FLAGS_SPENDS_OUTPUTS,
    };
    if flags & !allowed != 0 {
        return Err(RawBlockError::Flags { pool, flags });
    }
    let value_balance = r.i64()?;
    let anchor = r.array()?;
    r.var_bytes()?; // proofs
    r.skip_each(actions.len(), SIGNATURE_LEN)?;
    r.skip(SIGNATURE_LEN)?; // bindingSig
    Ok(Some(RawOrchard {
        actions,
        flags,
        value_balance,
        anchor,
    }))
}

/// Coinbase rules zebra applies at parse time: every coinbase input's script
/// encodes a height, and a coinbase spends no Sapling notes. Answers the
/// height a leading coinbase input encodes.
fn check_coinbase(raw: &RawTx<'_>) -> Result<Option<Height>, RawBlockError> {
    let mut leading = None;
    for (index, input) in raw.inputs.iter().enumerate() {
        if input.is_coinbase() {
            let height = coinbase_height(input.script)?;
            if index == 0 {
                leading = Some(height);
            }
        }
    }
    let is_coinbase = matches!(raw.inputs.as_slice(), [input] if input.is_coinbase());
    if is_coinbase && !raw.sapling.spends.is_empty() {
        return Err(RawBlockError::CoinbaseSaplingSpend);
    }
    Ok(leading)
}

/// The BIP 34 height a coinbase script encodes; genesis carries none and is 0.
///
/// Accepts exactly the minimal encodings: `OP_1..OP_16`, or a push of 1–5
/// bytes holding a minimal little-endian script number above 16.
fn coinbase_height(script: &[u8]) -> Result<Height, RawBlockError> {
    if script == GENESIS_COINBASE_SCRIPT_SIG {
        return Ok(Height::try_from(0)?);
    }
    if !(MIN_COINBASE_SCRIPT_LEN..=MAX_COINBASE_SCRIPT_LEN).contains(&script.len()) {
        return Err(RawBlockError::CoinbaseScript("length outside 2..=100"));
    }
    let non_canonical = RawBlockError::CoinbaseScript("non-canonical height encoding");
    let (&first, rest) = script
        .split_first()
        .ok_or(RawBlockError::CoinbaseScript("empty"))?;
    let height = match first {
        op_n @ 0x51..=0x60 => u64::from(op_n.saturating_sub(0x50)),
        len @ 1..=5 => {
            let bytes = rest
                .get(..usize::from(len))
                .ok_or(RawBlockError::CoinbaseScript("height push truncated"))?;
            let (&msb, lower) = bytes.split_last().ok_or(non_canonical.clone())?;
            let minimal =
                msb & 0x80 == 0 && (msb != 0 || lower.last().is_some_and(|byte| byte & 0x80 != 0));
            let mut le = [0u8; 8];
            for (dst, src) in le.iter_mut().zip(bytes) {
                *dst = *src;
            }
            let value = u64::from_le_bytes(le);
            // Heights 0..=16 have opcodes of their own.
            if !minimal || value <= 16 {
                return Err(non_canonical);
            }
            value
        }
        _ => return Err(RawBlockError::CoinbaseScript("invalid height prefix")),
    };
    let height =
        u32::try_from(height).map_err(|_| RawBlockError::CoinbaseScript("height overflow"))?;
    Ok(Height::try_from(height)?)
}

fn to_domain(raw: &RawTx<'_>, txid: [u8; 32]) -> Result<Transaction, RawBlockError> {
    let inputs = raw
        .inputs
        .iter()
        .filter(|input| !input.is_coinbase())
        .map(|input| TransparentInput {
            prev_txid: TransactionId::from(input.prev_txid),
            prev_index: input.prev_index,
        })
        .collect();
    let outputs = raw
        .outputs
        .iter()
        .map(|output| {
            Ok(TransparentOutput {
                value: Zatoshis::new(output.value)?,
                script: Script::new(output.script.to_vec()),
            })
        })
        .collect::<Result<_, RawBlockError>>()?;
    Ok(Transaction {
        txid: TransactionId::from(txid),
        transparent: TransparentData { inputs, outputs },
        sapling: SaplingData {
            spends: raw
                .sapling
                .spends
                .iter()
                .map(|spend| SaplingSpend {
                    nullifier: Nullifier::from(spend.nullifier),
                })
                .collect(),
            outputs: raw
                .sapling
                .outputs
                .iter()
                .map(|output| SaplingOutput {
                    cmu: NoteCommitment::from(output.note.cm),
                    ephemeral_key: EphemeralKey::from(output.note.epk),
                    enc_ciphertext: CompactCiphertext::from(output.note.enc_compact),
                })
                .collect(),
            value_balance: SignedZatoshis::try_new(raw.sapling.value_balance)?,
        },
        orchard: orchard_to_domain(raw.orchard.as_ref())?,
        ironwood: orchard_to_domain(raw.ironwood.as_ref())?,
    })
}

fn orchard_to_domain(bundle: Option<&RawOrchard<'_>>) -> Result<OrchardData, RawBlockError> {
    let Some(bundle) = bundle else {
        return Ok(OrchardData::default());
    };
    Ok(OrchardData {
        actions: bundle
            .actions
            .iter()
            .map(|action| OrchardAction {
                nullifier: Nullifier::from(action.nullifier),
                cmx: NoteCommitment::from(action.note.cm),
                ephemeral_key: EphemeralKey::from(action.note.epk),
                enc_ciphertext: CompactCiphertext::from(action.note.enc_compact),
            })
            .collect(),
        value_balance: SignedZatoshis::try_new(bundle.value_balance)?,
    })
}
