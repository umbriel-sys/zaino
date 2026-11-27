//! `PRIMARY`: the store primary's record for read-only readers (§5.2a), rewritten in place
//!
//! ```text
//! file = slot 0 ‖ slot 1                      2 × SLOT bytes
//! slot = magic b"ZAINOPR1" ‖ seq u64 ‖ len u32 ‖ record ‖ crc32
//! ```
//!
//! - record `seq` overwrites slot `seq % 2`: a reader mid-write finds the other slot whole
//! - never synced: a notice, not a commit (every primary rewrites it from boot)

use std::{io, path::Path, sync::Arc};

use crate::fs::{FileHandle, Fs};

/// The record's file, in the store root
pub(crate) const RECORD: &str = "PRIMARY";

const MAGIC: [u8; 8] = *b"ZAINOPR1";
const SLOT: usize = 1 << 16;
const FILE_LEN: u64 = 2 * SLOT as u64;
const HEADER: usize = MAGIC.len() + 8 + 4;
const CRC: usize = 4;

/// The longest record a slot holds
const MAX_RECORD: usize = SLOT - HEADER - CRC;

/// The primary's open record file
pub(crate) struct RecordFile {
    file: Arc<dyn FileHandle>,
    seq: u64,
}

impl RecordFile {
    /// `path` opened for writing (zeroed when absent or of another length), after its last record
    pub(crate) fn open(fs: &dyn Fs, path: &Path) -> io::Result<Self> {
        let file = fs.open(path)?;
        if file.len()? != FILE_LEN {
            file.set_len(0)?;
            file.set_len(FILE_LEN)?;
        }
        let seq = latest(file.as_ref())?.map_or(0, |(seq, _)| seq);
        Ok(Self { file, seq })
    }

    /// `record` over the older slot
    pub(crate) fn write(&mut self, record: &[u8]) -> io::Result<()> {
        let refused = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_owned());
        if record.len() > MAX_RECORD {
            return Err(refused("a record past its slot"));
        }
        let len = u32::try_from(record.len()).map_err(|_| refused("a record past u32"))?;
        let seq = self
            .seq
            .checked_add(1)
            .ok_or_else(|| refused("record sequence"))?;
        let mut slot = Vec::with_capacity(HEADER.saturating_add(record.len()).saturating_add(CRC));
        slot.extend_from_slice(&MAGIC);
        slot.extend_from_slice(&seq.to_le_bytes());
        slot.extend_from_slice(&len.to_le_bytes());
        slot.extend_from_slice(record);
        slot.extend_from_slice(&crc32fast::hash(&slot).to_le_bytes());
        self.file.write_all_at(&slot, offset(seq))?;
        self.seq = seq;
        Ok(())
    }
}

/// The last whole record at `path`; `None` when absent or none written
pub(crate) fn read(fs: &dyn Fs, path: &Path) -> io::Result<Option<Vec<u8>>> {
    let Some(file) = fs.open_read_only(path)? else {
        return Ok(None);
    };
    if file.len()? != FILE_LEN {
        return Ok(None);
    }
    Ok(latest(file.as_ref())?.map(|(_, record)| record))
}

fn offset(seq: u64) -> u64 {
    match seq.is_multiple_of(2) {
        true => 0,
        false => SLOT as u64,
    }
}

/// The whole slot with the highest `seq` (a slot failing its magic, length or CRC skipped)
fn latest(file: &dyn FileHandle) -> io::Result<Option<(u64, Vec<u8>)>> {
    let mut best: Option<(u64, Vec<u8>)> = None;
    for at in [0, SLOT as u64] {
        let mut header = [0; HEADER];
        file.read_exact_at(&mut header, at)?;
        let (magic, rest) = header.split_at(MAGIC.len());
        let (seq, len) = rest.split_at(8);
        let (Ok(seq), Ok(len)) = (<[u8; 8]>::try_from(seq), <[u8; 4]>::try_from(len)) else {
            continue;
        };
        let (seq, len) = (u64::from_le_bytes(seq), u32::from_le_bytes(len));
        let Some(len) = usize::try_from(len).ok().filter(|len| *len <= MAX_RECORD) else {
            continue;
        };
        if magic != MAGIC {
            continue;
        }
        let mut slot = vec![0; HEADER.saturating_add(len).saturating_add(CRC)];
        file.read_exact_at(&mut slot, at)?;
        let (covered, crc) = slot.split_at(HEADER.saturating_add(len));
        if crc32fast::hash(covered).to_le_bytes() != crc {
            continue;
        }
        if best.as_ref().is_none_or(|(held, _)| seq > *held) {
            let record = covered.get(HEADER..).unwrap_or_default().to_vec();
            best = Some((seq, record));
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{read, RecordFile, MAX_RECORD};
    use crate::fs::{Fs as _, SimFs};

    #[test]
    fn the_last_record_reads_back_over_alternating_slots() {
        let fs = SimFs::new();
        let path = Path::new("/store/PRIMARY");
        fs.create_dir_all(Path::new("/store")).expect("dir");
        assert_eq!(read(fs.as_ref(), path).expect("read"), None);
        let mut file = RecordFile::open(fs.as_ref(), path).expect("opened");
        assert_eq!(read(fs.as_ref(), path).expect("read"), None, "none written");
        for record in [&b"one"[..], b"two", b""] {
            file.write(record).expect("written");
            assert_eq!(
                read(fs.as_ref(), path).expect("read").as_deref(),
                Some(record)
            );
        }
        assert!(file.write(&vec![0; MAX_RECORD + 1]).is_err());

        let mut reopened = RecordFile::open(fs.as_ref(), path).expect("reopened");
        reopened.write(b"next").expect("written");
        assert_eq!(
            read(fs.as_ref(), path).expect("read").as_deref(),
            Some(&b"next"[..])
        );
    }

    /// A slot torn mid-write leaves the other one standing.
    #[test]
    fn a_torn_slot_reads_the_record_before_it() {
        let fs = SimFs::new();
        let path = Path::new("/store/PRIMARY");
        fs.create_dir_all(Path::new("/store")).expect("dir");
        let mut file = RecordFile::open(fs.as_ref(), path).expect("opened");
        file.write(b"first").expect("written");
        file.write(b"second").expect("written");
        // record 2 sits in slot 0; tear it
        let handle = fs.open(path).expect("open");
        handle.write_all_at(&[0xff; 4], 20).expect("torn");
        assert_eq!(
            read(fs.as_ref(), path).expect("read").as_deref(),
            Some(&b"first"[..])
        );
    }
}
