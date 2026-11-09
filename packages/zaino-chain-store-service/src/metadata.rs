//! `METADATA` (D-80): what the last boot enabled, so a changed set is
//! reported. Advisory and regenerable (D-34): unreadable is logged and
//! rewritten, never fatal.
//!
//! `magic (8) | version u16 | network | count u16 | (name, format u16)* |
//! BLAKE2b-256 of everything before`, strings as a length byte and bytes,
//! integers little-endian.

use blake2::{digest::consts::U32, Blake2b, Digest as _};
use zaino_indexes::IndexId;

/// The store file's name.
pub(crate) const NAME: &str = "METADATA";

const MAGIC: &[u8; 8] = b"ZAINOMD\0";
const VERSION: u16 = 1;
const CHECKSUM: usize = 32;

/// One enabled index, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) format: u16,
}

/// What a boot recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub(crate) network: String,
    pub(crate) indexes: Vec<Entry>,
}

/// Why recorded bytes were not read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MetadataError {
    #[error("checksum mismatch")]
    Checksum,
    #[error("not a METADATA file")]
    Magic,
    #[error("version {0}")]
    Version(u16),
    #[error("truncated")]
    Truncated,
    #[error("{0} too long to record")]
    TooLong(&'static str),
}

impl Metadata {
    /// The record of `network` enabling `ids`.
    pub(crate) fn of(network: &str, ids: impl IntoIterator<Item = IndexId>) -> Self {
        Self {
            network: network.to_owned(),
            indexes: ids
                .into_iter()
                .map(|id| Entry {
                    name: id.name().to_owned(),
                    format: id.schema().format,
                })
                .collect(),
        }
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, MetadataError> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(VERSION.to_le_bytes());
        put_str(&mut bytes, &self.network)?;
        let count = u16::try_from(self.indexes.len()).map_err(|_| MetadataError::TooLong("set"))?;
        bytes.extend(count.to_le_bytes());
        for entry in &self.indexes {
            put_str(&mut bytes, &entry.name)?;
            bytes.extend(entry.format.to_le_bytes());
        }
        let checksum = Blake2b::<U32>::digest(&bytes);
        bytes.extend(checksum);
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, MetadataError> {
        let body_len = bytes
            .len()
            .checked_sub(CHECKSUM)
            .ok_or(MetadataError::Truncated)?;
        let (body, checksum) = bytes.split_at(body_len);
        if Blake2b::<U32>::digest(body).as_slice() != checksum {
            return Err(MetadataError::Checksum);
        }
        let mut r = body;
        if take(&mut r, MAGIC.len())? != MAGIC {
            return Err(MetadataError::Magic);
        }
        let version = u16_le(&mut r)?;
        if version != VERSION {
            return Err(MetadataError::Version(version));
        }
        let network = get_str(&mut r)?;
        let count = u16_le(&mut r)?;
        let indexes = (0..count)
            .map(|_| {
                Ok(Entry {
                    name: get_str(&mut r)?,
                    format: u16_le(&mut r)?,
                })
            })
            .collect::<Result<_, MetadataError>>()?;
        if !r.is_empty() {
            return Err(MetadataError::Truncated);
        }
        Ok(Self { network, indexes })
    }
}

fn put_str(bytes: &mut Vec<u8>, value: &str) -> Result<(), MetadataError> {
    let len = u8::try_from(value.len()).map_err(|_| MetadataError::TooLong("name"))?;
    bytes.push(len);
    bytes.extend(value.as_bytes());
    Ok(())
}

fn take<'a>(r: &mut &'a [u8], len: usize) -> Result<&'a [u8], MetadataError> {
    if r.len() < len {
        return Err(MetadataError::Truncated);
    }
    let (head, rest) = r.split_at(len);
    *r = rest;
    Ok(head)
}

fn u16_le(r: &mut &[u8]) -> Result<u16, MetadataError> {
    let bytes = take(r, 2)?;
    Ok(u16::from_le_bytes([
        bytes.first().copied().unwrap_or_default(),
        bytes.get(1).copied().unwrap_or_default(),
    ]))
}

fn get_str(r: &mut &[u8]) -> Result<String, MetadataError> {
    let len = take(r, 1)?.first().copied().unwrap_or_default();
    let bytes = take(r, usize::from(len))?;
    String::from_utf8(bytes.to_vec()).map_err(|_| MetadataError::Magic)
}

#[cfg(test)]
mod tests {
    use zaino_indexes::IndexId;

    use super::{Metadata, MetadataError};

    #[test]
    fn round_trips_and_refuses_any_flipped_byte() {
        let metadata = Metadata::of("regtest", [IndexId::Headers, IndexId::Txids]);
        let bytes = metadata.encode().expect("encodes");
        assert_eq!(Metadata::decode(&bytes), Ok(metadata));
        for at in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[at] ^= 1;
            assert!(Metadata::decode(&bad).is_err(), "byte {at}");
        }
        assert_eq!(
            Metadata::decode(&bytes[..10]),
            Err(MetadataError::Truncated)
        );
    }
}
