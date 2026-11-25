//! Reading and writing the app's own files: the notes, the recent projects, the settings, the
//! layout and the open folders.
//!
//! Each is a JSON object with a `version` field, written as pretty JSON ending in a newline by
//! [`save_json`]. [`version_of`] reads that field alone, so a file in a newer format is told
//! apart before its new fields fail the parse. The recent projects, the layout and the open
//! folders are the app's own conveniences and read through [`load_versioned`], which never
//! fails; the settings and the notes are the engineer's and say why a file cannot be used.
//! The review file is changed under [`lock`], on a lock file beside it, from its read to its
//! write, so two windows cannot drop each other's changes.

use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// What a versioned file holds, as far as this build can tell ([`load_versioned`]).
#[derive(Debug)]
pub(crate) enum Stored<T> {
    /// A file of the version this build writes.
    Current(T),
    /// A file a newer build wrote: not read, and not to be replaced.
    Newer,
    /// No file, or one that cannot be read or parsed, or of an older version: the next save
    /// replaces it.
    Nothing,
}

/// Reads `file` as a `T` when its `version` is `version`, the one this build writes.
pub(crate) fn load_versioned<T: DeserializeOwned>(file: &Path, version: u32) -> Stored<T> {
    let Ok(json) = fs::read_to_string(file) else {
        return Stored::Nothing;
    };
    match version_of(json.as_bytes()) {
        Ok(stored) if stored > version => Stored::Newer,
        Ok(stored) if stored == version => {
            serde_json::from_str(&json).map_or(Stored::Nothing, Stored::Current)
        }
        Ok(_) | Err(_) => Stored::Nothing,
    }
}

/// The `version` field of the JSON object `json`. Every other field is ignored, so this reads
/// a file, or a run log's header line, of any version.
///
/// # Errors
///
/// When `json` is neither an object with a `version` that is a `u32` nor an array whose first
/// element is one (serde's derive accepts both).
pub(crate) fn version_of(json: &[u8]) -> serde_json::Result<u32> {
    // The name shows in serde's message for a value that is not an object ("expected struct
    // VersionOnly"), which the page shows for a broken review file or run log: renaming it
    // changes that message.
    #[derive(Deserialize)]
    struct VersionOnly {
        version: u32,
    }
    serde_json::from_slice::<VersionOnly>(json).map(|stored| stored.version)
}

