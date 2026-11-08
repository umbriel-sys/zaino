//! Differential and malformed-input tests for the raw block parser.
//!
//! The oracle is zebra's deserializer (`zcash_primitives` underneath) plus
//! `block_from_zebra`: every block it accepts must parse to the same domain
//! block here.

use proptest::prelude::*;
use zaino_primitives::types::{Block, ChainMetadata, ShieldedPool};
use zaino_primitives::zebra::block_from_zebra;
use zebra_chain::block::{arbitrary::LedgerState, Block as ZebraBlock};
use zebra_chain::parameters::NetworkUpgrade;
use zebra_chain::serialization::{ZcashDeserializeInto, ZcashSerialize};
use zebra_test::vectors::{MAINNET_BLOCKS, TESTNET_BLOCKS};

use super::*;

const MAX_HEIGHT: u32 = (1 << 31) - 1;
/// Version, three hashes, time, nBits, nonce, CompactSize(1344), solution.
const HEADER_LEN: usize = 4 + 32 * 3 + 4 + 4 + 32 + 3 + 1344;

/// The oracle's block, or `None` where it refuses the bytes.
fn oracle(bytes: &[u8]) -> Option<Block> {
    let zebra: ZebraBlock = bytes.zcash_deserialize_into().ok()?;
    block_from_zebra(&zebra, ChainMetadata::ZERO).ok()
}

fn parse(bytes: &[u8]) -> Result<Block, RawBlockError> {
    parse_block(bytes, ChainMetadata::ZERO)
}

/// What a block compares by: every field, as `Block` and `Transaction` have
/// no `PartialEq`.
fn parts(block: Block) -> String {
    format!("{block:?}")
}

fn vectors() -> impl Iterator<Item = (&'static str, u32, &'static [u8])> {
    let main = MAINNET_BLOCKS.iter().map(|(h, b)| ("mainnet", *h, *b));
    let test = TESTNET_BLOCKS.iter().map(|(h, b)| ("testnet", *h, *b));
    main.chain(test)
}

#[test]
fn every_vector_matches_the_oracle() {
    let mut count = 0;
    for (network, height, bytes) in vectors() {
        let expected = oracle(bytes).expect("zebra accepts its own vectors");
        let parsed = parse(bytes).unwrap_or_else(|e| panic!("{network} {height}: {e}"));
        assert_eq!(
            u32::from(parsed.header.height),
            height,
            "{network} {height}"
        );
        assert_eq!(parts(parsed), parts(expected), "{network} {height}");
        count += 1;
    }
    assert!(count >= 87, "only {count} vectors");
}

#[test]
fn the_vectors_cover_every_transaction_format() {
    let mut seen = std::collections::BTreeSet::new();
    for (_, _, bytes) in vectors() {
        let zebra: ZebraBlock = bytes.zcash_deserialize_into().expect("vector");
        seen.extend(zebra.transactions.iter().map(|tx| tx.version()));
    }
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        [1, 2, 3, 4, 5],
        "v6 is covered by the arbitrary blocks"
    );
}

/// Positions to cut a block at: all of a small one, else a spread plus the
/// first and last kilobyte.
fn cuts(len: usize) -> Vec<usize> {
    if len <= 8192 {
        return (0..len).collect();
    }
    let mut cuts: Vec<usize> = (0..1024).chain(len - 1024..len).collect();
    cuts.extend((0..len).step_by(len / 2048));
    cuts
}

#[test]
fn every_truncation_is_refused() {
    for (network, height, bytes) in vectors() {
        for cut in cuts(bytes.len()) {
            assert!(
                parse(&bytes[..cut]).is_err(),
                "{network} {height}: a {cut}-byte prefix parsed"
            );
        }
    }
}

