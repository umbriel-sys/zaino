//! The primary's record (D-95): what it is doing and how far each index is
//! durable, for linked readers. The storage keeps it whole (two slots, CRC).
//!
//! `magic (8) | version u8 | state | target | count u8 | (name, tip)*`;
//! a state is a tag byte, then an index name or a message where it carries
//! one; a target or tip is a flag byte, then `height u32 | hash (32)`;
//! strings are a length (`u8` for a name, `u16` for a message) and bytes,
//! integers little-endian.

use std::collections::BTreeMap;

use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle, Progress};
use zaino_indexes::IndexId;
use zaino_primitives::types::{BlockHash, BlockRef, Height};

const MAGIC: &[u8; 8] = b"ZAINOPS\0";
const VERSION: u8 = 1;

/// The longest message recorded; longer ones are cut at a character boundary.
const MAX_MESSAGE: usize = 4096;

/// What the primary is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimaryState {
    /// Opening the store.
    Booting,
    /// Deleting and recreating an index (another layout, corruption, another
    /// chain).
    Rebuilding(IndexId),
    /// Building toward its target.
    Building,
    /// Built to its target, or none set yet.
    Ready,
    /// The validator did not answer; building stopped until it does.
    Stalled(String),
    /// A fold or write failed; building stopped.
    Failed(String),
    /// Boot refused; the primary did not start.
    Halted(String),
    /// Shut down.
    Stopped,
}

/// The primary's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryRecord {
    /// What it is doing.
    pub state: PrimaryState,
    /// The height it last built toward.
    pub target: Option<Height>,
    /// Each enabled index's durable tip.
    pub durable: BTreeMap<IndexId, Option<BlockRef>>,
}

/// Why recorded bytes were not read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// Not a record.
    #[error("not a primary record")]
    Magic,
    /// A record version this build does not read.
    #[error("record version {0}")]
    Version(u8),
    /// Cut short, or bytes past its end.
    #[error("truncated")]
    Truncated,
    /// A field out of range.
    #[error("{0}")]
    Malformed(&'static str),
}

impl PrimaryRecord {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.push(VERSION);
        match &self.state {
            PrimaryState::Booting => bytes.push(0),
            PrimaryState::Rebuilding(id) => {
                bytes.push(1);
                put_name(&mut bytes, *id);
            }
            PrimaryState::Building => bytes.push(2),
            PrimaryState::Ready => bytes.push(3),
            PrimaryState::Stalled(message) => {
                bytes.push(4);
                put_message(&mut bytes, message);
            }
            PrimaryState::Failed(message) => {
                bytes.push(5);
                put_message(&mut bytes, message);
            }
            PrimaryState::Halted(message) => {
                bytes.push(6);
                put_message(&mut bytes, message);
            }
            PrimaryState::Stopped => bytes.push(7),
        }
        put_height(&mut bytes, self.target);
        // fewer indexes than a byte counts
        let count = u8::try_from(self.durable.len()).unwrap_or(u8::MAX);
        bytes.push(count);
        for (id, tip) in self.durable.iter().take(usize::from(count)) {
            put_name(&mut bytes, *id);
            put_height(&mut bytes, tip.map(|tip| tip.height));
            if let Some(tip) = tip {
                bytes.extend(<[u8; 32]>::from(tip.hash));
            }
        }
        bytes
    }

    /// A record of an index this build does not know drops that index.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        let mut r = bytes;
        if take(&mut r, MAGIC.len())? != MAGIC {
            return Err(RecordError::Magic);
        }
        let version = byte(&mut r)?;
        if version != VERSION {
            return Err(RecordError::Version(version));
        }
        let state = match byte(&mut r)? {
            0 => PrimaryState::Booting,
            1 => match IndexId::from_name(&get_str(&mut r, 1)?) {
                Some(id) => PrimaryState::Rebuilding(id),
                None => return Err(RecordError::Malformed("an unknown index rebuilding")),
            },
            2 => PrimaryState::Building,
            3 => PrimaryState::Ready,
            4 => PrimaryState::Stalled(get_str(&mut r, 2)?),
            5 => PrimaryState::Failed(get_str(&mut r, 2)?),
            6 => PrimaryState::Halted(get_str(&mut r, 2)?),
            7 => PrimaryState::Stopped,
            _ => return Err(RecordError::Malformed("an unknown state")),
        };
        let target = get_height(&mut r)?;
        let mut durable = BTreeMap::new();
        for _ in 0..byte(&mut r)? {
            let name = get_str(&mut r, 1)?;
            let tip = match get_height(&mut r)? {
                Some(height) => {
                    let hash = <[u8; 32]>::try_from(take(&mut r, 32)?)
                        .map_err(|_| RecordError::Truncated)?;
                    Some(BlockRef {
                        hash: BlockHash::from(hash),
                        height,
                    })
                }
                None => None,
            };
            if let Some(id) = IndexId::from_name(&name) {
                durable.insert(id, tip);
            }
        }
        if !r.is_empty() {
            return Err(RecordError::Truncated);
        }
        Ok(Self {
            state,
            target,
            durable,
        })
    }
}

