//! I-10 `address_history`: every receive and spend of each transparent
//! address, in chain order.
//!
//! One row per event, keyed `address hash (20) | script type (1) | height
//! (u32) | tx index (u16) | input or output index (u32) | kind`, all
//! big-endian, so an address's
//! history is one key range in chain order and the leading bytes are
//! uniformly distributed. The value is the amount. Spends are rows of their
//! own (the prevout arrives resolved in the `FoldBlock`); whether a receive
//! is still unspent is I-09's to say. Only P2PKH and P2SH outputs are
//! indexed: no address names any other script.
//!
//! Reads stream bounded chunks; nothing materialises an address's whole
//! history.

use core::num::NonZeroUsize;
use core::ops::{Bound, RangeInclusive};

use zaino_persistence::{BackendReader, Namespace, ScanDirection, ScanRange, WriteOp};
use zaino_primitives::types::{
    BlockRef, BlockTxPosition, Height, ScriptType, TransparentAddressKey, TransparentOutput,
    Zatoshis,
};

use crate::block::FoldBlock;
use crate::codec::{self, le_u64, POSITION_BYTES};
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout, LayoutError};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "address_history";

const EVENTS: Namespace = Namespace::new("address_history");

declared! {
    /// An event's key.
    pub const EVENT_KEY: Layout = Layout::new(
        "address_event_key",
        1,
        &[
            Field::fixed("address_hash", 20),
            Field::fixed("script_type", 1),
            Field::fixed("height", 4),
            Field::fixed("tx_index", 2),
            Field::fixed("io_index", 4),
            Field::fixed("kind", 1),
        ],
    );
}
declared! {
    /// An event's amount, `u64` little-endian.
    pub const EVENT_VALUE: Layout = Layout::new("address_event_value", 1, &[Field::fixed("value", 8)]);
}
declared! {
    const BY_ADDRESS: NamespaceLayout = NamespaceLayout::map(EVENTS, &EVENT_KEY, &EVENT_VALUE, false);
}
/// Bytes naming an address: hash then script type.
const ADDRESS_BYTES: usize = 21;
const KEY_BYTES: usize = 32;
const _: () = assert!(EVENT_KEY.fixed_len() == KEY_BYTES);

/// The schema: one map of events.
pub const SCHEMA: Schema = Schema::new(NAME, 1, &[BY_ADDRESS]);

/// Whether an event paid the address or spent from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Output `index` of the transaction paid the address.
    Received,
    /// Input `index` of the transaction spent one of its outputs.
    Spent,
}

/// One receive or spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressEvent {
    /// The transaction.
    pub position: BlockTxPosition,
    /// Its output index (received) or input index (spent).
    pub index: u32,
    /// Received or spent.
    pub kind: EventKind,
    /// The amount.
    pub value: Zatoshis,
}

/// I-10.
pub struct AddressHistory;

/// The last stored block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: BlockRef,
}

