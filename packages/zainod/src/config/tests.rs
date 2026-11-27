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

/// The chain head runs at the consensus depth, and at a tenth of it under
/// `fast-test-seam`.
#[test]
fn the_seam_depth_follows_the_fast_test_seam_feature() {
    let depth = ZainodConfig::default()
        .service_config()
        .chain_head
        .max_depth();
    #[cfg(not(feature = "fast-test-seam"))]
    assert_eq!(depth, zaino_consensus::MAX_NONFINALISED_DEPTH);
    #[cfg(feature = "fast-test-seam")]
    assert_eq!(depth, zaino_consensus::FAST_TEST_MAX_NONFINALISED_DEPTH);
    assert_eq!(zaino_consensus::MAX_NONFINALISED_DEPTH, 1_001);
    assert_eq!(zaino_consensus::FAST_TEST_MAX_NONFINALISED_DEPTH, 100);
}

/// The example config in `docs/` is the generated default but for its two
/// paths, which the default takes from this machine.
#[test]
fn the_documented_example_is_the_generated_default() {
    let example = include_str!("../../../../docs/example_configs/zainod.toml");
    let loaded = load(example, "ZAINOTEST_NONE").expect("loads");
    assert_eq!(loaded, example_paths(ZainodConfig::default()));
    let generated = generate_default_config().expect("generates");
    let path_line =
        |line: &&str| !line.starts_with("path = ") && !line.starts_with("state_path = ");
    assert_eq!(
        example.lines().filter(path_line).collect::<Vec<_>>(),
        generated.lines().filter(path_line).collect::<Vec<_>>()
    );
}

/// The full example is the default but for its index set, its store path and
/// its JSON-RPC server.
#[test]
fn the_full_example_builds_every_index_and_serves_jsonrpc() {
    let example = include_str!("../../../../docs/example_configs/zainod-full.toml");
    let loaded = load(example, "ZAINOTEST_NONE").expect("loads");
    loaded.check_config().expect("valid");
    let mut expected = example_paths(ZainodConfig::default());
    expected.indexes.set = Some(NamedSet::Full);
    expected.serve.jsonrpc = loaded.serve.jsonrpc.clone();
    assert_eq!(loaded, expected);
    let jsonrpc = loaded.serve.jsonrpc.expect("served");
    assert_eq!(jsonrpc.listen_address.to_string(), "127.0.0.1:8237");
    assert_eq!(jsonrpc.cookie_dir, None);

    let template = generate_default_config().expect("generates");
    assert_eq!(
        settings(example),
        settings(&template),
        "the full example documents every setting the default does"
    );
}

/// `config` with the documented examples' two paths.
fn example_paths(mut config: ZainodConfig) -> ZainodConfig {
    config.persistence.path = PathBuf::from("/var/cache/zaino");
    if let Some(read_state) = config.validator.read_state.as_mut() {
        read_state.state_path = PathBuf::from("/var/cache/zebra");
    }
    config
}

/// Every `table.key` and `table` a config file names, commented out or not.
fn settings(file: &str) -> std::collections::BTreeSet<String> {
    let mut table = String::new();
    let mut found = std::collections::BTreeSet::new();
    for line in file.lines() {
        let line = line.strip_prefix("# ").unwrap_or(line);
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            table = name.to_owned();
            found.insert(table.clone());
        } else if let Some((key, _)) = line.split_once(" = ") {
            if !key.is_empty() && key.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                found.insert(match table.is_empty() {
                    true => key.to_owned(),
                    false => format!("{table}.{key}"),
                });
            }
        }
    }
    found
}

/// Every table and key of a config.
fn keys(value: &toml::Value, path: &str, found: &mut std::collections::BTreeSet<String>) {
    let toml::Value::Table(table) = value else {
        return;
    };
    for (key, value) in table {
        let path = match path.is_empty() {
            true => key.clone(),
            false => format!("{path}.{key}"),
        };
        found.insert(path.clone());
        keys(value, &path, found);
    }
}

/// The generated default names every setting there is, optional ones
/// commented out: a config with each option set has no key it lacks.
#[test]
fn the_generated_default_documents_every_setting() {
    let default = ZainodConfig::default();
    let every_option = ZainodConfig {
        donation_address: Some("u1".to_owned()),
        validator: ValidatorConfig {
            cookie_path: Some(PathBuf::from("/cookie")),
            user: Some("u".to_owned()),
            password: Some("p".to_owned()),
            ..default.validator.clone()
        },
        persistence: PersistenceConfig {
            threads: NonZeroUsize::new(4),
            ..default.persistence.clone()
        },
        mempool: MempoolSettings {
            max_cost_bytes: Some(1),
            poll_interval_ms: Some(1),
            metadata_min_interval_ms: Some(1),
            max_exclude_count: Some(1),
        },
        serve: ServeConfig {
            grpc: GrpcServerConfig {
                tls: Some(crate::serve::config::GrpcTls {
                    cert_path: PathBuf::from("/cert"),
                    key_path: PathBuf::from("/key"),
                }),
                ..default.serve.grpc.clone()
            },
            jsonrpc: Some(JsonRpcServerConfig {
                cookie_dir: Some(PathBuf::from("/run")),
                ..JsonRpcServerConfig::default()
            }),
            ..default.serve.clone()
        },
        admin: Some(AdminConfig {
            listen_address: SocketAddr::from(([127, 0, 0, 1], 9998)),
        }),
        ..default
    };
    let value = toml::Value::try_from(&every_option).expect("serialises");
    let mut expected = std::collections::BTreeSet::new();
    keys(&value, "", &mut expected);

    let generated = generate_default_config().expect("generates");
    // `[indexes.paths]` is a map: its example entry is no fixed setting
    let mut documented = settings(&generated);
    documented.retain(|setting| !setting.starts_with("indexes.paths."));
    let missing: Vec<_> = expected.difference(&documented).collect();
    assert!(missing.is_empty(), "not in the template: {missing:?}");
    let unknown: Vec<_> = documented.difference(&expected).collect();
    assert!(unknown.is_empty(), "not a setting: {unknown:?}");
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

[ingest]
fetch_concurrency = 4
resolve_concurrency = 2

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
    assert_eq!(config.ingest.fetch_concurrency.get(), 4);
    assert_eq!(config.ingest.resolve_concurrency.get(), 2);
    assert_eq!(
        config.ingest.queue_bytes,
        IngestConfig::default().queue_bytes,
        "an unset key keeps its default"
    );
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
    assert_eq!(disk.store.ingest, config.ingest);
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
                network: Network::Mainnet
            }
        ));
    }
}

