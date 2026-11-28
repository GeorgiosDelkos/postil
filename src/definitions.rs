//! Jump to definition: an index from a name to the places it is defined, matched by name with
//! the grammars' tags queries (docs/review-assist.md, "Jump to definition").
//!
//! # Design
//!
//! Each open project has one thread, `index-definitions`, that reads the files and owns what it
//! learned of each: its stat data and its definitions. A pass lists the files, reparses only
//! those whose size or modification time changed, and publishes a new [`Index`] when anything
//! did. The index is immutable once published, behind an `Arc`, so a lookup holds the lock
//! only to clone that `Arc` and never waits on a pass. The request thread (`page-requests`)
//! only looks up; it never reads or parses a file for this.
//!
//! # Invariants
//!
//! The lock holds [`State`]:
//!
//! - `completed <= requested`. A pass runs while `completed < requested`, and when it ends,
//!   `completed` becomes the `requested` it saw when it started, so refreshes asked for
//!   during a pass are answered by one more pass, not one each.
//! - `index` is `None` only until the first pass ends, and then always holds the latest
//!   published index: a refresh never clears it, so a lookup during a refresh answers from
//!   the index as it was.
//! - Once `closed` is set, it stays set, and the thread ends at its next look at the state:
//!   between two files, or while it waits.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::SystemTime;

use serde::Serialize;
use tree_sitter_tags::TagsContext;

use crate::highlight::{self, MAX_HIGHLIGHTED, TagsGrammar};
use crate::repo::Repo;

/// The most definitions a lookup lists. Arbitrary: more than a list of one-line cards can be
/// read through; the reply says how many there were in all.
pub(crate) const MAX_LISTED: usize = 50;

/// The index of one open project, built and refreshed on its own thread. Cloning gives
/// another handle on the same index.
#[derive(Clone)]
pub(crate) struct Definitions {
    shared: Arc<Shared>,
}

/// What the handles and the thread share.
struct Shared {
    state: Mutex<State>,
    /// The published symbol index, updated incrementally on refresh so lookups never wait on a
    /// full-repo rebuild.
    index: RwLock<Option<Index>>,
    /// Lock-free mirror of `State::closed` checked between files without taking `state`.
    closed: AtomicBool,
    /// Signalled when a refresh is asked for, the handles are closed, or a pass ends.
    changed: Condvar,
}

/// See the module's invariants.
#[derive(Debug)]
struct State {
    /// Passes asked for: one at start, and one per refresh.
    requested: u64,
    /// The `requested` that the last finished pass started from.
    completed: u64,
    closed: bool,
    /// What the thread is doing, for the tests: nothing else can see it wait or end.
    #[cfg(test)]
    worker: Worker,
    /// Set by a test to stop a pass before its next file, until the test clears it.
    #[cfg(test)]
    hold: bool,
}

/// What the thread is doing: `State::worker`.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Worker {
    /// In a pass, or between two.
    Busy,
    /// Stopped in a pass before a file, while `State::hold` is set.
    Held,
    /// Asleep until a refresh or a close.
    Waiting,
    /// Ended.
    Stopped,
}

impl Definitions {
    /// Starts indexing `repo` on a thread of its own. Returns at once: lookups say the index
    /// is building until the first pass ends.
    pub(crate) fn new(repo: Arc<Repo>) -> Self {
        Self::start(repo, Shared::new())
    }

    /// As [`Definitions::new`], from `shared` as it is: a test sets `hold` in it first.
    fn start(repo: Arc<Repo>, shared: Shared) -> Self {
        let shared = Arc::new(shared);
        let worker = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("index-definitions".to_owned())
            .spawn(move || {
                work(&repo, &worker);
                #[cfg(test)]
                {
                    worker.lock().worker = Worker::Stopped;
                    worker.changed.notify_all();
                }
            });
        if spawned.is_err() {
            // Fails only when the process is out of threads or memory. Lookups then find no
            // definitions rather than say "building" forever.
            *shared.index.write().unwrap_or_else(PoisonError::into_inner) = Some(Index::default());
            let mut state = shared.lock();
            state.completed = state.requested;
        }
        Self { shared }
    }

    /// Asks the thread to look at the files again, and returns at once. Refreshes asked for
    /// while a pass runs share one more pass.
    pub(crate) fn refresh(&self) {
        let mut state = self.shared.lock();
        state.requested += 1;
        self.shared.changed.notify_all();
    }

