//! Multi-repo Agentic Developer Hub: monitored repositories (`hub.json`), background GitHub
//! issue/PR sync, `hippius-mem` recall enrichment, and cached priority/lane triage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::agents::{self, Scanner};
use crate::analysis;
use crate::error::{Error, Result, error_chain};
use crate::files::{Stored, load_versioned, save_json_relaxed};
use crate::github::{self, GhIssue, GhPr};
use crate::lane::{Lane, WorkKind};
use crate::memory::{HubMemoryHit, HubRecallRequest, Memory};
use crate::repo::Repo;
use crate::run::Notify;

const HUB_VERSION: u32 = 1;
const CACHE_VERSION: u32 = 1;

/// Priority tier assigned to a Hub item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum Priority {
    P0,
    P1,
    P2,
    P3,
}

/// One monitored repository persisted in `<config dir>/hub.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MonitoredRepo {
    pub(crate) path: String,
    pub(crate) selected: bool,
    #[serde(default)]
    pub(crate) added_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HubFile {
    version: u32,
    repos: Vec<MonitoredRepo>,
}

/// Per-repository cached Hub items stored in `<git dir>/postil/hub-cache.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepoCacheFile {
    version: u32,
    synced_at: u64,
    items: Vec<HubItem>,
}

/// One unified Issue or Pull Request analyzed for the Hub priority queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HubItem {
    pub(crate) repo_path: String,
    pub(crate) repo_name: String,
    pub(crate) kind: WorkKind,
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) labels: Vec<String>,
    #[serde(default)]
    pub(crate) assignees: Vec<String>,
    #[serde(default)]
    pub(crate) author: String,
    #[serde(default)]
    pub(crate) created_at: String,
    #[serde(default)]
    pub(crate) updated_at: String,
    #[serde(default)]
    pub(crate) comments_count: usize,
    // PR-specific metadata
    #[serde(default)]
    pub(crate) head_ref: Option<String>,
    #[serde(default)]
    pub(crate) head_oid: Option<String>,
    #[serde(default)]
    pub(crate) base_ref: Option<String>,
    #[serde(default)]
    pub(crate) is_draft: bool,
    #[serde(default)]
    pub(crate) mergeable: Option<String>,
    #[serde(default)]
    pub(crate) review_decision: Option<String>,
    #[serde(default)]
    pub(crate) checks_state: Option<String>,
    #[serde(default)]
    pub(crate) additions: u64,
    #[serde(default)]
    pub(crate) deletions: u64,
    #[serde(default)]
    pub(crate) changed_files: u64,
    // Evaluation & priority
    pub(crate) priority: Priority,
    pub(crate) priority_score: u8,
    pub(crate) priority_reason: String,
    pub(crate) recommended_lane: Lane,
    pub(crate) lane_reason: String,
    #[serde(default)]
    pub(crate) memories: Vec<HubMemoryHit>,
    #[serde(default)]
    pub(crate) affected_components: Vec<String>,
    /// Cache fingerprint (`updated_at` + `head_oid`) so unchanged items reuse cached triage.
    #[serde(default)]
    pub(crate) revision: String,
    /// True once `hippius-mem` recall ran for this revision (even when 0 memories matched).
    #[serde(default)]
    pub(crate) memories_checked: bool,
}

/// Summary of one monitored repository returned by `GET /api/hub`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HubRepoView {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) selected: bool,
    pub(crate) exists: bool,
    pub(crate) branch: Option<String>,
    pub(crate) memory_repo: Option<String>,
    pub(crate) open_issues: usize,
    pub(crate) open_prs: usize,
    pub(crate) syncing: bool,
    pub(crate) last_synced: Option<u64>,
    pub(crate) error: Option<String>,
}

/// Reply to `GET /api/hub`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HubView {
    pub(crate) repos: Vec<HubRepoView>,
    pub(crate) items: Vec<HubItem>,
    pub(crate) syncing: bool,
    pub(crate) gh_available: bool,
}

#[derive(Debug, Default)]
struct RepoSyncState {
    items: Vec<HubItem>,
    syncing: bool,
    generation: u64,
    last_synced: Option<u64>,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct Inner {
    repos: Vec<MonitoredRepo>,
    by_path: HashMap<String, RepoSyncState>,
    sync_generation: u64,
    newer_file: bool,
}

/// Manages monitored repositories, background GitHub syncing, and `hippius-mem` priority triage.
#[derive(Clone)]
pub(crate) struct HubManager {
    file: Option<PathBuf>,
    agents: Scanner,
    memory: Memory,
    notify: Notify,
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for HubManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = lock(&self.inner);
        f.debug_struct("HubManager")
            .field("file", &self.file)
            .field("repos", &inner.repos.len())
            .finish_non_exhaustive()
    }
}

