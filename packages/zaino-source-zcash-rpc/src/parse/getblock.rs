//! `getblock` at verbosity 1, 2 and 3, as zebrad's `BlockObject` and
//! `TransactionObject` serialize it. Opaque byte fields are kept exactly as
//! serialized; hashes and ids use their domain types.

use zaino_primitives::types::{
    rpc::{
        InputVerbose, JoinSplitVerbose, OrchardActionVerbose, OrchardFlagsVerbose, OrchardVerbose,
        OutputVerbose, PrevoutVerbose, SaplingOutputVerbose, SaplingSpendVerbose, ScriptPubKey,
        TransactionVerbose,
    },
    BlockConfirmations, BlockTransactions, BlockTreeSizes, BlockVerbose, Height, Script,
    SignedZatoshis, TransparentAddress, Zatoshis,
};

use super::{
    as_array, as_block_confirmations, as_bool, as_f64, as_height, as_i64, as_merkle_root, as_nonce,
    as_str, as_tree_root, as_txid, as_u32, as_u64, field, header_commitments, opt_chainwork,
    opt_field, parse_block_hash, parse_compact_difficulty, parse_optional_list, parse_value_pool,
    pool_tree_size, zatoshis_field, zec_to_zatoshis, ParseError,
};

/// Parse a verbose `getblock` response.
pub(crate) fn parse_block_verbose(value: &serde_json::Value) -> Result<BlockVerbose, ParseError> {
    let trees = opt_field(value, "trees");
    let (block_commitments, final_sapling_root) = header_commitments(value)?;
    Ok(BlockVerbose {
        hash: parse_block_hash(field(value, "hash")?)?,
        confirmations: as_block_confirmations(field(value, "confirmations")?)?,
        size: opt_field(value, "size").map(as_u64).transpose()?,
        height: as_height(field(value, "height")?)?,
        version: as_u32(field(value, "version")?)?,
        merkle_root: as_merkle_root(field(value, "merkleroot")?)?,
        block_commitments,
        final_sapling_root,
        final_orchard_root: opt_field(value, "finalorchardroot")
            .map(as_tree_root)
            .transpose()?,
        transactions: parse_block_transactions(field(value, "tx")?)?,
        time: as_u32(field(value, "time")?)?,
        nonce: as_nonce(field(value, "nonce")?)?,
        solution: bytes(field(value, "solution")?)?,
        bits: parse_compact_difficulty(field(value, "bits")?)?,
        difficulty: as_f64(field(value, "difficulty")?)?,
        chainwork: opt_chainwork(value)?,
        chain_supply: opt_field(value, "chainSupply")
            .map(parse_value_pool)
            .transpose()?,
        value_pools: parse_optional_list(value, "valuePools", parse_value_pool)?,
        tree_sizes: BlockTreeSizes {
            sapling: pool_tree_size(trees, "sapling")?,
            orchard: pool_tree_size(trees, "orchard")?,
            ironwood: pool_tree_size(trees, "ironwood")?,
        },
        previous_block_hash: opt_field(value, "previousblockhash")
            .map(parse_block_hash)
            .transpose()?,
        next_block_hash: opt_field(value, "nextblockhash")
            .map(parse_block_hash)
            .transpose()?,
    })
}

/// Transaction ids (verbosity 1) or objects (2, 3); never a mix.
fn parse_block_transactions(value: &serde_json::Value) -> Result<BlockTransactions, ParseError> {
    let list = as_array(value)?;
    if list.iter().all(serde_json::Value::is_string) {
        return list
            .iter()
            .map(as_txid)
            .collect::<Result<_, _>>()
            .map(BlockTransactions::Ids);
    }
    list.iter()
        .map(parse_transaction_verbose)
        .collect::<Result<_, _>>()
        .map(BlockTransactions::Objects)
}

/// A transaction's `height` and `confirmations`: its best-chain block's, or
/// neither in the mempool or a side-chain block, which is written `-1` and
/// `0` beside the block's hash.
fn parse_placement(
    value: &serde_json::Value,
) -> Result<(Option<Height>, Option<BlockConfirmations>), ParseError> {
    let height = opt_field(value, "height");
    let confirmations = opt_field(value, "confirmations");
    if height.map(as_i64).transpose()? != Some(-1) {
        return Ok((
            height.map(as_height).transpose()?,
            confirmations.map(as_block_confirmations).transpose()?,
        ));
    }
    let unconfirmed = confirmations.map(as_i64).transpose()? == Some(0);
    match unconfirmed && opt_field(value, "blockhash").is_some() {
        true => Ok((None, None)),
        false => Err(ParseError::Height(
            "a side-chain transaction (height -1) has 0 confirmations and names its block"
                .to_owned(),
        )),
    }
}

