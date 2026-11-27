//! A byte-bounded queue from the pipeline to one index writer: a send waits
//! for room, and a writer that stops closes it, so a waiting send returns.

use std::num::NonZeroUsize;
use std::sync::Arc;

use tokio::sync::{mpsc, Semaphore};
use zaino_indexes::FoldBlock;

/// One block and the share of the budget it holds until processed.
pub(super) struct Item {
    pub(super) block: Arc<FoldBlock>,
    permits: u32,
}

/// The writer stopped.
#[derive(Debug)]
pub(super) struct Closed;

pub(super) struct Sender {
    room: Arc<Semaphore>,
    budget: u32,
    items: mpsc::UnboundedSender<Item>,
}

pub(super) struct Receiver {
    room: Arc<Semaphore>,
    items: mpsc::UnboundedReceiver<Item>,
}

/// A queue holding at most `budget` bytes (a block larger than the budget
/// waits for an empty queue).
pub(super) fn queue(budget: NonZeroUsize) -> (Sender, Receiver) {
    let budget = u32::try_from(budget.get().min(Semaphore::MAX_PERMITS)).unwrap_or(u32::MAX);
    let room = Arc::new(Semaphore::new(budget as usize));
    let (items, received) = mpsc::unbounded_channel();
    (
        Sender {
            room: Arc::clone(&room),
            budget,
            items,
        },
        Receiver {
            room,
            items: received,
        },
    )
}

impl Sender {
    /// `block`, weighing `bytes`, once the queue has room.
    pub(super) async fn send(&self, block: Arc<FoldBlock>, bytes: usize) -> Result<(), Closed> {
        let permits = u32::try_from(bytes)
            .unwrap_or(u32::MAX)
            .clamp(1, self.budget);
        self.room
            .acquire_many(permits)
            .await
            .map_err(|_| Closed)?
            .forget();
        self.items.send(Item { block, permits }).map_err(|_| Closed)
    }
}

impl Receiver {
    /// The next block; `None` once every sender is gone.
    pub(super) fn next(&mut self) -> Option<Item> {
        self.items.blocking_recv()
    }

    /// `item` processed: its room back.
    pub(super) fn done(&self, item: Item) {
        self.room.add_permits(item.permits as usize);
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.room.close();
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use zaino_indexes::FoldBlock;
    use zaino_primitives::types::Height;
    use zaino_source_mock::ChainBuilder;

    use super::queue;
    use crate::assemble::assemble;

    /// A send waits while the queue is full and goes once room is freed; a
    /// dropped writer releases a waiting send.
    #[tokio::test]
    async fn a_full_queue_holds_the_sender_until_room_or_close() {
        let chain = ChainBuilder::new().blocks(1).build();
        let parts = assemble(&chain, &[], Height::GENESIS, false)
            .await
            .expect("assembled");
        let block = Arc::new(FoldBlock::try_new(parts).expect("a block"));
        let (sender, mut receiver) = queue(NonZeroUsize::new(100).expect("non-zero"));
        sender.send(Arc::clone(&block), 60).await.expect("room");
        let mut second = Box::pin(sender.send(Arc::clone(&block), 60));
        assert!(futures::poll!(second.as_mut()).is_pending(), "full");
        let first = tokio::task::spawn_blocking(move || {
            let item = receiver.next().expect("an item");
            receiver.done(item);
            receiver
        })
        .await
        .expect("joined");
        second.await.expect("room freed");
        drop(first);
        assert!(sender.send(block, 10).await.is_err(), "the writer is gone");
    }
}