    /// Stops the thread, between two files when a pass runs. Does not wait for it. Lookups
    /// answer from the last index after this.
    pub(crate) fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.lock().closed = true;
        self.shared.changed.notify_all();
    }

    /// The definitions of `name`, those nearest `from` (a path in the repository, or empty)
    /// first. Takes a shared read lock on the index; reads no file.
    pub(crate) fn lookup(&self, name: &str, from: &str) -> Lookup {
        let guard = self
            .shared
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        match guard.as_ref() {
            Some(index) => index.lookup(name, from),
            None => Lookup::Building,
        }
    }

    /// Waits until the passes asked for so far have ended.
    ///
    /// # Panics
    ///
    /// When they take longer than ten seconds: a test's repository takes milliseconds.
    #[cfg(test)]
    pub(crate) fn wait(&self) {
        let state = self.shared.lock();
        let (_state, waited) = self
            .shared
            .changed
            .wait_timeout_while(state, std::time::Duration::from_secs(10), |state| {
                state.completed < state.requested
            })
            .unwrap_or_else(PoisonError::into_inner);
        assert!(!waited.timed_out(), "the definitions index took over 10 s");
    }
}

impl fmt::Debug for Definitions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.shared.lock();
        let names = self
            .shared
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|index| index.by_name.len());
        f.debug_struct("Definitions")
            .field("requested", &state.requested)
            .field("completed", &state.completed)
            .field("names", &names)
            .field("closed", &state.closed)
            .finish()
    }
}

impl Shared {
    fn new() -> Self {
        Self {
            state: Mutex::new(State {
                requested: 1,
                completed: 0,
                closed: false,
                #[cfg(test)]
                worker: Worker::Busy,
                #[cfg(test)]
                hold: false,
            }),
            index: RwLock::new(None),
            closed: AtomicBool::new(false),
            changed: Condvar::new(),
        }
    }

    /// Locks the state. Poisoning is ignored: every change to the state is one assignment,
    /// made with no call in between that could panic, so no panic leaves it half-written.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Called by a pass before each file: whether to stop, the handles being closed. In the
    /// tests, it also waits here while `hold` is set.
    fn between_files(&self) -> bool {
        #[cfg(test)]
        {
            let mut state = self.lock();
            if state.hold && !state.closed {
                state.worker = Worker::Held;
                self.changed.notify_all();
                state = self
                    .changed
                    .wait_while(state, |state| state.hold && !state.closed)
                    .unwrap_or_else(PoisonError::into_inner);
                state.worker = Worker::Busy;
            }
        }
        self.closed()
    }
}

/// The reply to `GET /api/definitions`.
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub(crate) enum Lookup {
    /// The first pass has not ended: the page asks again shortly.
    Building,
    Ready {
        /// At most [`MAX_LISTED`], ranked.
        definitions: Vec<Definition>,
        /// How many there are in all.
        total: usize,
    },
}

/// A place a name is defined.
#[derive(Debug, Serialize)]
pub(crate) struct Definition {
    pub(crate) path: String,
    /// 1-based: the line of the name.
    pub(crate) line: u32,
    pub(crate) kind: DefinitionKind,
}

/// What a definition defines: the `@definition.<kind>` captures of the grammars' tags queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DefinitionKind {
    Class,
    Constant,
    Function,
    Interface,
    Macro,
    Method,
    Module,
    Type,
}

impl DefinitionKind {
    /// The kind a capture `definition.<kind>` names, by its `<kind>`; `None` for one no
    /// grammar of `GRAMMARS` uses, which `every_tags_query_compiles_and_names_known_kinds`
    /// (`highlight.rs`) rules out.
    pub(crate) fn from_capture(kind: &str) -> Option<Self> {
        Some(match kind {
            "class" => Self::Class,
            "constant" => Self::Constant,
            "function" => Self::Function,
            "interface" => Self::Interface,
            "macro" => Self::Macro,
            "method" => Self::Method,
            "module" => Self::Module,
            "type" => Self::Type,
            _ => return None,
        })
    }
}

/// Every definition of the project, by name. Built whole after a pass that changed anything,
/// then only read.
#[derive(Debug, Default)]
struct Index {
    by_name: HashMap<Arc<str>, Vec<StoredDefinition>>,
}

#[derive(Debug)]
struct StoredDefinition {
    /// Shared with every other definition of the file, and with the thread's record of it.
    path: Arc<str>,
    line: u32,
    kind: DefinitionKind,
}

impl Index {
    fn build(files: &HashMap<Arc<str>, IndexedFile>) -> Self {
        let mut by_name: HashMap<Arc<str>, Vec<StoredDefinition>> =
            HashMap::with_capacity(files.len().saturating_mul(4));
        for (path, file) in files {
            for symbol in &file.symbols {
                let def = StoredDefinition {
                    path: Arc::clone(path),
                    line: symbol.line,
                    kind: symbol.kind,
                };
                if let Some(list) = by_name.get_mut(&symbol.name) {
                    list.push(def);
                } else {
                    by_name.insert(Arc::clone(&symbol.name), vec![def]);
                }
            }
        }
        Self { by_name }
    }