/// Parse a transaction object of a verbose `getblock` or `getrawtransaction`.
pub fn parse_transaction_verbose(
    value: &serde_json::Value,
) -> Result<TransactionVerbose, ParseError> {
    let (height, confirmations) = parse_placement(value)?;
    Ok(TransactionVerbose {
        in_active_chain: opt_field(value, "in_active_chain")
            .map(as_bool)
            .transpose()?,
        hex: bytes(field(value, "hex")?)?,
        height,
        confirmations,
        fee: opt_field(value, "fee")
            .map(|fee| zec_to_zatoshis(as_f64(fee)?))
            .transpose()?,
        inputs: parse_optional_list(value, "vin", parse_input)?,
        outputs: parse_optional_list(value, "vout", parse_output)?,
        sapling_spends: parse_optional_list(value, "vShieldedSpend", parse_sapling_spend)?,
        sapling_outputs: parse_optional_list(value, "vShieldedOutput", parse_sapling_output)?,
        joinsplits: parse_optional_list(value, "vjoinsplit", parse_joinsplit)?,
        binding_sig: opt_field(value, "bindingSig").map(fixed).transpose()?,
        joinsplit_pub_key: opt_field(value, "joinSplitPubKey").map(fixed).transpose()?,
        joinsplit_sig: opt_field(value, "joinSplitSig").map(fixed).transpose()?,
        orchard: opt_field(value, "orchard").map(parse_orchard).transpose()?,
        ironwood: opt_field(value, "ironwood")
            .map(parse_orchard)
            .transpose()?,
        value_balance: opt_field(value, "valueBalanceZat")
            .map(signed_zatoshis)
            .transpose()?,
        size: opt_field(value, "size").map(as_u64).transpose()?,
        time: opt_field(value, "time").map(as_i64).transpose()?,
        txid: as_txid(field(value, "txid")?)?,
        auth_digest: opt_field(value, "authdigest").map(fixed).transpose()?,
        overwintered: as_bool(field(value, "overwintered")?)?,
        version: as_u32(field(value, "version")?)?,
        version_group_id: opt_field(value, "versiongroupid").map(bytes).transpose()?,
        lock_time: as_u32(field(value, "locktime")?)?,
        expiry_height: opt_field(value, "expiryheight")
            .map(as_height)
            .transpose()?,
        block_hash: opt_field(value, "blockhash")
            .map(parse_block_hash)
            .transpose()?,
        block_time: opt_field(value, "blocktime").map(as_i64).transpose()?,
    })
}

fn parse_input(value: &serde_json::Value) -> Result<InputVerbose, ParseError> {
    if let Some(coinbase) = opt_field(value, "coinbase") {
        return Ok(InputVerbose::Coinbase {
            coinbase: bytes(coinbase)?,
            sequence: as_u32(field(value, "sequence")?)?,
        });
    }
    let script_sig = field(value, "scriptSig")?;
    Ok(InputVerbose::Spend {
        txid: as_txid(field(value, "txid")?)?,
        vout: as_u32(field(value, "vout")?)?,
        script_sig_asm: as_str(field(script_sig, "asm")?)?.to_owned(),
        script_sig: bytes(field(script_sig, "hex")?)?,
        sequence: as_u32(field(value, "sequence")?)?,
        value: match opt_field(value, "valueSat") {
            Some(zats) => Some(zatoshis(zats)?),
            None => opt_field(value, "value")
                .map(|zec| zec_to_zatoshis(as_f64(zec)?))
                .transpose()?,
        },
        address: opt_field(value, "address").map(address).transpose()?,
        prevout: opt_field(value, "prevout")
            .map(|prevout| parse_prevout(prevout).map(Box::new))
            .transpose()?,
    })
}

