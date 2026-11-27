//! The port's conformance suite against the in-memory backend.

use std::path::Path;

use proptest::prelude::*;
use zaino_persistence::{
    conformance::{
        contract, history, linked_beside_a_writer, linked_history, steps, LinkedSubject, Subject,
        SPECS,
    },
    in_memory::{InMemoryBackend, InMemoryReadOnly, InMemoryStorage},
    Identity, SchemaDigest, Storage as _,
};

const DIR: &str = "/store/indexes/conformance_1";

const IDENTITY: Identity<'static> = Identity {
    name: "conformance",
    chain: "regtest",
    format: 1,
    schema: SchemaDigest::new([0; 32]),
};

/// One shared storage: a reopen is a fresh handle onto the same directory.
#[derive(Default)]
struct InMemory(InMemoryStorage);

impl Subject for InMemory {
    type Backend = InMemoryBackend;

    fn open(&self) -> InMemoryBackend {
        self.0
            .open(Path::new(DIR), IDENTITY, &SPECS)
            .expect("conformance specs are distinct")
    }

    fn keeps_unflushed(&self) -> bool {
        true
    }
}

impl LinkedSubject for InMemory {
    type ReadOnly = InMemoryReadOnly;

    fn open_read_only(&self) -> InMemoryReadOnly {
        self.0
            .open_read_only(Path::new(DIR), IDENTITY, &SPECS)
            .expect("linked")
    }
}

#[test]
fn in_memory_contract() {
    contract(InMemory::default());
}

#[test]
fn in_memory_read_only_beside_a_writer() {
    linked_beside_a_writer(InMemory::default(), 64);
}

proptest! {
    #[test]
    fn in_memory_history(steps in steps()) {
        history(InMemory::default(), &steps);
    }

    #[test]
    fn in_memory_linked_history(steps in steps()) {
        linked_history(InMemory::default(), &steps);
    }
}
