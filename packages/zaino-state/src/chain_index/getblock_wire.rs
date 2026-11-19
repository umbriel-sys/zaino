//! The validator's `getblock` answer in zebra-rpc's shape: rendered with
//! zebrad's field names and encodings, then read by zebra-rpc's own serde.

use serde_json::{json, Map, Value};
use zaino_primitives::types::{
    rpc::{
        InputVerbose, JoinSplitVerbose, OrchardVerbose, OutputVerbose, PrevoutVerbose,
        SaplingOutputVerbose, SaplingSpendVerbose, ScriptPubKey, TransactionVerbose,
    },
    BlockTransactions, BlockVerbose, SignedZatoshis, ValuePoolBalance, Zatoshis,
};
use zebra_rpc::methods::GetBlock;

/// Why a verbose block has no zebra-rpc form.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GetBlockWireError {
    /// A field zebra-rpc requires is absent from the validator's answer.
    #[error("the validator's getblock omits {0}")]
    Missing(&'static str),
    /// zebra-rpc refused the rendered answer.
    #[error("zebra-rpc refuses the getblock answer: {0}")]
    Refused(#[from] serde_json::Error),
}

type Result<T> = core::result::Result<T, GetBlockWireError>;

/// The validator's verbose `getblock` as zebra-rpc's `GetBlock`.
pub(crate) fn block_to_wire(block: &BlockVerbose) -> Result<GetBlock> {
    let tx = match &block.transactions {
        BlockTransactions::Ids(ids) => ids
            .iter()
            .map(|id| Value::String(display(<[u8; 32]>::from(*id))))
            .collect::<Vec<_>>(),
        BlockTransactions::Objects(objects) => objects
            .iter()
            .map(transaction)
            .collect::<Result<Vec<_>>>()?,
    };
    let mut object = Map::new();
    object.insert("hash".into(), display(block.hash.into()).into());
    object.insert(
        "confirmations".into(),
        block.confirmations.to_rpc_i64().into(),
    );
    insert_some(&mut object, "size", block.size.map(Value::from));
    object.insert("height".into(), u32::from(block.height).into());
    object.insert("version".into(), block.version.into());
    object.insert(
        "merkleroot".into(),
        display(block.merkle_root.into()).into(),
    );
    let header = zaino_primitives::types::rpc::HeaderDisplay::new(
        block.block_commitments,
        block.final_sapling_root,
        block.nonce,
    );
    insert_hex_option(
        &mut object,
        "blockcommitments",
        header
            .block_commitments
            .map(|commitments| hex::encode(commitments).into()),
    );
    object.insert(
        "finalsaplingroot".into(),
        hex::encode(header.final_sapling_root).into(),
    );
    insert_hex_option(
        &mut object,
        "finalorchardroot",
        block
            .final_orchard_root
            .map(|root| hex::encode(<[u8; 32]>::from(root)).into()),
    );
    object.insert("nTx".into(), tx.len().into());
    object.insert("tx".into(), tx.into());
    object.insert("time".into(), block.time.into());
    object.insert("nonce".into(), hex::encode(header.nonce).into());
    object.insert("solution".into(), hex::encode(&block.solution).into());
    object.insert(
        "bits".into(),
        format!("{:08x}", block.bits.as_bits()).into(),
    );
    object.insert("difficulty".into(), block.difficulty.into());
    insert_some(
        &mut object,
        "chainSupply",
        block.chain_supply.as_ref().map(balance),
    );
    if !block.value_pools.is_empty() {
        object.insert(
            "valuePools".into(),
            block
                .value_pools
                .iter()
                .map(balance)
                .collect::<Vec<_>>()
                .into(),
        );
    }
    object.insert(
        "trees".into(),
        json!({
            "sapling": { "size": u64::from(block.tree_sizes.sapling) },
            "orchard": { "size": u64::from(block.tree_sizes.orchard) },
            "ironwood": { "size": u64::from(block.tree_sizes.ironwood) },
        }),
    );
    insert_hex_option(
        &mut object,
        "previousblockhash",
        block
            .previous_block_hash
            .map(|hash| display(hash.into()).into()),
    );
    insert_hex_option(
        &mut object,
        "nextblockhash",
        block
            .next_block_hash
            .map(|hash| display(hash.into()).into()),
    );
    Ok(GetBlock::Object(Box::new(serde_json::from_value::<
        zebra_rpc::methods::BlockObject,
    >(Value::Object(object))?)))
}

fn transaction(tx: &TransactionVerbose) -> Result<Value> {
    let mut object = Map::new();
    insert_some(
        &mut object,
        "in_active_chain",
        tx.in_active_chain.map(Value::from),
    );
    object.insert("hex".into(), hex::encode(&tx.hex).into());
    insert_some(
        &mut object,
        "height",
        tx.height.map(|height| u32::from(height).into()),
    );
    insert_some(
        &mut object,
        "confirmations",
        tx.confirmations
            .map(|confirmations| confirmations.to_rpc_i64().into()),
    );
    insert_some(&mut object, "fee", tx.fee.map(|fee| zec(fee).into()));
    object.insert(
        "vin".into(),
        tx.inputs
            .iter()
            .map(input)
            .collect::<Result<Vec<_>>>()?
            .into(),
    );
    object.insert(
        "vout".into(),
        tx.outputs
            .iter()
            .map(output)
            .collect::<Result<Vec<_>>>()?
            .into(),
    );
    object.insert(
        "vShieldedSpend".into(),
        tx.sapling_spends
            .iter()
            .map(sapling_spend)
            .collect::<Vec<_>>()
            .into(),
    );
    object.insert(
        "vShieldedOutput".into(),
        tx.sapling_outputs
            .iter()
            .map(sapling_output)
            .collect::<Vec<_>>()
            .into(),
    );
    object.insert(
        "vjoinsplit".into(),
        tx.joinsplits
            .iter()
            .map(joinsplit)
            .collect::<Vec<_>>()
            .into(),
    );
    insert_some(&mut object, "bindingSig", tx.binding_sig.map(hex_value));
    insert_some(
        &mut object,
        "joinSplitPubKey",
        tx.joinsplit_pub_key.map(hex_value),
    );
    insert_some(&mut object, "joinSplitSig", tx.joinsplit_sig.map(hex_value));
    insert_some(&mut object, "orchard", tx.orchard.as_ref().map(orchard));
    insert_some(&mut object, "ironwood", tx.ironwood.as_ref().map(orchard));
    if let Some(balance) = tx.value_balance {
        object.insert("valueBalance".into(), signed_zec(balance).into());
        object.insert("valueBalanceZat".into(), i64::from(balance).into());
    }
    insert_some(&mut object, "size", tx.size.map(Value::from));
    insert_some(&mut object, "time", tx.time.map(Value::from));
    object.insert("txid".into(), display(tx.txid.into()).into());
    insert_some(&mut object, "authdigest", tx.auth_digest.map(hex_value));
    object.insert("overwintered".into(), tx.overwintered.into());
    object.insert("version".into(), tx.version.into());
    insert_some(
        &mut object,
        "versiongroupid",
        tx.version_group_id.as_ref().map(hex_value),
    );
    object.insert("locktime".into(), tx.lock_time.into());
    insert_some(
        &mut object,
        "expiryheight",
        tx.expiry_height.map(|height| u32::from(height).into()),
    );
    insert_some(
        &mut object,
        "blockhash",
        tx.block_hash.map(|hash| display(hash.into()).into()),
    );
    insert_some(&mut object, "blocktime", tx.block_time.map(Value::from));
    Ok(Value::Object(object))
}

fn input(input: &InputVerbose) -> Result<Value> {
    Ok(match input {
        InputVerbose::Coinbase { coinbase, sequence } => {
            json!({ "coinbase": hex::encode(coinbase), "sequence": sequence })
        }
        InputVerbose::Spend {
            txid,
            vout,
            script_sig_asm,
            script_sig,
            sequence,
            value,
            address,
            prevout,
        } => {
            let mut object = Map::new();
            object.insert("txid".into(), display((*txid).into()).into());
            object.insert("vout".into(), (*vout).into());
            object.insert(
                "scriptSig".into(),
                json!({ "asm": script_sig_asm, "hex": hex::encode(script_sig) }),
            );
            object.insert("sequence".into(), (*sequence).into());
            if let Some(value) = value {
                object.insert("value".into(), zec(*value).into());
                object.insert("valueSat".into(), value.as_u64().into());
            }
            insert_some(
                &mut object,
                "address",
                address.as_ref().map(|address| address.to_string().into()),
            );
            insert_some(
                &mut object,
                "prevout",
                prevout.as_deref().map(self::prevout).transpose()?,
            );
            Value::Object(object)
        }
    })
}

fn prevout(prevout: &PrevoutVerbose) -> Result<Value> {
    Ok(json!({
        "generated": prevout.generated,
        "height": u32::from(prevout.height),
        "value": zec(prevout.value),
        "scriptPubKey": script_pub_key(&prevout.script_pub_key)?,
    }))
}

fn output(output: &OutputVerbose) -> Result<Value> {
    Ok(json!({
        "value": zec(output.value),
        "valueZat": output.value.as_u64(),
        "n": output.n,
        "scriptPubKey": script_pub_key(&output.script_pub_key)?,
    }))
}

fn script_pub_key(script: &ScriptPubKey) -> Result<Value> {
    let mut object = Map::new();
    object.insert(
        "asm".into(),
        script
            .asm
            .clone()
            .ok_or(GetBlockWireError::Missing("a scriptPubKey's asm"))?
            .into(),
    );
    object.insert("hex".into(), hex::encode(script.script.as_bytes()).into());
    insert_some(
        &mut object,
        "reqSigs",
        script.required_signatures.map(Value::from),
    );
    object.insert(
        "type".into(),
        script
            .script_type
            .clone()
            .ok_or(GetBlockWireError::Missing("a scriptPubKey's type"))?
            .into(),
    );
    if !script.addresses.is_empty() {
        object.insert(
            "addresses".into(),
            script
                .addresses
                .iter()
                .map(|address| Value::String(address.to_string()))
                .collect::<Vec<_>>()
                .into(),
        );
    }
    Ok(Value::Object(object))
}

fn sapling_spend(spend: &SaplingSpendVerbose) -> Value {
    json!({
        "cv": hex::encode(spend.cv),
        "anchor": hex::encode(spend.anchor),
        "nullifier": hex::encode(spend.nullifier),
        "rk": hex::encode(spend.rk),
        "proof": hex::encode(&spend.proof),
        "spendAuthSig": hex::encode(&spend.spend_auth_sig),
    })
}

fn sapling_output(output: &SaplingOutputVerbose) -> Value {
    json!({
        "cv": hex::encode(output.cv),
        "cmu": hex::encode(output.cmu),
        "ephemeralKey": hex::encode(output.ephemeral_key),
        "encCiphertext": hex::encode(&output.enc_ciphertext),
        "outCiphertext": hex::encode(&output.out_ciphertext),
        "proof": hex::encode(&output.proof),
    })
}

fn joinsplit(joinsplit: &JoinSplitVerbose) -> Value {
    json!({
        "vpub_old": zec(joinsplit.vpub_old),
        "vpub_oldZat": joinsplit.vpub_old.as_u64(),
        "vpub_new": zec(joinsplit.vpub_new),
        "vpub_newZat": joinsplit.vpub_new.as_u64(),
        "anchor": hex::encode(joinsplit.anchor),
        "nullifiers": joinsplit.nullifiers.iter().map(hex::encode).collect::<Vec<_>>(),
        "commitments": joinsplit.commitments.iter().map(hex::encode).collect::<Vec<_>>(),
        "onetimePubKey": hex::encode(joinsplit.one_time_pub_key),
        "randomSeed": hex::encode(joinsplit.random_seed),
        "macs": joinsplit.macs.iter().map(hex::encode).collect::<Vec<_>>(),
        "proof": hex::encode(&joinsplit.proof),
        "ciphertexts": joinsplit.ciphertexts.iter().map(hex::encode).collect::<Vec<_>>(),
    })
}

fn orchard(bundle: &OrchardVerbose) -> Value {
    let mut object = Map::new();
    object.insert(
        "actions".into(),
        bundle
            .actions
            .iter()
            .map(|action| {
                json!({
                    "cv": hex::encode(action.cv),
                    "nullifier": hex::encode(action.nullifier),
                    "rk": hex::encode(action.rk),
                    "cmx": hex::encode(action.cmx),
                    "ephemeralKey": hex::encode(action.ephemeral_key),
                    "encCiphertext": hex::encode(&action.enc_ciphertext),
                    "spendAuthSig": hex::encode(&action.spend_auth_sig),
                    "outCiphertext": hex::encode(&action.out_ciphertext),
                })
            })
            .collect::<Vec<_>>()
            .into(),
    );
    object.insert(
        "valueBalance".into(),
        signed_zec(bundle.value_balance).into(),
    );
    object.insert(
        "valueBalanceZat".into(),
        i64::from(bundle.value_balance).into(),
    );
    insert_some(
        &mut object,
        "flags",
        bundle.flags.map(|flags| {
            json!({ "enableOutputs": flags.enable_outputs, "enableSpends": flags.enable_spends })
        }),
    );
    insert_some(&mut object, "anchor", bundle.anchor.map(hex_value));
    insert_some(&mut object, "proof", bundle.proof.as_ref().map(hex_value));
    insert_some(&mut object, "bindingSig", bundle.binding_sig.map(hex_value));
    Value::Object(object)
}

fn balance(pool: &ValuePoolBalance) -> Value {
    let mut object = Map::new();
    if !pool.id.is_empty() {
        object.insert("id".into(), pool.id.clone().into());
    }
    object.insert("chainValue".into(), zec(pool.chain_value).into());
    object.insert("chainValueZat".into(), pool.chain_value.as_u64().into());
    object.insert("monitored".into(), pool.monitored.into());
    if let Some(delta) = pool.value_delta {
        object.insert("valueDelta".into(), signed_zec(delta).into());
        object.insert("valueDeltaZat".into(), i64::from(delta).into());
    }
    Value::Object(object)
}

/// An optional hex field zebra-rpc reads without a default: absent is `null`,
/// which it reads as `None` and omits again on output.
fn insert_hex_option(object: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    object.insert(key.to_owned(), value.unwrap_or(Value::Null));
}

fn insert_some(object: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        object.insert(key.to_owned(), value);
    }
}

/// A hash in RPC display order (byte-reversed).
fn display(mut bytes: [u8; 32]) -> String {
    bytes.reverse();
    hex::encode(bytes)
}

fn hex_value(bytes: impl AsRef<[u8]>) -> Value {
    Value::String(hex::encode(bytes))
}

/// ZEC as zebrad's `Zec::lossy_zec` renders it.
#[allow(clippy::cast_precision_loss)]
fn zec(value: Zatoshis) -> f64 {
    value.as_u64() as f64 / 100_000_000.0
}

#[allow(clippy::cast_precision_loss)]
fn signed_zec(value: SignedZatoshis) -> f64 {
    i64::from(value) as f64 / 100_000_000.0
}
