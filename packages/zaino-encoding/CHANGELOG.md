# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-21
### Added
- `encode_header`: a block header back to its consensus bytes, for serving headers Zaino holds.
### Changed
- **Breaking: the crate name is reused for different code.** The old `zaino-encoding` (versioned serde traits and byte helpers, removed with zainodb) is gone and none of its API remains. The crate is now the non-validating decoder from Zcash consensus bytes to the domain `Block` and `Transaction`, moved out of `zaino-source-zcash-rpc` so a crate above the source layer can decode without an adapter or a node library. `parse_block(bytes, light)`, `parse_transaction(bytes)`, `DecodeError`, `GENESIS_COINBASE_SCRIPT_SIG` (C4).
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
