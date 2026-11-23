//! Review notes and the file that stores them.
//!
//! A note is anchored by its snippet: the text of its lines when it was written. Its line
//! numbers are a hint. Whenever the notes are read, each one is placed by [`locate`]: it stays
//! where its snippet still is, or moves to the only other copy of the snippet, or else its
//! anchor becomes [`Anchor::Lost`]. A snippet found several times elsewhere is lost rather
//! than guessed: a short line such as `}` repeats, and the nearest copy is often the wrong
//! one.
//!
//! The anchor says only whether the text is still there. It is kept apart from a note's
//! lifecycle (open, sent, resolved, from milestone 3 on): a sent note whose text the agent
//! replaced is the expected outcome, while an open note whose text vanished means the code
//! drifted under it, and the page shows the two differently. One enum would need a variant
//! for every combination.
//!
//! # The file
//!
//! The notes live in `<git dir>/postil/review.json` (docs/design.md, "Data structures"). That
//! file is the only copy: every request reads it, so an agent or a script that edits it is
//! seen on the next request, and the page holds nothing that a reload would lose. The module
//! offers [`current`] to read and [`add`], [`delete`], [`resolve`] and [`reopen`] to write,
//! each of which does every step (read, relocate, and for the writers lock and write) so
//! that no caller can skip one. [`mark_sent`] writes too, for the runner.
//!
//! Only those writers write. Reading the notes, which the page does whenever the
//! window regains focus, never rewrites the file, so it cannot overwrite an edit made by
//! hand. The cost: the stored lines catch up with the code only on the next write, and until
//! then each read relocates from the older lines.
//!
//! Each writer holds an exclusive lock on `review.lock` beside the file from its read to its
//! rename, so two app windows on one working tree cannot drop each other's changes. A script
//! that edits `review.json` without taking that lock can still lose an edit made during a
//! write, a window of milliseconds.
//!
//! The format has a version. Every field added is a new version: a build refuses fields it
//! does not know (so a typo in a hand edit fails loudly instead of being dropped on the next
//! write), and an older build must say "version 2" rather than "unknown field". A newer build
//! reads every older version. Version 2 added each note's `state`; a version 1 file reads
//! with every note open. Version 3 added the resolved state. The next write makes an older
//! file version 3.

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::Path;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::files::write_atomically;
use crate::repo::Repo;

/// The format version this build writes. It reads this one and every older one; a file with
/// a newer version is refused, not rewritten.
const VERSION: u32 = 3;

/// Reads the review file and places every note where its snippet is now. Does not write.
///
/// # Errors
///
/// [`Error::ReadReview`], [`Error::ParseReview`], or [`Error::InvalidReview`] when the review
/// file exists but cannot be read or accepted. A note whose own file cannot be read is not
/// an error: its anchor is lost.
pub(crate) fn current(repo: &Repo) -> Result<Review> {
    let mut review = Review::load(&repo.review_file())?;
    review.relocate(repo);
    Ok(review)
}

/// The ids of the notes, written `N1`, read from the review file as it is: no note is placed
/// in its file, so no source file is read.
///
/// # Errors
///
/// As [`current`] for reading the review file.
pub(crate) fn ids(repo: &Repo) -> Result<Vec<String>> {
    let review = Review::load(&repo.review_file())?;
    Ok(review
        .notes
        .iter()
        .map(|note| note.id.to_string())
        .collect())
}

/// Adds a note and saves the review file with every note at its current place, then returns
/// all notes.
///
/// `new.snippet` is the text the page showed on `new.lines`. When the file changed since,
/// the note goes where that text is now, by the same rule as [`current`].
///
/// # Errors
///
/// As [`current`]; the error of reading the note's file; [`Error::InvalidNote`] for a blank
/// body or a snippet that does not span `new.lines`; [`Error::NoteTextChanged`] when the
/// shown text cannot be placed in the file any more; [`Error::WriteReview`] when the lock or
/// the file cannot be written.
pub(crate) fn add(repo: &Repo, new: NewNote) -> Result<Review> {
    let file = repo.review_file();
    let body = new.body.trim();
    if body.is_empty() {
        return Err(Error::InvalidNote("the note has no text".to_owned()));
    }
    let count = new.lines.end - new.lines.start + 1;
    if new.snippet.split('\n').count() != count {
        return Err(Error::InvalidNote(format!(
            "the snippet does not have {count} lines"
        )));
    }

    let _lock = lock(&file)?;
    let text = repo.read(&new.path)?;
    let lines = locate(&text, &new.snippet, new.lines).ok_or_else(|| Error::NoteTextChanged {
        path: new.path.clone(),
    })?;

    let mut review = current(repo)?;
    // The highest id plus one, not the count plus one: ids stay unique when someone removes
    // a note by editing the file.
    let highest = review.notes.iter().map(|note| note.id.0).max().unwrap_or(0);
    let id = highest.checked_add(1).ok_or_else(|| Error::InvalidReview {
        path: file.clone(),
        problem: format!("no note id is left after N{highest}"),
    })?;

    review.notes.push(Note {
        id: NoteId(id),
        path: new.path,
        lines,
        snippet: new.snippet,
        body: body.to_owned(),
        anchor: Anchor::Found,
        state: NoteState::Open {},
    });
    review.save(&file)?;
    Ok(review)
}