    fn apply_delta(&mut self, delta: &PassDelta) {
        for (path, old_symbols) in &delta.removed {
            self.remove_file(path, old_symbols);
        }
        for (path, old_symbols, new_symbols) in &delta.updated {
            if !old_symbols.is_empty() {
                self.remove_file(path, old_symbols);
            }
            for symbol in new_symbols {
                let def = StoredDefinition {
                    path: Arc::clone(path),
                    line: symbol.line,
                    kind: symbol.kind,
                };
                if let Some(list) = self.by_name.get_mut(&symbol.name) {
                    list.push(def);
                } else {
                    self.by_name.insert(Arc::clone(&symbol.name), vec![def]);
                }
            }
        }
    }

    fn remove_file(&mut self, path: &str, old_symbols: &[FileSymbol]) {
        for symbol in old_symbols {
            let empty = if let Some(list) = self.by_name.get_mut(&symbol.name) {
                list.retain(|def| def.path.as_ref() != path);
                list.is_empty()
            } else {
                false
            };
            if empty {
                self.by_name.remove(&symbol.name);
            }
        }
    }

    /// The definitions of `name`, ranked: in the file `from`, then in its folder, then the
    /// rest; by path and line within each.
    fn lookup(&self, name: &str, from: &str) -> Lookup {
        let found = self.by_name.get(name).map_or(&[][..], Vec::as_slice);
        let mut ranked: Vec<&StoredDefinition> = found.iter().collect();
        let cmp = |a: &&StoredDefinition, b: &&StoredDefinition| {
            let ka = (nearness(&a.path, from), &*a.path, a.line);
            let kb = (nearness(&b.path, from), &*b.path, b.line);
            ka.cmp(&kb)
        };
        if ranked.len() > MAX_LISTED {
            ranked.select_nth_unstable_by(MAX_LISTED, cmp);
            ranked.truncate(MAX_LISTED);
        }
        ranked.sort_by(cmp);
        let definitions = ranked
            .into_iter()
            .map(|definition| Definition {
                path: definition.path.to_string(),
                line: definition.line,
                kind: definition.kind,
            })
            .collect();
        Lookup::Ready {
            definitions,
            total: found.len(),
        }
    }
}

/// How near a definition in `path` is to the file `from`, nearest first. The derived order
/// is the ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Nearness {
    SameFile,
    SameFolder,
    Elsewhere,
}

fn nearness(path: &str, from: &str) -> Nearness {
    fn folder(path: &str) -> &str {
        path.rsplit_once('/').map_or("", |(folder, _)| folder)
    }
    if path == from {
        Nearness::SameFile
    } else if !from.is_empty() && folder(path) == folder(from) {
        Nearness::SameFolder
    } else {
        Nearness::Elsewhere
    }
}

/// The thread's loop: a pass whenever one is asked for, until the handles close.
fn work(repo: &Repo, shared: &Shared) {
    let mut indexer = Indexer::new(repo);
    loop {
        let target = {
            let asleep = |state: &mut State| !state.closed && state.completed == state.requested;
            let state = shared.lock();
            // The lock is held from here until the wait releases it, so a test that sees
            // `Waiting` sees the thread asleep.
            #[cfg(test)]
            let state = {
                let mut state = state;
                if asleep(&mut state) {
                    state.worker = Worker::Waiting;
                    shared.changed.notify_all();
                }
                state
            };
            let state = shared
                .changed
                .wait_while(state, asleep)
                .unwrap_or_else(PoisonError::into_inner);
            #[cfg(test)]
            let state = {
                let mut state = state;
                state.worker = Worker::Busy;
                state
            };
            if state.closed {
                return;
            }
            state.requested
        };

        let has_index = shared
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some();
        match indexer.pass(|| shared.between_files(), has_index) {
            Pass::Closed => return,
            Pass::Changed(delta) => {
                if has_index {
                    let mut guard = shared.index.write().unwrap_or_else(PoisonError::into_inner);
                    if let Some(index) = guard.as_mut() {
                        index.apply_delta(&delta);
                    }
                } else {
                    let built = Index::build(&indexer.files);
                    *shared.index.write().unwrap_or_else(PoisonError::into_inner) = Some(built);
                }
            }
            Pass::Unchanged => {
                if !has_index {
                    // A project with no file to index still finishes building.
                    *shared.index.write().unwrap_or_else(PoisonError::into_inner) =
                        Some(Index::default());
                }
            }
        }
        {
            let mut state = shared.lock();
            state.completed = target;
            shared.changed.notify_all();
        }
    }
}

/// Incremental changes produced by a pass for updating the published [`Index`] in place.
#[derive(Default)]
struct PassDelta {
    removed: Vec<(Arc<str>, Vec<FileSymbol>)>,
    updated: Vec<(Arc<str>, Vec<FileSymbol>, Vec<FileSymbol>)>,
}

