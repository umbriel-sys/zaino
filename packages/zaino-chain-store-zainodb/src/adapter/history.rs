//! Transparent address history.
//!
//! Feature-gated and experimental. zainodb stores script hashes, never
//! scripts, so it cannot report the outputs an address received; it answers
//! `Unsupported`, as it does `outputs_by_outpoint` (deleted in Stage 6).

use zaino_chain_store::{ChainStoreError, ChainStoreSource};

use crate::store::reader::DbReader;

impl<T: ChainStoreSource> zaino_chain_store::TransparentHistoryIndex for DbReader<T> {
    async fn address_effects(
        &self,
        _query: &zaino_chain_store::TransparentHistoryQuery,
    ) -> Result<zaino_chain_store::StoreAddressEffects, ChainStoreError> {
        Err(ChainStoreError::Unsupported(
            "zainodb stores script hashes, not scripts",
        ))
    }
}
