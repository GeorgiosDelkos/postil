//! Writing the app's own files: the notes, the recent projects, the settings, the layout and
//! the open folders.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

/// Writes `bytes` to a temporary file beside `file`, syncs it, and renames it over `file`, so
/// a crash leaves the old file or the new one and never a part. Creates the directory first
/// when it does not exist.
///
/// A symbolic link at `file` is followed (through further links, up to [`MAX_LINKS`]) and the
/// file it points at is replaced, so the link stays in place: an engineer who links
/// `settings.json` into a dotfiles repository keeps the link. A link to a missing file
/// creates that file. The new file takes the old one's permissions, so a `0600` file stays
/// private; a new file gets the default mode (`0666` less the umask). The owner is not
/// copied: the process can only create files it owns.
///
/// A failure before the rename leaves the old file. After the rename the directory is synced,
/// so the rename survives a power loss, and so is its parent, so a directory just created
/// survives too; a second new level above that is not synced (accepted: only the recent list
/// lives that deep in a new `~/.config`). A failure to sync is ignored: the new file is in
/// place and readers see it, and reporting a failed write would make the caller redo a write
/// that happened (a note saved twice).
///
/// Every call writes a temporary file of its own, so calls on one file at once, from this
/// process or another, never write into each other's; the last rename wins. A crash between
/// creating the temporary and the rename leaves it behind; readers ignore it.
pub(crate) fn write_atomically(file: &Path, bytes: &[u8]) -> io::Result<()> {
    // A unique name only: nothing is ordered by it.
    static WRITES: AtomicU64 = AtomicU64::new(0);

    let file = follow_links(file)?;
    let invalid = |problem| io::Error::new(io::ErrorKind::InvalidInput, problem);
    let dir = file
        .parent()
        .ok_or_else(|| invalid("a file to write needs a directory"))?;
    let name = file
        .file_name()
        .ok_or_else(|| invalid("a file to write needs a name"))?;
    fs::create_dir_all(dir)?;
    let permissions = match fs::metadata(&file) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    let temporary = dir.join(format!(
        "{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut out = File::create(&temporary)?;
        // Before any byte is written, so the text is never readable under a wider mode.
        if let Some(permissions) = permissions {
            out.set_permissions(permissions)?;
        }
        out.write_all(bytes)?;
        out.sync_all()?;
        fs::rename(&temporary, &file)
    })();
    if written.is_err() {
        // Best effort: a leftover temporary file is ignored by every reader.
        drop(fs::remove_file(&temporary));
    }
    written?;

    drop(File::open(dir).and_then(|dir| dir.sync_all()));
    if let Some(parent) = dir.parent() {
        drop(File::open(parent).and_then(|parent| parent.sync_all()));
    }
    Ok(())
}

/// The most symbolic links [`write_atomically`] follows from one path: macOS's `MAXSYMLINKS`
/// (`<sys/param.h>`), the count at which the system itself gives up on a path.
const MAX_LINKS: usize = 32;

/// `file` with the symbolic links at its end followed: the file it names, the missing file
/// its last link points at, or `file` itself when it is not a link.
///
/// After [`MAX_LINKS`] links it stops and returns the path it reached, still a link: the
/// system then refuses that path with ELOOP ("too many levels of symbolic links") at the
/// first use, so a cycle fails with the system's own message instead of looping here.
fn follow_links(file: &Path) -> io::Result<PathBuf> {
    let mut file = file.to_owned();
    for _ in 0..MAX_LINKS {
        let target = match fs::read_link(&file) {
            Ok(target) => target,
            // `readlink` fails with EINVAL on a path that is not a link and with ENOENT on a
            // missing one: either way this is the file to write.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
                ) =>
            {
                return Ok(file);
            }
            Err(error) => return Err(error),
        };
        // A relative target is relative to the link's directory; `join` keeps an absolute one
        // as it is.
        let dir = file
            .parent()
            .expect("a path that is a link has a parent, the empty path for a bare name");
        file = dir.join(target);
    }
    Ok(file)
}

/// The app's configuration directory: `$XDG_CONFIG_HOME/postil` when that variable holds an
/// absolute path, else `~/.config/postil`, which holds `recent.json`, `settings.json` and
/// `layout.json`.
///
/// # Errors
///
/// [`Error::NoHome`] when neither variable gives a directory.
pub(crate) fn config_dir() -> Result<PathBuf> {
    config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// `config_dir` from the two variables' values, so tests need not change the environment.
fn config_dir_from(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(config) = xdg_config_home.map(PathBuf::from)
        && config.is_absolute()
    {
        return Ok(config.join("postil"));
    }
    let home = home.filter(|home| !home.is_empty()).ok_or(Error::NoHome)?;
    Ok(PathBuf::from(home).join(".config").join("postil"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_prefers_an_absolute_xdg_config_home_and_falls_back_to_home() {
        let dir = |xdg: Option<&str>, home: Option<&str>| {
            config_dir_from(xdg.map(OsString::from), home.map(OsString::from))
                .map_err(|error| error.to_string())
        };
        assert_eq!(dir(Some("/x"), Some("/h")), Ok(PathBuf::from("/x/postil")));
        assert_eq!(
            dir(Some("relative"), Some("/h")),
            Ok(PathBuf::from("/h/.config/postil")),
            "a relative XDG_CONFIG_HOME is ignored, as the XDG spec says"
        );
        assert_eq!(
            dir(None, Some("/h")),
            Ok(PathBuf::from("/h/.config/postil"))
        );
        for home in [None, Some("")] {
            let expected = Err("cannot find the home directory (is HOME set?)".to_owned());
            assert_eq!(dir(None, home), expected);
        }
    }

    #[test]
    fn a_path_without_a_file_name_is_refused_before_anything_is_written() {
        let error = write_atomically(Path::new("/"), b"x").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    /// An empty directory in the temporary directory. `name` must differ between tests.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("postil-files-{}-{name}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_link_to_a_missing_file_creates_that_file_and_stays_a_link() {
        let dir = temp_dir("dangling");
        let link = dir.join("settings.json");
        std::os::unix::fs::symlink("real.json", &link).unwrap();

        write_atomically(&link, b"new").unwrap();

        assert_eq!(fs::read_link(&link).unwrap(), Path::new("real.json"));
        assert_eq!(fs::read(dir.join("real.json")).unwrap(), b"new");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cycle_of_links_is_refused_and_nothing_is_written() {
        let dir = temp_dir("cycle");
        std::os::unix::fs::symlink("b", dir.join("a")).unwrap();
        std::os::unix::fs::symlink("a", dir.join("b")).unwrap();

        let error = write_atomically(&dir.join("a"), b"x").unwrap_err();

        assert!(error.to_string().contains("symbolic links"), "{error}");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2, "only the two links");
        fs::remove_dir_all(&dir).unwrap();
    }
}
