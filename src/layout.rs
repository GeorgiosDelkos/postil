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

/// The format version this build reads and writes. Version 2 added the map's width; a version 1
/// file still reads, with the map at its default width.
const VERSION: u32 = 2;

/// The file list's width in CSS px. At 200 px its foot still shows "Settings ⌘," whole (about
/// 140 px with its icon and padding) and the filter a short path; the owner asked on 2026-11-23
/// for no narrower list. Past 480 px it takes room the code needs. The maximum is arbitrary.
pub(crate) const SIDEBAR: RangeInclusive<u32> = 200..=480;

/// The tray's width in CSS px. 280 px fits a note card's header; 720 px is half a 1440 px
/// window.
pub(crate) const TRAY: RangeInclusive<u32> = 280..=720;

/// The map's width in CSS px. 320 px fits two diagram nodes side by side (184 px each at the
/// default zoom, `ui/diagram.js`, with the pan margin); 960 px is two thirds of a 1440 px
/// window. Kept in step with `MAP_MIN` in `ui/panes.js`.
pub(crate) const MAP: RangeInclusive<u32> = 320..=960;

/// The map's width before it is resized, and the width a version 1 file, which has none,
/// reads with: at 1440 px it leaves the code pane about 440 px beside the default file list
/// and tray. Arbitrary within that.
const DEFAULT_MAP: u32 = 400;

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
    /// The map's width in CSS px, within [`MAP`], kept while the map is closed.
    map: u32,
}

/// The pane widths as the page sends them, before they are checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawLayout {
    sidebar: u32,
    tray: u32,
    map: u32,
}

impl Default for Layout {
    /// The widths of the app before panes could be resized.
    fn default() -> Self {
        let raw = RawLayout {
            sidebar: 248,
            tray: 320,
            map: DEFAULT_MAP,
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
        let RawLayout { sidebar, tray, map } = raw;
        within("file list", sidebar, &SIDEBAR)?;
        within("tray", tray, &TRAY)?;
        within("map", map, &MAP)?;
        Ok(Self { sidebar, tray, map })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutFile {
    version: u32,
    sidebar: u32,
    tray: u32,
    map: u32,
}

/// A version 1 file: the widths before the map.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutFileV1 {
    #[expect(
        dead_code,
        reason = "serde checks it; load_versioned already read it as 1"
    )]
    version: u32,
    sidebar: u32,
    tray: u32,
}