impl HubManager {
    /// Creates a new Hub manager backed by `<config dir>/hub.json` when `file` is `Some`.
    pub(crate) fn new(
        file: Option<PathBuf>,
        agents: Scanner,
        memory: Memory,
        notify: Notify,
    ) -> Self {
        let (repos, newer_file) = file.as_deref().map_or((Vec::new(), false), load_monitored);
        let mut by_path = HashMap::new();
        for entry in &repos {
            if let Ok(repo) = Repo::open(Path::new(&entry.path))
                && let Some(cached) = load_repo_cache(&repo)
            {
                by_path.insert(
                    entry.path.clone(),
                    RepoSyncState {
                        items: cached.items,
                        syncing: false,
                        generation: 0,
                        last_synced: Some(cached.synced_at),
                        error: None,
                    },
                );
            }
        }
        Self {
            file,
            agents,
            memory,
            notify,
            inner: Arc::new(Mutex::new(Inner {
                repos,
                by_path,
                sync_generation: 0,
                newer_file,
            })),
        }
    }

    /// Adds a local git repository path to the monitored list (idempotent; marks it selected).
    ///
    /// # Errors
    ///
    /// [`Error::NotARepository`] / [`Error::NotAFolder`] if `path` is not a valid git repo.
    pub(crate) fn add_repo(&self, path: &Path, use_memory: bool) -> Result<()> {
        let repo = Repo::open(path)?;
        let canonical = repo.root().to_string_lossy().into_owned();
        let cached = load_repo_cache(&repo);
        {
            let mut inner = lock(&self.inner);
            if let Some(existing) = inner.repos.iter_mut().find(|r| r.path == canonical) {
                existing.selected = true;
            } else {
                inner.repos.push(MonitoredRepo {
                    path: canonical.clone(),
                    selected: true,
                    added_at: now_secs(),
                });
            }
            if let Some(cache) = cached {
                inner
                    .by_path
                    .entry(canonical.clone())
                    .or_insert(RepoSyncState {
                        items: cache.items,
                        syncing: false,
                        generation: 0,
                        last_synced: Some(cache.synced_at),
                        error: None,
                    });
            }
            self.save_locked(&inner);
        }
        self.sync_repo(&canonical, use_memory);
        Ok(())
    }

    /// Removes `path` from the monitored list.
    pub(crate) fn remove_repo(&self, path: &str) {
        let canonical = canonical_or_raw(path);
        let mut inner = lock(&self.inner);
        inner
            .repos
            .retain(|r| r.path != path && r.path != canonical);
        inner.by_path.remove(path);
        inner.by_path.remove(&canonical);
        self.save_locked(&inner);
    }

    /// Selects or unselects `path` for the active priority queue filter.
    pub(crate) fn set_selected(&self, path: &str, selected: bool) {
        let canonical = canonical_or_raw(path);
        let mut inner = lock(&self.inner);
        if let Some(repo) = inner
            .repos
            .iter_mut()
            .find(|r| r.path == path || r.path == canonical)
        {
            repo.selected = selected;
            self.save_locked(&inner);
        }
    }

    /// Selects or unselects all monitored repositories.
    pub(crate) fn set_all_selected(&self, selected: bool) {
        let mut inner = lock(&self.inner);
        for repo in &mut inner.repos {
            repo.selected = selected;
        }
        self.save_locked(&inner);
    }

    /// Triggers a background GitHub + `hippius-mem` sync for all monitored repositories.
    pub(crate) fn sync_all(&self, use_memory: bool) {
        let paths: Vec<String> = {
            let inner = lock(&self.inner);
            inner.repos.iter().map(|r| r.path.clone()).collect()
        };
        for path in paths {
            self.sync_repo(&path, use_memory);
        }
    }

