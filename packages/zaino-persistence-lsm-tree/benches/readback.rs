//! Per-row cost of the sealed-segment read-back (`LsmConfig::verify_writes`): one batch of
//! `ROWS` (default 1 000 000) 36-byte-key, 36-byte-value rows flushed with the read-back off,
//! then on, the difference per row printed
//!
//! `ROWS=200000 cargo bench -p zaino-persistence-lsm-tree --bench readback`

use std::{
    num::{NonZeroU32, NonZeroUsize},
    time::{Duration, Instant},
};

use zaino_persistence::{Backend, BackendWriter, Namespace, NamespaceSpec, WriteOp};
use zaino_persistence_lsm_tree::{LsmConfig, LsmTreeBackend, LsmTreeConfig};
use zaino_primitives::types::{BlockHash, BlockRef, Height};

const SPENT: Namespace = Namespace::new("spent");

/// `rows` committed and flushed as one batch segment: the flush's wall time
fn flush(verify_writes: bool, rows: u64) -> Duration {
    let dir = tempfile::tempdir().expect("tempdir");
    let width = NonZeroU32::new(36).expect("non-zero");
    let specs = [NamespaceSpec::map(SPENT, width, width, false)];
    let config = LsmTreeConfig {
        name: "readback",
        chain: "bench",
        format: 1,
        namespaces: &specs,
        tuning: &[],
        lsm: LsmConfig {
            verify_writes,
            ..LsmConfig::default()
        },
        write_buffer: NonZeroUsize::MAX,
        threads: None,
        strict_manifest_read: false,
    };
    let backend = LsmTreeBackend::open_real(dir.path(), &config).expect("open");
    let ops: Vec<WriteOp> = (0..rows)
        .map(|n| {
            let mut key = vec![0u8; 36];
            key[..8].copy_from_slice(&n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
            key[28..].copy_from_slice(&n.to_be_bytes());
            WriteOp::Put {
                namespace: SPENT,
                key,
                value: vec![n as u8; 36],
            }
        })
        .collect();
    let tip = BlockRef {
        hash: BlockHash::from([1; 32]),
        height: Height::try_from(1).expect("in range"),
    };
    backend
        .writer()
        .expect("writer")
        .commit(tip, ops)
        .expect("commit");
    let started = Instant::now();
    backend.flush().expect("flush");
    started.elapsed()
}

fn main() {
    let rows: u64 = std::env::var("ROWS")
        .ok()
        .and_then(|rows| rows.parse().ok())
        .unwrap_or(1_000_000);
    let without = flush(false, rows);
    let with = flush(true, rows);
    let per_row = with.saturating_sub(without).as_nanos() as f64 / rows as f64;
    println!(
        "{rows} rows: flush {with:?} with the read-back, {without:?} without: {per_row:.0} ns/row"
    );
}