/// Deletes the note whose id is `id` (written `N1`), saves, and returns the other notes.
///
/// The next note gets the highest id left plus one, so every id above the highest remaining
/// note is free again, including ones deleted earlier; an id below it is not reused while
/// that higher note remains. Never reusing any id would need the file to store the next id, a
/// new field and so a new format version.
///
/// # Errors
///
/// [`Error::BadRequest`] when `id` is not a note id, [`Error::NoSuchNote`] when no note has
/// it, as [`current`] for reading the file, and [`Error::WriteReview`] when the lock or the
/// file cannot be written.
pub(crate) fn delete(repo: &Repo, id: &str) -> Result<Review> {
    let wanted = NoteId::parse(id)
        .ok_or_else(|| Error::BadRequest(format!("note id {id:?}: expected N and a number")))?;
    let file = repo.review_file();
    let _lock = lock(&file)?;
    let mut review = current(repo)?;
    let before = review.notes.len();
    review.notes.retain(|note| note.id != wanted);
    if review.notes.len() == before {
        return Err(Error::NoSuchNote { id: id.to_owned() });
    }
    review.save(&file)?;
    Ok(review)
}

/// Marks the note `id` resolved, saves, and returns all notes. A resolved note keeps the run
/// it was sent in, so its dialog still shows what that run did. Only [`reopen`] takes a note
/// out of this state: [`prompt`] refuses it, and [`mark_sent`] only records its run.
/// Resolving a resolved note leaves its state as it is, but saves all the same, as every
/// write does: with every note at its current place, and the file at this version.
///
/// # Errors
///
/// As [`delete`].
pub(crate) fn resolve(repo: &Repo, id: &str) -> Result<Review> {
    change_state(repo, id, |state| match state {
        NoteState::Open {} => NoteState::Resolved { run: None },
        NoteState::Sent { run } => NoteState::Resolved { run: Some(run) },
        resolved @ NoteState::Resolved { .. } => resolved,
    })
}

/// Reopens the resolved note `id`, saves, and returns all notes: it is sent again in the run
/// it was resolved after, or open when it was never sent. Any other note keeps its state; the
/// file is saved all the same, as for [`resolve`].
///
/// # Errors
///
/// As [`delete`].
pub(crate) fn reopen(repo: &Repo, id: &str) -> Result<Review> {
    change_state(repo, id, |state| match state {
        NoteState::Resolved { run: Some(run) } => NoteState::Sent { run },
        NoteState::Resolved { run: None } => NoteState::Open {},
        other @ (NoteState::Open {} | NoteState::Sent { .. }) => other,
    })
}

/// Replaces the state of note `id` with `change(state)` and saves, under the lock.
fn change_state(
    repo: &Repo,
    id: &str,
    change: impl FnOnce(NoteState) -> NoteState,
) -> Result<Review> {
    let wanted = NoteId::parse(id)
        .ok_or_else(|| Error::BadRequest(format!("note id {id:?}: expected N and a number")))?;
    let file = repo.review_file();
    let _lock = lock(&file)?;
    let mut review = current(repo)?;
    let note = review
        .notes
        .iter_mut()
        .find(|note| note.id == wanted)
        .ok_or_else(|| Error::NoSuchNote { id: id.to_owned() })?;
    note.state = change(std::mem::take(&mut note.state));
    review.save(&file)?;
    Ok(review)
}

