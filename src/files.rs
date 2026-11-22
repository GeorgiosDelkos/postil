//! Writing the app's own files: the notes and the list of recent projects.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Writes `bytes` to a temporary file beside `file`, syncs it, and renames it over `file`, so
/// a crash leaves the old file or the new one and never a part. Creates the directory first
/// when it does not exist.
///
/// A failure before the rename leaves the old file. After the rename the directory, and its
/// parent, are synced so the rename survives a power loss; a failure there is ignored,
/// because the new file is in place and readers see it, and reporting a failed write would
/// make the caller redo a write that happened (a note saved twice).
pub(crate) fn write_atomically(file: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = file.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a file to write needs a directory",
        )
    })?;
    fs::create_dir_all(dir)?;

    // The process id keeps two app windows writing the same file from sharing a temporary.
    let name = file
        .file_name()
        .map_or_else(Default::default, |name| name.to_string_lossy());
    let temporary = dir.join(format!("{name}.{}.tmp", std::process::id()));
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
