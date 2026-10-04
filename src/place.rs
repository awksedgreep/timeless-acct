//! Where a local store is kept when no directory is given.
//!
//! Two places, each standard: the host's, which the service writes, and a
//! user's own, for a collector run by hand. A collector writes the one that
//! fits who runs it; a reader reads its user's own if there is one, and
//! the host's otherwise. So starting, watching, and stopping need nothing
//! said about where.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::sink::embedded::METRICS_DB;

/// The host's store: the service's state directory.
pub const HOST: &str = "/var/lib/timeless-acct";

/// The group whose members may read the host's store.
pub const GROUP: &str = "timeless-acct";

/// The user's own store, if this user has one place for it: under
/// `$XDG_DATA_HOME`, or `~/.local/share`. Root's is the host's.
fn own() -> Option<PathBuf> {
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    own_of(
        root,
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
    )
}

fn own_of(root: bool, data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if root {
        return None;
    }
    // The specification says a relative $XDG_DATA_HOME is to be ignored.
    let base = data_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/share"))
        })?;
    Some(base.join("timeless-acct"))
}

/// The store a collector writes: the one it was given, or its user's own,
/// or the host's.
pub fn to_write(given: Option<PathBuf>) -> PathBuf {
    given.or_else(own).unwrap_or_else(|| PathBuf::from(HOST))
}

/// The store a reader reads: the one it was given, or its user's own if
/// there is one, or the host's.
pub fn to_read(given: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = given {
        return Ok(dir);
    }
    choose(own(), Path::new(HOST))
}

fn choose(own: Option<PathBuf>, host: &Path) -> Result<PathBuf> {
    if let Some(own) = own.as_ref().filter(|own| own.join(METRICS_DB).exists()) {
        return Ok(own.clone());
    }
    match host.join(METRICS_DB).try_exists() {
        Ok(true) => return Ok(host.to_path_buf()),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => bail!(
            "the host's store, {}, is for the {GROUP} group to read: \
             `sudo usermod -aG {GROUP} $USER`, then log in again",
            host.display()
        ),
        _ => {}
    }
    let mut places = String::new();
    if let Some(own) = &own {
        places.push_str(&format!("{}, nor ", own.display()));
    }
    bail!(
        "no store yet: nothing in {places}{}. `timeless-acct run` makes one, \
         or `--data-dir` says where another is",
        host.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;

    #[test]
    fn a_users_own_store_is_under_their_data_home() {
        assert_eq!(
            own_of(false, Some("/data/me".into()), Some("/home/me".into())),
            Some(PathBuf::from("/data/me/timeless-acct"))
        );
        assert_eq!(
            own_of(false, None, Some("/home/me".into())),
            Some(PathBuf::from("/home/me/.local/share/timeless-acct"))
        );
        // A relative data home is ignored, as the specification says.
        assert_eq!(
            own_of(false, Some("data".into()), Some("/home/me".into())),
            Some(PathBuf::from("/home/me/.local/share/timeless-acct"))
        );
        assert_eq!(own_of(false, None, None), None);
        // Root keeps the host's.
        assert_eq!(own_of(true, None, Some("/root".into())), None);
    }

    #[test]
    fn a_given_directory_is_the_one() {
        assert_eq!(
            to_write(Some("/srv/store".into())),
            PathBuf::from("/srv/store")
        );
        assert_eq!(
            to_read(Some("/srv/store".into())).unwrap(),
            PathBuf::from("/srv/store")
        );
    }

    #[test]
    fn a_reader_reads_its_own_store_and_the_hosts_otherwise() {
        let fixture = Fixture::new("place_choose");
        let own = fixture.path("own");
        let host = fixture.path("host");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&host).unwrap();

        let error = choose(Some(own.clone()), &host).unwrap_err().to_string();
        assert!(error.starts_with("no store yet"), "{error}");
        assert!(error.contains(&*own.to_string_lossy()), "{error}");

        std::fs::write(host.join(METRICS_DB), b"").unwrap();
        assert_eq!(choose(Some(own.clone()), &host).unwrap(), host);
        assert_eq!(choose(None, &host).unwrap(), host);

        std::fs::write(own.join(METRICS_DB), b"").unwrap();
        assert_eq!(choose(Some(own.clone()), &host).unwrap(), own);
    }

    #[test]
    fn a_host_store_kept_from_this_user_says_how_to_be_let_in() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root is let in everywhere
        }
        let fixture = Fixture::new("place_refused");
        let host = fixture.path("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join(METRICS_DB), b"").unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o000)).unwrap();
        let error = choose(None, &host).unwrap_err().to_string();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(error.contains("usermod -aG timeless-acct"), "{error}");
    }
}
