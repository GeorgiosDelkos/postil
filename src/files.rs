//! Writing the app's own files: the notes, the recent projects, the settings, the layout and
//! the open folders.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
/// creates that file, and its directory when that is missing too. An error through a link
/// names the file it points at, since that is the directory that must be writable. The new
/// file takes the old one's permissions, from its creation, so a `0600` file stays private;
/// a new file gets the default mode (`0666` less the umask). Neither the owner nor the group
/// is copied: the process can only create files it owns, and on macOS a new file takes its
/// directory's group, so the group bits of a `0640` file then apply to that group.
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
    let real = follow_links(file);
    let written = replace(&real, bytes);
    if real == file {
        return written;
    }
    written.map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", real.display())))
}

/// The number in the next temporary file's name, so every call in this process uses a name of
/// its own. A unique name only: nothing is ordered by it.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// [`write_atomically`] for a `file` that is not a link.
fn replace(file: &Path, bytes: &[u8]) -> io::Result<()> {
    let invalid = |problem| io::Error::new(io::ErrorKind::InvalidInput, problem);
    let dir = file
        .parent()
        .ok_or_else(|| invalid("a file to write needs a directory"))?;
    let name = file
        .file_name()
        .ok_or_else(|| invalid("a file to write needs a name"))?;
    fs::create_dir_all(dir)?;
    let permissions = match fs::metadata(file) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    // Access is checked when a file is opened, so the temporary is created with the old mode
    // rather than narrowed after: another user cannot open it in between. `0o666` is what
    // `File::create` uses for a new file.
    let mode = permissions
        .as_ref()
        .map_or(0o666, |permissions| permissions.mode() & 0o7777);

    // Creating the temporary claims its name. A leftover of a crashed process that had this
    // pid holds a name already, so the next one is taken: the leftover is never reused with
    // its own mode, and never fails the write.
    let (temporary, mut out) = loop {
        let temporary = dir.join(format!(
            "{}.{}.{}.tmp",
            name.to_string_lossy(),
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        ));
        let created = File::options()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary);
        match created {
            Ok(out) => break (temporary, out),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    };
    let written = (|| {
        // The umask narrowed the mode above; this restores the old file's exact bits.
        if let Some(permissions) = permissions {
            out.set_permissions(permissions)?;
        }
        out.write_all(bytes)?;
        out.sync_all()?;
        fs::rename(&temporary, file)
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
fn follow_links(file: &Path) -> PathBuf {
    let mut file = file.to_owned();
    for _ in 0..MAX_LINKS {
        // `readlink` fails with EINVAL on a path that is not a link and with ENOENT on a
        // missing one: this is the file to write. Any other failure (no permission to search
        // a directory on the way) comes back from the write itself, naming the same path.
        let Ok(target) = fs::read_link(&file) else {
            return file;
        };
        // A relative target is relative to the link's directory; `join` keeps an absolute one
        // as it is.
        let dir = file
            .parent()
            .expect("a path that is a link has a parent, the empty path for a bare name");
        file = dir.join(target);
    }
    file
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
    fn a_failed_write_through_a_link_names_the_file_it_points_at() {
        let dir = temp_dir("blocked-target");
        let target_dir = dir.join("dotfiles");
        fs::create_dir(&target_dir).unwrap();
        let link = dir.join("settings.json");
        std::os::unix::fs::symlink("dotfiles/settings.json", &link).unwrap();
        fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let error = write_atomically(&link, b"new").unwrap_err();
        fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o755)).unwrap();

        let real = target_dir.join("settings.json");
        assert!(
            error
                .to_string()
                .starts_with(&format!("{}: ", real.display())),
            "{error}"
        );
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
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

    #[test]
    fn leftover_temporaries_with_this_pid_do_not_fail_the_write() {
        let dir = temp_dir("leftover");
        let file = dir.join("settings.json");
        // Leftovers of a crashed process that had this pid, under the next names this process
        // would use. Tests run in parallel and also take numbers, so a stretch ahead is taken.
        let next = WRITES.load(Ordering::Relaxed);
        for number in next..next + 64 {
            let name = format!("settings.json.{}.{number}.tmp", std::process::id());
            fs::write(dir.join(name), b"stale").unwrap();
        }

        write_atomically(&file, b"new").unwrap();

        assert_eq!(fs::read(&file).unwrap(), b"new");
        fs::remove_dir_all(&dir).unwrap();
    }
}
