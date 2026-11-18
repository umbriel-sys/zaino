//! Prometheus metric names the store emits, with the `# HELP` zainod
//! registers (the chain head's convention). Labels: [`INDEX_LABEL`] names an
//! index, [`OP_LABEL`] a reader operation, [`STAGE_LABEL`] a pipeline stage.

#![allow(missing_docs)] // the `# HELP` in the tables below is the description

pub const INDEX_LABEL: &str = "index";
pub const OP_LABEL: &str = "op";
pub const STAGE_LABEL: &str = "stage";

// The build pipeline, per block
pub const STORE_FETCH_SECONDS: &str = "zaino.store.fetch_seconds";
pub const STORE_RESOLVE_SECONDS: &str = "zaino.store.resolve_seconds";
pub const STORE_QUEUE_WAIT_SECONDS: &str = "zaino.store.queue_wait_seconds";
pub const STORE_WRITER_WAIT_SECONDS: &str = "zaino.store.writer_wait_seconds";
pub const STORE_FOLD_SECONDS: &str = "zaino.store.fold_seconds";
pub const STORE_COMMIT_SECONDS: &str = "zaino.store.commit_seconds";
// Reads
pub const STORE_READ_SECONDS: &str = "zaino.store.read_seconds";
// Progress
pub const STORE_WATERMARK_HEIGHT: &str = "zaino.store.watermark_height";
pub const STORE_INDEX_TIP_HEIGHT: &str = "zaino.store.index_tip_height";
pub const STORE_BUILD_TARGET_HEIGHT: &str = "zaino.store.build_target_height";
// Totals
pub const STORE_BLOCKS_BUILT_TOTAL: &str = "zaino.store.blocks_built_total";
pub const STORE_BUILD_FAILURES_TOTAL: &str = "zaino.store.build_failures_total";
pub const STORE_PANICS_TOTAL: &str = "zaino.store.panics_total";

#[rustfmt::skip]
pub const COUNTERS: &[(&str, &str)] = &[
    (STORE_BLOCKS_BUILT_TOTAL, "Blocks folded and committed, per index"),
    (STORE_BUILD_FAILURES_TOTAL, "Builds that ended in a failure (the store keeps what it committed)"),
    (STORE_PANICS_TOTAL, "Panics caught in the build pipeline, per stage; an index whose writer panicked is reopened"),
];

#[rustfmt::skip]
pub const GAUGES: &[(&str, &str)] = &[
    (STORE_WATERMARK_HEIGHT, "The store watermark: the lowest built height among the enabled indexes"),
    (STORE_INDEX_TIP_HEIGHT, "Each index's built height"),
    (STORE_BUILD_TARGET_HEIGHT, "The height the last build was asked for"),
];

#[rustfmt::skip]
pub const HISTOGRAMS: &[(&str, &str)] = &[
    (STORE_FETCH_SECONDS, "One block fetched from the validator and assembled for the indexes"),
    (STORE_RESOLVE_SECONDS, "One block's transparent prevouts resolved"),
    (STORE_QUEUE_WAIT_SECONDS, "The fetch waiting for room in an index's queue, per index (backpressure)"),
    (STORE_WRITER_WAIT_SECONDS, "An index writer waiting for its turn under the writers budget, per index"),
    (STORE_FOLD_SECONDS, "One block folded into an index, per index"),
    (STORE_COMMIT_SECONDS, "One block's writes committed to an index, per index"),
    (STORE_READ_SECONDS, "A store read, per reader operation"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name is distinct and under the store's prefix.
    #[test]
    fn names_are_distinct_and_prefixed() {
        let names: Vec<&str> = COUNTERS
            .iter()
            .chain(GAUGES)
            .chain(HISTOGRAMS)
            .map(|(name, _)| *name)
            .collect();
        for name in &names {
            assert!(name.starts_with("zaino.store."), "{name}");
            assert_eq!(
                names.iter().filter(|other| *other == name).count(),
                1,
                "{name}"
            );
        }
        assert_eq!(names.len(), 13);
    }
}
