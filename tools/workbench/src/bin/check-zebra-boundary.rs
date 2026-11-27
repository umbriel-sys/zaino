//! Guard: zebra types stay inside the source layer.
//!
//! Outside test code (`[dev-dependencies]`), a crate under `packages/` may:
//! - depend on a `zebra-*` crate, or turn on `zaino-primitives`' `zebra`
//!   feature, only if it is a source adapter, the fake validator,
//!   `zaino-state` (the frozen legacy shim) or `zaino-primitives` itself
//!   (its optional conversions);
//! - depend on a source adapter only if it is another source crate,
//!   `zaino-state`, or `zainod`, whose one source-selection module names
//!   them. Everything else is built on the ports.

use workbench::{git, read, repo_root, run};

/// May name zebra crates.
const HOLDS_ZEBRA: [&str; 5] = [
    "zaino-source-zebra",
    "zaino-source-zcash-rpc",
    "zaino-source-mock",
    "zaino-state",
    "zaino-primitives",
];

/// The source adapters.
const ADAPTERS: [&str; 2] = ["zaino-source-zebra", "zaino-source-zcash-rpc"];

/// May depend on a source adapter.
const NAMES_ADAPTERS: [&str; 5] = [
    "zaino-source-zebra",
    "zaino-source-zcash-rpc",
    "zaino-source-mock",
    "zaino-state",
    "zainod",
];

fn main() {
    run("check-zebra-boundary", check, |n| {
        println!("check-zebra-boundary: ok — zebra stays in the source layer across {n} crate(s)");
    })
}

fn check() -> Result<usize, Vec<String>> {
    let root = repo_root()?;
    let root_str = root
        .to_str()
        .ok_or_else(|| vec!["repo root path is not utf-8".to_string()])?;
    let manifests: Vec<String> = git(&["-C", root_str, "ls-files", "packages/*/Cargo.toml"])?
        .lines()
        .map(str::to_string)
        .collect();

    let mut offenders = Vec::new();
    for manifest in &manifests {
        let Some(krate) = manifest.split('/').nth(1) else {
            continue;
        };
        for (number, line) in production_dependencies(&read(&root.join(manifest))?) {
            if let Some(reason) = violation(krate, &line) {
                offenders.push(format!("{manifest}:{number}: {reason}: {}", line.trim()));
            }
        }
    }
    if !offenders.is_empty() {
        let mut message = vec!["zebra has leaked out of the source layer:".to_string()];
        message.extend(offenders);
        message.push(
            "build on the zaino-source ports, or move the code into a source adapter.".to_string(),
        );
        return Err(message);
    }
    Ok(manifests.len())
}

/// The dependency lines of every table but `[dev-dependencies]`, with their
/// line numbers.
fn production_dependencies(manifest: &str) -> Vec<(usize, String)> {
    let mut table = String::new();
    let mut lines = Vec::new();
    for (index, line) in manifest.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            table = trimmed.to_string();
            continue;
        }
        let production = table.ends_with("dependencies]") && !table.contains("dev-dependencies");
        if production && !trimmed.starts_with('#') && !trimmed.is_empty() {
            lines.push((index + 1, line.to_string()));
        }
    }
    lines
}

/// Why `krate` may not carry the dependency on `line`, if it may not.
fn violation(krate: &str, line: &str) -> Option<&'static str> {
    let name = line
        .trim()
        .split(|c: char| c == '=' || c == '.' || c.is_whitespace())
        .next()?;
    if name.starts_with("zebra-") && !HOLDS_ZEBRA.contains(&krate) {
        return Some("a zebra crate outside the source layer");
    }
    if name == "zaino-primitives" && line.contains("\"zebra\"") && !HOLDS_ZEBRA.contains(&krate) {
        return Some("zaino-primitives' zebra conversions outside the source layer");
    }
    if ADAPTERS.contains(&name) && !NAMES_ADAPTERS.contains(&krate) {
        return Some("a source adapter above the source layer");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "\
[package]
name = \"x\"

[dependencies]
zebra-chain = { workspace = true }
zaino-primitives = { workspace = true, features = [\"zebra\"] }
zaino-source-zebra.workspace = true
serde = \"1\"

[dev-dependencies]
zebra-test.workspace = true

[build-dependencies]
zebra-rpc = \"1\"
";

    #[test]
    fn only_production_tables_are_read() {
        let names: Vec<_> = production_dependencies(MANIFEST)
            .into_iter()
            .map(|(number, _)| number)
            .collect();
        assert_eq!(names, [5, 6, 7, 8, 14]);
    }

    #[test]
    fn zebra_and_adapters_are_allowed_only_where_listed() {
        let lines = production_dependencies(MANIFEST);
        let refused = |krate: &str| {
            lines
                .iter()
                .filter(|(_, line)| violation(krate, line).is_some())
                .count()
        };
        // zebra-chain, the feature, the adapter and the build dependency
        assert_eq!(refused("zaino-chain"), 4);
        // zainod names adapters, never zebra
        assert_eq!(refused("zainod"), 3);
        assert_eq!(refused("zaino-source-zebra"), 0);
        // primitives holds the conversions but builds on no adapter
        assert_eq!(refused("zaino-primitives"), 1);
    }
}
