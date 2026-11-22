//! Reading a repository by running the `git` binary.
//!
//! No git library is linked: the app needs a file list and a branch name, and `git` already
//! knows the ignore rules.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// A git working tree on disk.
#[derive(Debug)]
pub(crate) struct Repo {
    /// The top-level directory of the working tree.
    root: PathBuf,
}

impl Repo {
    /// Finds the working tree that contains `dir`.
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        let root = git(dir, &["rev-parse", "--show-toplevel"])?;
        Ok(Self {
            root: PathBuf::from(root.trim_end_matches('\n')),
        })
    }

    /// The directory name of the working tree, for display.
    pub(crate) fn name(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    /// The checked-out branch, or an empty string when `HEAD` is detached.
    pub(crate) fn branch(&self) -> Result<String> {
        let branch = git(&self.root, &["branch", "--show-current"])?;
        Ok(branch.trim_end().to_owned())
    }

    /// Every tracked file and every untracked file that is not ignored, sorted, relative to
    /// the root.
    pub(crate) fn files(&self) -> Result<Vec<String>> {
        let listing = git(
            &self.root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        let mut files: Vec<String> = listing.split_terminator('\0').map(str::to_owned).collect();
        files.sort_unstable();
        // During a merge conflict git lists a conflicted path once per stage.
        files.dedup();
        Ok(files)
    }

    /// Reads one file as text. `file` is a path relative to the root, as [`Repo::files`]
    /// returns it.
    pub(crate) fn read(&self, file: &str) -> Result<String> {
        // The path comes from the page. Only plain components are allowed, so the path itself
        // cannot climb above the root (`..`) or be absolute. A symlink inside the tree is
        // still followed, wherever it points: the reader sees what an editor would show.
        let inside = Path::new(file)
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
        if !inside || file.is_empty() {
            return Err(Error::OutsideRepository {
                path: file.to_owned(),
            });
        }

        let path = self.root.join(file);
        fs::read_to_string(&path).map_err(|source| Error::Read { path, source })
    }
}

/// Runs `git` in `dir` and returns what it printed.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(Error::SpawnGit)?;

    if !output.status.success() {
        return Err(Error::Git {
            command: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    // A file name that is not UTF-8 is shown with replacement characters and then fails to
    // open. Accepted: the page and its JSON are UTF-8, so such a name cannot round-trip.
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
