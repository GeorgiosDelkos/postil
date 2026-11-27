//! Placing lines of code that keep moving: a range of lines and the text they held, found
//! again in the file as it is now.
//!
//! A place is anchored by its snippet, the text of its lines when it was taken; its line
//! numbers are a hint. [`locate`] keeps it where its snippet still is, or moves it to the only
//! other copy of the snippet, or else finds nothing. A snippet found several times elsewhere
//! is not guessed: a short line such as `}` repeats, and the nearest copy is often the wrong
//! one. Notes and the analysis's code references are placed this way (`notes.rs`,
//! `analysis.rs`), and both are on a [`Target`].

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::repo::PathKind;

/// What a note or a code reference is on.
pub(crate) enum Target {
    /// Lines of the file at the note's or reference's path.
    Lines {
        /// Where the snippet was found last.
        lines: LineRange,
        /// The text of the lines when the note or reference was made, without line endings,
        /// joined with `\n`.
        snippet: String,
    },
    /// The whole file or folder at the path.
    Path(PathKind),
}

impl Target {
    /// The target that `kind`, `lines` and `snippet` describe, as read from JSON: a target on
    /// lines has both fields and one on a file or folder neither, so a typo in a hand edit is
    /// refused instead of dropped. `noun` names what has the target in the message ("note").
    pub(crate) fn read(
        kind: TargetKind,
        lines: Option<LineRange>,
        snippet: Option<String>,
        noun: &str,
    ) -> Result<Self, String> {
        match (kind, lines, snippet) {
            (TargetKind::Lines, Some(lines), Some(snippet)) => Ok(Self::Lines { lines, snippet }),
            (TargetKind::Lines, _, _) => {
                Err(format!("a {noun} on lines needs `lines` and `snippet`"))
            }
            (TargetKind::File, None, None) => Ok(Self::Path(PathKind::File)),
            (TargetKind::Folder, None, None) => Ok(Self::Path(PathKind::Folder)),
            (TargetKind::File | TargetKind::Folder, _, _) => Err(format!(
                "a {noun} on a file or folder has no `lines` or `snippet`"
            )),
        }
    }
}

/// A [`Target`]'s `target` field as the files and the page write it.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TargetKind {
    /// The default for a note: absent before `review.json` version 4, where every note is on
    /// lines; the page leaves it out too.
    #[default]
    Lines,
    File,
    Folder,
}

/// Lines `start` to `end` of a file, 1-based and inclusive, with `1 <= start <= end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawLineRange")]
pub(crate) struct LineRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// A [`LineRange`] before its bounds are checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLineRange {
    start: usize,
    end: usize,
}

/// Where the lines of `snippet` are in `text`, given that they were last at `last`:
/// `last` itself when they are still there, otherwise the only place they are, otherwise
/// `None` (gone, or several places and no way to tell which). `snippet` is whole lines joined
/// by `\n`, with no `\r` and no newline at its end, as a note stores its snippet. It is not
/// normalized: a trailing `\n` asks for an empty line after the snippet.
pub(crate) fn locate(text: &str, snippet: &str, last: LineRange) -> Option<LineRange> {
    // `str::lines` drops `\r\n` and `\n` alike, as the highlighter does, so the line numbers
    // here are the ones the page shows.
    locate_lines(&text.lines().collect::<Vec<_>>(), snippet, last)
}

/// [`locate`] in a text already split into `lines`.
pub(crate) fn locate_lines(lines: &[&str], snippet: &str, last: LineRange) -> Option<LineRange> {
    let wanted: Vec<&str> = snippet.split('\n').collect();

    // Fast path: when the snippet is still at `last`, no file scan is needed.
    if last.start >= 1
        && last.end <= lines.len()
        && last.end - last.start + 1 == wanted.len()
        && lines[last.start - 1..last.end] == wanted[..]
    {
        return Some(last);
    }

    let mut found = lines
        .windows(wanted.len())
        .enumerate()
        .filter(|(_, window)| *window == wanted.as_slice())
        .map(|(index, _)| LineRange {
            start: index + 1,
            end: index + wanted.len(),
        });

    let first = found.next()?;
    let mut others = found.peekable();
    if first == last || others.peek().is_none() {
        return Some(first);
    }
    others.find(|&range| range == last)
}

impl TryFrom<RawLineRange> for LineRange {
    type Error = String;

    fn try_from(raw: RawLineRange) -> Result<Self, Self::Error> {
        let RawLineRange { start, end } = raw;
        if start == 0 || start > end {
            return Err(format!(
                "lines {start}-{end}: lines are 1-based and start must not be after end"
            ));
        }
        Ok(Self { start, end })
    }
}

// The lines, never the snippet: a snippet quotes code that is not meant to leave the machine,
// and Debug output ends up in logs.
impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lines { lines, .. } => f
                .debug_struct("Lines")
                .field("lines", lines)
                .finish_non_exhaustive(),
            Self::Path(kind) => f.debug_tuple("Path").field(kind).finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: usize, end: usize) -> LineRange {
        LineRange { start, end }
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
                    Some(range(4 + count, 6 + count))
                } else if at < 6 {
                    None
                } else {
                    Some(range(4, 6))
                };
                let text = lines.join("\n");
                let found = locate(&text, &snippet, range(4, 6));
                assert_eq!(found, expected, "{count} at {at}");
            }
        }

        for at in 0..original.len() {
            let mut lines = original.clone();
            lines.remove(at);
            let expected = match at {
                0..=2 => Some(range(3, 5)),
                3..=5 => None,
                _ => Some(range(4, 6)),
            };
            let found = locate(&lines.join("\n"), &snippet, range(4, 6));
            assert_eq!(found, expected, "{at}");
        }
    }

    #[test]
    fn a_repeated_snippet_stays_put_or_is_lost_but_never_jumps_to_another_copy() {
        // `}` is on lines 2, 4, and 8.
        let text = "a\n}\nb\n}\nc\nd\ne\n}\n";
        assert_eq!(locate(text, "}", range(4, 4)), Some(range(4, 4)));
        assert_eq!(locate(text, "}", range(8, 8)), Some(range(8, 8)));
        // The note was on the `}` that is now on line 4 or 8 after lines moved: no way to
        // tell which, and the nearest copy (line 4) is the wrong one when two lines were
        // inserted above line 6.
        assert_eq!(locate(text, "}", range(6, 6)), None);
        // One copy left: it is the note's, wherever it moved.
        assert_eq!(locate("x\n}\n", "}", range(6, 6)), Some(range(2, 2)));
    }

    #[test]
    fn snippet_matches_whole_lines_only_and_ignores_line_endings() {
        assert_eq!(locate("ab\nb\n", "b", range(1, 1)), Some(range(2, 2)));
        assert_eq!(locate("ab\n", "b", range(1, 1)), None);
        let crlf = "a\r\nb\r\nc\r\n";
        assert_eq!(locate(crlf, "b\nc", range(1, 2)), Some(range(2, 3)));
        assert_eq!(
            locate("a\n", "a\nb", range(1, 2)),
            None,
            "longer than the file"
        );
    }
}
