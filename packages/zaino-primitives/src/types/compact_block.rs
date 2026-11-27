//! Pre-index and serving compact block types.
//!
//! A [`PreIndexCompactBlock`] is a block's own data with proofs/sigs
//! stripped. It does NOT contain indexed state like commitment tree sizes.
//!
//! A [`CompactBlock`] is the serving-ready format (proto-adherent) that includes
//! [`ChainMetadata`]: read from the store, or from the validator, which
//! answers a block and its tree sizes together.
//!
//! Transaction sub-types (`TransparentInput`, `SaplingOutput`, etc.) are
//! reused from [`super::transaction`] — no duplication. An action is the one
//! exception: a compact read may take its nullifier alone (the
//! nullifiers-only [`PoolFilter`](super::PoolFilter)), so its compact form,
//! [`CompactAction`], carries the output part as an `Option`.

use super::{
    BlockHash, BlockTime, ChainMetadata, CompactCiphertext, CompactDifficulty, EphemeralKey,
    NoteCommitment, Nullifier, OrchardAction, SaplingOutput, TransactionId, TransparentInput,
    TransparentOutput, Zatoshis,
};

/// An Orchard-shaped action (Orchard or Ironwood) as a compact transaction
/// carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactAction {
    /// The note the action spends.
    pub nullifier: Nullifier,
    /// The note the action creates; `None` where the read took nullifiers only.
    pub output: Option<ActionOutput>,
}

/// The output half of an action: what a wallet trial-decrypts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOutput {
    /// The note commitment.
    pub cmx: NoteCommitment,
    /// The ephemeral public key.
    pub ephemeral_key: EphemeralKey,
    /// The first 52 bytes of the note ciphertext.
    pub enc_ciphertext: CompactCiphertext,
}

