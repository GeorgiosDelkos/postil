//! The widths of the window's panes, kept per Mac in `<config dir>/layout.json`, as the owner
//! chose on 2026-11-23 over a layout per project.
//!
//! Like `recent.json`, this is the app's own convenience, not the engineer's work: a file that
//! cannot be read, parsed, or used counts as the default layout, and the next change replaces
//! it. A file a newer Postil wrote reads as the default and is never replaced. It is kept apart
//! from `settings.json` on purpose: a divider dragged while the settings dialog is open must not
//! make the dialog's next save conflict with the settings file's revision (`settings.rs`).
//!
//! Windows do not share widths live: each draws what it loaded, and the last one to save
//! decides what the next window opens with.

use std::fs;
use std::ops::RangeInclusive;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::files::write_atomically;

/// The format version this build reads and writes.
const VERSION: u32 = 1;

/// The file list's width in CSS px. At 200 px its foot still shows "Settings ⌘," whole (about
/// 140 px with its icon and padding) and the filter a short path; the owner asked on 2026-11-23
/// for no narrower list. Past 480 px it takes room the code needs. The maximum is arbitrary.
pub(crate) const SIDEBAR: RangeInclusive<u32> = 200..=480;

/// The tray's width in CSS px. 280 px fits a note card's header; 720 px is half a 1440 px
/// window.
pub(crate) const TRAY: RangeInclusive<u32> = 280..=720;

/// The pane widths. The code pane takes what is left of the window.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[serde(deny_unknown_fields)]
pub(crate) struct Layout {
    /// The file list's width in CSS px.
    sidebar: u32,
    /// The tray's width in CSS px.
    tray: u32,
}

impl Default for Layout {
    /// The widths of the app before panes could be resized.
    fn default() -> Self {
        Self {
            sidebar: 248,
            tray: 320,
        }
    }
}

impl Layout {
    /// Checks the widths against [`SIDEBAR`] and [`TRAY`]; the error says which is out.
    fn check(&self) -> std::result::Result<(), String> {
        let within = |name: &str, width: u32, range: &RangeInclusive<u32>| {
            if range.contains(&width) {
                Ok(())
            } else {
                Err(format!(
                    "the {name} is {width} px wide, outside {} to {} px",
                    range.start(),
                    range.end()
                ))
            }
        };
        within("file list", self.sidebar, &SIDEBAR)?;
        within("tray", self.tray, &TRAY)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutFile {
    version: u32,
    sidebar: u32,
    tray: u32,
}

/// The one field every version of the file has; other fields are ignored here on purpose.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// What the file on disk holds, as far as this build can tell.
enum Stored {
    Layout(Layout),
    /// A file from a newer build: not read, and not to be replaced.
    Newer,
    /// No file, or one that cannot be read, parsed, or used.
    Nothing,
}

fn stored(file: &Path) -> Stored {
    let Ok(json) = fs::read_to_string(file) else {
        return Stored::Nothing;
    };
    match serde_json::from_str::<VersionOnly>(&json) {
        Ok(VersionOnly { version }) if version > VERSION => Stored::Newer,
        _ => serde_json::from_str::<LayoutFile>(&json)
            .ok()
            .filter(|stored| stored.version == VERSION)
            .map(|stored| Layout {
                sidebar: stored.sidebar,
                tray: stored.tray,
            })
            .filter(|layout| layout.check().is_ok())
            .map_or(Stored::Nothing, Stored::Layout),
    }
}

/// The layout in `file`, or the default when there is none this build can use (module docs).
pub(crate) fn load(file: &Path) -> Layout {
    match stored(file) {
        Stored::Layout(layout) => layout,
        Stored::Newer | Stored::Nothing => Layout::default(),
    }
}

/// Saves `layout` to `file`, unless a newer build wrote the file (module docs).
///
/// # Errors
///
/// [`Error::BadRequest`] when a width is out of range; [`Error::WriteLayout`] when the file
/// cannot be written.
pub(crate) fn save(file: &Path, layout: &Layout) -> Result<()> {
    layout.check().map_err(Error::BadRequest)?;
    if matches!(stored(file), Stored::Newer) {
        return Ok(());
    }
    let stored = LayoutFile {
        version: VERSION,
        sidebar: layout.sidebar,
        tray: layout.tray,
    };
    let mut json =
        serde_json::to_vec_pretty(&stored).expect("a version and two numbers always serialize");
    json.push(b'\n');
    write_atomically(file, &json).map_err(|source| Error::WriteLayout {
        path: file.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("postil-layout-{}-{name}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        dir.join("layout.json")
    }

    #[test]
    fn a_saved_layout_is_read_back_and_the_file_is_readable_json() {
        let file = temp_file("round");
        let layout = Layout {
            sidebar: 300,
            tray: 460,
        };
        save(&file, &layout).unwrap();
        assert_eq!(load(&file), layout);
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "version": 1, "sidebar": 300, "tray": 460 })
        );
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }

    #[test]
    fn widths_outside_their_range_are_refused_and_their_limits_allowed() {
        let file = temp_file("range");
        let with = |sidebar, tray| Layout { sidebar, tray };
        for (sidebar, tray, words) in [
            (199, 320, "file list"),
            (481, 320, "file list"),
            (248, 279, "tray"),
            (248, 721, "tray"),
        ] {
            match save(&file, &with(sidebar, tray)) {
                Err(Error::BadRequest(problem)) => assert!(problem.contains(words), "{problem}"),
                other => panic!("{sidebar}/{tray}: expected BadRequest, got {other:?}"),
            }
        }
        assert!(!file.exists(), "nothing refused was written");
        save(&file, &with(200, 280)).unwrap();
        save(&file, &with(480, 720)).unwrap();
        assert_eq!(load(&file), with(480, 720));
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }

    #[test]
    fn a_missing_broken_or_out_of_range_file_reads_as_the_default_and_is_replaced() {
        let file = temp_file("broken");
        assert_eq!(load(&file), Layout::default(), "no file");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        for contents in [
            "{ not json",
            r#"{"version": 1, "sidebar": 10, "tray": 320}"#,
            r#"{"version": 0, "sidebar": 248, "tray": 320}"#,
            r#"{"version": 1, "sidebar": 248, "tray": 320, "x": 1}"#,
        ] {
            fs::write(&file, contents).unwrap();
            assert_eq!(load(&file), Layout::default(), "{contents}");
        }
        let layout = Layout {
            sidebar: 200,
            ..Layout::default()
        };
        save(&file, &layout).unwrap();
        assert_eq!(load(&file), layout);
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }

    #[test]
    fn a_layout_that_cannot_be_written_is_a_write_layout_error() {
        let file = temp_file("unwritable");
        // The path is a directory, so the write over it fails.
        fs::create_dir_all(&file).unwrap();
        match save(&file, &Layout::default()) {
            Err(Error::WriteLayout { path, .. }) => assert_eq!(path, file),
            other => panic!("expected WriteLayout, got {other:?}"),
        }
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }

    #[test]
    fn a_layout_from_a_newer_build_is_neither_read_nor_replaced() {
        let file = temp_file("newer");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let newer = r#"{"version": 2, "panes": [248, 600, 320]}"#;
        fs::write(&file, newer).unwrap();
        assert_eq!(load(&file), Layout::default());
        save(&file, &Layout::default()).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), newer);
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }
}