/// How a pass ended.
enum Pass {
    /// A file was added, removed, or reparsed.
    Changed(PassDelta),
    Unchanged,
    /// The handles closed during the pass, which stopped between two files.
    Closed,
}

/// What the thread knows of the files, kept between passes. Never shared.
struct Indexer<'a> {
    repo: &'a Repo,
    /// Every file indexed by the last pass, by path.
    files: HashMap<Arc<str>, IndexedFile>,
    /// Monotonic pass counter used to prune deleted files in place without reallocating `files`.
    pass_id: u64,
    /// Kept between files so the parser is allocated once.
    context: TagsContext,
    /// Scratch buffer reused when reading files, avoiding a fresh `Vec<u8>` per file.
    read_buf: Vec<u8>,
}

/// A file as the last pass saw it.
struct IndexedFile {
    stamp: Stamp,
    symbols: Vec<FileSymbol>,
    seen_pass: u64,
}

/// What tells a changed file apart without reading it, as the design says: its size and
/// modification time. An edit that keeps both, within the file system's time resolution
/// (nanoseconds on APFS), goes unseen until the next change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone)]
struct FileSymbol {
    name: Arc<str>,
    line: u32,
    kind: DefinitionKind,
}

type ParsedFile = (String, Stamp, Option<Vec<FileSymbol>>);

impl<'a> Indexer<'a> {
    fn new(repo: &'a Repo) -> Self {
        Self {
            repo,
            files: HashMap::new(),
            pass_id: 0,
            context: TagsContext::new(),
            read_buf: Vec::new(),
        }
    }

    /// Looks at every file of the repository with a tags query: one `stat` for each, and a
    /// read and a parse for those that are new or changed. Files gone since the last pass
    /// drop out. Stops between two files once `closed` says so.
    fn pass(&mut self, closed: impl Fn() -> bool + Sync, track_delta: bool) -> Pass {
        // When git cannot list the files, the index stays as it was, an empty one after the
        // first pass, and the next refresh tries again. The file list fails the same way
        // (`GET /api/repo`), which is where the engineer sees git's message.
        let Ok(paths) = self.repo.files() else {
            return Pass::Unchanged;
        };
        self.pass_id += 1;
        let pass_id = self.pass_id;
        let mut delta = PassDelta::default();
        let mut full = PathBuf::from(self.repo.root());
        let mut to_parse: Vec<(String, PathBuf, Stamp, TagsGrammar)> = Vec::new();
        for path in paths {
            if closed() {
                return Pass::Closed;
            }
            let Some(grammar) = highlight::tags_grammar(&path) else {
                continue;
            };
            // Reuse a single `PathBuf` across files to avoid a heap allocation per path.
            full.clear();
            full.push(self.repo.root());
            full.push(&path);
            // A symlink is followed, wherever it points, as `Repo::read` does: the index then
            // holds what opening the file shows.
            let Some(metadata) = fs::metadata(&full).ok().filter(fs::Metadata::is_file) else {
                continue;
            };
            let stamp = Stamp {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            };
            if let Some(existing) = self.files.get_mut(path.as_str())
                && existing.stamp == stamp
            {
                existing.seen_pass = pass_id;
                continue;
            }
            to_parse.push((path, full.clone(), stamp, grammar));
        }

        // Parse changed files in parallel when there are enough to amortize thread spawning
        // (e.g. initial open of a large repo), falling back to sequential for small incremental
        // updates.
        let Some(parsed) = self.parse_batch(&to_parse, &closed) else {
            return Pass::Closed;
        };
        let mut changed = self.apply_parsed(parsed, pass_id, track_delta, &mut delta);
        self.files.retain(|key, file| {
            if file.seen_pass == pass_id {
                true
            } else {
                changed = true;
                if track_delta {
                    delta
                        .removed
                        .push((Arc::clone(key), std::mem::take(&mut file.symbols)));
                }
                false
            }
        });
        // Release peak buffer capacity after scanning large files so idle threads stay light.
        if self.read_buf.capacity() > 64 * 1024 {
            self.read_buf.shrink_to(64 * 1024);
        }
        if changed {
            Pass::Changed(delta)
        } else {
            Pass::Unchanged
        }
    }