/// The prompt that sends the notes `ids` to an agent, in that order (docs/design.md, "The
/// prompt"): an introduction, then each note's heading, its quoted snippet and its body. The
/// quoted snippet travels with each note because line numbers go stale the moment the agent
/// edits a file.
///
/// # Errors
///
/// As [`current`]; [`Error::BadRequest`] when `ids` is empty, repeats an id, holds
/// something that is not a note id, or names a resolved note (reopen it first);
/// [`Error::NoSuchNote`] when no note has one of them.
pub(crate) fn prompt(repo: &Repo, ids: &[String]) -> Result<Prompt> {
    let wanted = parse_ids(ids)?;
    let review = current(repo)?;
    let mut text = String::from(
        "These are review notes an engineer wrote while reading this repository.\n\
         Address every note. Change nothing the notes do not ask for.\n\
         When you finish, list each note id with one line on what you did.\n",
    );
    let mut files: Vec<String> = Vec::new();
    for id in &wanted {
        let note = review
            .notes
            .iter()
            .find(|note| note.id == *id)
            .ok_or_else(|| Error::NoSuchNote { id: id.to_string() })?;
        if matches!(note.state, NoteState::Resolved { .. }) {
            return Err(Error::BadRequest(format!(
                "note {id} is resolved: reopen it to send it"
            )));
        }
        let LineRange { start, end } = note.lines;
        let lines = if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        };
        text.push_str(&format!("\n[{}] {}:{lines}\n", note.id, note.path));
        if note.anchor == Anchor::Lost {
            text.push_str("(These lines are no longer in the file; they were:)\n");
        }
        for line in note.snippet.split('\n') {
            text.push_str(&format!("> {line}\n"));
        }
        text.push_str(note.body.trim_end());
        text.push('\n');
        if !files.contains(&note.path) {
            files.push(note.path.clone());
        }
    }
    Ok(Prompt { text, files })
}

/// A prompt built from notes. Its `Debug` output leaves out the text, as for [`Note`].
pub(crate) struct Prompt {
    /// The text the agent receives.
    pub(crate) text: String,
    /// The notes' files, relative to the repository root, each once, in note order.
    pub(crate) files: Vec<String>,
}

/// Marks the notes `ids` sent in `run` and saves. The mark comes when the agent takes the
/// turn, which can be seconds after the send. A note deleted meanwhile is skipped: the run it
/// went to is under way, and the engineer removed it. A note resolved meanwhile stays
/// resolved, the engineer's word being later than the send, and records `run`: the agent did
/// get it, so its dialog shows this run and a reopen makes it sent in this run.
///
/// # Errors
///
/// As [`current`]; [`Error::BadRequest`] as for [`prompt`]; [`Error::WriteReview`] when
/// the lock or the file cannot be written.
pub(crate) fn mark_sent(repo: &Repo, ids: &[String], run: &str) -> Result<()> {
    let wanted = parse_ids(ids)?;
    let file = repo.review_file();
    let _lock = lock(&file)?;
    let mut review = current(repo)?;
    for note in review
        .notes
        .iter_mut()
        .filter(|note| wanted.contains(&note.id))
    {
        match &mut note.state {
            NoteState::Resolved { run: last } => *last = Some(run.to_owned()),
            state @ (NoteState::Open {} | NoteState::Sent { .. }) => {
                *state = NoteState::Sent {
                    run: run.to_owned(),
                };
            }
        }
    }
    review.save(&file)
}

/// Reads note ids as the page sends them: at least one, none twice.
fn parse_ids(ids: &[String]) -> Result<Vec<NoteId>> {
    if ids.is_empty() {
        return Err(Error::BadRequest("no notes were chosen".to_owned()));
    }
    let mut wanted = Vec::with_capacity(ids.len());
    for text in ids {
        let id = NoteId::parse(text).ok_or_else(|| {
            Error::BadRequest(format!("note id {text:?}: expected N and a number"))
        })?;
        if wanted.contains(&id) {
            return Err(Error::BadRequest(format!("note {text} is chosen twice")));
        }
        wanted.push(id);
    }
    Ok(wanted)
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
    /// Recomputed on every read, so a note written by hand may leave it out.
    #[serde(default)]
    anchor: Anchor,
    /// Absent in version 1, where every note is open; a note written by hand may leave it
    /// out too.
    #[serde(default)]
    state: NoteState,
}

/// Where a note is in the loop. Kept apart from [`Anchor`] (module docs). Always an object
/// with a `kind` (`{"kind": "open"}`, `{"kind": "sent", "run": "R1"}`), so a script reads
/// one shape whatever the state.
///
/// Each variant has braces: `deny_unknown_fields` checks only struct variants, so a unit
/// `Open` would accept `{"kind": "open", "typo": 1}`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum NoteState {
    /// Not sent yet, or sent in a run the agent never took up.
    Open {},
    /// Sent to the agent in the run with this id (`R1`).
    Sent { run: String },
    /// The engineer is done with it. `run` is the run it was sent in last, if any.
    Resolved {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
    },
}

/// What the page sends to create a note.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewNote {
    /// Relative to the repository root.
    path: String,
    lines: LineRange,
    /// The text of `lines` as the page showed it, lines joined with `\n`.
    snippet: String,
    body: String,
}

