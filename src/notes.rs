//! Review notes and the file that stores them.
//!
//! A note is on one of three [`Target`]s: lines of a file, a whole file, or a whole folder.
//! The engineer writes a line note from a selection in the code, and a file or folder note
//! from the file list's right-click menu.
//!
//! A line note is anchored by its snippet: the text of its lines when it was written. Its
//! line numbers are a hint. Whenever the notes are read, each one is placed by [`locate`]
//! (`anchor.rs` gives the rule), and a note it cannot place becomes [`Anchor::Lost`]. A file
//! or folder note is found while its path is still a file or a folder of that kind, and lost
//! otherwise: a rename is not followed, since nothing tells a renamed folder from a new one.
//!
//! The anchor says only whether the note's target (its text, or its file or folder) is still
//! there. It is kept apart from a note's lifecycle (open, sent, resolved, from milestone 3
//! on): a sent note whose text the agent replaced is the expected outcome, while an open note
//! whose text vanished means the code drifted under it, and the page shows the two
//! differently. One enum would need a variant for every combination.
//!
//! # The file
//!
//! The notes live in `<git dir>/postil/review.json` (docs/design.md, "Data structures"). That
//! file is the only copy: every request reads it, so an agent or a script that edits it is
//! seen on the next request, and the page holds nothing that a reload would lose. The module
//! offers [`current`] to read and [`add`], [`edit`], [`delete`], [`resolve`] and [`reopen`] to
//! write, each of which does every step (read, relocate, and for the writers lock and write)
//! so that no caller can skip one. [`mark_sent`] writes too, for the runner.
//!
//! Only those writers write. Reading the notes, which the page does whenever the
//! window regains focus, never rewrites the file, so it cannot overwrite an edit made by
//! hand. The cost: the stored lines catch up with the code only on the next write, and until
//! then each read relocates from the older lines.
//!
//! Each writer holds an exclusive lock on `review.lock` beside the file from its read to its
//! rename, so two app windows on one working tree cannot drop each other's changes. A writer
//! waits for the lock only so long, then fails naming the lock file, so a process that keeps
//! it does not freeze the window. A script that edits `review.json` without taking that lock
//! can still lose an edit made during a write, a window of milliseconds.
//!
//! The format has a version. Every field added is a new version: a build refuses fields it
//! does not know (so a typo in a hand edit fails loudly instead of being dropped on the next
//! write), and an older build must say "version 2" rather than "unknown field". A newer build
//! reads every older version. Version 2 added each note's `state`; a version 1 file reads
//! with every note open. Version 3 added the resolved state. Version 4 added each note's
//! `target` (`lines`, `file` or `folder`); an older file reads with every note on lines, and
//! a file or folder note has no `lines` or `snippet`. Version 5 added each note's `intent`
//! (`change` or `ask`); an older file reads with every note a change. The next write makes an
//! older file version 5. A path in the file must be written as git writes paths
//! (`Repo::kind`): one written another way (`./a.rs`, `src//a.rs`), which the page never sent,
//! reads as lost.
//!
//! # Asks
//!
//! A note with intent [`Intent::Ask`] is a question about its target, answered by an agent
//! that only reads, in a thread of its own (docs/review-assist.md, "Ask without changing"). It
//! is saved only when its run starts ([`check_new`], then [`save_ask`] inside `Run::create`),
//! already sent in that run, which it belongs to whatever becomes of it. It is never sent with
//! change notes: [`prompt`] refuses it, and [`save_ask`] gives its own prompt.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::Path;

use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::anchor::{LineRange, Target, TargetKind, locate, locate_lines};
use crate::error::{Error, Result};
use crate::files::{self, save_json, version_of};
use crate::repo::{PathKind, Repo};
use crate::report;

/// The format version this build writes. It reads this one and every older one; a file with
/// a newer version is refused, not rewritten.
const VERSION: u32 = 5;