impl From<&OrchardAction> for CompactAction {
    fn from(action: &OrchardAction) -> Self {
        Self {
            nullifier: action.nullifier,
            output: Some(ActionOutput {
                cmx: action.cmx,
                ephemeral_key: action.ephemeral_key,
                enc_ciphertext: action.enc_ciphertext,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Pre-index compact block — source adapter output
// ---------------------------------------------------------------------------

/// A block's own compact data, without cumulative indexed state (tree
/// sizes).
///
/// Becomes a [`CompactBlock`] once its [`ChainMetadata`] is known
/// ([`into_compact`](Self::into_compact)).
#[derive(Debug, Clone)]
pub struct PreIndexCompactBlock {
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Block height (raw u32).
    pub height: u32,
    /// Block timestamp.
    pub time: BlockTime,
    /// Compact difficulty target (nBits).
    pub bits: CompactDifficulty,
    /// Compact transactions.
    pub transactions: Vec<PreIndexCompactTx>,
}

/// A compact transaction in the pre-index representation.
///
/// Reuses existing primitives types for sub-components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreIndexCompactTx {
    /// Transaction hash.
    pub txid: TransactionId,
    /// Transparent inputs — outpoint only (no script/sequence).
    pub transparent_inputs: Vec<TransparentInput>,
    /// Transparent outputs — value + lock script.
    pub transparent_outputs: Vec<TransparentOutput>,
    /// Sapling nullifiers (one per spend).
    pub sapling_nullifiers: Vec<Nullifier>,
    /// Sapling outputs — cmu + epk + enc_ciphertext head.
    pub sapling_outputs: Vec<SaplingOutput>,
    /// Orchard actions — nullifier, and cmx + epk + enc_ciphertext head.
    pub orchard_actions: Vec<CompactAction>,
    /// Ironwood actions (NU6.3) — same shape as Orchard, separate pool.
    pub ironwood_actions: Vec<CompactAction>,
    /// The fee: zero for the coinbase; `None` where the provider cannot know
    /// it (a block parsed without the outputs its inputs spend).
    pub fee: Option<Zatoshis>,
    /// Sapling outputs a nullifiers-only read of an included pool left out;
    /// zero when outputs are read.
    pub sapling_outputs_unread: u16,
}

impl PreIndexCompactTx {
    /// With the pools `pools` leaves out cleared; under a nullifiers-only
    /// filter the shielded outputs too, their count kept.
    pub fn filtered(mut self, pools: super::PoolFilter) -> Self {
        use super::ShieldedPool;
        if !pools.includes_transparent() {
            self.transparent_inputs.clear();
            self.transparent_outputs.clear();
        }
        if !pools.includes(ShieldedPool::Sapling) {
            self.sapling_nullifiers.clear();
            self.sapling_outputs.clear();
        }
        if !pools.includes(ShieldedPool::Orchard) {
            self.orchard_actions.clear();
        }
        if !pools.includes(ShieldedPool::Ironwood) {
            self.ironwood_actions.clear();
        }
        if !pools.includes_outputs() {
            // a transaction's outputs fit its size cap many times over in a u16
            self.sapling_outputs_unread =
                u16::try_from(self.sapling_outputs.len()).unwrap_or(u16::MAX);
            self.sapling_outputs.clear();
            for action in self
                .orchard_actions
                .iter_mut()
                .chain(self.ironwood_actions.iter_mut())
            {
                action.output = None;
            }
        }
        self
    }

    /// Whether anything is left in a pool the transaction was read for,
    /// outputs a nullifiers-only read left out included (lightwalletd
    /// serves such a transaction empty, and drops one with nothing).
    pub fn has_pool_data(&self) -> bool {
        self.sapling_outputs_unread > 0
            || !self.transparent_inputs.is_empty()
            || !self.transparent_outputs.is_empty()
            || !self.sapling_nullifiers.is_empty()
            || !self.sapling_outputs.is_empty()
            || !self.orchard_actions.is_empty()
            || !self.ironwood_actions.is_empty()
    }
}

impl PreIndexCompactBlock {
    /// The complete compact block: this one with its tree sizes, and each
    /// transaction with the pools `pools` leaves out cleared. Every
    /// transaction is kept, since position is the index.
    pub fn into_compact(
        self,
        chain_metadata: ChainMetadata,
        pools: super::PoolFilter,
    ) -> CompactBlock {
        CompactBlock {
            hash: self.hash,
            prev_hash: self.prev_hash,
            height: self.height,
            time: self.time,
            bits: self.bits,
            transactions: self
                .transactions
                .into_iter()
                .map(|tx| tx.filtered(pools))
                .collect(),
            chain_metadata,
        }
    }
}

impl From<&super::Block> for PreIndexCompactBlock {
    fn from(block: &super::Block) -> Self {
        Self {
            hash: block.header.hash,
            prev_hash: block.header.prev_hash,
            height: u32::from(block.header.height),
            time: block.header.time,
            bits: block.header.bits,
            transactions: block
                .transactions()
                .iter()
                .map(PreIndexCompactTx::from)
                .collect(),
        }
    }
}

impl From<&super::transaction::Transaction> for PreIndexCompactTx {
    fn from(tx: &super::transaction::Transaction) -> Self {
        Self {
            txid: tx.txid,
            transparent_inputs: tx.transparent.inputs.clone(),
            transparent_outputs: tx.transparent.outputs.clone(),
            sapling_nullifiers: tx.sapling.spends.iter().map(|s| s.nullifier).collect(),
            sapling_outputs: tx.sapling.outputs.clone(),
            orchard_actions: tx.orchard.actions.iter().map(CompactAction::from).collect(),
            ironwood_actions: tx
                .ironwood
                .actions
                .iter()
                .map(CompactAction::from)
                .collect(),
            // a parsed transaction does not hold the outputs its inputs spend
            fee: None,
            sapling_outputs_unread: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// CompactBlock — serving format (proto-adherent, post-indexing)
// ---------------------------------------------------------------------------

/// A complete compact block ready for serving to light wallets.
///
/// This is the proto-adherent `CompactBlock` format. It includes
/// [`ChainMetadata`] (commitment tree sizes) which is cumulative indexed
/// state — not available from the raw block data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactBlock {
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Block height (raw u32).
    pub height: u32,
    /// Block timestamp.
    pub time: BlockTime,
    /// Compact difficulty target (nBits).
    pub bits: CompactDifficulty,
    /// Compact transactions (same shape as pre-index).
    pub transactions: Vec<PreIndexCompactTx>,
    /// Indexed chain metadata — commitment tree sizes at this block.
    pub chain_metadata: ChainMetadata,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::transaction::{OrchardData, Transaction};

    fn action(tag: u8) -> OrchardAction {
        OrchardAction {
            nullifier: [tag; 32].into(),
            cmx: [tag; 32].into(),
            ephemeral_key: [tag; 32].into(),
            enc_ciphertext: CompactCiphertext::from([tag; 52]),
        }
    }

    /// Ironwood and Orchard share an action shape, so a conversion that merged
    /// or mis-routed them would still typecheck. Pin the separation.
    #[test]
    fn compact_conversion_keeps_ironwood_and_orchard_apart() {
        let tx = Transaction {
            txid: [0; 32].into(),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: OrchardData {
                actions: vec![action(1)],
                ..Default::default()
            },
            ironwood: OrchardData {
                actions: vec![action(2), action(3)],
                ..Default::default()
            },
        };

        let compact = PreIndexCompactTx::from(&tx);

        let compact_action = |tag| CompactAction::from(&action(tag));
        assert_eq!(compact.orchard_actions, vec![compact_action(1)]);
        assert_eq!(
            compact.ironwood_actions,
            vec![compact_action(2), compact_action(3)]
        );
    }

    /// A parsed action's compact form keeps every byte, output included.
    #[test]
    fn a_parsed_action_keeps_its_output() {
        let compact = CompactAction::from(&action(7));
        assert_eq!(compact.nullifier, action(7).nullifier);
        assert_eq!(
            compact.output,
            Some(ActionOutput {
                cmx: action(7).cmx,
                ephemeral_key: action(7).ephemeral_key,
                enc_ciphertext: action(7).enc_ciphertext,
            })
        );
    }
}
