//! Writing the app's own files: the notes and the list of recent projects.

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
/// (`notes` holds `review.lock`, and `recent` runs only in the URL scheme handler, which wry
/// calls on the main thread). Separate processes use separate temporary files. A crash
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
/// absolute path, else `~/.config/postil`, where docs/design.md also puts `agents.json`.
///
/// # Errors
///
/// [`Error::NoHome`] when neither variable gives a directory.
pub(crate) fn config_dir() -> Result<PathBuf> {
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        && config.is_absolute()
    {
        return Ok(config.join("postil"));
    }
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or(Error::NoHome)?;
    Ok(PathBuf::from(home).join(".config").join("postil"))
}
