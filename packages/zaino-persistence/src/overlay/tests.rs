use std::num::{NonZeroU32, NonZeroUsize};
use std::ops::Bound::Unbounded;

use zaino_primitives::types::{BlockHash, BlockRef, Height};

use super::{Overlay, RebaseError};
use crate::backend::{
    sequence_key, BackendReader, MaintenanceReader, Namespace, NamespaceSpec, ScanDirection,
    ScanRange, Width, WriteOp,
};
use crate::error::{CommitError, ReadError};

const SEQ: Namespace = Namespace::new("seq");
const MAP: Namespace = Namespace::new("map");

fn specs() -> [NamespaceSpec; 2] {
    [
        NamespaceSpec::sequence(SEQ, Width::Variable),
        NamespaceSpec::map(MAP, NonZeroU32::MIN, NonZeroU32::MIN, true),
    ]
}

fn block(height: u32) -> BlockRef {
    BlockRef {
        hash: BlockHash::from([height as u8; 32]),
        height: Height::try_from(height).expect("test heights are small"),
    }
}

fn append(position: u64) -> WriteOp {
    WriteOp::Put {
        namespace: SEQ,
        key: sequence_key(position),
        value: vec![position as u8],
    }
}

fn put(key: u8) -> WriteOp {
    WriteOp::Put {
        namespace: MAP,
        key: vec![key],
        value: vec![key],
    }
}

fn delete(key: u8) -> WriteOp {
    WriteOp::Delete {
        namespace: MAP,
        key: vec![key],
    }
}

fn over(root: Option<u32>) -> Overlay {
    Overlay::new(&specs(), root.map(block)).expect("distinct specs")
}

fn value(overlay: &Overlay, namespace: Namespace, key: &[u8]) -> Option<Vec<u8>> {
    overlay
        .get(namespace, key)
        .expect("a declared key of its width")
}

/// Every entry of both namespaces, plus the tips.
fn view(overlay: &Overlay) -> impl PartialEq + std::fmt::Debug {
    (
        BackendReader::tip(overlay).expect("tip"),
        overlay.durable_tip().expect("durable tip"),
        overlay.scan(SEQ).expect("seq"),
        overlay.scan(MAP).expect("map"),
    )
}

#[test]
fn a_sequence_above_a_block_starts_at_its_first_append() {
    let overlay = over(Some(10))
        .with(block(11), vec![append(100)])
        .and_then(|overlay| overlay.with(block(12), vec![append(101)]))
        .expect("consecutive appends from any start");
    assert!(matches!(
        overlay.with(block(13), vec![append(103)]),
        Err(CommitError::NotNextPosition { .. })
    ));
    assert_eq!(
        value(&overlay, SEQ, &sequence_key(99)),
        None,
        "below the root: absent"
    );
    assert_eq!(value(&overlay, SEQ, &sequence_key(101)), Some(vec![101]));
}

#[test]
fn over_genesis_a_sequence_starts_at_zero() {
    assert!(matches!(
        over(None).with(block(0), vec![append(5)]),
        Err(CommitError::NotNextPosition { .. })
    ));
}

#[test]
fn map_keys_above_a_block() {
    let overlay = over(Some(10))
        .with(block(11), vec![delete(1), put(2)])
        .expect("a delete of an unseen key is a key below the root");
    assert!(matches!(
        overlay.with(block(12), vec![put(1)]),
        Err(CommitError::KeyRemoved { .. })
    ));
    assert!(matches!(
        overlay.with(block(12), vec![put(2)]),
        Err(CommitError::KeyExists { .. })
    ));
    let overlay = overlay
        .with(block(12), vec![delete(2)])
        .expect("a held key deletes");
    assert_eq!(value(&overlay, MAP, &[2]), None);
}

#[test]
fn over_genesis_every_key_rule_holds() {
    assert!(matches!(
        over(None).with(block(0), vec![delete(1)]),
        Err(CommitError::KeyAbsent { .. })
    ));
}

#[test]
fn a_commit_must_sit_above_the_tip() {
    assert!(matches!(
        over(Some(10)).with(block(10), vec![put(1)]),
        Err(CommitError::TipNotAbove { .. })
    ));
}

#[test]
fn a_refused_commit_and_the_parent_are_unchanged() {
    let parent = over(None)
        .with(block(0), vec![append(0), put(1)])
        .expect("genesis");
    let before = view(&parent);
    let child = parent
        .with(block(1), vec![append(1), delete(1)])
        .expect("child");
    assert!(parent.with(block(1), vec![append(1), put(1)]).is_err());
    assert_eq!(view(&parent), before, "the parent keeps its version");
    assert_eq!(value(&child, MAP, &[1]), None);
    assert_eq!(value(&parent, MAP, &[1]), Some(vec![1]));
}

#[test]
fn rebase_equals_a_fresh_overlay_over_the_new_root() {
    let blocks = [
        vec![append(0), put(1), put(2)],
        vec![append(1), delete(1)],
        vec![append(2), put(3)],
        vec![append(3), delete(2), put(4)],
    ];
    let mut whole = over(None);
    for (height, ops) in (0..).zip(&blocks) {
        whole = whole.with(block(height), ops.clone()).expect("block");
    }
    let rebased = whole.rebase(block(1)).expect("block 1 is held");
    let mut fresh = over(Some(1));
    for (height, ops) in (2..).zip(blocks.iter().skip(2)) {
        fresh = fresh.with(block(height), ops.clone()).expect("block");
    }
    assert_eq!(view(&rebased), view(&fresh));
    assert_eq!(rebased.durable_tip().expect("durable tip"), Some(block(1)));
}

#[test]
fn rebase_onto_an_unheld_block_is_refused() {
    let overlay = over(Some(10)).with(block(11), vec![put(1)]).expect("block");
    assert!(matches!(
        overlay.rebase(block(12)),
        Err(RebaseError::NotHeld)
    ));
    assert!(matches!(
        overlay.rebase(block(9)),
        Err(RebaseError::NotHeld)
    ));
    let same = overlay.rebase(block(10)).expect("the current root");
    assert_eq!(view(&same), view(&overlay));
}

#[test]
fn a_scan_of_an_undeclared_namespace_fails_once() {
    let range = ScanRange {
        lower: Unbounded,
        upper: Unbounded,
        direction: ScanDirection::Forward,
        chunk_bytes: NonZeroUsize::MIN,
    };
    let mut chunks = over(None).scan_range(Namespace::new("other"), range, |_, _| Some(()));
    assert!(matches!(
        chunks.next(),
        Some(Err(ReadError::NamespaceNotFound(_)))
    ));
    assert!(chunks.next().is_none());
}
