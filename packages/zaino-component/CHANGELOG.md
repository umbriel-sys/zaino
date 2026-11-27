# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. The component abstraction: a supervised in-process subsystem, and
  what every subsystem otherwise hand-rolls — `ComponentStatus`, `Lifecycle`,
  `Health`, `Managed`, and `Task`.
- A component's state has two independent axes. `Lifecycle` is a management
  phase, moved only by `Managed`; `Health` is a condition the component reaches
  on its own. Neither overwrites the other, so `ComponentStatus` reports both
  rather than collapsing them into one verdict.
- Reading and driving are separate capabilities. `StatusSource` is universal and
  synchronous, so a supervisor samples every component without awaiting.
  `Managed` is implemented only by components the runtime owns, which is what
  distinguishes them from observed dependencies such as a validator.
- `Lifecycle::try_advance_to` owns the state machine. A restart is a full lap
  through `Closing → Offline → Spawning`, never a jump back to running, and an
  illegal transition is the typed `IllegalTransition` rather than a silent
  correction.
- `Task` wraps `tokio::spawn` with a typed name, cooperative cancellation, a
  hard abort, and panic capture surfaced as `TaskError`.

## [0.2.0] - 2026-11-21
### Added
- `CpuBudget` splits the process's cores between components by `Share` (chain store 8/16, chain head 4/16, mempool 2/16, chain view 2/16, each with a floor and a cap), so no two components both size themselves from the core count.
- `StatusCell` (a shared status a runtime sets and handles read and watch) and `ComponentStatus::{is_live, is_ready}`.
- A component status now carries the cause of its condition: the failing error's full source chain when unhealthy, cleared on any clean transition. Adds `RunLoop` for a supervised run body, `Progress` for bring-up progress, and `ReachabilityProbe` for asking whether a dependency is reachable.
- `StatusCell::degraded_unless_closing`: `Recoverable` with a reason, unless closing.
### Changed
- `Task`, `TaskName` and `TaskError` move to zaino-async: a component runs on a task rather than defining one. `CancellationToken` is re-exported from this crate so an implementor needs to name only one.
  _Migration:_ Import `Task`, `TaskName` and `TaskError` from `zaino_async` instead of `zaino_component`. `ComponentStatus` is now `Clone` and no longer `Copy`, so read it out of a watch with `.clone()`.
### Internal
- Denies panicking code in production.
