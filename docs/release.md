# Changesets and releases

Every change to a crate is recorded as a changeset. Changelogs are generated
from changesets; no `CHANGELOG.md` is edited by hand. The tool is `relman`,
in this repository at `tools/relman/`.

## Writing a changeset

One TOML file per pull request under `.changesets/`, any name:

```toml
[[changes]]
crate = "zainod"
kind = "feature"
description = """
`getblockhash` is served over JSON-RPC."""

[[changes]]
crate = "zaino-service"
kind = "breaking"
description = """
`ServiceSnapshot::block_hash` takes a signed index."""
migration = """
Pass `-1` for the tip."""
```

| Field | Required | Meaning |
| --- | --- | --- |
| `crate` | yes | a target declared in `relman.toml` (every crate under `packages/`) |
| `kind` | yes | `breaking`, `feature`, `fix` or `internal` |
| `description` | yes | one changelog bullet, written to stand alone |
| `migration` | no | upgrade notes; expected on `breaking` |
| `section` | no | changelog section override (`Added`, `Changed`, `Fixed`, `Removed`, `Security`, `Deprecated`) |
| `issues` | no | issue references, e.g. `["#987"]` |

- One `[[changes]]` entry per user-visible change, even for the same crate.
  Internal-only changes to a crate may share one `internal` entry.
- A pull request that changes a crate's source needs an entry naming that
  crate. One that changes none uses the empty form:

  ```toml
  [empty]
  reason = "CI only."
  ```

- `kind` states the intent, not the version bump: relman derives the bump.

`cargo run --manifest-path tools/relman/Cargo.toml --bin relman -- changeset new`
scaffolds a file.

## The check

`makers lint-changesets` (part of `makers lint`, so of CI and the pre-push
hook) runs `relman changeset check --base <ref>`. It fails when:

- a changeset is malformed, uses an unknown `kind`, or names a crate that is
  not a target;
- the branch changes a target's source, against the base, with no entry
  naming that target.

The base is `origin/dev`; set `RELMAN_BASE` to check against another ref.

## Targets

`relman.toml` lists the targets: every crate of the root workspace. A crate
marked `publish = false` there is versioned and gets a changelog but is left
out of the publish plan. A new crate is added to `relman.toml` in the pull
request that creates it; relman's own tests fail if a target depends on a
workspace crate that is not one.

## Cutting a release

Releases are cut by hand; no workflow does it.

```sh
relman versions                  # the bump each crate would take, and why
relman bump                      # write the versions into the manifests
relman changelog                 # write each crate's and the workspace's changelog section
relman publish-plan              # the publishable crates in dependency order
relman changeset consume --cycle <name> --yes
```

- `versions`, `bump --dry-run` and `changelog --dry-run` change nothing.
- relman derives each crate's version from its own changesets: before 1.0, a
  `breaking` entry is a minor bump and everything else a patch. A crate whose
  dependency's bump crosses its requirement takes a patch bump too.
- `consume` marks each pending changeset with `consumed_in` and records its
  id in `.release/consumed-ledger.toml`. Consumed changesets stay on disk and
  are skipped from then on; they are the record of what shipped when.
- Before publishing, `check-published-versions` (`tools/workbench`) confirms
  no crate reuses a version already on crates.io with different content.
- Tag the release commit `zainod-X.Y.Z`.

Container images are built by the `Container image` workflow, started by
hand.