fn parse_prevout(value: &serde_json::Value) -> Result<PrevoutVerbose, ParseError> {
    let height = as_u32(field(value, "height")?)?;
    Ok(PrevoutVerbose {
        generated: as_bool(field(value, "generated")?)?,
        height: Height::try_from(height).map_err(|e| ParseError::Height(e.to_string()))?,
        value: zec_to_zatoshis(as_f64(field(value, "value")?)?)?,
        script_pub_key: parse_script_pub_key(field(value, "scriptPubKey")?)?,
    })
}

fn parse_output(value: &serde_json::Value) -> Result<OutputVerbose, ParseError> {
    Ok(OutputVerbose {
        value: zatoshis_field(value, "valueZat", "value")?,
        n: as_u32(field(value, "n")?)?,
        script_pub_key: parse_script_pub_key(field(value, "scriptPubKey")?)?,
    })
}

/// A `scriptPubKey` object, as `gettxout` and `getblock` write it.
pub(crate) fn parse_script_pub_key(script: &serde_json::Value) -> Result<ScriptPubKey, ParseError> {
    Ok(ScriptPubKey {
        script: Script::new(bytes(field(script, "hex")?)?),
        asm: opt_field(script, "asm")
            .map(|v| as_str(v).map(str::to_owned))
            .transpose()?,
        script_type: opt_field(script, "type")
            .map(|v| as_str(v).map(str::to_owned))
            .transpose()?,
        required_signatures: opt_field(script, "reqSigs").map(as_u32).transpose()?,
        addresses: parse_optional_list(script, "addresses", address)?,
    })
}

fn parse_sapling_spend(value: &serde_json::Value) -> Result<SaplingSpendVerbose, ParseError> {
    Ok(SaplingSpendVerbose {
        cv: fixed(field(value, "cv")?)?,
        anchor: fixed(field(value, "anchor")?)?,
        nullifier: fixed(field(value, "nullifier")?)?,
        rk: fixed(field(value, "rk")?)?,
        proof: bytes(field(value, "proof")?)?,
        spend_auth_sig: bytes(field(value, "spendAuthSig")?)?,
    })
}

fn parse_sapling_output(value: &serde_json::Value) -> Result<SaplingOutputVerbose, ParseError> {
    Ok(SaplingOutputVerbose {
        cv: fixed(field(value, "cv")?)?,
        cmu: fixed(field(value, "cmu")?)?,
        ephemeral_key: fixed(field(value, "ephemeralKey")?)?,
        enc_ciphertext: bytes(field(value, "encCiphertext")?)?,
        out_ciphertext: bytes(field(value, "outCiphertext")?)?,
        proof: bytes(field(value, "proof")?)?,
    })
}

fn parse_joinsplit(value: &serde_json::Value) -> Result<JoinSplitVerbose, ParseError> {
    Ok(JoinSplitVerbose {
        vpub_old: zatoshis_field(value, "vpub_oldZat", "vpub_old")?,
        vpub_new: zatoshis_field(value, "vpub_newZat", "vpub_new")?,
        anchor: fixed(field(value, "anchor")?)?,
        nullifiers: parse_optional_list(value, "nullifiers", fixed)?,
        commitments: parse_optional_list(value, "commitments", fixed)?,
        one_time_pub_key: fixed(field(value, "onetimePubKey")?)?,
        random_seed: fixed(field(value, "randomSeed")?)?,
        macs: parse_optional_list(value, "macs", fixed)?,
        proof: bytes(field(value, "proof")?)?,
        ciphertexts: parse_optional_list(value, "ciphertexts", bytes)?,
    })
}

fn parse_orchard(value: &serde_json::Value) -> Result<OrchardVerbose, ParseError> {
    Ok(OrchardVerbose {
        actions: parse_optional_list(value, "actions", parse_orchard_action)?,
        value_balance: signed_zatoshis(field(value, "valueBalanceZat")?)?,
        flags: opt_field(value, "flags")
            .map(|flags| {
                Ok::<_, ParseError>(OrchardFlagsVerbose {
                    enable_outputs: as_bool(field(flags, "enableOutputs")?)?,
                    enable_spends: as_bool(field(flags, "enableSpends")?)?,
                })
            })
            .transpose()?,
        anchor: opt_field(value, "anchor").map(fixed).transpose()?,
        proof: opt_field(value, "proof").map(bytes).transpose()?,
        binding_sig: opt_field(value, "bindingSig").map(fixed).transpose()?,
    })
}

