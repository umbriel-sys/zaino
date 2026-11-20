//! Loading, defaults, the old keys and what `check_config` refuses.

use std::sync::Mutex;

use tempfile::TempDir;
use zaino_chain_store_service::NamedSet;
use zaino_service::Store;

use super::*;

/// Environment variables are process-wide: the tests that read them take
/// turns, each with its own prefix.
static ENV: Mutex<()> = Mutex::new(());

fn load(toml: &str, prefix: &str) -> Result<ZainodConfig, IndexerError> {
    let dir = TempDir::new().expect("a temp dir");
    let path = dir.path().join("zainod.toml");
    std::fs::write(&path, toml).expect("written");
    load_config_with_env(&path, prefix)
}

fn refused(toml: &str) -> String {
    load(toml, "ZAINOTEST_NONE")
        .expect_err("refused")
        .to_string()
}

/// The generated default loads back to the default, and an empty file is
/// the default too.
#[test]
fn the_generated_default_round_trips() {
    let generated = generate_default_config().expect("generates");
    assert!(generated.starts_with(GENERATED_CONFIG_HEADER));
    assert_eq!(
        load(&generated, "ZAINOTEST_NONE").expect("loads"),
        ZainodConfig::default()
    );
    assert_eq!(
        load("", "ZAINOTEST_NONE").expect("loads"),
        ZainodConfig::default()
    );
}

/// Every section is read; the JSON-RPC server and the admin endpoint are
/// off unless their tables are present.
#[test]
fn every_section_is_read() {
    let dir = TempDir::new().expect("a temp dir");
    let config = load(
        &format!(
            r#"
network = "Mainnet"

[validator]
kind = "zebra"
rpc_address = "127.0.0.1:8232"
user = "u"
password = "p"

[validator.read_state]
state_path = "{state}"
grpc_address = "127.0.0.1:8230"

[indexes]
set = "lightclient"
enable = ["address_history"]
delete_stale = false

[persistence]
path = "{state}/zaino"
write_buffer_mib = 32
threads = 2

[mempool]
max_cost_bytes = 50000000

[serve]
stream_timeout_secs = 5
stream_channel_size = 8

[serve.grpc]
listen_address = "127.0.0.1:9000"

[serve.grpc.limits]
max_concurrent_streams = 8

[serve.jsonrpc]
listen_address = "127.0.0.1:9001"

[admin]
listen_address = "127.0.0.1:9002"
"#,
            state = dir.path().display()
        ),
        "ZAINOTEST_NONE",
    )
    .expect("loads");

    assert_eq!(config.network, Network::Mainnet);
    assert_eq!(config.validator.kind, ValidatorKind::Zebra);
    assert_eq!(config.validator.user.as_deref(), Some("u"));
    let read_state = config.validator.read_state.as_ref().expect("set");
    assert_eq!(read_state.grpc_address, "127.0.0.1:8230");
    assert_eq!(config.indexes.set, Some(NamedSet::Lightclient));
    assert_eq!(config.indexes.enable, ["address_history"]);
    assert!(!config.indexes.delete_stale);
    assert_eq!(config.persistence.write_buffer_mib.get(), 32);
    assert_eq!(config.persistence.threads.map(NonZeroUsize::get), Some(2));
    assert_eq!(config.mempool.max_cost_bytes, Some(50_000_000));
    assert_eq!(config.serve.stream_timeout_secs, 5);
    assert_eq!(config.serve.grpc.limits.max_concurrent_streams.get(), 8);
    let jsonrpc = config.serve.jsonrpc.as_ref().expect("served");
    assert_eq!(jsonrpc.listen_address.port(), 9001);
    assert_eq!(config.admin.expect("set").listen_address.port(), 9002);

    let ServiceConfig { store, .. } = config.service_config();
    let Store::DiskAccelerated(disk) = store else {
        panic!("a named set is stored");
    };
    assert_eq!(disk.store.root, config.persistence.path);
    assert_eq!(disk.store.indexes, config.indexes);
    assert_eq!(disk.storage.write_buffer.get(), 32 << 20);
}

/// No named index is no store: every read goes to the validator.
#[test]
fn an_empty_set_is_validator_backed() {
    for indexes in ["set = \"empty\"", ""] {
        let config = ZainodConfig {
            indexes: toml::from_str(indexes).expect("parses"),
            ..ZainodConfig::default()
        };
        assert!(matches!(
            config.service_config().store,
            Store::ValidatorBacked {
                network: Network::PubTestnet
            }
        ));
    }
}