impl Index for AddressHistory {
    const SCHEMA: Schema = SCHEMA;
    const READS: &'static [&'static str] = &[crate::txids::NAME, crate::spent::NAME];
    const BUILDS_FROM: &'static [&'static str] =
        &[crate::txid_location::NAME, crate::transparent::NAME];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        Ok(backend.tip()?.map(|tip| State { tip }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let reference = block.reference();
        let mut ops = Vec::new();
        // Consensus: the genesis coinbase's outputs can never be spent, and
        // no address index holds them.
        let txs = match crate::block::is_genesis(block) {
            true => &[][..],
            false => block.transactions(),
        };
        for (tx_index, tx) in (0u32..).zip(txs) {
            let position = BlockTxPosition::new(reference.height, tx_index);
            let received = (0u32..).zip(&tx.compact().transparent_outputs);
            let spent = (0u32..).zip(tx.resolved_prevouts(NAME)?);
            let events = received
                .map(|(index, output)| (index, EventKind::Received, output))
                .chain(spent.map(|(index, prevout)| (index, EventKind::Spent, prevout)));
            for (index, kind, output) in events {
                if let Some(address) = address_of(output) {
                    ops.push(WriteOp::Put {
                        namespace: EVENTS,
                        key: event_key(address, position, index, kind)?.to_vec(),
                        value: u64::from(output.value).to_le_bytes().to_vec(),
                    });
                }
            }
        }
        Ok(Fold {
            ops,
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

    /// `address`'s events at `heights`, in chain order, as lazily read
    /// chunks of at most about `chunk_bytes` each.
    pub fn events(
        &self,
        address: TransparentAddressKey,
        heights: RangeInclusive<Height>,
        chunk_bytes: NonZeroUsize,
    ) -> impl Iterator<Item = Result<Vec<AddressEvent>, FoldError>> + Send + 'static + use<'a, R>
    {
        let bound = |height: Height| {
            let mut key = Vec::with_capacity(ADDRESS_BYTES.saturating_add(4));
            key.extend_from_slice(&address_bytes(address));
            key.extend_from_slice(&u32::from(height).to_be_bytes());
            key
        };
        let upper = match u32::from(*heights.end()).checked_add(1) {
            Some(next) => Bound::Excluded({
                let mut key = Vec::with_capacity(ADDRESS_BYTES.saturating_add(4));
                key.extend_from_slice(&address_bytes(address));
                key.extend_from_slice(&next.to_be_bytes());
                key
            }),
            None => Bound::Included(bound(*heights.end())),
        };
        let range = ScanRange {
            lower: Bound::Included(bound(*heights.start())),
            upper,
            direction: ScanDirection::Forward,
            chunk_bytes,
        };
        self.backend
            .scan_range(EVENTS, range, |key, value| Some(event(key, value)))
            .map(|chunk| chunk?.into_iter().collect::<Result<Vec<_>, _>>())
    }
}

/// An output's address, for the two scripts an address can name.
fn address_of(output: &TransparentOutput) -> Option<TransparentAddressKey> {
    TransparentAddressKey::from_standard_script(output.script.as_bytes())
}

fn address_bytes(address: TransparentAddressKey) -> [u8; ADDRESS_BYTES] {
    let mut out = [0; ADDRESS_BYTES];
    let (hash, kind) = out.split_at_mut(20);
    hash.copy_from_slice(&address.hash);
    kind.copy_from_slice(&[script_type_byte(address.script_type)]);
    out
}

fn script_type_byte(script_type: ScriptType) -> u8 {
    match script_type {
        ScriptType::P2PKH => 0,
        ScriptType::P2SH => 1,
        ScriptType::NonStandard => 2,
    }
}

fn event_key(
    address: TransparentAddressKey,
    position: BlockTxPosition,
    index: u32,
    kind: EventKind,
) -> Result<[u8; KEY_BYTES], LayoutError> {
    let mut out = [0; KEY_BYTES];
    let (head, rest) = out.split_at_mut(ADDRESS_BYTES);
    head.copy_from_slice(&address_bytes(address));
    let (at, rest) = rest.split_at_mut(POSITION_BYTES);
    at.copy_from_slice(&codec::position_bytes(position)?);
    let (io, tail) = rest.split_at_mut(4);
    io.copy_from_slice(&index.to_be_bytes());
    tail.copy_from_slice(&[match kind {
        EventKind::Received => 0,
        EventKind::Spent => 1,
    }]);
    Ok(out)
}

fn event(key: &[u8], value: &[u8]) -> Result<AddressEvent, FoldError> {
    let key = codec::array::<KEY_BYTES>(key, "address_event_key")?;
    let (_, rest) = key.split_at(ADDRESS_BYTES);
    let (at, rest) = rest.split_at(POSITION_BYTES);
    let (io, kind) = rest.split_at(4);
    let kind = match kind {
        [0] => EventKind::Received,
        [1] => EventKind::Spent,
        _ => return Err(FoldError::continuity(NAME, "unknown event kind")),
    };
    let amount = le_u64(value, "value")?;
    Ok(AddressEvent {
        position: codec::position(NAME, at)?,
        index: codec::array::<4>(io, "io_index").map(u32::from_be_bytes)?,
        kind,
        value: Zatoshis::new(amount)
            .map_err(|_| FoldError::continuity(NAME, format!("stored value {amount}")))?,
    })
}

#[cfg(test)]
mod tests {
    use zaino_persistence::{Backend as _, SchemaDigest};
    use zaino_primitives::types::{PoolBalances, Script, TransactionId, TransparentInput};

    use super::*;
    use crate::block::FoldTx;
    use crate::testing::{backend, block_with, fold_all, tx};

    fn p2pkh(hash: u8) -> Script {
        let mut script = vec![0x76, 0xa9, 0x14];
        script.extend_from_slice(&[hash; 20]);
        script.extend_from_slice(&[0x88, 0xac]);
        Script::from(script)
    }

    fn pay(hash: u8, value: u64) -> TransparentOutput {
        TransparentOutput {
            value: Zatoshis::new(value).expect("in range"),
            script: p2pkh(hash),
        }
    }

    fn with(n: u32, spends: Vec<TransparentOutput>, outputs: Vec<TransparentOutput>) -> FoldTx {
        let mut compact = tx(n).compact().clone();
        compact.transparent_inputs = spends
            .iter()
            .map(|_| TransparentInput {
                prev_txid: TransactionId::from([9; 32]),
                prev_index: 0,
            })
            .collect();
        compact.transparent_outputs = outputs;
        FoldTx::try_new(compact, PoolBalances::default(), Some(spends)).expect("paired")
    }

    fn key_of(hash: u8) -> TransparentAddressKey {
        TransparentAddressKey::from_script(p2pkh(hash).as_bytes())
    }

    fn all(
        reader: &Reader<'_, impl BackendReader>,
        hash: u8,
        heights: RangeInclusive<u32>,
    ) -> Vec<AddressEvent> {
        let heights = Height::try_from(*heights.start()).expect("h")
            ..=Height::try_from(*heights.end()).expect("h");
        reader
            .events(key_of(hash), heights, NonZeroUsize::MIN)
            .collect::<Result<Vec<_>, _>>()
            .expect("scan")
            .concat()
    }

    /// Receives and spends read back per address in chain order, bounded
    /// by height; other addresses, non-standard scripts and the genesis
    /// coinbase's outputs do not appear.
    #[test]
    fn events_stream_per_address_in_chain_order() {
        let odd = TransparentOutput {
            value: Zatoshis::new(3).expect("3"),
            script: Script::from(vec![0x6a, 1, 2]),
        };
        // A raw 21-byte script tagged like zainodb's legacy rows names no
        // address, so it must not land under address 1.
        let mut tagged = vec![0x00];
        tagged.extend_from_slice(&[1; 20]);
        let legacy_form = TransparentOutput {
            value: Zatoshis::new(4).expect("4"),
            script: Script::from(tagged),
        };
        let chain = [
            block_with(0, vec![with(0, vec![], vec![pay(1, 99)])]),
            block_with(1, vec![with(1, vec![], vec![pay(1, 50), pay(2, 7)])]),
            block_with(
                2,
                vec![
                    tx(2),
                    with(
                        3,
                        vec![pay(1, 50)],
                        vec![pay(1, 20), odd, pay(2, 30), legacy_form],
                    ),
                ],
            ),
        ];
        let backend = backend(&SCHEMA);
        fold_all::<AddressHistory>(&backend, &chain).expect("folded");
        let reader = backend.reader().expect("reader");
        let reader = Reader::new(&reader);
        let at = |height: u32, tx_index| {
            BlockTxPosition::new(Height::try_from(height).expect("h"), tx_index)
        };
        let ev = |position, index, kind, value| AddressEvent {
            position,
            index,
            kind,
            value: Zatoshis::new(value).expect("v"),
        };
        assert_eq!(
            all(&reader, 1, 0..=2),
            vec![
                ev(at(1, 0), 0, EventKind::Received, 50),
                ev(at(2, 1), 0, EventKind::Received, 20),
                ev(at(2, 1), 0, EventKind::Spent, 50),
            ]
        );
        assert!(all(&reader, 1, 0..=0).is_empty(), "genesis outputs");
        assert_eq!(all(&reader, 1, 2..=2).len(), 2, "height-bounded");
        assert_eq!(
            all(&reader, 2, 0..=2),
            vec![
                ev(at(1, 0), 1, EventKind::Received, 7),
                ev(at(2, 1), 2, EventKind::Received, 30),
            ]
        );
        assert!(all(&reader, 3, 0..=2).is_empty());
    }

    /// The schema digest every `address_history_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0xf0, 0xfe, 0x67, 0xa0, 0x42, 0x16, 0x84, 0x1f, 0x52, 0x54, 0xcd, 0xd8, 0xd9, 0x49,
            0x91, 0x0e, 0xca, 0xbf, 0x75, 0x09, 0x2c, 0x51, 0x20, 0x12, 0x8f, 0x36, 0x05, 0x28,
            0x30, 0x78, 0xc1, 0x32,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "address_history_1");
    }
}
