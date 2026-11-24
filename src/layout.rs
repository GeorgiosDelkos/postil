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

use std::ops::RangeInclusive;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::files::{Stored, load_versioned, save_json};

/// The format version this build reads and writes.
const VERSION: u32 = 1;

/// The file list's width in CSS px. At 200 px its foot still shows "Settings ⌘," whole (about
/// 140 px with its icon and padding) and the filter a short path; the owner asked on 2026-11-23
/// for no narrower list. Past 480 px it takes room the code needs. The maximum is arbitrary.
pub(crate) const SIDEBAR: RangeInclusive<u32> = 200..=480;

/// The tray's width in CSS px. 280 px fits a note card's header; 720 px is half a 1440 px
/// window.
pub(crate) const TRAY: RangeInclusive<u32> = 280..=720;

/// The pane widths, each within its range: made only from a [`RawLayout`] that passes the
/// check, or as the default, so a width out of range never reaches the file or the page. The
/// code pane takes what is left of the window.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct Layout {
    /// The file list's width in CSS px, within [`SIDEBAR`].
    sidebar: u32,
    /// The tray's width in CSS px, within [`TRAY`].
    tray: u32,
}

/// The pane widths as the page sends them, before they are checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawLayout {
    sidebar: u32,
    tray: u32,
}

impl Default for Layout {
    /// The widths of the app before panes could be resized.
    fn default() -> Self {
        let raw = RawLayout {
            sidebar: 248,
            tray: 320,
        };
        Self::try_from(raw).expect("the default widths are within their ranges")
    }
}

impl TryFrom<RawLayout> for Layout {
    /// Which width is out of its range, in words for the page.
    type Error = String;

    fn try_from(raw: RawLayout) -> std::result::Result<Self, Self::Error> {
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
        let RawLayout { sidebar, tray } = raw;
        within("file list", sidebar, &SIDEBAR)?;
        within("tray", tray, &TRAY)?;
        Ok(Self { sidebar, tray })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutFile {
    version: u32,
    sidebar: u32,
    tray: u32,
}

/// The layout in `file`, or the default when there is none this build can use (module docs).
pub(crate) fn load(file: &Path) -> Layout {
    match load_versioned::<LayoutFile>(file, VERSION) {
        Stored::Current(LayoutFile { sidebar, tray, .. }) => {
            Layout::try_from(RawLayout { sidebar, tray }).unwrap_or_default()
        }
        Stored::Newer | Stored::Nothing => Layout::default(),
    }
}

/// Saves `layout` to `file`, unless a newer build wrote the file (module docs).
///
/// # Errors
///
/// [`Error::WriteLayout`] when the file cannot be written.
pub(crate) fn save(file: &Path, layout: &Layout) -> Result<()> {
    if matches!(load_versioned::<LayoutFile>(file, VERSION), Stored::Newer) {
        return Ok(());
    }
    let stored = LayoutFile {
        version: VERSION,
        sidebar: layout.sidebar,
        tray: layout.tray,
    };
    save_json(file, &stored).map_err(|source| Error::WriteLayout {
        path: file.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("postil-layout-{}-{name}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        dir.join("layout.json")
    }

    #[test]
    fn a_saved_layout_is_read_back_and_the_file_is_readable_json() {
        let file = temp_file("round");
        let layout = Layout::try_from(RawLayout {
            sidebar: 300,
            tray: 460,
        })
        .unwrap();
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
        let with = |sidebar, tray| Layout::try_from(RawLayout { sidebar, tray });
        for (sidebar, tray, expected) in [
            (
                199,
                320,
                "the file list is 199 px wide, outside 200 to 480 px",
            ),
            (
                481,
                320,
                "the file list is 481 px wide, outside 200 to 480 px",
            ),
            (248, 279, "the tray is 279 px wide, outside 280 to 720 px"),
            (248, 721, "the tray is 721 px wide, outside 280 to 720 px"),
        ] {
            assert_eq!(with(sidebar, tray), Err(expected.to_owned()));
        }
        save(&file, &with(200, 280).unwrap()).unwrap();
        assert_eq!(load(&file), with(200, 280).unwrap());
        save(&file, &with(480, 720).unwrap()).unwrap();
        assert_eq!(load(&file), with(480, 720).unwrap());
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
        let layout = Layout::try_from(RawLayout {
            sidebar: 200,
            tray: 320,
        })
        .unwrap();
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

    #[test]
    fn the_file_is_pretty_json_ending_in_a_newline_byte_for_byte() {
        let file = temp_file("bytes");
        save(&file, &Layout::default()).unwrap();
        let expected = "{\n  \"version\": 1,\n  \"sidebar\": 248,\n  \"tray\": 320\n}\n";
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
        drop(fs::remove_dir_all(file.parent().unwrap()));
    }
}
