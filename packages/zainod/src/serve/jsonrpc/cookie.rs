//! Cookie authentication for the JSON-RPC server, as zebrad's: a random
//! secret in an owner-only `.cookie` file, sent back as the Basic password.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::Path;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::RngCore as _;
use subtle::ConstantTimeEq as _;

/// The cookie file's name in its directory.
const FILE: &str = ".cookie";

/// The secret every request must carry while authentication is on.
#[derive(Clone)]
pub(super) struct Cookie(String);

impl Cookie {
    /// A fresh random secret.
    pub(super) fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        Self(STANDARD.encode(bytes))
    }

    /// Whether `password` is the secret, compared in constant time.
    pub(super) fn authenticate(&self, password: &str) -> bool {
        password.len() == self.0.len() && bool::from(password.as_bytes().ct_eq(self.0.as_bytes()))
    }

    /// Writes `__cookie__:<secret>` to `dir`, readable by the owner only;
    /// refuses to write through a symlink.
    pub(super) fn write_to(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE);
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(io::Error::other(format!(
                "cookie path {path:?} is a symlink"
            )));
        }
        owner_only_file(&path)?.write_all(format!("__cookie__:{}", self.0).as_bytes())
    }
}

/// A file only its owner can read and write (0600 whatever the umask).
fn owner_only_file(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

/// Removes the cookie file from `dir`.
pub(super) fn remove_from(dir: &Path) -> io::Result<()> {
    std::fs::remove_file(dir.join(FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cookie_file_holds_the_secret_and_only_the_secret_authenticates() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let cookie = Cookie::generate();
        cookie.write_to(dir.path()).expect("written");
        let written = std::fs::read_to_string(dir.path().join(FILE)).expect("readable");
        let secret = written.strip_prefix("__cookie__:").expect("the prefix");
        assert!(cookie.authenticate(secret));
        assert!(!cookie.authenticate("not the secret"));
        assert!(!Cookie::generate().authenticate(secret));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(FILE))
                .expect("exists")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        remove_from(dir.path()).expect("removed");
        assert!(!dir.path().join(FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_cookie_path_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"").expect("written");
        std::os::unix::fs::symlink(&target, dir.path().join(FILE)).expect("linked");
        assert!(Cookie::generate().write_to(dir.path()).is_err());
    }
}