/// The store's status for `state`, built to `tip` of `target`.
pub(crate) fn status_of(
    state: &PrimaryState,
    tip: Option<Height>,
    target: Option<Height>,
) -> ComponentStatus {
    let (lifecycle, health, reason) = match state {
        PrimaryState::Booting => (Lifecycle::Spawning, Health::Healthy, None),
        PrimaryState::Rebuilding(id) => (
            Lifecycle::Syncing,
            Health::Healthy,
            Some(format!("rebuilding {}", id.name())),
        ),
        PrimaryState::Building => (Lifecycle::Syncing, Health::Healthy, None),
        PrimaryState::Ready => (Lifecycle::Ready, Health::Healthy, None),
        PrimaryState::Stalled(message) => (
            Lifecycle::Syncing,
            Health::Recoverable,
            Some(message.clone()),
        ),
        PrimaryState::Failed(message) => {
            (Lifecycle::Syncing, Health::Critical, Some(message.clone()))
        }
        PrimaryState::Halted(message) => {
            (Lifecycle::Offline, Health::Offline, Some(message.clone()))
        }
        PrimaryState::Stopped => (Lifecycle::Closing, Health::Offline, None),
    };
    let mut status = ComponentStatus::new(ComponentName("chain store"), lifecycle, health);
    status.reason = reason;
    status.progress = tip.map(|tip| Progress {
        current: u64::from(u32::from(tip)),
        target: target.map(|target| u64::from(u32::from(target))),
    });
    status
}

fn put_name(bytes: &mut Vec<u8>, id: IndexId) {
    let name = id.name();
    // index names are short constants
    let len = u8::try_from(name.len()).unwrap_or(u8::MAX);
    bytes.push(len);
    bytes.extend(name.as_bytes().iter().take(usize::from(len)));
}

fn put_message(bytes: &mut Vec<u8>, message: &str) {
    let mut end = message.len().min(MAX_MESSAGE);
    while !message.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let kept = message.get(..end).unwrap_or_default();
    let len = u16::try_from(kept.len()).unwrap_or(u16::MAX);
    bytes.extend(len.to_le_bytes());
    bytes.extend(kept.as_bytes().iter().take(usize::from(len)));
}

fn put_height(bytes: &mut Vec<u8>, height: Option<Height>) {
    match height {
        Some(height) => {
            bytes.push(1);
            bytes.extend(u32::from(height).to_le_bytes());
        }
        None => bytes.push(0),
    }
}

fn take<'a>(r: &mut &'a [u8], len: usize) -> Result<&'a [u8], RecordError> {
    let (head, rest) = r.split_at_checked(len).ok_or(RecordError::Truncated)?;
    *r = rest;
    Ok(head)
}

fn byte(r: &mut &[u8]) -> Result<u8, RecordError> {
    take(r, 1)?.first().copied().ok_or(RecordError::Truncated)
}

/// A string after its length of `width` bytes (1 or 2).
fn get_str(r: &mut &[u8], width: usize) -> Result<String, RecordError> {
    let len = match width {
        1 => usize::from(byte(r)?),
        _ => {
            let bytes = <[u8; 2]>::try_from(take(r, 2)?).map_err(|_| RecordError::Truncated)?;
            usize::from(u16::from_le_bytes(bytes))
        }
    };
    String::from_utf8(take(r, len)?.to_vec()).map_err(|_| RecordError::Malformed("not UTF-8"))
}

fn get_height(r: &mut &[u8]) -> Result<Option<Height>, RecordError> {
    match byte(r)? {
        0 => Ok(None),
        1 => {
            let bytes = <[u8; 4]>::try_from(take(r, 4)?).map_err(|_| RecordError::Truncated)?;
            Height::try_from(u32::from_le_bytes(bytes))
                .map(Some)
                .map_err(|_| RecordError::Malformed("a height past the limit"))
        }
        _ => Err(RecordError::Malformed("a height flag")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use zaino_indexes::IndexId;
    use zaino_primitives::types::{BlockHash, BlockRef, Height};

    use super::{PrimaryRecord, PrimaryState, RecordError, MAX_MESSAGE};

    fn height(n: u32) -> Height {
        Height::try_from(n).expect("in range")
    }

    #[test]
    fn every_state_round_trips_and_a_cut_record_is_refused() {
        let durable = BTreeMap::from([
            (IndexId::Headers, None),
            (
                IndexId::Txids,
                Some(BlockRef {
                    hash: BlockHash::from([7; 32]),
                    height: height(9),
                }),
            ),
        ]);
        for state in [
            PrimaryState::Booting,
            PrimaryState::Rebuilding(IndexId::Txids),
            PrimaryState::Building,
            PrimaryState::Ready,
            PrimaryState::Stalled("validator down".to_owned()),
            PrimaryState::Failed("disk full".to_owned()),
            PrimaryState::Halted("corrupted".to_owned()),
            PrimaryState::Stopped,
        ] {
            let record = PrimaryRecord {
                state,
                target: Some(height(10)),
                durable: durable.clone(),
            };
            let bytes = record.encode();
            assert_eq!(PrimaryRecord::decode(&bytes), Ok(record));
            for cut in 0..bytes.len() {
                assert!(
                    PrimaryRecord::decode(&bytes[..cut]).is_err(),
                    "cut at {cut}"
                );
            }
        }
        let mut other = PrimaryRecord {
            state: PrimaryState::Ready,
            target: None,
            durable: BTreeMap::new(),
        }
        .encode();
        other[8] = 2;
        assert_eq!(PrimaryRecord::decode(&other), Err(RecordError::Version(2)));
    }

    #[test]
    fn a_long_message_is_cut_at_a_character_boundary() {
        let record = PrimaryRecord {
            state: PrimaryState::Failed("é".repeat(MAX_MESSAGE)),
            target: None,
            durable: BTreeMap::new(),
        };
        let PrimaryState::Failed(kept) = PrimaryRecord::decode(&record.encode())
            .expect("decodes")
            .state
        else {
            panic!("another state");
        };
        assert_eq!(kept.len(), MAX_MESSAGE);
        assert!(kept.chars().all(|c| c == 'é'));
    }
}
