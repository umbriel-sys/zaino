//! The port's conformance suite against the backend on the real filesystem, through its public
//! API only (crash states and merges' internals: the crate's own tests, on `SimFs`)

use std::{num::NonZeroUsize, path::PathBuf};

use proptest::prelude::*;
use tempfile::TempDir;
use zaino_persistence::conformance::{
    self, linked_beside_a_writer, linked_history, LinkedSubject, Subject, SCANNED, SPECS,
};
use zaino_persistence_lsm_tree::{
    fs::RealFs, LsmConfig, LsmTreeBackend, LsmTreeConfig, LsmTreeReadOnly, Tuning,
};

/// A temporary directory reopened as the conformance index, merging every two segments; written
/// only by `flush`, or also whenever the buffer reaches `write_buffer`
struct RealDir {
    _root: TempDir,
    path: PathBuf,
    write_buffer: NonZeroUsize,
}

impl RealDir {
    fn new() -> Self {
        Self::writing_at(NonZeroUsize::MAX)
    }

    fn writing_at(write_buffer: NonZeroUsize) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("idx");
        Self {
            _root: root,
            path,
            write_buffer,
        }
    }
}

impl Subject for RealDir {
    type Backend = LsmTreeBackend;

    fn open(&self) -> LsmTreeBackend {
        let tuning = [(
            SCANNED,
            Tuning {
                scope: 8,
                cache_writes: false,
            },
        )];
        let config = LsmTreeConfig {
            name: "conformance",
            chain: "regtest",
            format: 1,
            schema: zaino_persistence::SchemaDigest::new([0; 32]),
            namespaces: &SPECS,
            tuning: &tuning,
            lsm: LsmConfig {
                fanout: 2,
                spill_bytes: NonZeroUsize::new(64).expect("non-zero"),
                ..LsmConfig::default()
            },
            write_buffer: self.write_buffer,
            // the shipped default: the machine's available parallelism
            threads: None,
            strict_manifest_read: false,
        };
        LsmTreeBackend::open_real(&self.path, &config).expect("open")
    }

    fn writes_on_its_own(&self) -> bool {
        self.write_buffer != NonZeroUsize::MAX
    }
}

impl LinkedSubject for RealDir {
    type ReadOnly = LsmTreeReadOnly;

    fn open_read_only(&self) -> LsmTreeReadOnly {
        LsmTreeReadOnly::open(
            RealFs::shared(),
            &self.path,
            "conformance",
            "regtest",
            1,
            zaino_persistence::SchemaDigest::new([0; 32]),
            &SPECS,
        )
        .expect("linked")
    }
}

/// Real mappings: a merge unlinks segments a read-only reader still maps
#[test]
fn a_read_only_backend_reads_each_announced_state_beside_a_writer_on_a_real_filesystem() {
    linked_beside_a_writer(RealDir::new(), 200);
    linked_beside_a_writer(
        RealDir::writing_at(NonZeroUsize::new(2048).expect("non-zero")),
        200,
    );
}

#[test]
fn the_backend_keeps_the_port_contract_on_a_real_filesystem() {
    conformance::contract(RealDir::new());
    conformance::contract(RealDir::writing_at(
        NonZeroUsize::new(2048).expect("non-zero"),
    ));
}

proptest! {
    // a real directory per case: fewer cases than the crate's `SimFs` run
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    #[test]
    fn random_histories_answer_like_a_btreemap_on_a_real_filesystem(
        steps in conformance::steps(),
    ) {
        conformance::history(RealDir::new(), &steps);
    }

    #[test]
    fn random_histories_read_read_only_at_each_durable_tip_on_a_real_filesystem(
        steps in conformance::steps(),
    ) {
        linked_history(RealDir::new(), &steps);
    }
}