/// A note's number, written `N1`, `N2`: shown in the page and sent to the agent.
#[derive(Debug, PartialEq, Eq)]
struct NoteId(u32);

impl NoteId {
    /// Reads `N` and a positive number, as `Display` writes it.
    fn parse(text: &str) -> Option<Self> {
        text.strip_prefix('N')
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse().ok())
            .filter(|&number| number > 0)
            .map(Self)
    }
}

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

/// Whether a note's snippet is still in its file.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Anchor {
    /// The snippet is in the file, at `lines`.
    #[default]
    Found,
    /// The snippet is not in the file where [`locate`] may place it, or the file cannot be
    /// read. `lines` is where it was last seen.
    Lost,
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
        if !(1..=VERSION).contains(&version) {
            return Err(invalid(format!(
                "version {version}, but this build of Postil reads versions 1 to {VERSION}"
            )));
        }

        let mut review: Self = serde_json::from_str(&json).map_err(parse_error)?;
        // An older file is read as this version, and written as this version next time.
        review.version = VERSION;
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

    /// Replaces the review file with this review (`write_atomically`).
    fn save(&self, file: &Path) -> Result<()> {
        let mut json = serde_json::to_vec_pretty(self)
            .expect("a review is plain structs, strings, and numbers, which always serialize");
        json.push(b'\n');
        write_atomically(file, &json).map_err(|source| Error::WriteReview {
            path: file.to_owned(),
            source,
        })
    }

    /// Places every note by [`locate`] in its file as it is now.
    ///
    /// Reads one file per note and compares every position in it with the snippet. A review
    /// holds tens of notes, so this stays well under a frame for files of a few thousand
    /// lines; an index of each file's lines would be the next step if it does not.
    fn relocate(&mut self, repo: &Repo) {
        for note in &mut self.notes {
            let text = repo.read(&note.path).ok();
            match text.and_then(|text| locate(&text, &note.snippet, note.lines)) {
                Some(lines) => {
                    note.lines = lines;
                    note.anchor = Anchor::Found;
                }
                None => note.anchor = Anchor::Lost,
            }
        }
    }
}

/// Where the lines of `snippet` are in `text`, given that they were last at `last`:
/// `last` itself when they are still there, otherwise the only place they are, otherwise
/// `None` (gone, or several places and no way to tell which).
fn locate(text: &str, snippet: &str, last: LineRange) -> Option<LineRange> {
    // `str::lines` drops `\r\n` and `\n` alike, as the highlighter does, so the line numbers
    // here are the ones the page shows.
    let lines: Vec<&str> = text.lines().collect();
    let wanted: Vec<&str> = snippet.split('\n').collect();

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

/// Takes the exclusive lock on `review.lock` beside the review file, waiting for another
/// window's add to finish. The lock is released when the returned file is dropped.
fn lock(review_file: &Path) -> Result<File> {
    let path = review_file.with_extension("lock");
    let error = |source| Error::WriteReview {
        path: path.clone(),
        source,
    };
    let dir = path
        .parent()
        .expect("the review file is <git dir>/postil/review.json, which has a parent");
    fs::create_dir_all(dir).map_err(error)?;

    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(error)?;
    file.lock().map_err(error)?;
    Ok(file)
}

// Debug output shows ids, paths, and anchors, never note text: notes can quote code that is
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
            .field("anchor", &self.anchor)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Prompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prompt")
            .field("len", &self.text.len())
            .field("files", &self.files)
            .finish()
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

impl Default for NoteState {
    fn default() -> Self {
        Self::Open {}
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
        Self::parse(&text).ok_or_else(|| {
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
    fn debug_output_names_notes_but_never_shows_their_text() {
        let new = NewNote {
            path: "src/a.rs".to_owned(),
            lines: range(1, 1),
            snippet: "secret snippet".to_owned(),
            body: "secret body".to_owned(),
        };
        let note = Note {
            id: NoteId(1),
            path: "src/a.rs".to_owned(),
            lines: range(1, 1),
            snippet: "secret snippet".to_owned(),
            body: "secret body".to_owned(),
            anchor: Anchor::Found,
            state: NoteState::Sent {
                run: "R1".to_owned(),
            },
        };
        let review = Review {
            version: VERSION,
            notes: vec![note],
        };

        for debug in [format!("{review:?}"), format!("{new:?}")] {
            assert!(debug.contains("src/a.rs"), "{debug}");
            assert!(!debug.contains("secret"), "{debug}");
        }
        assert!(format!("{review:?}").contains("NoteId(1)"));
    }
}