    /// Triggers a background sync for one monitored repository.
    pub(crate) fn sync_repo(&self, path: &str, use_memory: bool) {
        let generation = {
            let mut inner = lock(&self.inner);
            if inner.by_path.get(path).is_some_and(|e| e.syncing) {
                return;
            }
            inner.sync_generation += 1;
            let gen_id = inner.sync_generation;
            let entry = inner.by_path.entry(path.to_owned()).or_default();
            entry.syncing = true;
            entry.generation = gen_id;
            entry.error = None;
            gen_id
        };
        let manager = self.clone();
        let target = path.to_owned();
        let spawned = std::thread::Builder::new()
            .name("hub-sync".to_owned())
            .spawn(move || {
                manager.run_sync(&target, generation, use_memory);
            });
        if let Err(err) = spawned {
            let mut inner = lock(&self.inner);
            if let Some(entry) = inner.by_path.get_mut(path)
                && entry.generation == generation
            {
                entry.syncing = false;
                entry.error = Some(format!("could not start sync thread: {err}"));
            }
            drop(inner);
            (self.notify)();
        }
    }

    /// Re-evaluates a single item with fresh `hippius-mem` recall and architecture map matching.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when the repo or item cannot be found.
    pub(crate) fn evaluate_item(
        &self,
        repo_path: &str,
        kind: WorkKind,
        number: u64,
        use_memory: bool,
    ) -> Result<HubItem> {
        let canonical = canonical_or_raw(repo_path);
        let (key_used, existing) = {
            let inner = lock(&self.inner);
            let found = inner
                .by_path
                .get(&canonical)
                .map(|s| (canonical.as_str(), s))
                .or_else(|| inner.by_path.get(repo_path).map(|s| (repo_path, s)));
            found.and_then(|(k, s)| {
                s.items
                    .iter()
                    .find(|i| i.kind == kind && i.number == number)
                    .cloned()
                    .map(|item| (k.to_owned(), item))
            })
        }
        .ok_or_else(|| Error::BadRequest(format!("item #{number} not found in {repo_path}")))?;

        let repo = Repo::open(Path::new(&key_used))?;
        let memory_repo = repo.memory_name();
        let components = repo_components(&repo);
        let memories = if use_memory {
            let query = format!("{} {}", existing.title, existing.labels.join(" "));
            self.memory.recall_for_hub_repo(&HubRecallRequest {
                repo_dir: repo.root(),
                repo_name: memory_repo.as_deref(),
                query: &query,
                limit: 4,
                wait_for_check: false,
            })
        } else {
            Vec::new()
        };
        let mut updated = re_score_item(existing, memories, &components);
        updated.memories_checked = use_memory && self.memory.is_ready();

        let to_save = {
            let mut inner = lock(&self.inner);
            inner.by_path.get_mut(&key_used).map(|state| {
                if let Some(slot) = state
                    .items
                    .iter_mut()
                    .find(|i| i.kind == kind && i.number == number)
                {
                    *slot = updated.clone();
                }
                (
                    state.last_synced.unwrap_or_else(now_secs),
                    state.items.clone(),
                )
            })
        };
        if let Some((ts, items_snapshot)) = to_save {
            save_repo_cache(&repo, ts, &items_snapshot);
        }
        (self.notify)();
        Ok(updated)
    }

    /// Finds a cached Hub item by repo path, kind, and number.
    pub(crate) fn find_item(
        &self,
        repo_path: &str,
        kind: WorkKind,
        number: u64,
    ) -> Option<HubItem> {
        let canonical = canonical_or_raw(repo_path);
        let inner = lock(&self.inner);
        inner
            .by_path
            .get(&canonical)
            .or_else(|| inner.by_path.get(repo_path))?
            .items
            .iter()
            .find(|item| item.kind == kind && item.number == number)
            .cloned()
    }