fn parse_orchard_action(value: &serde_json::Value) -> Result<OrchardActionVerbose, ParseError> {
    Ok(OrchardActionVerbose {
        cv: fixed(field(value, "cv")?)?,
        nullifier: fixed(field(value, "nullifier")?)?,
        rk: fixed(field(value, "rk")?)?,
        cmx: fixed(field(value, "cmx")?)?,
        ephemeral_key: fixed(field(value, "ephemeralKey")?)?,
        enc_ciphertext: bytes(field(value, "encCiphertext")?)?,
        spend_auth_sig: bytes(field(value, "spendAuthSig")?)?,
        out_ciphertext: bytes(field(value, "outCiphertext")?)?,
    })
}

/// Hex bytes, as serialized.
fn bytes(value: &serde_json::Value) -> Result<Vec<u8>, ParseError> {
    hex::decode(as_str(value)?).map_err(|e| ParseError::Hex(e.to_string()))
}

/// `N` hex bytes, as serialized.
fn fixed<const N: usize>(value: &serde_json::Value) -> Result<[u8; N], ParseError> {
    bytes(value)?
        .try_into()
        .map_err(|b: Vec<u8>| ParseError::WrongLength {
            expected: N,
            got: b.len(),
        })
}

fn zatoshis(value: &serde_json::Value) -> Result<Zatoshis, ParseError> {
    Zatoshis::new(as_u64(value)?).map_err(|e| ParseError::Amount(e.to_string()))
}

fn signed_zatoshis(value: &serde_json::Value) -> Result<SignedZatoshis, ParseError> {
    SignedZatoshis::try_new(as_i64(value)?).map_err(|e| ParseError::Amount(e.to_string()))
}

fn address(value: &serde_json::Value) -> Result<TransparentAddress, ParseError> {
    Ok(TransparentAddress::try_new(as_str(value)?)?)
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use zaino_primitives::types::TransactionLocation;

    use super::*;

    const BLOCK: &str = "00000000000000000000000000000000000000000000000000000000000000aa";

    /// A transaction object with `placement`'s keys beside the required ones.
    fn object(placement: Value) -> Value {
        let mut object = json!({
            "hex": "00",
            "txid": "11".repeat(32),
            "overwintered": false,
            "version": 1,
            "locktime": 0,
        });
        for (key, value) in placement.as_object().expect("an object") {
            object[key] = value.clone();
        }
        object
    }

    /// The best chain carries its height and confirmations; a side-chain
    /// block (`-1`, `0`, its hash) and the mempool carry neither.
    #[test]
    fn every_placement_is_read() {
        let best = parse_transaction_verbose(&object(json!({
            "height": 12, "confirmations": 3, "blockhash": BLOCK, "in_active_chain": true,
        })))
        .expect("best chain");
        let height = Height::try_from(12).expect("small");
        assert_eq!(best.location(), TransactionLocation::BestChain(height));
        assert_eq!(
            best.confirmations.map(BlockConfirmations::to_rpc_i64),
            Some(3)
        );

        let side = parse_transaction_verbose(&object(json!({
            "height": -1, "confirmations": 0, "blockhash": BLOCK, "in_active_chain": false,
        })))
        .expect("side chain");
        assert_eq!(side.location(), TransactionLocation::NonBestChain);
        assert_eq!((side.height, side.confirmations), (None, None));
        assert_eq!(side.in_active_chain, Some(false));

        let mempool = parse_transaction_verbose(&object(json!({ "in_active_chain": false })))
            .expect("mempool");
        assert_eq!(mempool.location(), TransactionLocation::Mempool);
    }

    /// `-1` and `0` are the side-chain pair and nothing else.
    #[test]
    fn a_placement_that_encodes_no_state_is_refused() {
        for placement in [
            json!({ "height": -1, "confirmations": 0 }),
            json!({ "height": -1, "confirmations": 2, "blockhash": BLOCK }),
            json!({ "height": -1, "blockhash": BLOCK }),
            json!({ "height": -7, "confirmations": 0, "blockhash": BLOCK }),
            json!({ "height": 12, "confirmations": 0, "blockhash": BLOCK }),
        ] {
            assert!(
                parse_transaction_verbose(&object(placement.clone())).is_err(),
                "{placement}"
            );
        }
    }
}