/// The layout in `file`, or the default when there is none this build can use (module docs).
pub(crate) fn load(file: &Path) -> Layout {
    let raw = match load_versioned::<LayoutFile>(file, VERSION) {
        Stored::Current(LayoutFile {
            sidebar, tray, map, ..
        }) => RawLayout { sidebar, tray, map },
        Stored::Newer => return Layout::default(),
        // `load_versioned` reads only the current version, so an older file comes back as
        // nothing: read it again as version 1, which keeps the engineer's widths.
        Stored::Nothing => match load_versioned::<LayoutFileV1>(file, 1) {
            Stored::Current(LayoutFileV1 { sidebar, tray, .. }) => RawLayout {
                sidebar,
                tray,
                map: DEFAULT_MAP,
            },
            Stored::Newer | Stored::Nothing => return Layout::default(),
        },
    };
    Layout::try_from(raw).unwrap_or_default()
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
        map: layout.map,
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

    use crate::testing::TempDir;

    fn layout(sidebar: u32, tray: u32, map: u32) -> std::result::Result<Layout, String> {
        Layout::try_from(RawLayout { sidebar, tray, map })
    }

    #[test]
    fn a_saved_layout_is_read_back_and_the_file_is_readable_json() {
        let dir = TempDir::new("layout-round");
        let file = dir.path().join("layout.json");
        let layout = layout(300, 460, 520).unwrap();
        save(&file, &layout).unwrap();
        assert_eq!(load(&file), layout);
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "version": 2, "sidebar": 300, "tray": 460, "map": 520 })
        );
    }

    #[test]
    fn widths_outside_their_range_are_refused_and_their_limits_allowed() {
        let dir = TempDir::new("layout-range");
        let file = dir.path().join("layout.json");
        for (sidebar, tray, map, expected) in [
            (
                199,
                320,
                400,
                "the file list is 199 px wide, outside 200 to 480 px",
            ),
            (
                481,
                320,
                400,
                "the file list is 481 px wide, outside 200 to 480 px",
            ),
            (
                248,
                279,
                400,
                "the tray is 279 px wide, outside 280 to 720 px",
            ),
            (
                248,
                721,
                400,
                "the tray is 721 px wide, outside 280 to 720 px",
            ),
            (
                248,
                320,
                319,
                "the map is 319 px wide, outside 320 to 960 px",
            ),
            (
                248,
                320,
                961,
                "the map is 961 px wide, outside 320 to 960 px",
            ),
        ] {
            assert_eq!(layout(sidebar, tray, map), Err(expected.to_owned()));
        }
        save(&file, &layout(200, 280, 320).unwrap()).unwrap();
        assert_eq!(load(&file), layout(200, 280, 320).unwrap());
        save(&file, &layout(480, 720, 960).unwrap()).unwrap();
        assert_eq!(load(&file), layout(480, 720, 960).unwrap());
    }

    #[test]
    fn a_missing_broken_or_out_of_range_file_reads_as_the_default_and_is_replaced() {
        let dir = TempDir::new("layout-broken");
        let file = dir.path().join("layout.json");
        assert_eq!(load(&file), Layout::default(), "no file");
        for contents in [
            "{ not json",
            r#"{"version": 2, "sidebar": 10, "tray": 320, "map": 400}"#,
            r#"{"version": 2, "sidebar": 248, "tray": 320, "map": 100}"#,
            r#"{"version": 2, "sidebar": 248, "tray": 320}"#,
            r#"{"version": 1, "sidebar": 10, "tray": 320}"#,
            r#"{"version": 1, "sidebar": 248, "tray": 320, "map": 500}"#,
            r#"{"version": 0, "sidebar": 248, "tray": 320}"#,
            r#"{"version": 2, "sidebar": 248, "tray": 320, "map": 400, "x": 1}"#,
        ] {
            fs::write(&file, contents).unwrap();
            assert_eq!(load(&file), Layout::default(), "{contents}");
        }
        let layout = layout(200, 320, 400).unwrap();
        save(&file, &layout).unwrap();
        assert_eq!(load(&file), layout);
    }

    #[test]
    fn a_version_1_layout_keeps_its_widths() {
        let dir = TempDir::new("layout-v1");
        let file = dir.path().join("layout.json");
        fs::write(&file, r#"{"version": 1, "sidebar": 300, "tray": 440}"#).unwrap();

        let read = load(&file);

        assert_eq!(read, layout(300, 440, 400).unwrap());
        save(&file, &read).unwrap();
        let expected =
            "{\n  \"version\": 2,\n  \"sidebar\": 300,\n  \"tray\": 440,\n  \"map\": 400\n}\n";
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    }

    #[test]
    fn a_layout_that_cannot_be_written_is_a_write_layout_error() {
        let dir = TempDir::new("layout-unwritable");
        let file = dir.path().join("layout.json");
        // The path is a directory, so the write over it fails.
        fs::create_dir_all(&file).unwrap();
        match save(&file, &Layout::default()) {
            Err(Error::WriteLayout { path, .. }) => assert_eq!(path, file),
            other => panic!("expected WriteLayout, got {other:?}"),
        }
    }

    #[test]
    fn a_layout_from_a_newer_build_is_neither_read_nor_replaced() {
        let dir = TempDir::new("layout-newer");
        let file = dir.path().join("layout.json");
        let newer = r#"{"version": 3, "panes": [248, 600, 320]}"#;
        fs::write(&file, newer).unwrap();
        assert_eq!(load(&file), Layout::default());
        save(&file, &Layout::default()).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), newer);
    }

    #[test]
    fn the_file_is_pretty_json_ending_in_a_newline_byte_for_byte() {
        let dir = TempDir::new("layout-bytes");
        let file = dir.path().join("layout.json");
        save(&file, &Layout::default()).unwrap();
        let expected =
            "{\n  \"version\": 2,\n  \"sidebar\": 248,\n  \"tray\": 320,\n  \"map\": 400\n}\n";
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    }
}