#[test]
fn trailing_bytes_are_refused() {
    for (network, height, bytes) in vectors() {
        let mut long = bytes.to_vec();
        long.push(0);
        assert!(
            matches!(parse(&long), Err(RawBlockError::TrailingBytes(1))),
            "{network} {height}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// A flipped byte never panics; whatever the oracle still accepts parses
    /// the same. (This parser skips curve checks, so it may accept more.)
    #[test]
    fn a_mutated_vector_never_disagrees(pick in any::<prop::sample::Index>(), at in any::<prop::sample::Index>(), mask in 1u8..) {
        let all: Vec<_> = vectors().collect();
        let (_, _, bytes) = all[pick.index(all.len())];
        let mut bytes = bytes.to_vec();
        let at = at.index(bytes.len());
        bytes[at] ^= mask;
        let parsed = parse(&bytes);
        if let Some(expected) = oracle(&bytes) {
            prop_assert_eq!(parsed.ok().map(parts), Some(parts(expected)), "byte {}", at);
        }
    }
}

/// Arbitrary blocks under `upgrade`: every format it admits, v6 with fake
/// Orchard and Ironwood bundles from NU6.3.
fn arbitrary_blocks(upgrade: NetworkUpgrade) -> impl Strategy<Value = Vec<u8>> {
    LedgerState::coinbase_strategy(upgrade, None, true)
        .prop_flat_map(ZebraBlock::arbitrary_with)
        .prop_map(|block| block.zcash_serialize_to_vec().expect("serializes"))
}

/// Every arbitrary block parses as the oracle does, and enough of them are
/// accepted (with v6 among them) that the comparison is not vacuous.
#[test]
fn arbitrary_blocks_match_the_oracle() {
    use proptest::strategy::ValueTree as _;
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let mut v6 = 0;
    for upgrade in [
        NetworkUpgrade::Overwinter,
        NetworkUpgrade::Sapling,
        NetworkUpgrade::Canopy,
        NetworkUpgrade::Nu5,
        NetworkUpgrade::Nu6_2,
        NetworkUpgrade::Nu6_3,
        NetworkUpgrade::Nu7,
    ] {
        let mut accepted = 0;
        for case in 0..64 {
            let bytes = arbitrary_blocks(upgrade)
                .new_tree(&mut runner)
                .expect("strategy")
                .current();
            let parsed = parse(&bytes);
            let Some(expected) = oracle(&bytes) else {
                assert!(
                    parsed.is_err(),
                    "{upgrade:?} case {case}: parsed what the oracle refused"
                );
                continue;
            };
            let zebra: ZebraBlock = bytes.zcash_deserialize_into().expect("oracle accepted");
            v6 += zebra
                .transactions
                .iter()
                .filter(|tx| tx.version() == 6 && tx.has_shielded_data())
                .count();
            assert_eq!(
                parsed.ok().map(parts),
                Some(parts(expected)),
                "{upgrade:?} case {case}"
            );
            accepted += 1;
        }
        assert!(
            accepted >= 32,
            "{upgrade:?}: the oracle accepted {accepted}/64"
        );
    }
    assert!(v6 > 0, "no v6 transaction was compared");
}

// Hand-built inputs, one per rule.

fn compact(out: &mut Vec<u8>, n: usize) {
    assert!(n < 0xfd, "builders write small counts");
    out.push(n as u8);
}

/// A v5 or v6 transaction with no transparent or Sapling parts.
fn shielded_tx(version: u32, branch: u32, orchard: Option<u8>, ironwood: Option<u8>) -> Vec<u8> {
    let group = if version == 6 {
        V6_GROUP_ID
    } else {
        V5_GROUP_ID
    };
    let mut tx = Vec::new();
    tx.extend((0x8000_0000 | version).to_le_bytes());
    tx.extend(group.to_le_bytes());
    tx.extend(branch.to_le_bytes());
    tx.extend([0; 8]); // lock time, expiry
    tx.extend([0; 4]); // no inputs, outputs, spends, Sapling outputs
    bundle(&mut tx, orchard);
    if version == 6 {
        bundle(&mut tx, ironwood);
    }
    tx
}

/// One action with `flags`, or an empty bundle.
fn bundle(tx: &mut Vec<u8>, flags: Option<u8>) {
    let Some(flags) = flags else {
        compact(tx, 0);
        return;
    };
    compact(tx, 1);
    tx.extend([0; ACTION_LEN]);
    tx.push(flags);
    tx.extend([0; 8 + 32]); // value balance, anchor
    compact(tx, 0); // proofs
    tx.extend([0; 2 * SIGNATURE_LEN]);
}

/// A v4 transaction: one input (coinbase with `script`, or spending `prev`),
/// `spends` zeroed Sapling spends and `value_balance`.
fn v4_tx(coinbase: Option<&[u8]>, spends: usize, value_balance: i64) -> Vec<u8> {
    let mut tx = Vec::new();
    tx.extend((0x8000_0004u32).to_le_bytes());
    tx.extend(V4_GROUP_ID.to_le_bytes());
    compact(&mut tx, 1);
    match coinbase {
        Some(script) => {
            tx.extend([0; 32]);
            tx.extend(u32::MAX.to_le_bytes());
            compact(&mut tx, script.len());
            tx.extend(script);
        }
        None => {
            tx.extend([1; 32]);
            tx.extend(0u32.to_le_bytes());
            compact(&mut tx, 0);
        }
    }
    tx.extend(u32::MAX.to_le_bytes()); // sequence
    compact(&mut tx, 0); // outputs
    tx.extend([0; 8]); // lock time, expiry
    tx.extend(value_balance.to_le_bytes());
    compact(&mut tx, spends);
    tx.extend(vec![0; spends * SPEND_V4_LEN]);
    compact(&mut tx, 0); // Sapling outputs
    compact(&mut tx, 0); // JoinSplits
    if spends > 0 {
        tx.extend([0; SIGNATURE_LEN]);
    }
    tx
}

fn read_tx(bytes: &[u8]) -> Result<ParsedTransaction, RawBlockError> {
    let mut r = Reader::new(bytes);
    let parsed = read_transaction(&mut r)?;
    r.finish()?;
    Ok(parsed)
}

/// The genesis header followed by `txs`.
fn block_of(txs: &[Vec<u8>]) -> Vec<u8> {
    let mut block = MAINNET_BLOCKS[&0][..HEADER_LEN].to_vec();
    compact(&mut block, txs.len());
    for tx in txs {
        block.extend(tx);
    }
    block
}

/// The canonical BIP 34 push of `height` (1 and up).
fn push_height(height: u32) -> Vec<u8> {
    if (1..=16).contains(&height) {
        return vec![0x50 + height as u8];
    }
    let mut bytes = height.to_le_bytes().to_vec();
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    if bytes.last().is_some_and(|byte| byte & 0x80 != 0) {
        bytes.push(0);
    }
    let mut script = vec![bytes.len() as u8];
    script.extend(bytes);
    script
}

/// `push` padded to the two-byte minimum script.
fn coinbase_script(push: Vec<u8>) -> Vec<u8> {
    let mut script = push;
    script.push(0);
    script
}

proptest! {
    #[test]
    fn every_height_round_trips(height in 1..=MAX_HEIGHT) {
        let parsed = coinbase_height(&coinbase_script(push_height(height)))
            .expect("canonical");
        prop_assert_eq!(u32::from(parsed), height);
    }

    #[test]
    fn a_padded_height_is_refused(height in 17..=(1u32 << 24)) {
        let mut push = push_height(height);
        push[0] += 1;
        push.push(0);
        prop_assert!(coinbase_height(&coinbase_script(push)).is_err());
    }
}

#[test]
fn coinbase_height_edges() {
    let err = |script: &[u8]| coinbase_height(script).is_err();
    assert_eq!(
        coinbase_height(&GENESIS_COINBASE_SCRIPT_SIG)
            .map(u32::from)
            .ok(),
        Some(0)
    );
    assert!(err(&[0x51]), "one byte is below the minimum");
    assert!(err(&[0x51; 101]), "101 bytes is above the maximum");
    assert!(!err(&[0x51; 100]));
    assert!(err(&[0x00, 0x00]), "OP_0 is not a height");
    assert!(err(&[0x06, 1, 1, 1, 1, 1, 1]), "a six-byte push");
    assert!(err(&[0x01, 0x10]), "16 has its own opcode");
    assert!(!err(&[0x01, 0x11]));
    assert!(err(&[0x01, 0x80]), "negative");
    assert!(err(&[0x02, 0x7f, 0x00]), "padded");
    assert!(!err(&[0x02, 0x80, 0x00]));
    assert!(err(&[0x03, 0x01, 0x00]), "truncated push");
    let past_max = coinbase_script(push_height(MAX_HEIGHT).into_iter().collect());
    assert!(!err(&past_max));
    assert!(err(&[0x05, 0x00, 0x00, 0x00, 0x80, 0x00]), "2^31");
}

#[test]
fn branch_ids() {
    use zaino_consensus::branch_id::{CANOPY, NU5, NU6_2, NU6_3, NU7};
    assert!(read_tx(&shielded_tx(5, SPROUT_BRANCH_ID, None, None)).is_ok());
    assert!(matches!(
        read_tx(&shielded_tx(5, 0x1234_5678, None, None)),
        Err(RawBlockError::BranchId(0x1234_5678))
    ));
    assert!(matches!(
        read_tx(&shielded_tx(6, NU6_2, None, None)),
        Err(RawBlockError::V6BeforeNu6_3(NU6_2))
    ));
    assert!(read_tx(&shielded_tx(6, NU6_3, Some(3), Some(7))).is_ok());
    assert!(read_tx(&shielded_tx(6, NU7, Some(3), Some(7))).is_ok());
    assert!(read_tx(&shielded_tx(5, NU5, Some(3), None)).is_ok());
    assert!(matches!(
        read_tx(&shielded_tx(5, CANOPY, Some(3), None)),
        Err(RawBlockError::PoolInactive {
            pool: ShieldedPool::Orchard,
            branch: CANOPY
        })
    ));
}

#[test]
fn reserved_flags() {
    use zaino_consensus::branch_id::{NU5, NU6_3};
    let flags = |version, orchard, ironwood| {
        read_tx(&shielded_tx(
            version,
            if version == 6 { NU6_3 } else { NU5 },
            orchard,
            ironwood,
        ))
    };
    for bits in [0, 1, 2, 3] {
        assert!(flags(5, Some(bits), None).is_ok(), "{bits}");
    }
    for bits in [4, 8, 0x80] {
        assert!(
            matches!(
                flags(5, Some(bits), None),
                Err(RawBlockError::Flags {
                    pool: ShieldedPool::Orchard,
                    ..
                })
            ),
            "{bits}"
        );
    }
    assert!(matches!(
        flags(6, Some(4), None),
        Err(RawBlockError::Flags {
            pool: ShieldedPool::Orchard,
            flags: 4
        })
    ));
    assert!(
        flags(6, None, Some(7)).is_ok(),
        "cross-address is Ironwood's"
    );
    assert!(matches!(
        flags(6, None, Some(8)),
        Err(RawBlockError::Flags {
            pool: ShieldedPool::Ironwood,
            flags: 8
        })
    ));
}

#[test]
fn transaction_versions() {
    let header = |word: u32, group: u32| {
        let mut tx = word.to_le_bytes().to_vec();
        tx.extend(group.to_le_bytes());
        tx
    };
    assert!(matches!(
        read_tx(&header(0, 0)),
        Err(RawBlockError::TxVersion(0))
    ));
    assert!(matches!(
        read_tx(&header(0x8000_0005, V4_GROUP_ID)),
        Err(RawBlockError::VersionGroup { version: 5, .. })
    ));
    assert!(matches!(
        read_tx(&header(0x8000_0007, V6_GROUP_ID)),
        Err(RawBlockError::VersionGroup { version: 7, .. })
    ));
    assert!(matches!(
        read_tx(&header(0x8000_0002, V3_GROUP_ID)),
        Err(RawBlockError::VersionGroup { version: 2, .. })
    ));
    // Pre-Overwinter, `zcash_primitives` takes any version from 1 (JoinSplits from 2).
    let mut v3 = 3u32.to_le_bytes().to_vec();
    v3.extend([0, 0, 0, 0, 0, 0, 0]); // no inputs or outputs, lock time, no JoinSplits
    assert!(read_tx(&v3).is_ok());
}

#[test]
fn sapling_rules() {
    let coinbase = coinbase_script(push_height(500_000));
    assert!(read_tx(&v4_tx(Some(&coinbase), 0, 0)).is_ok());
    assert!(matches!(
        read_tx(&v4_tx(Some(&coinbase), 1, 0)),
        Err(RawBlockError::CoinbaseSaplingSpend)
    ));
    assert!(read_tx(&v4_tx(None, 1, 5)).is_ok());
    assert!(matches!(
        read_tx(&v4_tx(None, 0, 1)),
        Err(RawBlockError::UnbalancedEmptySapling)
    ));
}

#[test]
fn compact_sizes() {
    let count = |bytes: &[u8]| Reader::new(bytes).count(1);
    assert!(matches!(
        count(&[0xfd, 0xfc, 0x00]),
        Err(RawBlockError::NonCanonicalCompactSize(0))
    ));
    assert!(matches!(
        count(&[0xfe, 0xff, 0xff, 0x00, 0x00]),
        Err(RawBlockError::NonCanonicalCompactSize(0))
    ));
    assert!(matches!(
        count(&[0xff, 0, 0, 0, 0, 1, 0, 0, 0]),
        Err(RawBlockError::CompactSizeTooLarge(_))
    ));
    assert!(matches!(
        count(&[0xfe, 0x01, 0x00, 0x00, 0x02]),
        Err(RawBlockError::CompactSizeTooLarge(0x0200_0001))
    ));
    assert!(matches!(
        count(&[0x02, 0x00]),
        Err(RawBlockError::CountExceedsData {
            count: 2,
            remaining: 1,
            ..
        })
    ));
    assert_eq!(count(&[0x01, 0x00]).ok(), Some(1));
}

#[test]
fn block_rules() {
    let coinbase = v4_tx(Some(&coinbase_script(push_height(500_000))), 0, 0);
    let block = block_of(std::slice::from_ref(&coinbase));
    assert_eq!(
        parse(&block).map(|b| u32::from(b.header.height)).ok(),
        Some(500_000)
    );

    let mut version = block.clone();
    version[..4].copy_from_slice(&3u32.to_le_bytes());
    assert!(matches!(
        parse(&version),
        Err(RawBlockError::BlockVersion(3))
    ));
    version[..4].copy_from_slice(&0x8000_0004u32.to_le_bytes());
    assert!(matches!(
        parse(&version),
        Err(RawBlockError::BlockVersion(_))
    ));

    let mut solution = block[..HEADER_LEN - 1347].to_vec();
    solution.extend([100]);
    solution.extend([0; 100]);
    assert!(matches!(
        parse(&solution),
        Err(RawBlockError::SolutionLength(100))
    ));

    assert!(matches!(
        parse(&block_of(&[v4_tx(None, 0, 0)])),
        Err(RawBlockError::NoCoinbaseHeight)
    ));
    assert!(matches!(
        parse(&block_of(&[])),
        Err(RawBlockError::NoCoinbaseHeight)
    ));

    let too_large = vec![0; zaino_consensus::MAX_BLOCK_BYTES as usize + 1];
    assert!(matches!(
        parse(&too_large),
        Err(RawBlockError::BlockTooLarge(_))
    ));
}