    fn apply_parsed(
        &mut self,
        parsed: Vec<ParsedFile>,
        pass_id: u64,
        track_delta: bool,
        delta: &mut PassDelta,
    ) -> bool {
        let mut changed = false;
        for (path, stamp, symbols) in parsed {
            // A file that cannot be read now is left out, so the next pass tries again;
            // the definitions it had go with it.
            let Some(symbols) = symbols else {
                if let Some((key, old)) = self.files.remove_entry(path.as_str()) {
                    changed = true;
                    if track_delta {
                        delta.removed.push((key, old.symbols));
                    }
                }
                continue;
            };
            changed = true;
            if let Some(key) = self
                .files
                .get_key_value(path.as_str())
                .map(|(k, _)| Arc::clone(k))
            {
                let entry = self
                    .files
                    .get_mut(&key)
                    .expect("key came from get_key_value");
                let old_symbols = std::mem::replace(&mut entry.symbols, symbols);
                entry.stamp = stamp;
                entry.seen_pass = pass_id;
                if track_delta {
                    delta
                        .updated
                        .push((key, old_symbols, entry.symbols.clone()));
                }
            } else {
                let key: Arc<str> = Arc::from(path);
                if track_delta {
                    delta
                        .updated
                        .push((Arc::clone(&key), Vec::new(), symbols.clone()));
                }
                self.files.insert(
                    key,
                    IndexedFile {
                        stamp,
                        symbols,
                        seen_pass: pass_id,
                    },
                );
            }
        }
        changed
    }