    /// Current view of all monitored repos and the priority-ranked queue for selected repos.
    pub(crate) fn view(&self) -> HubView {
        struct RepoSnapshot {
            entry: MonitoredRepo,
            syncing: bool,
            last_synced: Option<u64>,
            error: Option<String>,
            items: Vec<HubItem>,
        }
        let gh_available = matches!(
            self.agents.find_program("gh"),
            agents::ProgramLookup::Found(_)
        );
        // Snapshot in-memory state under the lock, then perform git/fs checks unlocked.
        let snapshot: Vec<RepoSnapshot> = {
            let inner = lock(&self.inner);
            inner
                .repos
                .iter()
                .map(|entry| {
                    let sync = inner.by_path.get(&entry.path);
                    RepoSnapshot {
                        entry: entry.clone(),
                        syncing: sync.is_some_and(|s| s.syncing),
                        last_synced: sync.and_then(|s| s.last_synced),
                        error: sync.and_then(|s| s.error.clone()),
                        items: sync.map_or_else(Vec::new, |s| s.items.clone()),
                    }
                })
                .collect()
        };

        let mut repos = Vec::with_capacity(snapshot.len());
        let mut items = Vec::new();
        let mut any_syncing = false;

        for RepoSnapshot {
            entry,
            syncing,
            last_synced,
            error,
            items: repo_items,
        } in snapshot
        {
            let folder = Path::new(&entry.path);
            let exists = folder.is_dir();
            let opened = exists.then(|| Repo::open(folder).ok()).flatten();
            let name = opened.as_ref().map_or_else(
                || {
                    folder
                        .file_name()
                        .map_or_else(|| entry.path.clone(), |n| n.to_string_lossy().into_owned())
                },
                Repo::name,
            );
            let branch = opened.as_ref().and_then(|r| r.branch().ok());
            let memory_repo = opened.as_ref().and_then(Repo::memory_name);
            any_syncing |= syncing;
            let open_issues = repo_items
                .iter()
                .filter(|i| i.kind == WorkKind::Issue)
                .count();
            let open_prs = repo_items
                .iter()
                .filter(|i| i.kind == WorkKind::PullRequest)
                .count();

            repos.push(HubRepoView {
                path: entry.path,
                name,
                selected: entry.selected,
                exists,
                branch,
                memory_repo,
                open_issues,
                open_prs,
                syncing,
                last_synced,
                error,
            });

            if entry.selected {
                items.extend(repo_items);
            }
        }

        // Sort highest priority score first, then P0..P3, then newest updated_at.
        items.sort_by(|a, b| {
            b.priority_score
                .cmp(&a.priority_score)
                .then_with(|| a.priority.cmp(&b.priority))
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| b.number.cmp(&a.number))
        });

        HubView {
            repos,
            items,
            syncing: any_syncing,
            gh_available,
        }
    }

    fn run_sync(&self, path: &str, generation: u64, use_memory: bool) {
        let result = self.fetch_repo_items(path, use_memory);
        let cache_to_write = {
            let mut inner = lock(&self.inner);
            let Some(state) = inner.by_path.get_mut(path) else {
                return;
            };
            if state.generation != generation {
                return;
            }
            state.syncing = false;
            match result {
                Ok(items) => {
                    let ts = now_secs();
                    state.items.clone_from(&items);
                    state.last_synced = Some(ts);
                    state.error = None;
                    Some((ts, items))
                }
                Err(error) => {
                    state.error = Some(error);
                    None
                }
            }
        };
        if let Some((ts, items)) = cache_to_write
            && let Ok(repo) = Repo::open(Path::new(path))
        {
            save_repo_cache(&repo, ts, &items);
        }
        (self.notify)();
    }

    fn fetch_repo_items(
        &self,
        path: &str,
        use_memory: bool,
    ) -> std::result::Result<Vec<HubItem>, String> {
        let repo = Repo::open(Path::new(path)).map_err(|e| error_chain(&e))?;
        let search_path = self.agents.wait();
        let Some(gh) = agents::find_program("gh", &search_path) else {
            return Err("GitHub CLI (`gh`) is not installed on PATH".to_owned());
        };
        let (issues, prs) = std::thread::scope(|scope| {
            let pr_handle = scope.spawn(|| github::list_prs(&gh, repo.root(), &search_path));
            let issues = github::list_issues(&gh, repo.root(), &search_path);
            let prs = pr_handle.join().expect("gh pr list thread does not panic");
            (issues, prs)
        });
        let issues = issues.map_err(|e| error_chain(&e))?;
        let prs = prs.map_err(|e| error_chain(&e))?;

        let prev_by_key: HashMap<(WorkKind, u64), HubItem> = {
            let inner = lock(&self.inner);
            inner.by_path.get(path).map_or_default(|s| {
                s.items
                    .iter()
                    .map(|i| ((i.kind, i.number), i.clone()))
                    .collect()
            })
        };

        let repo_name = repo.name();
        let memory_repo = repo.memory_name();
        let components = repo_components(&repo);
        let mem_ready = use_memory && self.memory.is_ready();

        let mut items = Vec::with_capacity(issues.len() + prs.len());

        for issue in issues {
            let rev = issue.updated_at.clone();
            let cached_memories = prev_by_key
                .get(&(WorkKind::Issue, issue.number))
                .filter(|prev| {
                    prev.revision == rev && (prev.memories_checked || !prev.memories.is_empty())
                })
                .map(|prev| prev.memories.clone());
            let (memories, checked) = match cached_memories {
                Some(m) if use_memory => (m, true),
                _ if use_memory => {
                    let query = format!("{} {}", issue.title, issue.labels.join(" "));
                    let hits = self.memory.recall_for_hub_repo(&HubRecallRequest {
                        repo_dir: repo.root(),
                        repo_name: memory_repo.as_deref(),
                        query: &query,
                        limit: 3,
                        wait_for_check: true,
                    });
                    (hits, mem_ready || self.memory.is_ready())
                }
                _ => (Vec::new(), false),
            };
            let mut evaluated = evaluate_issue(path, &repo_name, issue, memories, &components);
            evaluated.memories_checked = checked;
            items.push(evaluated);
        }

        for pr in prs {
            let rev = format!("{}:{}", pr.updated_at, pr.head_oid);
            let cached_memories = prev_by_key
                .get(&(WorkKind::PullRequest, pr.number))
                .filter(|prev| {
                    prev.revision == rev && (prev.memories_checked || !prev.memories.is_empty())
                })
                .map(|prev| prev.memories.clone());
            let (memories, checked) = match cached_memories {
                Some(m) if use_memory => (m, true),
                _ if use_memory => {
                    let query = format!("{} {}", pr.title, pr.labels.join(" "));
                    let hits = self.memory.recall_for_hub_repo(&HubRecallRequest {
                        repo_dir: repo.root(),
                        repo_name: memory_repo.as_deref(),
                        query: &query,
                        limit: 3,
                        wait_for_check: true,
                    });
                    (hits, mem_ready || self.memory.is_ready())
                }
                _ => (Vec::new(), false),
            };
            let mut evaluated = evaluate_pr(path, &repo_name, pr, memories, &components);
            evaluated.memories_checked = checked;
            items.push(evaluated);
        }

        Ok(items)
    }

    fn save_locked(&self, inner: &Inner) {
        if inner.newer_file {
            return;
        }
        let Some(file) = &self.file else { return };
        let stored = HubFile {
            version: HUB_VERSION,
            repos: inner.repos.clone(),
        };
        drop(save_json_relaxed(file, &stored));
    }
}

