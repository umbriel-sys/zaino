//! Default paths, by the XDG Base Directory specification.

use std::path::PathBuf;

/// `$XDG_CONFIG_HOME/<subpath>`, else `$HOME/.config/<subpath>`.
pub fn config_path(subpath: &str) -> PathBuf {
    resolve("XDG_CONFIG_HOME", Some(".config"), subpath)
}

/// `$XDG_CACHE_HOME/<subpath>`, else `$HOME/.cache/<subpath>`.
pub fn cache_path(subpath: &str) -> PathBuf {
    resolve("XDG_CACHE_HOME", Some(".cache"), subpath)
}

/// `$XDG_RUNTIME_DIR/<subpath>`, else `/tmp/<subpath>`: the specification
/// gives the runtime directory no `$HOME` fallback.
pub fn runtime_path(subpath: &str) -> PathBuf {
    resolve("XDG_RUNTIME_DIR", None, subpath)
}

/// Whether [`cache_path`] has a real base: `XDG_CACHE_HOME` or `HOME` is
/// set. Without either it falls under `/tmp`, which a reboot clears.
pub(crate) fn cache_base_known() -> bool {
    std::env::var_os("XDG_CACHE_HOME").is_some() || std::env::var_os("HOME").is_some()
}

/// The XDG variable's directory, else `$HOME/<home_subdir>`, else under
/// `/tmp`.
fn resolve(variable: &str, home_subdir: Option<&str>, subpath: &str) -> PathBuf {
    if let Ok(base) = std::env::var(variable) {
        return PathBuf::from(base).join(subpath);
    }
    let Some(home_subdir) = home_subdir else {
        return PathBuf::from("/tmp").join(subpath);
    };
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(home_subdir).join(subpath),
        Err(_) => PathBuf::from("/tmp/zaino").join(home_subdir).join(subpath),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_path_ends_with_its_subpath() {
        assert!(config_path("zaino/zainod.toml").ends_with("zaino/zainod.toml"));
        assert!(cache_path("zaino").ends_with("zaino"));
        assert!(runtime_path("zaino/.cookie").ends_with("zaino/.cookie"));
    }
}
