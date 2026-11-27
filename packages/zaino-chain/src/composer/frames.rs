//! Compact blocks as encoded frames, and the cache every client shares.
//!
//! A frame is written once by `zaino_proto::compact::write_compact_block` and
//! then copied, never re-encoded, by every client asking for that block under
//! that filter. Only final frames are kept:
//! - the store's, which never change (the store holds finalised blocks, fees
//!   from I-05);
//! - the head's from its `compact_blocks` fold, which are exactly those whose
//!   fees are all known (the projection's are all `None`), tagged with the
//!   epoch they were read in and dropped once it moves;
//! - never the validator's.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use bytes::Bytes;
use futures::Stream;
use zaino_chain_head::{ChainHeadCompactBlockService, ChainHeadSnapshot as _};
use zaino_chain_store::{ChainStoreReader, CompactBlockRead as StoreCompactBlockRead, PoolFilter};
use zaino_primitives::types::{ChainStateEpoch, CompactBlock, Height};

use super::coverage::Provider;
use super::{stream, uncoverable, ComposerSnapshot};
use crate::error::Result;
use crate::ports::CompactFrameRead;
use crate::source::ChainViewSource;

/// Whose frame an entry is, and so when it stays valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Store,
    Head(ChainStateEpoch),
}

/// A byte-bounded LRU of frames keyed by (height, filter).
#[derive(Debug)]
pub(crate) struct FrameCache {
    capacity: usize,
    lru: Mutex<Lru>,
}

#[derive(Debug, Default)]
struct Lru {
    entries: HashMap<(Height, PoolFilter), Entry>,
    /// Use stamp to key, oldest first.
    order: BTreeMap<u64, (Height, PoolFilter)>,
    next: u64,
    bytes: usize,
}

#[derive(Debug)]
struct Entry {
    frame: Bytes,
    tag: Tag,
    stamp: u64,
}

impl FrameCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            lru: Mutex::default(),
        }
    }

    /// The frame at `key` if it was kept under `tag`. A poisoned lock is a
    /// miss: the cache only ever saves work.
    fn get(&self, key: (Height, PoolFilter), tag: Tag) -> Option<Bytes> {
        let mut lru = self.lru.lock().ok()?;
        let stamp = lru.stamp();
        let entry = lru.entries.get_mut(&key).filter(|entry| entry.tag == tag)?;
        let old = std::mem::replace(&mut entry.stamp, stamp);
        let frame = entry.frame.clone();
        lru.order.remove(&old);
        lru.order.insert(stamp, key);
        Some(frame)
    }

    /// Keeps `frame` at `key`, evicting the least recently used to fit.
    fn insert(&self, key: (Height, PoolFilter), frame: Bytes, tag: Tag) {
        if frame.len() > self.capacity {
            return;
        }
        let Ok(mut lru) = self.lru.lock() else {
            return;
        };
        lru.remove(&key);
        while lru.bytes.saturating_add(frame.len()) > self.capacity {
            let Some((_, oldest)) = lru.order.pop_first() else {
                break;
            };
            lru.remove(&oldest);
        }
        let stamp = lru.stamp();
        lru.bytes = lru.bytes.saturating_add(frame.len());
        lru.order.insert(stamp, key);
        lru.entries.insert(key, Entry { frame, tag, stamp });
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.lru.lock().map_or(0, |lru| lru.bytes)
    }
}

impl Lru {
    fn stamp(&mut self) -> u64 {
        let stamp = self.next;
        self.next = self.next.saturating_add(1);
        stamp
    }

    fn remove(&mut self, key: &(Height, PoolFilter)) {
        if let Some(entry) = self.entries.remove(key) {
            self.order.remove(&entry.stamp);
            self.bytes = self.bytes.saturating_sub(entry.frame.len());
        }
    }
}

/// The tag a provider's final frame is kept under; `None` keeps nothing.
fn tag_of(provider: Provider, epoch: ChainStateEpoch, block: &CompactBlock) -> Option<Tag> {
    match provider {
        Provider::Store => Some(Tag::Store),
        Provider::Head if block.transactions.iter().all(|tx| tx.fee.is_some()) => {
            Some(Tag::Head(epoch))
        }
        Provider::Head | Provider::Source => None,
    }
}

/// The tag a provider's frame would have been kept under, for a lookup.
fn lookup_tag(provider: Provider, epoch: ChainStateEpoch) -> Option<Tag> {
    match provider {
        Provider::Store => Some(Tag::Store),
        Provider::Head => Some(Tag::Head(epoch)),
        Provider::Source => None,
    }
}