/// Evaluates an open GitHub Issue deterministically + with `hippius-mem` context.
pub(crate) fn evaluate_issue(
    repo_path: &str,
    repo_name: &str,
    issue: GhIssue,
    memories: Vec<HubMemoryHit>,
    components: &[String],
) -> HubItem {
    let rev = issue.updated_at.clone();
    let base = HubItem {
        repo_path: repo_path.to_owned(),
        repo_name: repo_name.to_owned(),
        kind: WorkKind::Issue,
        number: issue.number,
        title: issue.title,
        body: issue.body,
        url: issue.url,
        labels: issue.labels,
        assignees: issue.assignees,
        author: issue.author,
        created_at: issue.created_at,
        updated_at: issue.updated_at,
        comments_count: issue.comments_count,
        head_ref: None,
        head_oid: None,
        base_ref: None,
        is_draft: false,
        mergeable: None,
        review_decision: None,
        checks_state: None,
        additions: 0,
        deletions: 0,
        changed_files: 0,
        priority: Priority::P2,
        priority_score: 50,
        priority_reason: String::new(),
        recommended_lane: Lane::Middle,
        lane_reason: String::new(),
        memories: Vec::new(),
        affected_components: Vec::new(),
        revision: rev,
        memories_checked: false,
    };
    re_score_item(base, memories, components)
}

