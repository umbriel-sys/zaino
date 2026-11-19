//! A transaction as a verbose `getblock` (verbosity 2 and 3) answers it.
//!
//! Byte fields hold the bytes exactly as the validator serializes them;
//! hashes use their domain types.

use crate::types::{
    BlockConfirmations, BlockHash, Height, SignedZatoshis, TransactionId, TransparentAddress,
    Zatoshis,
};

use super::ScriptPubKey;

/// A transaction object from a verbose `getblock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionVerbose {
    /// Whether its block is on the best chain.
    pub in_active_chain: Option<bool>,
    /// The serialized transaction.
    pub hex: Vec<u8>,
    /// The height of its block.
    pub height: Option<Height>,
    /// Its block's confirmations.
    pub confirmations: Option<BlockConfirmations>,
    /// The fee (verbosity 3, when every prevout is known).
    pub fee: Option<Zatoshis>,
    /// Transparent inputs.
    pub inputs: Vec<InputVerbose>,
    /// Transparent outputs.
    pub outputs: Vec<OutputVerbose>,
    /// Sapling spends.
    pub sapling_spends: Vec<SaplingSpendVerbose>,
    /// Sapling outputs.
    pub sapling_outputs: Vec<SaplingOutputVerbose>,
    /// Sprout JoinSplits.
    pub joinsplits: Vec<JoinSplitVerbose>,
    /// The Sapling binding signature.
    pub binding_sig: Option<[u8; 64]>,
    /// The JoinSplit public key.
    pub joinsplit_pub_key: Option<[u8; 32]>,
    /// The JoinSplit signature.
    pub joinsplit_sig: Option<[u8; 64]>,
    /// The Orchard bundle.
    pub orchard: Option<OrchardVerbose>,
    /// The Ironwood bundle.
    pub ironwood: Option<OrchardVerbose>,
    /// The Sapling value balance.
    pub value_balance: Option<SignedZatoshis>,
    /// Serialized size in bytes.
    pub size: Option<u64>,
    /// Its block's time.
    pub time: Option<i64>,
    /// The transaction id.
    pub txid: TransactionId,
    /// The authorizing data digest (v5 and later).
    pub auth_digest: Option<[u8; 32]>,
    /// The overwintered flag.
    pub overwintered: bool,
    /// The version.
    pub version: u32,
    /// The version group id.
    pub version_group_id: Option<Vec<u8>>,
    /// The lock time.
    pub lock_time: u32,
    /// The expiry height.
    pub expiry_height: Option<Height>,
    /// Its block's hash.
    pub block_hash: Option<BlockHash>,
    /// Its block's time.
    pub block_time: Option<i64>,
}

/// A transparent input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputVerbose {
    /// The coinbase input.
    Coinbase {
        /// The coinbase script.
        coinbase: Vec<u8>,
        /// The sequence number.
        sequence: u32,
    },
    /// An input spending a previous output.
    Spend {
        /// The spent output's transaction.
        txid: TransactionId,
        /// The spent output's index.
        vout: u32,
        /// The signature script's disassembly.
        script_sig_asm: String,
        /// The signature script.
        script_sig: Vec<u8>,
        /// The sequence number.
        sequence: u32,
        /// The spent value, when the validator reports it.
        value: Option<Zatoshis>,
        /// The spent output's address, when the validator reports it.
        address: Option<TransparentAddress>,
        /// The spent output (verbosity 3).
        prevout: Option<Box<PrevoutVerbose>>,
    },
}

/// The output an input spends (verbosity 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevoutVerbose {
    /// Whether a coinbase transaction created it.
    pub generated: bool,
    /// The height of the block that created it.
    pub height: Height,
    /// Its value.
    pub value: Zatoshis,
    /// Its locking script.
    pub script_pub_key: ScriptPubKey,
}

/// A transparent output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputVerbose {
    /// Its value.
    pub value: Zatoshis,
    /// Its index.
    pub n: u32,
    /// Its locking script.
    pub script_pub_key: ScriptPubKey,
}

/// A Sapling spend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaplingSpendVerbose {
    /// The value commitment.
    pub cv: [u8; 32],
    /// The anchor.
    pub anchor: [u8; 32],
    /// The nullifier.
    pub nullifier: [u8; 32],
    /// The randomized key.
    pub rk: [u8; 32],
    /// The proof.
    pub proof: Vec<u8>,
    /// The spend authorization signature.
    pub spend_auth_sig: Vec<u8>,
}

/// A Sapling output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaplingOutputVerbose {
    /// The value commitment.
    pub cv: [u8; 32],
    /// The note commitment's u-coordinate.
    pub cmu: [u8; 32],
    /// The ephemeral key.
    pub ephemeral_key: [u8; 32],
    /// The note ciphertext.
    pub enc_ciphertext: Vec<u8>,
    /// The outgoing ciphertext.
    pub out_ciphertext: Vec<u8>,
    /// The proof.
    pub proof: Vec<u8>,
}

/// A Sprout JoinSplit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinSplitVerbose {
    /// Value leaving the transparent pool.
    pub vpub_old: Zatoshis,
    /// Value entering the transparent pool.
    pub vpub_new: Zatoshis,
    /// The anchor.
    pub anchor: [u8; 32],
    /// The input nullifiers.
    pub nullifiers: Vec<[u8; 32]>,
    /// The output commitments.
    pub commitments: Vec<[u8; 32]>,
    /// The one-time public key.
    pub one_time_pub_key: [u8; 32],
    /// The random seed.
    pub random_seed: [u8; 32],
    /// The input MACs.
    pub macs: Vec<[u8; 32]>,
    /// The proof.
    pub proof: Vec<u8>,
    /// The output ciphertexts.
    pub ciphertexts: Vec<Vec<u8>>,
}

/// An Orchard-shaped bundle (Orchard or Ironwood).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchardVerbose {
    /// The actions.
    pub actions: Vec<OrchardActionVerbose>,
    /// The bundle's value balance.
    pub value_balance: SignedZatoshis,
    /// The flags, when the bundle is present.
    pub flags: Option<OrchardFlagsVerbose>,
    /// The anchor.
    pub anchor: Option<[u8; 32]>,
    /// The aggregated proof.
    pub proof: Option<Vec<u8>>,
    /// The binding signature.
    pub binding_sig: Option<[u8; 64]>,
}

/// An Orchard bundle's flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrchardFlagsVerbose {
    /// Outputs enabled.
    pub enable_outputs: bool,
    /// Spends enabled.
    pub enable_spends: bool,
}

/// An Orchard action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchardActionVerbose {
    /// The value commitment.
    pub cv: [u8; 32],
    /// The nullifier.
    pub nullifier: [u8; 32],
    /// The randomized key.
    pub rk: [u8; 32],
    /// The note commitment's x-coordinate.
    pub cmx: [u8; 32],
    /// The ephemeral key.
    pub ephemeral_key: [u8; 32],
    /// The note ciphertext.
    pub enc_ciphertext: Vec<u8>,
    /// The spend authorization signature.
    pub spend_auth_sig: Vec<u8>,
    /// The outgoing ciphertext.
    pub out_ciphertext: Vec<u8>,
}
