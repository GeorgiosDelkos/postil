//! Review notes and the file that stores them.
//!
//! A note is anchored by its snippet: the text of its lines when it was written. Its line
//! numbers are a hint. Whenever the notes are read, each one is moved to the place where its
//! snippet is now, nearest the old place, or marked outdated when the snippet is gone.
//!
//! The notes live in `<git dir>/postil/review.json` (docs/design.md, "Data structures"). That
//! file is the only copy: every request reads it, so an agent or a script that edits it is
//! seen on the next request, and the page holds nothing that a reload would lose. The module
//! offers two operations, [`current`] and [`add`], each of which does every step (read,
//! relocate, and for `add` write) so that no caller can skip one.
//!
//! Only [`add`] writes. Reading the notes, which the page does whenever the window regains
//! focus, never rewrites the file, so it cannot overwrite an edit made by hand. The cost: the
//! stored lines catch up with the code only on the next added note, and until then each read
//! relocates from the older lines. Writers are not locked against each other, so an edit
//! made by someone else between an [`add`]'s read and its write, a few milliseconds, is lost.
//! Acceptable while only the engineer adds notes; it needs a lock once agents write notes
//! while the app runs.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The format version this build reads and writes. A file with another version is refused,
/// not rewritten.
const VERSION: u32 = 1;

/// Reads the review file and moves every note to where its snippet is now. Does not write.
///
/// `read` returns a file's text by its repository path. A note whose file cannot be read is
/// outdated.
///
/// # Errors
///
/// As [`Review::load`]: the review file exists but cannot be read, parsed, or accepted.
pub(crate) fn current(file: &Path, mut read: impl FnMut(&str) -> Result<String>) -> Result<Review> {
    let mut review = Review::load(file)?;
    review.relocate(|path| read(path).ok());
    Ok(review)
}

/// Adds a note and saves the review file with every note at its current place, then returns
/// all notes.
///
/// The snippet is taken from the file as `read` returns it, so it is what is on disk,
/// whatever the page showed.
///
/// # Errors
///
/// As [`current`]; the error of `read` for the note's file; [`Error::InvalidNote`] when the
/// lines are past the end of the file or the body is blank; [`Error::WriteReview`] when the
/// file cannot be saved.
pub(crate) fn add(
    file: &Path,
    new: NewNote,
    mut read: impl FnMut(&str) -> Result<String>,
) -> Result<Review> {
    let text = read(&new.path)?;
    let mut review = current(file, read)?;
    review.push(new, &text)?;
    review.save(file)?;
    Ok(review)
}

/// Every note of one working tree, as stored in `review.json`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Review {
    version: u32,
    notes: Vec<Note>,
}

/// The one field of a review file that every format version has. Unknown fields are
/// ignored here on purpose.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// A note the engineer wrote on a line range.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Note {
    id: NoteId,
    /// Relative to the repository root.
    path: String,
    /// Where the snippet was found last.
    lines: LineRange,
    /// The text of the lines when the note was written, without line endings, joined with
    /// `\n`.
    snippet: String,
    body: String,
    state: NoteState,
}

/// What the page sends to create a note.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewNote {
    /// Relative to the repository root.
    path: String,
    lines: LineRange,
    body: String,
}

/// A note's number, written `N1`, `N2`: shown in the page and sent to the agent.
#[derive(Debug, PartialEq, Eq)]
struct NoteId(u32);

/// Lines `start` to `end` of a file, 1-based and inclusive, with `1 <= start <= end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawLineRange")]
struct LineRange {
    start: usize,
    end: usize,
}

/// A [`LineRange`] before its bounds are checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLineRange {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum NoteState {
    /// The snippet is in the file, at `lines`.
    Open,
    /// The snippet is no longer in the file, or the file cannot be read. `lines` is where it
    /// was last seen.
    Outdated,
}