/// Range fills passed to the validator (two requests a permit) cannot alone
/// fill the validator client's requests in flight, which stay under the 100
/// zebrad serves at once. Single reads take no permit.
#[test]
fn range_fills_alone_cannot_fill_the_validator_client() {
    let passthrough = ZainodConfig::default()
        .service_config()
        .chain_view
        .passthrough_permits;
    let in_flight = zaino_rpc::RpcClientConfig::default().max_in_flight;
    assert!(passthrough * 2 < in_flight, "{passthrough} {in_flight}");
    assert!(in_flight < 100, "{in_flight}");
}

/// An index set the store would refuse is refused at load, naming the key,
/// the indexes involved and what to change.
#[test]
fn an_index_set_that_cannot_be_built_is_refused() {
    let dependency = refused("[indexes]\nset = \"lightclient\"\ndisable = [\"headers\"]");
    assert!(
        dependency.contains(
            "indexes.disable names `headers`, but `compact_block` is enabled and depends on it"
        ),
        "{dependency}"
    );
    assert!(
        dependency.contains("add `compact_block` to indexes.disable"),
        "{dependency}"
    );

    let unknown = refused("[indexes]\nenable = [\"nope\"]");
    assert!(
        unknown.contains("indexes.enable names `nope`, which is not an index"),
        "{unknown}"
    );
    assert!(unknown.contains("history_tree"), "{unknown}");
    assert!(refused("[indexes]\ndisable = [\"nope\"]").contains("indexes.disable names `nope`"));

    load(
        "[indexes]\nset = \"lightclient\"\ndisable = [\"compact_block\", \"headers\"]",
        "ZAINOTEST_NONE",
    )
    .expect("an index and everything that needs it may be disabled together");
}

/// The dependencies the default config documents are the indexes' own.
#[test]
fn the_documented_index_dependencies_are_the_real_ones() {
    let template = generate_default_config().expect("generates");
    for &id in zaino_indexes::IndexId::ALL {
        let mut needs: Vec<&str> = zaino_chain_store_service::IndexSet::of([id])
            .iter()
            .filter(|needed| *needed != id)
            .map(|needed| needed.name())
            .collect();
        needs.sort_unstable();
        let prefix = format!("#   {} needs ", id.name());
        let mut documented: Vec<&str> = template
            .lines()
            .find_map(|line| line.strip_prefix(prefix.as_str()))
            .map(|list| list.split(", ").collect())
            .unwrap_or_default();
        documented.sort_unstable();
        assert_eq!(documented, needs, "{}", id.name());
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
    // a `[validator]` table without the read state has none: the default's
    // is not carried in
    assert!(refused("[validator]\nrpc_address = \"127.0.0.1:8232\"").contains("read_state"));
    assert!(refused(
        "[validator]\nkind = \"zcash_rpc\"\n[validator.read_state]\nstate_path = \"/zebra\"\n\
         grpc_address = \"127.0.0.1:8230\""
    )
    .contains("kind = \"zebra\""));
    let generic = |key: &str| refused(&format!("[validator]\nkind = \"zcash_rpc\"\n{key}"));
    assert!(generic("user = \"u\"").contains("together"));
    assert!(generic("rpc_address = \"8.8.8.8:8232\"").contains("private"));
    assert!(generic("cookie_path = \"/no/such/cookie\"").contains("does not exist"));
    load("[validator]\nkind = \"zcash_rpc\"", "ZAINOTEST_NONE").expect("needs no read state");
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

/// With no cache directory to default under, a store or state path left to
/// its default is refused; one that is set is not.
#[test]
fn a_defaulted_path_needs_a_cache_directory() {
    let defaulted = ZainodConfig::default();
    defaulted.check_paths(true).expect("a cache directory");
    let refused = defaulted.check_paths(false).expect_err("refused");
    assert!(
        refused.to_string().contains("persistence.path"),
        "{refused}"
    );

    let mut store_set = ZainodConfig::default();
    store_set.persistence.path = "/var/lib/zaino".into();
    let refused = store_set.check_paths(false).expect_err("refused");
    assert!(
        refused
            .to_string()
            .contains("validator.read_state.state_path"),
        "{refused}"
    );

    if let Some(read_state) = store_set.validator.read_state.as_mut() {
        read_state.state_path = "/var/lib/zebra".into();
    }
    store_set.check_paths(false).expect("both set");
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