/// Evaluates an open GitHub Pull Request deterministically + with `hippius-mem` context.
pub(crate) fn evaluate_pr(
    repo_path: &str,
    repo_name: &str,
    pr: GhPr,
    memories: Vec<HubMemoryHit>,
    components: &[String],
) -> HubItem {
    let rev = format!("{}:{}", pr.updated_at, pr.head_oid);
    let base = HubItem {
        repo_path: repo_path.to_owned(),
        repo_name: repo_name.to_owned(),
        kind: WorkKind::PullRequest,
        number: pr.number,
        title: pr.title,
        body: pr.body,
        url: pr.url,
        labels: pr.labels,
        assignees: pr.assignees,
        author: pr.author,
        created_at: pr.created_at,
        updated_at: pr.updated_at,
        comments_count: 0,
        head_ref: (!pr.head_ref.is_empty()).then_some(pr.head_ref),
        head_oid: (!pr.head_oid.is_empty()).then_some(pr.head_oid),
        base_ref: (!pr.base_ref.is_empty()).then_some(pr.base_ref),
        is_draft: pr.is_draft,
        mergeable: (!pr.mergeable.is_empty()).then_some(pr.mergeable),
        review_decision: (!pr.review_decision.is_empty()).then_some(pr.review_decision),
        checks_state: (!pr.checks_state.is_empty()).then_some(pr.checks_state),
        additions: pr.additions,
        deletions: pr.deletions,
        changed_files: pr.changed_files,
        priority: Priority::P2,
        priority_score: 55,
        priority_reason: String::new(),
        recommended_lane: Lane::Middle,
        lane_reason: String::new(),
        memories: Vec::new(),
        affected_components: Vec::new(),
        revision: rev,
        memories_checked: false,
    };
    re_score_item(base, memories, components)
}

/// Computes priority score (`0..=100`), tier (`P0..P3`), rationale, affected architecture map
/// components, and recommended execution lane (`Easy` / `Middle` / `Hard`).
fn re_score_item(mut item: HubItem, memories: Vec<HubMemoryHit>, components: &[String]) -> HubItem {
    let haystack = format!(
        "{} {} {}",
        item.title,
        item.labels.join(" "),
        item.body.chars().take(1200).collect::<String>()
    )
    .to_ascii_lowercase();
    let labels_lower: Vec<String> = item.labels.iter().map(|l| l.to_ascii_lowercase()).collect();

    let mut score: i32 = match item.kind {
        WorkKind::PullRequest if !item.is_draft => 58,
        WorkKind::PullRequest => 42,
        WorkKind::Issue => 50,
    };
    let mut reasons: Vec<String> = Vec::new();
    score += score_severity_signals(&labels_lower, &haystack, &mut reasons);

    if item.kind == WorkKind::PullRequest {
        score += score_pr_signals(&item, &mut reasons);
    }

    if has_any(&labels_lower, LOW_URGENCY_LABELS) && score < 70 {
        score -= 18;
        reasons.push("low-urgency docs/chore".to_owned());
    }

    if !memories.is_empty() {
        let boost = i32::try_from(memories.len().min(3)).unwrap_or(3) * 6;
        score += boost;
        reasons.push(format!(
            "matches {} team memor{}",
            memories.len(),
            if memories.len() == 1 { "y" } else { "ies" }
        ));
    }

    let affected_components: Vec<String> = components
        .iter()
        .filter(|comp| comp.len() >= 3 && haystack.contains(&comp.to_ascii_lowercase()))
        .take(5)
        .cloned()
        .collect();
    if affected_components.len() >= 2 {
        score += 6;
        reasons.push(format!(
            "spans {} map components",
            affected_components.len()
        ));
    }

    let final_score = u8::try_from(score.clamp(5, 100)).unwrap_or(50);
    let priority = match final_score {
        82..=100 => Priority::P0,
        64..=81 => Priority::P1,
        42..=63 => Priority::P2,
        _ => Priority::P3,
    };

    let (recommended_lane, lane_reason) = recommend_lane(
        &item,
        priority,
        &memories,
        &affected_components,
        &haystack,
        &labels_lower,
    );

    let priority_reason = if reasons.is_empty() {
        match item.kind {
            WorkKind::Issue => "Standard open issue".to_owned(),
            WorkKind::PullRequest => "Open pull request".to_owned(),
        }
    } else {
        reasons.join(" · ")
    };

    item.priority = priority;
    item.priority_score = final_score;
    item.priority_reason = priority_reason;
    item.recommended_lane = recommended_lane;
    item.lane_reason = lane_reason;
    item.memories = memories;
    item.affected_components = affected_components;
    item
}

const LOW_URGENCY_LABELS: &[&str] = &[
    "p3",
    "low",
    "docs",
    "documentation",
    "chore",
    "good first issue",
    "typo",
];

fn has_any(labels_lower: &[String], needles: &[&str]) -> bool {
    labels_lower
        .iter()
        .any(|l| needles.iter().any(|n| l.contains(n)))
}