/// Writes `value` to `file` as pretty JSON ending in a newline, through [`write_atomically`].
///
/// # Errors
///
/// As [`write_atomically`].
pub(crate) fn save_json<T: Serialize>(file: &Path, value: &T) -> io::Result<()> {
    let mut json = serde_json::to_vec_pretty(value).expect(
        "serde_json fails only on a map key that is not a string or a failing Serialize impl, \
         and the app's files have neither",
    );
    json.push(b'\n');
    write_atomically(file, &json)
}

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
fn write_atomically(file: &Path, bytes: &[u8]) -> io::Result<()> {
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

/// How long [`lock`] waits for another process to release a lock. A writer holds one while it
/// reads, changes and writes its file, milliseconds; arbitrary, far above that and short
/// enough that a stuck process shows as an error rather than as page requests that never
/// answer.
pub(crate) const LOCK_WAIT: Duration = Duration::from_secs(2);

/// How often [`lock`] tries again while it waits. Arbitrary: small against [`LOCK_WAIT`], and
/// rare enough that waiting costs no noticeable CPU.
const LOCK_RETRY: Duration = Duration::from_millis(20);

/// Takes the exclusive lock on the file at `path`, creating it and its directory when they
/// are missing, and waiting up to [`LOCK_WAIT`] for another window or process to release it.
/// The lock is released when the returned file is dropped. Blocks the calling thread while it
/// waits.
///
/// Run and thread locks do not use it (`run/log.rs`, `take_lock`): a window that runs an
/// agent holds them for the whole run, so a send waits only out a forked child's copy (fork
/// to exec), 200 ms at most, and then reports the other run.
///
/// # Errors
///
/// [`TryLockError::WouldBlock`] when the lock is still held after [`LOCK_WAIT`];
/// [`TryLockError::Error`] when the file cannot be created, opened or locked.
pub(crate) fn lock(path: &Path) -> std::result::Result<File, TryLockError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(TryLockError::Error)?;
    }
    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(TryLockError::Error)?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(LOCK_RETRY);
            }
            Err(error) => return Err(error),
        }
    }
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
    use crate::testing::{TempDir, skip_as_root};

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

    #[test]
    fn a_link_to_a_missing_file_creates_that_file_and_stays_a_link() {
        let dir = TempDir::new("files-dangling");
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink("real.json", &link).unwrap();

        write_atomically(&link, b"new").unwrap();

        assert_eq!(fs::read_link(&link).unwrap(), Path::new("real.json"));
        assert_eq!(fs::read(dir.path().join("real.json")).unwrap(), b"new");
    }

    #[test]
    fn a_failed_write_through_a_link_names_the_file_it_points_at() {
        if skip_as_root() {
            return;
        }
        let dir = TempDir::new("files-blocked-target");
        let target_dir = dir.path().join("dotfiles");
        fs::create_dir(&target_dir).unwrap();
        let link = dir.path().join("settings.json");
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
    }

    #[test]
    fn a_cycle_of_links_is_refused_and_nothing_is_written() {
        let dir = TempDir::new("files-cycle");
        std::os::unix::fs::symlink("b", dir.path().join("a")).unwrap();
        std::os::unix::fs::symlink("a", dir.path().join("b")).unwrap();

        let error = write_atomically(&dir.path().join("a"), b"x").unwrap_err();

        assert!(error.to_string().contains("symbolic links"), "{error}");
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            2,
            "only the two links"
        );
    }

    #[test]
    fn leftover_temporaries_with_this_pid_do_not_fail_the_write() {
        let dir = TempDir::new("files-leftover");
        let file = dir.path().join("settings.json");
        // Leftovers of a crashed process that had this pid, under the next names this process
        // would use. Tests run in parallel and also take numbers, so a stretch ahead is taken.
        let next = WRITES.load(Ordering::Relaxed);
        for number in next..next + 64 {
            let name = format!("settings.json.{}.{number}.tmp", std::process::id());
            fs::write(dir.path().join(name), b"stale").unwrap();
        }

        write_atomically(&file, b"new").unwrap();

        assert_eq!(fs::read(&file).unwrap(), b"new");
    }

    /// A versioned file of the shape [`load_versioned`] is given in the app.
    #[derive(Debug, PartialEq, Eq, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Probe {
        version: u32,
        value: u32,
    }

    /// What `load_versioned` makes of `contents` (`None`: no file), for a build at version 2.
    fn loaded(name: &str, contents: Option<&str>) -> Stored<Probe> {
        let dir = TempDir::new(&format!("files-{name}"));
        let file = dir.path().join("probe.json");
        if let Some(contents) = contents {
            fs::write(&file, contents).unwrap();
        }
        load_versioned(&file, 2)
    }

    #[test]
    fn a_file_of_this_version_is_read_and_a_newer_one_is_newer_whatever_its_fields() {
        assert!(matches!(
            loaded("current", Some(r#"{"version": 2, "value": 7}"#)),
            Stored::Current(Probe {
                version: 2,
                value: 7
            })
        ));
        for newer in [
            r#"{"version": 3, "value": 7}"#,
            r#"{"version": 3, "renamed": [1, 2]}"#,
        ] {
            assert!(
                matches!(loaded("newer", Some(newer)), Stored::Newer),
                "{newer}"
            );
        }
    }

    #[test]
    fn a_missing_unreadable_broken_or_older_file_is_nothing() {
        for (name, contents) in [
            ("missing", None),
            ("broken", Some("{ not json")),
            ("no-version", Some(r#"{"value": 7}"#)),
            ("older", Some(r#"{"version": 1, "value": 7}"#)),
            (
                "unknown-field",
                Some(r#"{"version": 2, "value": 7, "x": 1}"#),
            ),
        ] {
            assert!(matches!(loaded(name, contents), Stored::Nothing), "{name}");
        }

        // A directory where the file should be: it exists, and reading it fails.
        let dir = TempDir::new("files-unreadable");
        let file = dir.path().join("probe.json");
        fs::create_dir(&file).unwrap();
        assert!(matches!(load_versioned::<Probe>(&file, 2), Stored::Nothing));
    }
}
