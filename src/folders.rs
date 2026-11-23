//! Which folders of the file list are open, kept per working tree in
//! `<git dir>/postil/folders.json`, so the tree opens as the engineer left it after a restart
//! (docs/design.md, milestone 1).
//!
//! Like `layout.json`, this is the app's own convenience, not the engineer's work: a file that
//! cannot be read or parsed counts as no folder open, and the next change replaces it. A file a
//! newer Postil wrote reads as none open and is never replaced. A folder that no longer exists
//! is kept, and simply opens nothing: the page draws only folders that are in the tree.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::files::write_atomically;

/// The format version this build reads and writes.
const VERSION: u32 = 1;

/// The open folders, as `GET /api/folders` gives them and `PUT /api/folders` takes them: paths
/// relative to the repository root, as the file list names them.
#[derive(Debug, Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[serde(deny_unknown_fields)]
pub(crate) struct Folders {
    open: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FoldersFile {
    version: u32,
    open: Vec<String>,
}

/// The one field every version of the file has; other fields are ignored here on purpose.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// Whether `json` was written by a newer build.
fn newer(json: &str) -> bool {
    serde_json::from_str::<VersionOnly>(json).is_ok_and(|stored| stored.version > VERSION)
}

/// The open folders in `file`, or none when there is no file this build can use (module docs).
pub(crate) fn load(file: &Path) -> Folders {
    let Ok(json) = fs::read_to_string(file) else {
        return Folders::default();
    };
    serde_json::from_str::<FoldersFile>(&json)
        .ok()
        .filter(|stored| stored.version == VERSION)
        .map_or_else(Folders::default, |stored| Folders { open: stored.open })
}

/// Saves `folders` to `file`, unless a newer build wrote the file (module docs).
///
/// # Errors
///
/// [`Error::WriteFolders`] when the file cannot be written.
pub(crate) fn save(file: &Path, folders: Folders) -> Result<()> {
    if fs::read_to_string(file).is_ok_and(|json| newer(&json)) {
        return Ok(());
    }
    let failed = |source| Error::WriteFolders {
        path: file.to_owned(),
        source,
    };
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(failed)?;
    }
    let stored = FoldersFile {
        version: VERSION,
        open: folders.open,
    };
    let mut json =
        serde_json::to_vec_pretty(&stored).expect("a version and a list of strings serialize");
    json.push(b'\n');
    write_atomically(file, &json).map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("postil-folders-{}-{name}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        dir.join("postil").join("folders.json")
    }

    #[test]
    fn saved_folders_are_read_back_and_a_bad_file_reads_as_none_and_is_replaced() {
        let file = temp_file("round");
        assert_eq!(load(&file), Folders::default(), "no file");
        let folders = || Folders {
            open: vec!["src".to_owned(), "src/acp".to_owned()],
        };
        save(&file, folders()).unwrap();
        assert_eq!(load(&file), folders());
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "version": 1, "open": ["src", "src/acp"] })
        );

        for broken in [
            "not json",
            r#"{"version": 1, "open": "src"}"#,
            r#"{"version": 0, "open": []}"#,
        ] {
            fs::write(&file, broken).unwrap();
            assert_eq!(load(&file), Folders::default(), "{broken}");
            save(&file, folders()).unwrap();
            assert_eq!(load(&file), folders(), "{broken} is replaced");
        }
        drop(fs::remove_dir_all(file.parent().unwrap().parent().unwrap()));
    }

    #[test]
    fn a_file_from_a_newer_postil_reads_as_none_and_is_never_replaced() {
        let file = temp_file("newer");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let newer = r#"{"version": 2, "open": ["src"], "closed": ["docs"]}"#;
        fs::write(&file, newer).unwrap();
        assert_eq!(load(&file), Folders::default());
        save(&file, Folders::default()).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), newer);
        drop(fs::remove_dir_all(file.parent().unwrap().parent().unwrap()));
    }
}