/// Reads the review file and places every note: a line note where its snippet is now, a file
/// or folder note by whether its path is still there. Does not write.
///
/// # Errors
///
/// [`Error::ReadReview`], [`Error::ParseReview`], or [`Error::InvalidReview`] when the review
/// file exists but cannot be read or accepted. A note whose own file cannot be read is not
/// an error: its anchor is lost.
pub(crate) fn current(repo: &Repo) -> Result<Review> {
    let mut review = Review::load(&repo.review_file())?;
    review.relocate(repo, HashMap::new());
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

/// Whether note `id` is an ask, read from the review file as it is, as [`ids`] reads it: no
/// source file is read. False for an id no note has; the change asked for then says so.
///
/// # Errors
///
/// As [`current`] for reading the review file.
pub(crate) fn is_ask(repo: &Repo, id: &str) -> Result<bool> {
    let wanted = parse_ids(&[id.to_owned()])?;
    let review = Review::load(&repo.review_file())?;
    Ok(review
        .notes
        .iter()
        .any(|note| note.id == wanted[0] && note.intent == Intent::Ask))
}

/// Adds a change note and saves the review file with every note at its current place, then
/// returns all notes: [`check_new`], then [`save`].
///
/// # Errors
///
/// As [`check_new`] and [`save`].
pub(crate) fn add(repo: &Repo, new: NewNote) -> Result<Review> {
    save(repo, check_new(repo, new)?)
}

/// Checks the new note `new` against the files as they are now, and places it, without
/// reading or writing the review file. [`save`] then saves it as a change, or [`save_ask`]
/// as an ask.
///
/// For a line note, the snippet is the text the page showed on its lines. When the file
/// changed since, the note goes where that text is now, by the same rule as [`current`]. A
/// file or folder note needs its path to be a file or a folder now.
///
/// # Errors
///
/// The error of reading the note's file; [`Error::InvalidNote`] for a blank body, a snippet
/// that does not span its lines, or a path that is not a file or a folder as the note says;
/// [`Error::OutsideRepository`] for a path that leaves the root; [`Error::NoteTextChanged`]
/// when the shown text cannot be placed in the file any more.
pub(crate) fn check_new(repo: &Repo, new: NewNote) -> Result<CheckedNote> {
    let body = new.body.trim();
    if body.is_empty() {
        return Err(Error::InvalidNote("the note has no text".to_owned()));
    }
    if let Target::Lines { lines, snippet } = &new.target {
        let count = lines.end - lines.start + 1;
        if snippet.split('\n').count() != count {
            return Err(Error::InvalidNote(format!(
                "the snippet does not have {count} lines"
            )));
        }
    }

    let target = match new.target {
        Target::Lines { lines, snippet } => {
            let text = repo.read(&new.path)?;
            let lines = locate(&text, &snippet, lines).ok_or_else(|| Error::NoteTextChanged {
                path: new.path.clone(),
            })?;
            Target::Lines { lines, snippet }
        }
        Target::Path(kind) => {
            if repo.kind(&new.path)? != Some(kind) {
                return Err(Error::InvalidNote(format!(
                    "{} is not a {kind} in the repository",
                    new.path
                )));
            }
            Target::Path(kind)
        }
    };
    Ok(CheckedNote {
        path: new.path,
        target,
        body: body.to_owned(),
    })
}

/// Saves the note `checked` as an open change note with the next free id, and every other
/// note at its current place, then returns all notes, the new one last.
///
/// # Errors
///
/// As [`current`]; [`Error::InvalidReview`] when no note id is left; [`Error::WriteReview`]
/// when the lock or the file cannot be written; [`Error::ReviewLocked`] when another process
/// holds the lock.
pub(crate) fn save(repo: &Repo, checked: CheckedNote) -> Result<Review> {
    save_note(repo, checked, Intent::Change, NoteState::Open {})
}

/// Saves the note `checked` as an ask, sent in `run` (`R4`), the run made for it (module docs,
/// "Asks"), as [`save`] saves a change, and returns all notes, the ask's id and its prompt.
/// Sent from the start: whatever becomes of the run, the ask's status is that run's.
///
/// # Errors
///
/// As [`save`].
pub(crate) fn save_ask(repo: &Repo, checked: CheckedNote, run: &str) -> Result<SavedAsk> {
    let state = NoteState::Sent {
        run: run.to_owned(),
    };
    let review = save_note(repo, checked, Intent::Ask, state)?;
    let ask = review.notes.last().expect("save_note pushed the ask last");
    Ok(SavedAsk {
        id: ask.id.to_string(),
        prompt: ask.ask_prompt(),
        review,
    })
}

/// An ask [`save_ask`] saved: every note, the ask's id, and the prompt that asks it.
#[derive(Debug)]
pub(crate) struct SavedAsk {
    pub(crate) review: Review,
    pub(crate) id: String,
    pub(crate) prompt: Prompt,
}

/// Saves `checked` as a note of `intent` in `state` with the next free id, under the lock.
/// Every note is placed in its file as it is now, the new one too: the file `check_new` read
/// may have changed since, and a read from before the lock would write another note back to
/// where it was then.
fn save_note(
    repo: &Repo,
    checked: CheckedNote,
    intent: Intent,
    state: NoteState,
) -> Result<Review> {
    let file = repo.review_file();
    let _lock = lock(&file)?;
    let mut review = Review::load(&file)?;
    // The highest id plus one, not the count plus one: ids stay unique when someone removes
    // a note by editing the file.
    let highest = review.notes.iter().map(|note| note.id.0).max().unwrap_or(0);
    let id = highest.checked_add(1).ok_or_else(|| Error::InvalidReview {
        path: file.clone(),
        problem: format!("no note id is left after N{highest}"),
    })?;

    // Pushed last: the page finds the note it just added as the last of the reply's notes.
    review.notes.push(Note {
        id: NoteId(id),
        path: checked.path,
        target: checked.target,
        body: checked.body,
        anchor: Anchor::Found,
        state,
        intent,
    });
    review.relocate(repo, HashMap::new());
    review.save(&file)?;
    Ok(review)
}

/// A new note [`check_new`] accepted, for [`save`] or [`save_ask`]. Its `Debug` output leaves
/// out the text, as for [`Note`].
pub(crate) struct CheckedNote {
    path: String,
    /// Placed in the file as it was when checked.
    target: Target,
    /// Trimmed, never blank.
    body: String,
}

impl CheckedNote {
    /// The note's text: a thread's branch is named from it.
    pub(crate) fn body(&self) -> &str {
        &self.body
    }
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
/// it, as [`current`] for reading the file, [`Error::WriteReview`] when the lock or the file
/// cannot be written, and [`Error::ReviewLocked`] when another process holds the lock.
pub(crate) fn delete(repo: &Repo, id: &str) -> Result<Review> {
    change_notes(repo, id, |notes, index| drop(notes.remove(index)))
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
    change_note(repo, id, |note| {
        note.state = match std::mem::take(&mut note.state) {
            NoteState::Open {} => NoteState::Resolved { run: None },
            NoteState::Sent { run } => NoteState::Resolved { run: Some(run) },
            resolved @ NoteState::Resolved { .. } => resolved,
        };
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
    change_note(repo, id, |note| {
        note.state = match std::mem::take(&mut note.state) {
            NoteState::Resolved { run: Some(run) } => NoteState::Sent { run },
            NoteState::Resolved { run: None } => NoteState::Open {},
            other @ (NoteState::Open {} | NoteState::Sent { .. }) => other,
        };
    })
}

/// Replaces the text of note `id` with `body`, saves, and returns all notes. Its lines,
/// snippet and state stay: a run it was sent in got the text it had then, which the run's
/// prompt keeps.
///
/// # Errors
///
/// [`Error::InvalidNote`] for a blank body, and as [`delete`].
pub(crate) fn edit(repo: &Repo, id: &str, body: &str) -> Result<Review> {
    let body = body.trim();
    if body.is_empty() {
        return Err(Error::InvalidNote("the note has no text".to_owned()));
    }
    change_note(repo, id, |note| body.clone_into(&mut note.body))
}

/// Changes note `id` with `change` and saves, under the lock.
fn change_note(repo: &Repo, id: &str, change: impl FnOnce(&mut Note)) -> Result<Review> {
    change_notes(repo, id, |notes, index| change(&mut notes[index]))
}

/// Changes the notes with `change`, given the index of note `id` among them, and saves, under
/// the lock. Ids are unique ([`Review::load`] refuses a file that repeats one), so the index
/// is the only note with that id.
fn change_notes(
    repo: &Repo,
    id: &str,
    change: impl FnOnce(&mut Vec<Note>, usize),
) -> Result<Review> {
    let wanted = NoteId::parse(id)
        .ok_or_else(|| Error::BadRequest(format!("note id {id:?}: expected N and a number")))?;
    let file = repo.review_file();
    let _lock = lock(&file)?;
    let mut review = current(repo)?;
    let index = review
        .notes
        .iter()
        .position(|note| note.id == wanted)
        .ok_or_else(|| Error::NoSuchNote { id: id.to_owned() })?;
    change(&mut review.notes, index);
    review.save(&file)?;
    Ok(review)
}

/// How a prompt of notes starts, before `report::ASK` and the notes.
pub(crate) const INTRO: &str = "These are review notes an engineer wrote while reading this \
     repository.\nAddress every note. Change nothing the notes do not ask for.\nWhen you \
     finish, list each note id with one line on what you did.\n";

/// The prompt that sends the notes `ids` to an agent, in that order (docs/design.md, "The
/// prompt"): an introduction, then each note's heading, a line note's quoted snippet, and its
/// body. The quoted snippet travels with a line note because line numbers go stale the moment
/// the agent edits a file. A file or folder note is its path and its body.
///
/// # Errors
///
/// As [`current`]; [`Error::BadRequest`] when `ids` is empty, repeats an id, holds
/// something that is not a note id, or names a resolved note (reopen it first) or an ask
/// (module docs, "Asks"); [`Error::NoSuchNote`] when no note has one of them.
pub(crate) fn prompt(repo: &Repo, ids: &[String]) -> Result<Prompt> {
    let wanted = parse_ids(ids)?;
    let review = current(repo)?;
    let mut text = String::from(INTRO);
    text.push_str(report::ASK);
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
        // A stale page could tick one: its question would read as a change to make.
        if note.intent == Intent::Ask {
            return Err(Error::BadRequest(format!(
                "note {id} is a question, answered in a thread of its own: it is never sent"
            )));
        }
        note.quote(&mut text, &mut files);
    }
    Ok(Prompt { text, files })
}

/// What an ask asks for, in place of the change notes' introduction and report: an answer,
/// and no change. Kept by a fresh agent's follow-up of an ask too (`run.rs`).
pub(crate) const ASK_RULES: &str = "Answer from the files in this repository as they are. Do \
     not edit, create or delete any file, do not run a command that changes one, and do not \
     commit.\n";

/// The notes `ids` that are still there, as Markdown for a pull request's description: each
/// note's id, its place and its text, with no snippet of the code it is on. A snippet can hold
/// what must not leave the machine (a key a note asks to remove), and the pull request's diff
/// shows the code. Notes deleted since are left out.
///
/// # Errors
///
/// As [`current`], and [`Error::InvalidNote`] for an id that is not `N` and a number.
pub(crate) fn listed(repo: &Repo, ids: &[String]) -> Result<String> {
    let wanted = parse_ids(ids)?;
    let review = current(repo)?;
    let mut text = String::new();
    for note in wanted
        .iter()
        .filter_map(|id| review.notes.iter().find(|note| note.id == *id))
    {
        text.push_str(&format!(
            "**{}**, `{}`\n\n{}\n\n",
            note.id,
            note.place(),
            note.body.trim_end()
        ));
    }
    Ok(text)
}

/// The text of note `id`: a thread's branch is named from its first note's.
///
/// # Errors
///
/// As [`current`]; [`Error::BadRequest`] when `id` is not a note id, and
/// [`Error::NoSuchNote`] when no note has it.
pub(crate) fn body(repo: &Repo, id: &str) -> Result<String> {
    let wanted = parse_ids(&[id.to_owned()])?;
    current(repo)?
        .notes
        .into_iter()
        .find(|note| note.id == wanted[0])
        .map(|note| note.body)
        .ok_or_else(|| Error::NoSuchNote { id: id.to_owned() })
}

/// A prompt built from notes. Its `Debug` output leaves out the text, as for [`Note`].
pub(crate) struct Prompt {
    /// The text the agent receives.
    pub(crate) text: String,
    /// The files linked after the text: the files of the line and file notes, relative to the
    /// repository root, each once, in note order. Folders are named in the text only.
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
/// As [`current`]; [`Error::BadRequest`] when `ids` is empty, repeats an id, or holds
/// something that is not a note id; [`Error::WriteReview`] when the lock or the file
/// cannot be written; [`Error::ReviewLocked`] when another process holds the lock.
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

/// A note the engineer wrote on lines, a file or a folder.
///
/// Stored as one flat object: `id`, `path`, `target`, then `lines` and `snippet` for a line
/// note only, `body`, `anchor`, `state`, `intent`. A script reads every note the same way, and
/// a line note keeps the fields it had before version 4. Read through [`StoredNote`].
#[derive(Deserialize)]
#[serde(try_from = "StoredNote")]
struct Note {
    id: NoteId,
    /// Relative to the repository root.
    path: String,
    target: Target,
    body: String,
    /// Recomputed on every read.
    anchor: Anchor,
    state: NoteState,
    intent: Intent,
}

/// What a note asks of an agent (module docs, "Asks").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Intent {
    /// A change to make: sent with other notes to an agent that works.
    #[default]
    Change,
    /// A question to answer, by an agent that only reads.
    Ask,
}

impl Note {
    /// The prompt that asks this note as a question (module docs, "Asks"): the engineer's
    /// question about its target, quoted as [`prompt`] quotes a note, after the rule that
    /// nothing is changed. It asks for no report: there is no change to report on.
    fn ask_prompt(&self) -> Prompt {
        let mut text = format!(
            "An engineer reading this repository asks a question about the place named below.\n\
             {ASK_RULES}"
        );
        let mut files = Vec::new();
        self.quote(&mut text, &mut files);
        Prompt { text, files }
    }

    /// Appends the note to a prompt's `text`, as [`prompt`] quotes each note: its heading, a
    /// line note's quoted snippet, and its body; and its file to `files`, the prompt's links,
    /// unless it is there already.
    fn quote(&self, text: &mut String, files: &mut Vec<String>) {
        let lost = self.anchor == Anchor::Lost;
        text.push_str(&format!("\n[{}] {}\n", self.id, self.place()));
        match &self.target {
            Target::Lines { snippet, .. } => {
                if lost {
                    text.push_str("(These lines are no longer in the file; they were:)\n");
                }
                for line in snippet.split('\n') {
                    text.push_str(&format!("> {line}\n"));
                }
            }
            Target::Path(kind) => {
                if lost {
                    text.push_str(&format!("(This {kind} is no longer in the repository.)\n"));
                }
            }
        }
        text.push_str(self.body.trim_end());
        text.push('\n');
        // A folder gets no link: agents read a `resource_link` as a file, and the path in the
        // text already names the folder. A symlink to a folder is linked all the same: git and
        // the file list treat it as a file (`Repo::kind`), and its note is a file note.
        let linked = !matches!(self.target, Target::Path(PathKind::Folder));
        if linked && !files.contains(&self.path) {
            files.push(self.path.clone());
        }
    }

    /// Where the note is, as a prompt names it: `src/a.rs:3-5`, `src/a.rs:3`, `src/ (folder)`
    /// or `a.rs (file)`.
    fn place(&self) -> String {
        match &self.target {
            Target::Lines { lines, .. } => {
                let LineRange { start, end } = *lines;
                if start == end {
                    format!("{}:{start}", self.path)
                } else {
                    format!("{}:{start}-{end}", self.path)
                }
            }
            Target::Path(kind) => {
                let slash = if *kind == PathKind::Folder { "/" } else { "" };
                format!("{}{slash} ({kind})", self.path)
            }
        }
    }
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

/// A [`Note`] as stored, before [`Target::read`] checks that its fields fit its target.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredNote {
    id: NoteId,
    path: String,
    #[serde(default)]
    target: TargetKind,
    lines: Option<LineRange>,
    snippet: Option<String>,
    body: String,
    /// Recomputed on every read, so a note written by hand may leave it out.
    #[serde(default)]
    anchor: Anchor,
    /// Absent in version 1, where every note is open; a note written by hand may leave it
    /// out too.
    #[serde(default)]
    state: NoteState,
    /// Absent before version 5, where every note is a change.
    #[serde(default)]
    intent: Intent,
}

/// What the page sends to create a note: `{path, lines, snippet, body}` for lines, as before
/// version 4, or `{path, target: "file" | "folder", body}`.
#[derive(Deserialize)]
#[serde(try_from = "RawNewNote")]
pub(crate) struct NewNote {
    /// Relative to the repository root.
    path: String,
    /// For lines, the snippet is their text as the page showed it.
    target: Target,
    body: String,
}

/// A [`NewNote`] before its fields are checked against its target.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNewNote {
    path: String,
    #[serde(default)]
    target: TargetKind,
    lines: Option<LineRange>,
    snippet: Option<String>,
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

/// Whether a note's target is still there: its snippet in its file, or its file or folder.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Anchor {
    /// The snippet is in the file, at `lines`; or the path is a file or folder as the note
    /// says.
    #[default]
    Found,
    /// The snippet is not in the file where [`locate`] may place it, or the file cannot be
    /// read, and `lines` is where it was last seen; or the path is gone, or is no longer of
    /// the note's kind.
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
        let version = version_of(json.as_bytes()).map_err(parse_error)?;
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

    /// Replaces the review file with this review (`files::save_json`).
    fn save(&self, file: &Path) -> Result<()> {
        save_json(file, self).map_err(|source| Error::WriteReview {
            path: file.to_owned(),
            source,
        })
    }

    /// Places every line note by [`locate`] in its file as it is now, and finds every file
    /// and folder note's path. `read` holds the files read already, by path, `None` for one
    /// that cannot be read; the others are read here.
    ///
    /// Reads each file and splits it into lines once, however many notes it has: splitting it
    /// again for each note took 17 ms of a `GET /api/notes` with 200 notes on ten files of
    /// 5000 lines, 2 ms once split. Then compares every position in the file with each of its
    /// notes' snippets. A file or folder note costs one `lstat`, and reads nothing.
    fn relocate(&mut self, repo: &Repo, mut read: HashMap<String, Option<String>>) {
        for note in &self.notes {
            let lines = matches!(note.target, Target::Lines { .. });
            if lines && !read.contains_key(&note.path) {
                read.insert(note.path.clone(), repo.read(&note.path).ok());
            }
        }
        // Split as `locate` splits.
        let split: HashMap<&str, Vec<&str>> = read
            .iter()
            .filter_map(|(path, text)| Some((path.as_str(), text.as_deref()?.lines().collect())))
            .collect();
        for note in &mut self.notes {
            let found = match &mut note.target {
                Target::Lines { lines, snippet } => {
                    let place = split.get(note.path.as_str());
                    match place.and_then(|text| locate_lines(text, snippet, *lines)) {
                        Some(now) => {
                            *lines = now;
                            true
                        }
                        None => false,
                    }
                }
                Target::Path(kind) => repo.kind(&note.path).ok().flatten() == Some(*kind),
            };
            note.anchor = if found { Anchor::Found } else { Anchor::Lost };
        }
    }
}

/// Takes the exclusive lock on `review.lock` beside the review file (`files::lock`). The lock
/// is released when the returned file is dropped.
fn lock(review_file: &Path) -> Result<File> {
    let path = review_file.with_extension("lock");
    files::lock(&path).map_err(|error| match error {
        TryLockError::WouldBlock => Error::ReviewLocked {
            path,
            seconds: files::LOCK_WAIT.as_secs(),
        },
        TryLockError::Error(source) => Error::WriteReview { path, source },
    })
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
            .field("target", &self.target)
            .field("anchor", &self.anchor)
            .field("state", &self.state)
            .field("intent", &self.intent)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for CheckedNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckedNote")
            .field("path", &self.path)
            .field("target", &self.target)
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
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl Serialize for Note {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let (kind, lines) = match &self.target {
            Target::Lines { lines, snippet } => (TargetKind::Lines, Some((lines, snippet))),
            Target::Path(PathKind::File) => (TargetKind::File, None),
            Target::Path(PathKind::Folder) => (TargetKind::Folder, None),
        };
        let mut note = serializer.serialize_struct("Note", 9)?;
        note.serialize_field("id", &self.id)?;
        note.serialize_field("path", &self.path)?;
        note.serialize_field("target", &kind)?;
        if let Some((lines, snippet)) = lines {
            note.serialize_field("lines", lines)?;
            note.serialize_field("snippet", snippet)?;
        } else {
            note.skip_field("lines")?;
            note.skip_field("snippet")?;
        }
        note.serialize_field("body", &self.body)?;
        note.serialize_field("anchor", &self.anchor)?;
        note.serialize_field("state", &self.state)?;
        note.serialize_field("intent", &self.intent)?;
        note.end()
    }
}

impl TryFrom<StoredNote> for Note {
    type Error = String;

    fn try_from(stored: StoredNote) -> std::result::Result<Self, Self::Error> {
        let StoredNote {
            id,
            path,
            target,
            lines,
            snippet,
            body,
            anchor,
            state,
            intent,
        } = stored;
        let target = Target::read(target, lines, snippet, "note")
            .map_err(|problem| format!("{id}: {problem}"))?;
        Ok(Self {
            id,
            path,
            target,
            body,
            anchor,
            state,
            intent,
        })
    }
}

impl TryFrom<RawNewNote> for NewNote {
    type Error = String;

    fn try_from(raw: RawNewNote) -> std::result::Result<Self, Self::Error> {
        let RawNewNote {
            path,
            target,
            lines,
            snippet,
            body,
        } = raw;
        Ok(Self {
            path,
            target: Target::read(target, lines, snippet, "note")?,
            body,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let lines = || Target::Lines {
            lines: LineRange { start: 1, end: 1 },
            snippet: "secret snippet".to_owned(),
        };
        let new = NewNote {
            path: "src/a.rs".to_owned(),
            target: lines(),
            body: "secret body".to_owned(),
        };
        let note = Note {
            id: NoteId(1),
            path: "src/a.rs".to_owned(),
            target: lines(),
            body: "secret body".to_owned(),
            anchor: Anchor::Found,
            state: NoteState::Sent {
                run: "R1".to_owned(),
            },
            intent: Intent::Ask,
        };
        let review = Review {
            version: VERSION,
            notes: vec![note],
        };
        let checked = CheckedNote {
            path: "src/a.rs".to_owned(),
            target: lines(),
            body: "secret body".to_owned(),
        };
        let shown = format!("{checked:?}");
        assert!(
            shown.contains("src/a.rs") && shown.contains("start: 1"),
            "{shown}"
        );
        assert!(!shown.contains("secret"), "{shown}");

        for debug in [format!("{review:?}"), format!("{new:?}")] {
            assert!(debug.contains("src/a.rs"), "{debug}");
            assert!(!debug.contains("secret"), "{debug}");
        }
        assert!(format!("{review:?}").contains("NoteId(1)"));
        // The lines show, where the snippet is left out.
        for debug in [format!("{review:?}"), format!("{new:?}")] {
            assert!(debug.contains("Lines { lines: LineRange { start: 1, end: 1 }, .. }"));
        }
    }

    #[test]
    fn the_file_is_pretty_json_ending_in_a_newline_byte_for_byte() {
        let dir = std::env::temp_dir().join(format!("postil-notes-{}-bytes", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        let file = dir.join("review.json");
        let lines = || Target::Lines {
            lines: LineRange { start: 2, end: 3 },
            snippet: "b\nc".to_owned(),
        };
        let note = |id, path: &str, target, state| Note {
            id: NoteId(id),
            path: path.to_owned(),
            target,
            body: "Why?".to_owned(),
            anchor: Anchor::Found,
            state,
            intent: Intent::Change,
        };
        let ask = Note {
            intent: Intent::Ask,
            ..note(6, "b.rs", Target::Path(PathKind::File), NoteState::Open {})
        };
        let review = Review {
            version: VERSION,
            notes: vec![
                note(1, "src/a.rs", lines(), NoteState::Open {}),
                note(
                    2,
                    "src/a.rs",
                    lines(),
                    NoteState::Sent {
                        run: "R1".to_owned(),
                    },
                ),
                note(3, "src/a.rs", lines(), NoteState::Resolved { run: None }),
                note(4, "src", Target::Path(PathKind::Folder), NoteState::Open {}),
                note(5, "b.rs", Target::Path(PathKind::File), NoteState::Open {}),
                ask,
            ],
        };

        review.save(&file).unwrap();

        let open = "        \"kind\": \"open\"";
        let note_json = |id: u32, path: &str, target: &str, state: &str, intent: &str| {
            let lines = if target == "lines" {
                "      \"lines\": {\n        \"start\": 2,\n        \"end\": 3\n      },\n      \
                 \"snippet\": \"b\\nc\",\n"
            } else {
                ""
            };
            format!(
                "    {{\n      \"id\": \"N{id}\",\n      \"path\": \"{path}\",\n      \
                 \"target\": \"{target}\",\n{lines}      \"body\": \"Why?\",\n      \
                 \"anchor\": \"found\",\n      \"state\": {{\n{state}\n      }},\n      \
                 \"intent\": \"{intent}\"\n    }}"
            )
        };
        let expected = format!(
            "{{\n  \"version\": 5,\n  \"notes\": [\n{},\n{},\n{},\n{},\n{},\n{}\n  ]\n}}\n",
            note_json(1, "src/a.rs", "lines", open, "change"),
            note_json(
                2,
                "src/a.rs",
                "lines",
                "        \"kind\": \"sent\",\n        \"run\": \"R1\"",
                "change"
            ),
            note_json(
                3,
                "src/a.rs",
                "lines",
                "        \"kind\": \"resolved\"",
                "change"
            ),
            note_json(4, "src", "folder", open, "change"),
            note_json(5, "b.rs", "file", open, "change"),
            note_json(6, "b.rs", "file", open, "ask"),
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
        drop(fs::remove_dir_all(&dir));
    }
}