fn encode(block: &CompactBlock, pools: PoolFilter) -> Bytes {
    let mut frame = Vec::new();
    zaino_proto::compact::write_compact_block(block, pools, &mut frame);
    Bytes::from(frame)
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadCompactBlockService,
    Source: ChainViewSource,
{
    /// Frames for a run of heights from one provider: cached where kept, the
    /// misses read as one run and encoded. Contiguous from the first height,
    /// short where the provider ran out.
    async fn frames_from(
        &self,
        provider: Provider,
        heights: Vec<Height>,
        pools: PoolFilter,
    ) -> Result<Vec<Bytes>> {
        let epoch = self.head.epoch();
        let lookup = lookup_tag(provider, epoch);
        let mut frames: Vec<Option<Bytes>> = heights
            .iter()
            .map(|height| lookup.and_then(|tag| self.frames.get((*height, pools), tag)))
            .collect();
        let first = frames.iter().position(Option::is_none);
        let last = frames.iter().rposition(Option::is_none);
        if let (Some(first), Some(last)) = (first, last) {
            let run: Vec<Height> = heights
                .iter()
                .copied()
                .skip(first)
                .take(last.saturating_sub(first).saturating_add(1))
                .collect();
            let mut blocks = self
                .compact_from(provider, run.clone(), pools)
                .await?
                .into_iter();
            for (slot, height) in frames.iter_mut().skip(first).zip(run) {
                let Some(block) = blocks.next() else {
                    break;
                };
                if u32::from(height) != block.height {
                    break;
                }
                if slot.is_none() {
                    let frame = encode(&block, pools);
                    if let Some(tag) = tag_of(provider, epoch, &block) {
                        self.frames.insert((height, pools), frame.clone(), tag);
                    }
                    *slot = Some(frame);
                }
            }
        }
        Ok(frames.into_iter().map_while(|frame| frame).collect())
    }
}

impl<Reader, HeadSnapshot, Source> CompactFrameRead
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadCompactBlockService,
    Source: ChainViewSource,
{
    async fn compact_frame(&self, height: Height, pools: PoolFilter) -> Result<Option<Bytes>> {
        match self
            .coverage
            .provider_at(height)
            .map_err(|()| uncoverable())?
        {
            None => Ok(None),
            Some(provider) => Ok(self
                .frames_from(provider, vec![height], pools)
                .await?
                .into_iter()
                .next()),
        }
    }

    fn stream_compact_frames(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> impl Stream<Item = Result<Vec<Bytes>>> + Send + use<Reader, HeadSnapshot, Source> {
        stream::walk_sized(
            self.clone(),
            start,
            end,
            move |snapshot, segment, heights| {
                Box::pin(
                    async move { snapshot.frames_from(segment.provider, heights, pools).await },
                )
            },
            |frames: &[Bytes]| frames.iter().map(Bytes::len).fold(0, usize::saturating_add),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{BlockHash, BlockRef};

    fn key(h: u32) -> (Height, PoolFilter) {
        (Height::try_from(h).expect("small"), PoolFilter::all())
    }

    fn frame(len: usize) -> Bytes {
        Bytes::from(vec![1; len])
    }

    fn epoch(generation: u64) -> ChainStateEpoch {
        ChainStateEpoch {
            generation,
            best_tip: BlockRef {
                hash: BlockHash::from([0; 32]),
                height: Height::try_from(1).expect("small"),
            },
        }
    }

    /// The least recently used frames go first, and the bytes stay bounded.
    #[test]
    fn evicts_the_least_recently_used_within_its_bytes() {
        let cache = FrameCache::new(30);
        for h in 0..3 {
            cache.insert(key(h), frame(10), Tag::Store);
        }
        assert!(cache.get(key(0), Tag::Store).is_some(), "touched");
        cache.insert(key(3), frame(10), Tag::Store);
        assert!(cache.get(key(1), Tag::Store).is_none(), "the oldest went");
        assert!(cache.get(key(0), Tag::Store).is_some());
        assert_eq!(cache.bytes(), 30);
        cache.insert(key(4), frame(31), Tag::Store);
        assert!(
            cache.get(key(4), Tag::Store).is_none(),
            "larger than the cache"
        );
        assert_eq!(cache.bytes(), 30);
    }

    /// A frame answers only under the tag it was kept with: a head frame from
    /// another epoch, or a store lookup of a head frame, is a miss.
    #[test]
    fn a_frame_answers_only_under_its_tag() {
        let cache = FrameCache::new(100);
        cache.insert(key(1), frame(5), Tag::Head(epoch(1)));
        assert!(cache.get(key(1), Tag::Head(epoch(1))).is_some());
        assert!(cache.get(key(1), Tag::Head(epoch(2))).is_none());
        assert!(cache.get(key(1), Tag::Store).is_none());
        cache.insert(key(1), frame(7), Tag::Store);
        assert_eq!(cache.bytes(), 7, "replaced, not added");
    }

    /// A cache of no bytes keeps nothing.
    #[test]
    fn a_zero_capacity_cache_keeps_nothing() {
        let cache = FrameCache::new(0);
        cache.insert(key(1), frame(1), Tag::Store);
        assert!(cache.get(key(1), Tag::Store).is_none());
    }
}
