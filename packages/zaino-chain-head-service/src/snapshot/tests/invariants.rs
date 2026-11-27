//! [`PersistentSnapshot`]'s representation invariants, checked against its
//! private fields: what the read traits cannot show.

use std::collections::HashSet;

use zaino_primitives::types::BlockHash;

use crate::{
    graph::tests::InspectableGraph,
    snapshot::{eviction_key, PersistentSnapshot, MAX_BRANCHES},
};

impl InspectableGraph for PersistentSnapshot {
    fn retained_hashes(&self) -> HashSet<BlockHash> {
        self.directory.keys().copied().collect()
    }

    fn retained_block_count(&self) -> usize {
        Self::retained_block_count(self)
    }

    fn check_representation(&self) -> Result<(), String> {
        self.check_trunk_contiguous_and_linked()?;
        self.check_branches()?;
        self.check_directory_is_trunk_and_branches()
    }
}

impl PersistentSnapshot {
    /// Heights run without gaps and each block names the one below as parent.
    fn check_trunk_contiguous_and_linked(&self) -> Result<(), String> {
        let blocks: Vec<_> = self.trunk.iter().collect();
        for pair in blocks.windows(2) {
            let (below, above) = (&pair[0], &pair[1]);
            if below.height().checked_add(1) != Some(above.height()) {
                return Err(format!(
                    "trunk heights jump from {} to {}",
                    below.height(),
                    above.height()
                ));
            }
            if above.parent_hash != below.hash() {
                return Err(format!(
                    "trunk block {} does not name {} as parent",
                    above.hash(),
                    below.hash()
                ));
            }
        }
        Ok(())
    }

    /// At most `MAX_BRANCHES`, strongest first; each non-empty, linked from
    /// its fork, off the trunk, with a tip no other branch has.
    fn check_branches(&self) -> Result<(), String> {
        if self.branches.len() > MAX_BRANCHES {
            return Err(format!("{} branches retained", self.branches.len()));
        }
        let mut tips = HashSet::new();
        let mut previous_key = None;
        for branch in &self.branches {
            let Some(first) = branch.blocks.front() else {
                return Err("an empty branch".to_owned());
            };
            if first.parent_hash != branch.fork.hash
                || branch.fork.height.checked_add(1) != Some(first.height())
            {
                return Err(format!(
                    "branch starting at {} does not grow from its fork {:?}",
                    first.hash(),
                    branch.fork
                ));
            }
            let blocks: Vec<_> = branch.blocks.iter().collect();
            for pair in blocks.windows(2) {
                let (below, above) = (&pair[0], &pair[1]);
                if above.parent_hash != below.hash()
                    || below.height().checked_add(1) != Some(above.height())
                {
                    return Err(format!(
                        "branch block {} does not follow {}",
                        above.hash(),
                        below.hash()
                    ));
                }
            }
            if let Some(on_trunk) = blocks
                .iter()
                .find(|block| self.trunk.holds(block.reference))
            {
                return Err(format!("branch block {} is on the trunk", on_trunk.hash()));
            }
            let Some(tip) = branch.tip() else {
                return Err("a branch without a tip".to_owned());
            };
            if !tips.insert(tip.hash()) {
                return Err(format!("two branches end at {}", tip.hash()));
            }
            let key = eviction_key(tip);
            if previous_key.is_some_and(|previous| previous < key) {
                return Err(format!("branch tipped by {} is out of order", tip.hash()));
            }
            previous_key = Some(key);
        }
        Ok(())
    }

    /// `directory = trunk ∪ branches`, keyed by each block's own hash.
    fn check_directory_is_trunk_and_branches(&self) -> Result<(), String> {
        let held: HashSet<BlockHash> = self
            .trunk
            .iter()
            .chain(self.branches.iter().flat_map(|branch| branch.blocks.iter()))
            .map(|block| block.hash())
            .collect();
        let keyed: HashSet<BlockHash> = self.directory.keys().copied().collect();
        if held != keyed {
            return Err(format!(
                "directory holds {:?}, trunk and branches hold {:?}",
                keyed.symmetric_difference(&held).collect::<Vec<_>>(),
                held.symmetric_difference(&keyed).collect::<Vec<_>>()
            ));
        }
        match self
            .directory
            .iter()
            .find(|(key, block)| **key != block.hash())
        {
            Some((key, block)) => Err(format!(
                "directory holds block {} under key {key}",
                block.hash()
            )),
            None => Ok(()),
        }
    }
}