fn score_severity_signals(
    labels_lower: &[String],
    haystack: &str,
    reasons: &mut Vec<String>,
) -> i32 {
    if has_any(
        labels_lower,
        &["p0", "critical", "urgent", "security", "sev0", "sev1"],
    ) || [
        "security",
        "vulnerability",
        "deadlock",
        "data loss",
        "crash",
        "panic",
    ]
    .iter()
    .any(|n| haystack.contains(n))
    {
        reasons.push("critical/security or stability signal".to_owned());
        32
    } else if has_any(
        labels_lower,
        &["p1", "high", "bug", "regression", "blocker"],
    ) || ["regression", "race condition", "broken", "memory leak"]
        .iter()
        .any(|n| haystack.contains(n))
    {
        reasons.push("high-priority bug or regression".to_owned());
        18
    } else {
        0
    }
}

fn score_pr_signals(item: &HubItem, reasons: &mut Vec<String>) -> i32 {
    let mut delta = 0;
    if item.checks_state.as_deref() == Some("failing") {
        delta += 12;
        reasons.push("CI failing".to_owned());
    }
    if item.mergeable.as_deref() == Some("CONFLICTING") {
        delta += 10;
        reasons.push("merge conflict".to_owned());
    }
    if item.review_decision.as_deref() == Some("CHANGES_REQUESTED") {
        delta += 10;
        reasons.push("changes requested".to_owned());
    } else if !item.is_draft && item.review_decision.as_deref() != Some("APPROVED") {
        delta += 6;
        reasons.push("ready for review".to_owned());
    }
    delta
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper split out of `re_score_item` to keep both functions concise"
)]
fn recommend_lane(
    item: &HubItem,
    priority: Priority,
    memories: &[HubMemoryHit],
    affected_components: &[String],
    haystack: &str,
    labels_lower: &[String],
) -> (Lane, String) {
    let has_label = |needles: &[&str]| {
        labels_lower
            .iter()
            .any(|l| needles.iter().any(|n| l.contains(n)))
    };
    let has_word = |needles: &[&str]| needles.iter().any(|n| haystack.contains(n));
    let churn = item.additions.saturating_add(item.deletions);
    let hard_signal = has_label(&[
        "architecture",
        "security",
        "concurrency",
        "breaking",
        "rfc",
        "hard",
    ]) || has_word(&[
        "architecture",
        "refactor",
        "concurrency",
        "deadlock",
        "race",
        "lock order",
        "migration",
        "protocol",
        "encryption",
        "auth",
        "distributed",
    ]) || (item.kind == WorkKind::PullRequest
        && (churn > 400 || item.changed_files > 10))
        || affected_components.len() >= 3
        || (priority == Priority::P0 && memories.len() >= 2);

    let easy_signal = !hard_signal
        && (has_label(&[
            "typo",
            "docs",
            "documentation",
            "trivial",
            "good first issue",
            "chore",
        ]) || has_word(&[
            "typo",
            "rename",
            "tooltip",
            "readme",
            "comment only",
            "wording",
        ]) || (item.kind == WorkKind::PullRequest
            && churn > 0
            && churn <= 35
            && item.changed_files <= 2
            && memories.is_empty()));

    if hard_signal {
        let reason = if item.kind == WorkKind::PullRequest
            && (churn > 400 || item.changed_files > 10)
        {
            format!(
                "Large PR (+{} -{} across {} files) — 2 Frontier audits + synthesis judge recommended",
                item.additions, item.deletions, item.changed_files
            )
        } else {
            "High-risk/architectural scope — 2 Frontier models for planning & judging + dual review recommended".to_owned()
        };
        (Lane::Hard, reason)
    } else if easy_signal {
        (
            Lane::Easy,
            "Small, low-risk change — single agent direct execution with no extra review needed"
                .to_owned(),
        )
    } else {
        (
            Lane::Middle,
            "Standard scope — main agent plus an independent peer review pass recommended"
                .to_owned(),
        )
    }
}

fn canonical_or_raw(path: &str) -> String {
    std::fs::canonicalize(path)
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| path.to_owned())
}

fn repo_components(repo: &Repo) -> Vec<String> {
    analysis::node_labels(repo)
}

fn load_monitored(file: &Path) -> (Vec<MonitoredRepo>, bool) {
    match load_versioned::<HubFile>(file, HUB_VERSION) {
        Stored::Current(hub) => (hub.repos, false),
        Stored::Newer => (Vec::new(), true),
        Stored::Nothing => (Vec::new(), false),
    }
}