/// Each key of the old configuration is a typed error naming it and what
/// replaced it.
#[test]
fn an_old_key_is_named_with_its_replacement() {
    for (key, value) in [
        ("backend", "\"fetch\""),
        ("zebra_db_path", "\"/zebra\""),
        ("ephemeral_finalised_state", "true"),
        ("metrics_endpoint", "\"127.0.0.1:9998\""),
    ] {
        let error = load(&format!("{key} = {value}"), "ZAINOTEST_NONE").expect_err("old key");
        assert!(
            matches!(error, IndexerError::OldConfigKey { key: named, .. } if named == key),
            "{key}: {error}"
        );
    }
    for table in [
        "json_server_settings",
        "grpc_settings",
        "validator_settings",
        "service",
        "storage",
    ] {
        let error = load(&format!("[{table}]\nx = 1"), "ZAINOTEST_NONE").expect_err("old key");
        assert!(
            matches!(error, IndexerError::OldConfigKey { key, .. } if key == table),
            "{table}: {error}"
        );
    }
    let named = refused("backend = \"state\"");
    assert!(
        named.contains("backend") && named.contains("validator.kind"),
        "{named}"
    );
}

/// An unknown key is refused, at the top level and inside a section.
#[test]
fn an_unknown_key_is_refused() {
    assert!(refused("no_such_key = 1").contains("no_such_key"));
    assert!(refused("[validator]\nno_such_key = 1").contains("no_such_key"));
}

/// The source and its settings agree: zebra reads its state, the generic
/// source does not; credentials come as a pair.
#[test]
fn the_validator_settings_must_agree() {
    assert!(refused("[validator]\nkind = \"zebra\"").contains("read_state"));
    assert!(refused(
        "[validator.read_state]\nstate_path = \"/zebra\"\ngrpc_address = \"127.0.0.1:8230\""
    )
    .contains("kind = \"zebra\""));
    assert!(refused("[validator]\nuser = \"u\"").contains("together"));
    assert!(refused("[validator]\nrpc_address = \"8.8.8.8:8232\"").contains("private"));
    assert!(refused("[validator]\ncookie_path = \"/no/such/cookie\"").contains("does not exist"));
}

/// The servers' addresses are checked before anything binds.
#[test]
fn the_server_settings_are_checked() {
    #[cfg(not(feature = "no_tls_use_unencrypted_traffic"))]
    assert!(refused("[serve.grpc]\nlisten_address = \"8.8.8.8:8137\"").contains("tls"));
    #[cfg(not(feature = "allow_unencrypted_public_json_rpc_bind"))]
    assert!(refused("[serve.jsonrpc]\nlisten_address = \"8.8.8.8:8232\"").contains("private"));
    assert!(refused("[serve.jsonrpc]\nlisten_address = \"127.0.0.1:8137\"").contains("different"));
    assert!(refused(
        "[serve.grpc]\nlisten_address = \"127.0.0.1:8137\"\n[serve.grpc.tls]\n\
         cert_path = \"/no/cert\"\nkey_path = \"/no/key\""
    )
    .contains("cert_path"));
}

/// A mempool that could hold nothing, or poll never, is refused; a donation
/// address must parse.
#[test]
fn the_mempool_and_donation_address_are_checked() {
    assert!(refused("[mempool]\nmax_cost_bytes = 1").contains("ZIP-401"));
    assert!(refused("[mempool]\npoll_interval_ms = 0").contains("greater than zero"));
    assert!(refused("donation_address = \"not an address\"").contains("donation_address"));
}

/// The environment overrides the file; a secret may not come from it.
#[test]
fn the_environment_overrides_the_file_but_carries_no_secret() {
    let _turn = ENV
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("ZAINOTESTENV_NETWORK", "Mainnet");
    std::env::set_var("ZAINOTESTENV_SERVE__STREAM_CHANNEL_SIZE", "7");
    let config = load("network = \"Regtest\"", "ZAINOTESTENV");
    std::env::remove_var("ZAINOTESTENV_NETWORK");
    std::env::remove_var("ZAINOTESTENV_SERVE__STREAM_CHANNEL_SIZE");
    let config = config.expect("loads");
    assert_eq!(config.network, Network::Mainnet);
    assert_eq!(config.serve.stream_channel_size, 7);

    std::env::set_var("ZAINOTESTSECRET_VALIDATOR__PASSWORD", "p");
    let refused = load("", "ZAINOTESTSECRET");
    std::env::remove_var("ZAINOTESTSECRET_VALIDATOR__PASSWORD");
    assert!(refused
        .expect_err("a secret in the environment")
        .to_string()
        .contains("sensitive"));

    std::env::set_var("ZAINOTESTOLD_BACKEND", "state");
    let old = load("", "ZAINOTESTOLD");
    std::env::remove_var("ZAINOTESTOLD_BACKEND");
    assert!(matches!(
        old,
        Err(IndexerError::OldConfigKey { key: "backend", .. })
    ));
}

/// An empty cookie directory takes the default one.
#[test]
fn an_empty_cookie_directory_is_the_default_one() {
    let config = load(
        "[serve.jsonrpc]\nlisten_address = \"127.0.0.1:9001\"\ncookie_dir = \"\"",
        "ZAINOTEST_NONE",
    )
    .expect("loads");
    assert_eq!(
        config.serve.jsonrpc.expect("served").cookie_dir,
        Some(default_ephemeral_cookie_path())
    );
}
