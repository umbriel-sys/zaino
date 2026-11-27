# Contributing to Zaino

Thank you for your interest in Zaino. Contributions of code, documentation,
bug reports and feature requests are all welcome.

## Reporting bugs and asking for features

Open an [issue](https://github.com/umbriel-sys/zaino/issues) on this
repository; the templates ask for what is needed. For a bug, include the
version or commit, the operating system, the validator and its version, and
logs (`RUST_LOG=zaino=debug,zainod=debug,zainodlib=debug`).

**Security issues are never reported in a public issue.** Report them
privately through the repository's
[security advisories](https://github.com/umbriel-sys/zaino/security).

## Contributing code and documentation

1. Fork the repository and branch from `dev`. Pull requests target `dev`.
2. Follow the coding rules in [`CLAUDE.md`](./CLAUDE.md). They bind every
   contributor, human or not: minimum visibility, no `.unwrap()` in
   production code, no placeholder data, named conversions at the
   persistence and wire boundaries, a current usage guide for every crate.
3. Record the change as a changeset under `.changesets/`
   ([format](./docs/release.md)). `CHANGELOG.md` files are generated from
   changesets and are never edited by hand.
4. Run `makers pre-push` before pushing: `makers lint` (every lint CI runs)
   plus the tool workspaces' own checks. `git config core.hooksPath
   .githooks` makes it the pre-push hook.
5. Run the unit suite: `cargo nextest run --workspace`. If the change touches
   serving, a validator adapter or the indexes, run the live suite too
   ([testing](./docs/testing.md)); CI does not.
6. Open the pull request as a draft until it is ready for review. Every
   pull request is reviewed in public before it is merged; requested changes
   are how the code stays good, not a judgement of the contributor.

Signed commits are encouraged
([about commit signature verification](https://docs.github.com/en/authentication/managing-commit-signature-verification/about-commit-signature-verification)).

### What CI runs

One job on every pull request and on every push to `dev`: `makers lint`
without the tool workspaces' checks, then the unit suite. The live suite, the
per-feature compile check, the dispatch to `zcash/integration-tests` and the
container image build are started by hand by a maintainer. A green check
therefore does not mean the live suite passed.

### Served RPCs

A change to a served RPC matches the validator's behaviour: zebrad is the
reference. It comes with a unit test against the shared fake validator and,
where the answer depends on a real node, a live test comparing Zaino's answer
with zebrad's.

## Platforms

The reference platform is current stable Debian on `x86_64-unknown-linux-gnu`.
The Rust toolchain is pinned in [`rust-toolchain.toml`](./rust-toolchain.toml).

## More documentation

[`README.md`](./README.md) indexes the repository documents and the per-crate
usage guides.

## Software philosophy

Zaino is Free and Open Source Software, licensed under Apache-2.0. Users are
free to run it for any purpose, to study and adapt it, to redistribute it, and
to publish their improvements. Developing in the open reduces duplicated
effort, lets work build on other work, and keeps quality visible.