impl Review {
    /// Reads the review file, or returns an empty review when there is none yet. Leaves the
    /// file as it is in every case.
    fn load(file: &Path) -> Result<Self> {
        let json = match fs::read_to_string(file) {
            Ok(json) => json,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: VERSION,
                    notes: Vec::new(),
                });
            }
            Err(source) => {
                return Err(Error::ReadReview {
                    path: file.to_owned(),
                    source,
                });
            }
        };
        let parse_error = |source| Error::ParseReview {
            path: file.to_owned(),
            source,
        };
        let invalid = |problem| Error::InvalidReview {
            path: file.to_owned(),
            problem,
        };

        // The version is read on its own first: a newer format would otherwise fail on its
        // first new field, with a message that hides the real reason.
        let VersionOnly { version } = serde_json::from_str(&json).map_err(parse_error)?;
        if version != VERSION {
            return Err(invalid(format!(
                "version {version}, but this build of Postil reads version {VERSION}"
            )));
        }

        let review: Self = serde_json::from_str(&json).map_err(parse_error)?;
        for (index, note) in review.notes.iter().enumerate() {
            if review.notes[..index]
                .iter()
                .any(|other| other.id == note.id)
            {
                return Err(invalid(format!("note id {} is used twice", note.id)));
            }
        }
        Ok(review)
    }

    /// Replaces the review file with this review, creating its directory when needed.
    fn save(&self, file: &Path) -> Result<()> {
        let mut json = serde_json::to_vec_pretty(self)
            .expect("a review is plain structs, strings, and numbers, which always serialize");
        json.push(b'\n');
        write_atomically(file, &json).map_err(|source| Error::WriteReview {
            path: file.to_owned(),
            source,
        })
    }

    /// Moves every note to where its snippet is in its file now. `read` returns a file's
    /// text by its repository path, or `None` when it cannot be read.
    ///
    /// Reads one file per note and compares every position in it with the snippet. A review
    /// holds tens of notes, so this stays well under a frame for files of a few thousand
    /// lines; an index of each file's lines would be the next step if it does not.
    fn relocate(&mut self, mut read: impl FnMut(&str) -> Option<String>) {
        for note in &mut self.notes {
            let text = read(&note.path);
            let found = text
                .as_deref()
                .and_then(|text| find_snippet(text, &note.snippet, note.lines.start));
            match found {
                Some(lines) => {
                    note.lines = lines;
                    note.state = NoteState::Open;
                }
                None => note.state = NoteState::Outdated,
            }
        }
    }

    /// Appends a note on `new.lines` of the file whose current text is `text`.
    fn push(&mut self, new: NewNote, text: &str) -> Result<()> {
        let lines: Vec<&str> = text.lines().collect();
        let LineRange { start, end } = new.lines;
        if end > lines.len() {
            return Err(Error::InvalidNote(format!(
                "lines {start}-{end} are past the end of {} ({} lines)",
                new.path,
                lines.len()
            )));
        }
        let body = new.body.trim();
        if body.is_empty() {
            return Err(Error::InvalidNote("the note has no text".to_owned()));
        }

        // The highest id plus one, not the count plus one: ids stay unique when someone
        // removes a note by editing the file.
        let highest = self.notes.iter().map(|note| note.id.0).max().unwrap_or(0);
        let id = highest
            .checked_add(1)
            .map(NoteId)
            .ok_or_else(|| Error::InvalidNote(format!("no note id is left after N{highest}")))?;

        self.notes.push(Note {
            id,
            path: new.path,
            lines: new.lines,
            snippet: lines[start - 1..end].join("\n"),
            body: body.to_owned(),
            state: NoteState::Open,
        });
        Ok(())
    }
}

// Debug output shows ids, paths, and states, never note text: notes can quote code that is
// not meant to leave the machine, and Debug output ends up in logs.
impl fmt::Debug for Review {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Review")
            .field("version", &self.version)
            .field("notes", &self.notes)
            .finish()
    }
}

impl fmt::Debug for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Note")
            .field("id", &self.id)
            .field("path", &self.path)
            .field("lines", &self.lines)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for NewNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewNote")
            .field("path", &self.path)
            .field("lines", &self.lines)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for NoteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "N{}", self.0)
    }
}

impl Serialize for NoteId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for NoteId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.strip_prefix('N')
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse().ok())
            .filter(|&number| number > 0)
            .map(Self)
            .ok_or_else(|| {
                de::Error::custom(format!(
                    "note id {text:?}: expected N and a number, like N1"
                ))
            })
    }
}

impl TryFrom<RawLineRange> for LineRange {
    type Error = String;

    fn try_from(raw: RawLineRange) -> std::result::Result<Self, Self::Error> {
        let RawLineRange { start, end } = raw;
        if start == 0 || start > end {
            return Err(format!(
                "lines {start}-{end}: lines are 1-based and start must not be after end"
            ));
        }
        Ok(Self { start, end })
    }
}

/// Finds the lines of `text` that equal the lines of `snippet`, and returns the match whose
/// first line is nearest line `near` (1-based). Of two equally near, the earlier wins.
fn find_snippet(text: &str, snippet: &str, near: usize) -> Option<LineRange> {
    // `str::lines` drops `\r\n` and `\n` alike, as the highlighter does, so the line numbers
    // here are the ones the page shows.
    let lines: Vec<&str> = text.lines().collect();
    let wanted: Vec<&str> = snippet.split('\n').collect();

    let index = lines
        .windows(wanted.len())
        .enumerate()
        .filter(|(_, window)| *window == wanted.as_slice())
        .map(|(index, _)| index)
        .min_by_key(|index| index.abs_diff(near - 1))?;

    Some(LineRange {
        start: index + 1,
        end: index + wanted.len(),
    })
}

