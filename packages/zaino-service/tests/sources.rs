//! The two sources a daemon can be configured with satisfy everything the
//! service asks of a validator.

use zaino_service::ServiceSource;

#[test]
fn both_configurable_sources_satisfy_the_service_bound() {
    fn assert_satisfied<T: ServiceSource>() {}
    assert_satisfied::<zaino_source_zcash_rpc::ZcashRpcAdapter>();
    assert_satisfied::<zaino_source_zebra::ZebraValidator>();
}
