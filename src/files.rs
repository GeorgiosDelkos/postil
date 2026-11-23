//! Writing the app's own files: the notes, the recent projects, and the settings.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Writes `bytes` to a temporary file beside `file`, syncs it, and renames it over `file`, so
/// a crash leaves the old file or the new one and never a part. Creates the directory first
/// when it does not exist.
///
/// A failure before the rename leaves the old file. After the rename the directory is synced,
/// so the rename survives a power loss, and so is its parent, so a directory just created
/// survives too; a second new level above that is not synced (accepted: only the recent list
/// lives that deep in a new `~/.config`). A failure to sync is ignored: the new file is in
/// place and readers see it, and reporting a failed write would make the caller redo a write
/// that happened (a note saved twice).
///
/// At most one call per `file` may run at a time in one process; callers serialise
/// (`notes` holds `review.lock`; `recent` and `settings` run only on the main thread: from
/// `serve` before the event loop starts, then in the URL scheme handler, which wry calls on
/// that thread). Separate processes use separate temporary files. A crash
/// between creating the temporary and the rename leaves it behind; readers ignore it.
pub(crate) fn write_atomically(file: &Path, bytes: &[u8]) -> io::Result<()> {
    let invalid = |problem| io::Error::new(io::ErrorKind::InvalidInput, problem);
    let dir = file
        .parent()
        .ok_or_else(|| invalid("a file to write needs a directory"))?;
    let name = file
        .file_name()
        .ok_or_else(|| invalid("a file to write needs a name"))?;
    fs::create_dir_all(dir)?;

    let temporary = dir.join(format!(
        "{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let written = (|| {
        let mut out = File::create(&temporary)?;
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
}
