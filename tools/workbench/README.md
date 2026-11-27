# workbench

Repository guards: one small binary per rule, each exiting non-zero with the
offending lines. An isolated Cargo workspace, not a member of Zaino's
production workspace.

| Binary | Rule | Run by |
| --- | --- | --- |
| `check-code-duplication` | no exact or near-duplicate Rust logic in `packages/` or `tools/`; exceptions are annotated in `.dupes-ignore.toml` | `makers lint-code-duplication` |
| `check-crypto-provider` | rustls resolves with aws-lc-rs and post-quantum preference, and `ring` is not in the dependency graph | `makers lint-crypto-provider` |
| `check-no-openssl-apt` | no Dockerfile installs an OpenSSL system package | `makers lint-no-openssl-apt` |
| `check-zebra-boundary` | no crate outside the source layer depends on a zebra crate or a source adapter | `makers lint-zebra-boundary` |
| `check-published-versions` | no publishable crate reuses an already-published version with different content | by hand, before publishing |

The four `makers` tasks are part of `makers lint`. A new guard is a new file
under `src/bin/` using `workbench::run`, plus its task in `Makefile.toml`.
