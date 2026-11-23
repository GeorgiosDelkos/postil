//! The projects opened recently, newest first, kept in `<config dir>/recent.json` for the
//! welcome screen.
//!
//! Unlike `review.json`, this list is the app's own convenience, not the engineer's work: a
//! file that cannot be read or parsed counts as an empty list, and the next open replaces
//! it. The exception is a file a newer Postil wrote: it reads as empty here and is never
//! replaced, so running an older build does not wipe the newer one's list. Two windows that
//! open projects at the same moment can lose one entry; the next open of that project adds
//! it back.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::files::write_atomically;

/// The format version this build reads and writes.
const VERSION: u32 = 1;

/// How many projects the list keeps. Arbitrary: enough for a week of switching between
/// repositories, short enough to scan at a glance.
const LIMIT: usize = 10;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecentFile {
    version: u32,
    /// Working tree roots, newest first, each at most once.
    projects: Vec<String>,
}

/// The one field every version of the file has; other fields are ignored here on purpose.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// What the file on disk holds, as far as this build can tell.
enum Stored {
    /// A list this build reads.
    List(Vec<String>),
    /// A list from a newer build: not read, and not to be replaced.
    Newer,
    /// No file, or one that cannot be read or parsed: an empty list, to be replaced.
    Nothing,
}

fn stored(file: &Path) -> Stored {
    let Ok(json) = fs::read_to_string(file) else {
        return Stored::Nothing;
    };
    match serde_json::from_str::<VersionOnly>(&json) {
        Ok(VersionOnly { version }) if version > VERSION => Stored::Newer,
        _ => serde_json::from_str::<RecentFile>(&json)
            .ok()
            .filter(|recent| recent.version == VERSION)
            .map_or(Stored::Nothing, |recent| Stored::List(recent.projects)),
    }
}

/// The recent projects' roots, newest first. Empty when the file is missing, unreadable, or
/// from a newer build (see the module docs).
pub(crate) fn list(file: &Path) -> Vec<String> {
    match stored(file) {
        Stored::List(projects) => projects,
        Stored::Newer | Stored::Nothing => Vec::new(),
    }
}

/// Puts `root` first in the list, removing an older entry for it and the oldest entries
/// beyond the limit.
///
/// A root that is not UTF-8 is not recorded: the list is JSON and the page could not show it.
/// Nothing is recorded over a list from a newer build either (see the module docs).
///
/// # Errors
///
/// [`Error::WriteRecent`] when the file cannot be written.
pub(crate) fn record(file: &Path, root: &Path) -> Result<()> {
    let Some(root) = root.to_str() else {
        return Ok(());
    };
    let mut projects = match stored(file) {
        Stored::List(projects) => projects,
        Stored::Nothing => Vec::new(),
        Stored::Newer => return Ok(()),
    };
    projects.retain(|project| project != root);
    projects.insert(0, root.to_owned());
    projects.truncate(LIMIT);

    let recent = RecentFile {
        version: VERSION,
        projects,
    };
    let mut json = serde_json::to_vec_pretty(&recent)
        .expect("a list of strings and a number always serializes");
    json.push(b'\n');
    write_atomically(file, &json).map_err(|source| Error::WriteRecent {
        path: file.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("postil-recent-{}-{name}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        dir.join("recent.json")
    }

    #[test]
    fn newest_first_each_once_and_at_most_the_limit() {
        let file = temp_file("order");
        for index in 0..12 {
            record(&file, Path::new(&format!("/p/{index}"))).unwrap();
        }
        record(&file, Path::new("/p/5")).unwrap();

        let projects = list(&file);
        assert_eq!(projects.len(), LIMIT);
        assert_eq!(projects[0], "/p/5");
        assert_eq!(projects[1], "/p/11");
        assert_eq!(
            projects.iter().filter(|project| *project == "/p/5").count(),
            1
        );
        assert!(
            !projects.contains(&"/p/0".to_owned()),
            "the oldest fell off"
        );
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }

    #[test]
    fn a_broken_file_reads_as_empty_and_is_replaced_by_the_next_open() {
        let file = temp_file("broken");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "{ not json").unwrap();
        assert_eq!(list(&file), Vec::<String>::new());

        record(&file, Path::new("/p/a")).unwrap();
        assert_eq!(list(&file), ["/p/a"]);
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }

    #[test]
    fn a_list_from_a_newer_build_is_neither_read_nor_replaced() {
        let file = temp_file("newer");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let newer = r#"{"version": 2, "projects": [{"path": "/p/a", "pinned": true}]}"#;
        fs::write(&file, newer).unwrap();

        assert_eq!(list(&file), Vec::<String>::new());
        record(&file, Path::new("/p/b")).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), newer);
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }

    #[test]
    fn a_list_from_an_older_version_number_is_replaced() {
        // Version 0 never existed; it stands for any version this build does not read but a
        // newer build would not have written either.
        let file = temp_file("older");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, r#"{"version": 0, "projects": ["/p/old"]}"#).unwrap();

        assert_eq!(list(&file), Vec::<String>::new());
        record(&file, Path::new("/p/b")).unwrap();
        assert_eq!(list(&file), ["/p/b"]);
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }

    #[test]
    fn the_file_is_pretty_json_ending_in_a_newline_byte_for_byte() {
        let file = temp_file("bytes");
        record(&file, Path::new("/p/a")).unwrap();
        record(&file, Path::new("/p/b")).unwrap();
        let expected =
            "{\n  \"version\": 1,\n  \"projects\": [\n    \"/p/b\",\n    \"/p/a\"\n  ]\n}\n";
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }
}
