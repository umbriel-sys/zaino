# relman

Zaino's release manager: changeset checking, version derivation, changelogs
and the publish plan. How to use it is in
[`docs/release.md`](../../docs/release.md).

A hexagonal (ports and adapters) Rust CLI: `relman-core` (types and ports),
`relman-domain` (services), `relman-config` (`relman.toml`),
`relman-adapters` (filesystem, git, cargo metadata), `relman-cli` (clap) and
the `relman` binary (composition root).

This is an isolated Cargo workspace, like `tools/workbench`: it is not a
member of Zaino's production workspace and every crate is `publish = false`.

```sh
cargo run --manifest-path tools/relman/Cargo.toml --bin relman -- --help
```

Its own format, clippy and tests run under `makers lint-tools` (the pre-push
hook). Two of its tests read the repository's `relman.toml` and `Cargo.toml`:
they fail when a release target depends on a workspace crate that is not a
target.
