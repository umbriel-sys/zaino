//! Helpers for the `clientless` partition of the live tests go here.
//!
//! This crate also exposes test-vectors.

pub mod rpc {
    pub mod json_rpc {
        // One vector is enough here: the per-kind table (including Sprout,
        // which Zaino reports invalid) is a unit test in `zaino-serve`'s
        // `wire::address`.
        pub const VALID_SAPLING_ADDRESS: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";
    }

    pub mod z_validate_address {
        use anyhow::{Context, Result};
        use serde_json::{json, Value};
        use zaino_testutils::JsonRpcClient;

        use crate::rpc::json_rpc::VALID_SAPLING_ADDRESS;

        /// Asserts the served `z_validateaddress` endpoint reaches the
        /// classifier and renders zebrad's shape. Classification itself is a
        /// pure function of (address, network) and is unit-tested.
        pub async fn run_z_validate_for(irpc: &JsonRpcClient) -> Result<()> {
            let resp: Value = irpc
                .call_value("z_validateaddress", json!([VALID_SAPLING_ADDRESS]))
                .await
                .with_context(|| format!("z_validateaddress {VALID_SAPLING_ADDRESS}"))?;

            assert_eq!(
                resp,
                json!({
                    "isvalid": true,
                    "address": VALID_SAPLING_ADDRESS,
                    "address_type": "sapling",
                    "ismine": false,
                }),
                "z_validateaddress served shape"
            );
            Ok(())
        }
    }
}