fn cache_path(repo: &Repo) -> PathBuf {
    repo.postil_dir().join("hub-cache.json")
}

fn load_repo_cache(repo: &Repo) -> Option<RepoCacheFile> {
    match load_versioned::<RepoCacheFile>(&cache_path(repo), CACHE_VERSION) {
        Stored::Current(cache) => Some(cache),
        Stored::Newer | Stored::Nothing => None,
    }
}

fn save_repo_cache(repo: &Repo, synced_at: u64, items: &[HubItem]) {
    let file = RepoCacheFile {
        version: CACHE_VERSION,
        synced_at,
        items: items.to_vec(),
    };
    drop(save_json_relaxed(&cache_path(repo), &file));
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_issue(number: u64, title: &str, labels: &[&str]) -> GhIssue {
        GhIssue {
            number,
            title: title.to_owned(),
            body: String::new(),
            url: format!("https://github.com/org/repo/issues/{number}"),
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            assignees: Vec::new(),
            author: "alice".to_owned(),
            created_at: "2026-11-28T10:00:00Z".to_owned(),
            updated_at: "2026-11-28T11:00:00Z".to_owned(),
            comments_count: 1,
        }
    }

    #[test]
    fn critical_concurrency_issue_with_memories_ranks_p0_and_hard_lane() {
        let issue = sample_issue(
            84,
            "Race condition in worktree eviction lock order",
            &["bug", "critical"],
        );
        let memories = vec![
            HubMemoryHit {
                id: "mem_01K9".to_owned(),
                summary: "Slots lock order invariant".to_owned(),
                kind: Some("decision".to_owned()),
            },
            HubMemoryHit {
                id: "mem_01J4".to_owned(),
                summary: "Eviction thread lock gotcha".to_owned(),
                kind: Some("gotcha".to_owned()),
            },
        ];
        let item = evaluate_issue("/tmp/r", "postil", issue, memories, &[]);
        assert_eq!(item.priority, Priority::P0);
        assert!(item.priority_score >= 82);
        assert_eq!(item.recommended_lane, Lane::Hard);
    }

    #[test]
    fn typo_issue_ranks_low_and_recommends_easy_lane() {
        let issue = sample_issue(86, "Fix typo in Settings tooltip", &["docs", "typo"]);
        let item = evaluate_issue("/tmp/r", "postil", issue, Vec::new(), &[]);
        assert_eq!(item.priority, Priority::P3);
        assert_eq!(item.recommended_lane, Lane::Easy);
    }

    #[test]
    fn large_pr_recommends_hard_lane_and_small_pr_recommends_easy_lane() {
        let big = GhPr {
            number: 12,
            title: "Rewrite scheduler".to_owned(),
            body: String::new(),
            url: "https://github.com/org/repo/pull/12".to_owned(),
            labels: Vec::new(),
            assignees: Vec::new(),
            author: "bob".to_owned(),
            head_ref: "feat/sched".to_owned(),
            head_oid: "abc".to_owned(),
            base_ref: "main".to_owned(),
            is_draft: false,
            mergeable: "MERGEABLE".to_owned(),
            review_decision: String::new(),
            checks_state: "failing".to_owned(),
            additions: 450,
            deletions: 120,
            changed_files: 14,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let evaluated_big = evaluate_pr("/tmp/r", "postil", big, Vec::new(), &[]);
        assert_eq!(evaluated_big.recommended_lane, Lane::Hard);
        assert_eq!(evaluated_big.priority, Priority::P1);

        let small = GhPr {
            number: 13,
            title: "Update README badge".to_owned(),
            body: String::new(),
            url: "https://github.com/org/repo/pull/13".to_owned(),
            labels: vec!["docs".to_owned()],
            assignees: Vec::new(),
            author: "bob".to_owned(),
            head_ref: "docs/badge".to_owned(),
            head_oid: "def".to_owned(),
            base_ref: "main".to_owned(),
            is_draft: false,
            mergeable: "MERGEABLE".to_owned(),
            review_decision: String::new(),
            checks_state: "passing".to_owned(),
            additions: 4,
            deletions: 2,
            changed_files: 1,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let evaluated_small = evaluate_pr("/tmp/r", "postil", small, Vec::new(), &[]);
        assert_eq!(evaluated_small.recommended_lane, Lane::Easy);
    }
}