    fn parse_batch(
        &mut self,
        to_parse: &[(String, PathBuf, Stamp, TagsGrammar)],
        closed: &(impl Fn() -> bool + Sync),
    ) -> Option<Vec<ParsedFile>> {
        if to_parse.len() < 8 {
            let mut out = Vec::with_capacity(to_parse.len());
            for (path, file_path, stamp, grammar) in to_parse {
                if closed() {
                    return None;
                }
                let syms = self.symbols(file_path, *stamp, *grammar);
                out.push((path.clone(), *stamp, syms));
            }
            return Some(out);
        }
        let workers = std::thread::available_parallelism()
            .map_or(2, std::num::NonZeroUsize::get)
            .min(4)
            .min(to_parse.len());
        let chunk_size = to_parse.len().div_ceil(workers);
        let stopped = AtomicBool::new(false);
        let mut batches: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = to_parse
                .chunks(chunk_size)
                .map(|chunk| {
                    let stopped_ref = &stopped;
                    scope.spawn(move || {
                        let mut ctx = TagsContext::new();
                        let mut buf = Vec::new();
                        let mut out = Vec::with_capacity(chunk.len());
                        for (path, file_path, stamp, grammar) in chunk {
                            if stopped_ref.load(Ordering::Relaxed) || closed() {
                                stopped_ref.store(true, Ordering::Relaxed);
                                return out;
                            }
                            let syms =
                                parse_symbols(&mut ctx, &mut buf, file_path, *stamp, *grammar);
                            out.push((path.clone(), *stamp, syms));
                        }
                        out
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("definitions parser worker does not panic"))
                .collect()
        });
        if stopped.load(Ordering::Relaxed) || closed() {
            return None;
        }
        let mut merged = Vec::with_capacity(to_parse.len());
        for mut batch in batches.drain(..) {
            merged.append(&mut batch);
        }
        Some(merged)
    }

    /// The definitions in the file at `full`; none for one over [`MAX_HIGHLIGHTED`] bytes or
    /// not UTF-8. `None` when it cannot be read.
    fn symbols(
        &mut self,
        full: &std::path::Path,
        stamp: Stamp,
        grammar: TagsGrammar,
    ) -> Option<Vec<FileSymbol>> {
        parse_symbols(&mut self.context, &mut self.read_buf, full, stamp, grammar)
    }
}

/// Parses definitions in `full` using the caller's reusable `context` and `read_buf`.
fn parse_symbols(
    context: &mut TagsContext,
    read_buf: &mut Vec<u8>,
    full: &std::path::Path,
    stamp: Stamp,
    grammar: TagsGrammar,
) -> Option<Vec<FileSymbol>> {
    let limit = MAX_HIGHLIGHTED as u64;
    if stamp.len > limit {
        return Some(Vec::new());
    }
    // At most one byte past the limit: the file may have grown since its `stat`.
    read_buf.clear();
    File::open(full)
        .and_then(|file| file.take(limit + 1).read_to_end(read_buf))
        .ok()?;
    let Ok(source) = std::str::from_utf8(read_buf) else {
        return Some(Vec::new());
    };
    if source.len() > MAX_HIGHLIGHTED {
        return Some(Vec::new());
    }

    let configuration = grammar.configuration();
    let Ok((tags, _)) = context.generate_tags(configuration, source.as_bytes(), None) else {
        // Only a cancelled parse fails, and none is cancelled.
        return Some(Vec::new());
    };
    let mut symbols = Vec::new();
    // A tag that fails ends the file's tags: the iterator fails only when cancelled.
    for tag in tags.map_while(Result::ok) {
        if !tag.is_definition {
            continue;
        }
        let Some(kind) =
            DefinitionKind::from_capture(configuration.syntax_type_name(tag.syntax_type_id))
        else {
            continue;
        };
        let line = u32::try_from(tag.span.start.row + 1)
            .expect("a file of at most MAX_HIGHLIGHTED bytes has fewer lines than u32::MAX");
        symbols.push(FileSymbol {
            name: Arc::from(&source[tag.name_range]),
            line,
            kind,
        });
    }
    Some(symbols)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::testing::TempDir;

    /// A git working tree in a temporary directory, holding `files`, with no commit: the
    /// index lists untracked files too.
    fn repo_with(name: &str, files: &[(&str, &str)]) -> (TempDir, Arc<Repo>) {
        let dir = TempDir::new(name);
        let init = Command::new("git")
            .args(["init", "--quiet"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        for (path, text) in files {
            write(dir.path(), path, text);
        }
        let repo = Arc::new(Repo::open(dir.path()).unwrap());
        (dir, repo)
    }

    fn write(root: &Path, path: &str, text: &str) {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
    }

    /// Where `name` is defined, as `path:line kind`, in ranked order, and the total.
    fn places(definitions: &Definitions, name: &str, from: &str) -> (Vec<String>, usize) {
        match definitions.lookup(name, from) {
            Lookup::Building => panic!("still building after wait()"),
            Lookup::Ready { definitions, total } => {
                let places = definitions
                    .iter()
                    .map(|d| format!("{}:{} {:?}", d.path, d.line, d.kind))
                    .collect();
                (places, total)
            }
        }
    }

    #[test]
    fn every_tags_query_finds_its_definitions() {
        // A file for every language with a tags query, with what each query must find, and a
        // reason for each: a Rust method in an `impl` (listed once, as a method, though the
        // function pattern matches it too), a JavaScript helper inside a function (a
        // definition in a local scope, which the index must keep), TypeScript's own
        // declarations on top of JavaScript's.
        let (_dir, repo) = repo_with(
            "definitions-languages",
            &[
                (
                    "a.rs",
                    "struct Point;\n\nimpl Point {\n    fn norm(&self) {}\n}\n\nfn main() {}\n",
                ),
                (
                    "a.js",
                    "function outer() {\n  const inner = () => 1;\n  return inner;\n}\n",
                ),
                (
                    "a.ts",
                    "interface Shape {}\n\nfunction area(s: Shape): number {\n  return 0;\n}\n",
                ),
                ("a.tsx", "function View() {\n  return <div />;\n}\n"),
                ("a.py", "class Model:\n    def save(self):\n        pass\n"),
                (
                    "a.go",
                    "package a\n\ntype Pair struct{}\n\nfunc Swap() {}\n",
                ),
                ("a.json", "{\"main\": 1}\n"),
            ],
        );
        let definitions = Definitions::new(repo);
        definitions.wait();

        let cases = [
            ("Point", "a.rs:1 Class"),
            ("norm", "a.rs:4 Method"),
            ("main", "a.rs:7 Function"),
            ("outer", "a.js:1 Function"),
            ("inner", "a.js:2 Function"),
            ("Shape", "a.ts:1 Interface"),
            ("area", "a.ts:3 Function"),
            ("View", "a.tsx:1 Function"),
            ("Model", "a.py:1 Class"),
            ("save", "a.py:2 Function"),
            ("Pair", "a.go:3 Type"),
            ("Swap", "a.go:5 Function"),
        ];
        for (name, place) in cases {
            assert_eq!(
                places(&definitions, name, ""),
                (vec![place.to_owned()], 1),
                "{name}"
            );
        }
        definitions.close();
    }

    #[test]
    fn a_name_defined_twice_lists_both_nearest_first_and_caps_the_list() {
        // `parse` in the file asked from, its folder, another folder, and the root, plus
        // enough more elsewhere to pass the cap.
        let mut files = vec![
            ("src/site/a.rs", "fn parse() {}\n".to_owned()),
            ("src/site/b.rs", "fn parse() {}\n".to_owned()),
            ("src/repo.rs", "fn parse() {}\n".to_owned()),
            ("build.rs", "fn parse() {}\n".to_owned()),
        ];
        let many: Vec<String> = (0..MAX_LISTED).map(|n| format!("z/{n:02}.rs")).collect();
        files.extend(
            many.iter()
                .map(|path| (path.as_str(), "fn parse() {}\n".to_owned())),
        );
        let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
        let (_dir, repo) = repo_with("definitions-ranking", &files);
        let definitions = Definitions::new(repo);
        definitions.wait();

        let (listed, total) = places(&definitions, "parse", "src/site/a.rs");
        assert_eq!(total, MAX_LISTED + 4);
        assert_eq!(listed.len(), MAX_LISTED);
        assert_eq!(
            listed[..4],
            [
                "src/site/a.rs:1 Function",
                "src/site/b.rs:1 Function",
                "build.rs:1 Function",
                "src/repo.rs:1 Function",
            ],
            "same file, same folder, then by path"
        );

        // From a file at the root, the root's files are its folder.
        let (listed, _) = places(&definitions, "parse", "x.rs");
        assert_eq!(listed[0], "build.rs:1 Function");
        definitions.close();
    }

    #[test]
    fn a_refresh_reparses_changed_files_and_drops_deleted_ones() {
        let (dir, repo) = repo_with(
            "definitions-refresh",
            &[
                ("kept.rs", "fn kept() {}\n"),
                ("edited.rs", "fn before() {}\n"),
                ("deleted.rs", "fn gone() {}\n"),
            ],
        );
        let definitions = Definitions::new(repo);
        definitions.wait();
        assert_eq!(places(&definitions, "before", "").1, 1);

        write(dir.path(), "edited.rs", "\nfn after() {}\n");
        write(dir.path(), "added.rs", "fn added() {}\n");
        fs::remove_file(dir.path().join("deleted.rs")).unwrap();
        // An unchanged file is not read again: made unreadable, which leaves its size and
        // modification time alone, it keeps its definitions. Root reads it anyway.
        if !crate::testing::skip_as_root() {
            let unreadable = fs::Permissions::from_mode(0o000);
            fs::set_permissions(dir.path().join("kept.rs"), unreadable).unwrap();
        }
        // Before the refresh, the index is as it was: nothing watches the files.
        assert_eq!(places(&definitions, "before", "").1, 1);
        definitions.refresh();
        definitions.wait();

        assert_eq!(places(&definitions, "before", "").1, 0);
        assert_eq!(
            places(&definitions, "after", ""),
            (vec!["edited.rs:2 Function".to_owned()], 1)
        );
        assert_eq!(places(&definitions, "added", "").1, 1);
        assert_eq!(places(&definitions, "gone", "").1, 0);
        assert_eq!(
            places(&definitions, "kept", "").1,
            1,
            "an unchanged file stays"
        );

        // A pass that only deletes builds the index again too.
        fs::remove_file(dir.path().join("kept.rs")).unwrap();
        definitions.refresh();
        definitions.wait();
        assert_eq!(places(&definitions, "kept", "").1, 0);
        definitions.close();
    }

    #[test]
    fn large_files_and_files_not_utf8_are_skipped_and_one_at_the_limit_is_read() {
        let line = "fn big() {}\n";
        // Ends in a two-byte character, so a read that stops one byte short is not UTF-8.
        let mut at = line.repeat(MAX_HIGHLIGHTED / line.len() - 1);
        at.push_str(&"/".repeat(MAX_HIGHLIGHTED - at.len() - 2));
        at.push('é');
        assert_eq!(at.len(), MAX_HIGHLIGHTED);
        let over = format!("{at}/");
        let (dir, repo) = repo_with(
            "definitions-skipped",
            &[
                ("at.rs", &at),
                ("over.rs", &over),
                ("ok.py", "def fine():\n  pass\n"),
            ],
        );
        fs::write(dir.path().join("latin1.py"), b"def bad():\n  x = '\xe9'\n").unwrap();
        let definitions = Definitions::new(repo);
        definitions.wait();

        assert_eq!(places(&definitions, "big", "").0[0], "at.rs:1 Function");
        assert_eq!(
            places(&definitions, "big", "").1,
            at.matches(line).count(),
            "every definition of the file at the limit, none of the one over it"
        );
        assert!(
            !places(&definitions, "big", "")
                .0
                .iter()
                .any(|p| p.starts_with("over.rs"))
        );
        assert_eq!(places(&definitions, "bad", "").1, 0);
        assert_eq!(places(&definitions, "fine", "").1, 1);
        definitions.close();
    }

    /// Waits until the thread is `worker`.
    ///
    /// # Panics
    ///
    /// After ten seconds: the thread did not get there.
    fn wait_for_worker(definitions: &Definitions, worker: Worker) {
        let state = definitions.shared.lock();
        let (_state, waited) = definitions
            .shared
            .changed
            .wait_timeout_while(state, std::time::Duration::from_secs(10), |state| {
                state.worker != worker
            })
            .unwrap();
        assert!(
            !waited.timed_out(),
            "the thread is not {worker:?} after 10 s"
        );
    }

    #[test]
    fn the_thread_sleeps_between_passes_and_ends_on_close() {
        let (_dir, repo) = repo_with("definitions-close", &[("a.rs", "fn alpha() {}\n")]);
        let definitions = Definitions::new(repo);
        definitions.wait();

        // Asleep, not passing again and again until the next refresh.
        wait_for_worker(&definitions, Worker::Waiting);
        definitions.close();
        wait_for_worker(&definitions, Worker::Stopped);

        assert_eq!(
            places(&definitions, "alpha", "").1,
            1,
            "lookups still answer"
        );
    }

    /// Indexing that stops before the first file of its first pass, until [`release`].
    fn held(repo: Arc<Repo>) -> Definitions {
        let shared = Shared::new();
        shared.lock().hold = true;
        let definitions = Definitions::start(repo, shared);
        wait_for_worker(&definitions, Worker::Held);
        definitions
    }

    fn release(definitions: &Definitions) {
        definitions.shared.lock().hold = false;
        definitions.shared.changed.notify_all();
    }

    fn hold(definitions: &Definitions) {
        definitions.shared.lock().hold = true;
    }

    #[test]
    fn a_lookup_says_building_during_the_first_pass_and_answers_during_a_refresh() {
        let (dir, repo) = repo_with("definitions-held", &[("a.rs", "fn alpha() {}\n")]);
        let definitions = held(repo);

        let reply = serde_json::to_value(definitions.lookup("alpha", "")).unwrap();
        assert_eq!(reply, serde_json::json!({ "state": "building" }));
        release(&definitions);
        definitions.wait();
        assert_eq!(places(&definitions, "alpha", "").1, 1);

        // In the middle of a refresh, the index as it was answers.
        write(dir.path(), "a.rs", "fn beta() {}\n");
        hold(&definitions);
        definitions.refresh();
        wait_for_worker(&definitions, Worker::Held);
        assert_eq!(places(&definitions, "alpha", "").1, 1);
        assert_eq!(places(&definitions, "beta", "").1, 0);
        release(&definitions);
        definitions.wait();
        assert_eq!(places(&definitions, "beta", "").1, 1);
    }

    #[test]
    fn a_changed_file_that_cannot_be_read_loses_its_definitions() {
        if crate::testing::skip_as_root() {
            return;
        }
        let (dir, repo) = repo_with("definitions-unreadable", &[("a.rs", "fn alpha() {}\n")]);
        let definitions = Definitions::new(repo);
        definitions.wait();

        // The only change of the pass: no other file makes it build the index again.
        write(dir.path(), "a.rs", "fn alpha() {}\nfn more() {}\n");
        fs::set_permissions(dir.path().join("a.rs"), fs::Permissions::from_mode(0o000)).unwrap();
        definitions.refresh();
        definitions.wait();

        assert_eq!(places(&definitions, "alpha", "").1, 0);
    }

    /// The measurement the design asks for (docs/review-assist-plan.md, 7.7): the first build
    /// and an unchanged refresh of 50,000 files. Run it with `cargo test --release --lib
    /// fifty_thousand -- --ignored --nocapture`; it prints both times.
    #[test]
    #[ignore = "a measurement: writes 50,000 files and takes seconds"]
    fn fifty_thousand_files_build_and_refresh() {
        let dir = TempDir::new("definitions-fifty-thousand");
        let init = Command::new("git")
            .args(["init", "--quiet"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        let rust =
            "pub struct Item;\n\nimpl Item {\n    pub fn get(&self) {}\n}\n\npub fn make() {}\n";
        let js =
            "export function load(path) {\n  const parse = (t) => t;\n  return parse(path);\n}\n";
        let files = 50_000;
        for n in 0..files {
            let (name, text) = if n % 2 == 0 {
                (format!("d{:03}/f{n}.rs", n / 100), rust)
            } else {
                (format!("d{:03}/f{n}.js", n / 100), js)
            };
            write(dir.path(), &name, text);
        }
        let repo = Arc::new(Repo::open(dir.path()).unwrap());

        let start = std::time::Instant::now();
        let definitions = Definitions::new(repo);
        definitions.wait();
        let first = start.elapsed();
        let start = std::time::Instant::now();
        definitions.refresh();
        definitions.wait();
        let refresh = start.elapsed();

        assert_eq!(places(&definitions, "make", "").1, files / 2);
        assert_eq!(places(&definitions, "parse", "").1, files / 2);
        eprintln!("{files} files: first build {first:?}, unchanged refresh {refresh:?}");
        definitions.close();
    }

    #[test]
    fn close_stops_a_pass_between_two_files() {
        let (_dir, repo) = repo_with(
            "definitions-close-pass",
            &[("a.rs", "fn alpha() {}\n"), ("b.rs", "fn beta() {}\n")],
        );
        let definitions = held(repo);

        definitions.close();
        wait_for_worker(&definitions, Worker::Stopped);

        // The pass ended without publishing: no file after the held one was read.
        assert!(matches!(definitions.lookup("alpha", ""), Lookup::Building));
    }
}