/// Writes `bytes` to a temporary file beside `file`, syncs it, and renames it over `file`.
///
/// A failure before the rename leaves the old file. The directory is synced after the rename
/// (and its parent too when the directory was just created, so the first save survives a
/// crash); a failure there is returned although the new file is already in place, possibly
/// not yet durable.
fn write_atomically(file: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = file
        .parent()
        .expect("the review file is <git dir>/postil/review.json, which has a parent");
    let created = !dir.exists();
    fs::create_dir_all(dir)?;

    // The process id keeps two app windows on one repository from sharing a temporary file.
    let temporary = dir.join(format!("review.json.{}.tmp", std::process::id()));
    let written = (|| {
        let mut out = fs::File::create(&temporary)?;
        out.write_all(bytes)?;
        out.sync_all()?;
        fs::rename(&temporary, file)
    })();
    if written.is_err() {
        // Best effort: a leftover temporary file is ignored by every reader.
        drop(fs::remove_file(&temporary));
    }
    written?;

    fs::File::open(dir)?.sync_all()?;
    if created && let Some(parent) = dir.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::unnecessary_wraps,
        reason = "every caller compares it with a result that is an Option"
    )]
    fn range(start: usize, end: usize) -> Option<LineRange> {
        Some(LineRange { start, end })
    }

    #[test]
    fn snippet_moves_with_every_insertion_and_deletion_around_it() {
        // Ten distinct lines with a note on lines 4 to 6. Every way to insert one or three
        // lines, and every way to delete one line, is checked against where the snippet
        // must end up.
        let original: Vec<String> = (1..=10).map(|n| format!("line {n}")).collect();
        let snippet = original[3..6].join("\n");

        for count in [1, 3] {
            for at in 0..=original.len() {
                let mut lines = original.clone();
                for _ in 0..count {
                    lines.insert(at, "new".to_owned());
                }
                let expected = if at <= 3 {
                    range(4 + count, 6 + count)
                } else if at < 6 {
                    None
                } else {
                    range(4, 6)
                };
                let text = lines.join("\n");
                assert_eq!(
                    find_snippet(&text, &snippet, 4),
                    expected,
                    "{count} at {at}"
                );
            }
        }

        for at in 0..original.len() {
            let mut lines = original.clone();
            lines.remove(at);
            let expected = match at {
                0..=2 => range(3, 5),
                3..=5 => None,
                _ => range(4, 6),
            };
            assert_eq!(
                find_snippet(&lines.join("\n"), &snippet, 4),
                expected,
                "{at}"
            );
        }
    }

    #[test]
    fn of_several_matches_the_nearest_wins_and_ties_go_to_the_earlier() {
        let text = "x\nb\nx\nb\nx\nx\nx\nb\n";
        // `b` is on lines 2, 4, and 8.
        assert_eq!(find_snippet(text, "b", 1), range(2, 2));
        assert_eq!(find_snippet(text, "b", 5), range(4, 4));
        assert_eq!(find_snippet(text, "b", 7), range(8, 8));
        assert_eq!(find_snippet(text, "b", 3), range(2, 2), "lines 2 and 4 tie");
    }

    #[test]
    fn snippet_matches_whole_lines_only_and_ignores_line_endings() {
        assert_eq!(find_snippet("ab\nb\n", "b", 1), range(2, 2));
        assert_eq!(find_snippet("ab\n", "b", 1), None);
        assert_eq!(find_snippet("a\r\nb\r\nc\r\n", "b\nc", 1), range(2, 3));
        assert_eq!(
            find_snippet("a\n", "a\nb", 1),
            None,
            "snippet longer than the file"
        );
    }

    #[test]
    fn note_ids_read_only_n_and_a_positive_number() {
        let parse = |json: &str| serde_json::from_str::<NoteId>(json).ok();
        assert_eq!(parse("\"N1\""), Some(NoteId(1)));
        assert_eq!(parse("\"N42\""), Some(NoteId(42)));
        for bad in [
            "\"N0\"", "\"N\"", "\"N+1\"", "\"1\"", "\"n1\"", "\"N1x\"", "1",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(serde_json::to_string(&NoteId(7)).unwrap(), "\"N7\"");
    }

    #[test]
    fn line_ranges_are_one_based_and_ordered() {
        let parse = |json: &str| serde_json::from_str::<LineRange>(json).ok();
        assert_eq!(parse(r#"{"start":1,"end":1}"#), range(1, 1));
        assert_eq!(parse(r#"{"start":2,"end":5}"#), range(2, 5));
        assert_eq!(parse(r#"{"start":0,"end":1}"#), None);
        assert_eq!(parse(r#"{"start":3,"end":2}"#), None);
    }

    #[test]
    fn debug_output_names_notes_but_never_shows_their_text() {
        let new = || NewNote {
            path: "src/a.rs".to_owned(),
            lines: LineRange { start: 1, end: 1 },
            body: "secret body".to_owned(),
        };
        let mut review = Review {
            version: VERSION,
            notes: Vec::new(),
        };
        review.push(new(), "secret snippet\n").unwrap();

        for debug in [format!("{review:?}"), format!("{:?}", new())] {
            assert!(debug.contains("src/a.rs"), "{debug}");
            assert!(!debug.contains("secret"), "{debug}");
        }
        assert!(format!("{review:?}").contains("NoteId(1)"));
    }
}
