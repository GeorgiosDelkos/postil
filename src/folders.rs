//! Which folders of the file list are open, kept per working tree in
//! `<git dir>/postil/folders.json`, so the tree opens as the engineer left it after a restart
//! (docs/design.md, milestone 1).
//!
//! Like `layout.json`, this is the app's own convenience, not the engineer's work: a file that
//! cannot be read or parsed counts as no folder open, and the next change replaces it. A file a
//! newer Postil wrote reads as none open and is never replaced. A folder that no longer exists
//! is kept, and simply opens nothing: the page draws only folders that are in the tree.
//!
//! Windows on one working tree do not share open folders live: each `PUT` replaces the whole
//! list, and the last window to save decides what the next one opens with.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::files::{Stored, load_versioned, save_json};

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

/// The open folders in `file`, or none when there is no file this build can use (module docs).
pub(crate) fn load(file: &Path) -> Folders {
    match load_versioned::<FoldersFile>(file, VERSION) {
        Stored::Current(stored) => Folders { open: stored.open },
        Stored::Newer | Stored::Nothing => Folders::default(),
    }
}

/// Saves `folders` to `file`, unless a newer build wrote the file (module docs).
///
/// # Errors
///
/// [`Error::WriteFolders`] when the file cannot be written.
pub(crate) fn save(file: &Path, folders: Folders) -> Result<()> {
    if matches!(load_versioned::<FoldersFile>(file, VERSION), Stored::Newer) {
        return Ok(());
    }
    let stored = FoldersFile {
        version: VERSION,
        open: folders.open,
    };
    save_json(file, &stored).map_err(|source| Error::WriteFolders {
        path: file.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use crate::testing::TempDir;

    #[test]
    fn saved_folders_are_read_back_and_a_bad_file_reads_as_none_and_is_replaced() {
        let dir = TempDir::new("folders-round");
        let file = dir.path().join("postil").join("folders.json");
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
    }

    #[test]
    fn folders_that_cannot_be_written_are_a_write_folders_error() {
        let dir = TempDir::new("folders-unwritable");
        let file = dir.path().join("postil").join("folders.json");
        // The path is a directory, so the write over it fails.
        fs::create_dir_all(&file).unwrap();
        match save(&file, Folders::default()) {
            Err(Error::WriteFolders { path, .. }) => assert_eq!(path, file),
            other => panic!("expected WriteFolders, got {other:?}"),
        }
    }

    #[test]
    fn a_file_from_a_newer_postil_reads_as_none_and_is_never_replaced() {
        let dir = TempDir::new("folders-newer");
        let file = dir.path().join("postil").join("folders.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let newer = r#"{"version": 2, "open": ["src"], "closed": ["docs"]}"#;
        fs::write(&file, newer).unwrap();
        assert_eq!(load(&file), Folders::default());
        save(&file, Folders::default()).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), newer);
    }

    #[test]
    fn the_file_is_pretty_json_ending_in_a_newline_byte_for_byte() {
        let dir = TempDir::new("folders-bytes");
        let file = dir.path().join("postil").join("folders.json");
        let folders = Folders {
            open: vec!["src".to_owned(), "src/acp".to_owned()],
        };
        save(&file, folders).unwrap();
        let expected =
            "{\n  \"version\": 1,\n  \"open\": [\n    \"src\",\n    \"src/acp\"\n  ]\n}\n";
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    }
}
