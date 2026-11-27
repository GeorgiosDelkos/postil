//! Read-only background scanner for local CLI session transcripts on disk.
//!
//! Reads token, model, duration, and cost counters from the local session directories written
//! by `claude` (`~/.claude/projects`), `codex` (`~/.codex/sessions`), `grok`
//! (`~/.grok/sessions`), and `agy` (`~/.gemini/antigravity-cli/brain`). Sessions started inside
//! Postil's own worktrees (`~/.postil`) or probe scratch folders (`postil-models`,
//! `postil-quota`) are skipped so Postil runs in `usage.jsonl` are never counted twice.
//!
//! Only metadata counters, timestamps, repository paths, and model identifiers are kept in
//! memory; prompt and response text are discarded line by line. Unchanged files are served
//! from an in-memory `(modified, len)` stamp cache so refreshes only parse new or updated
//! transcripts.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
#[cfg(test)]
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use crate::acp::{PromptUsage, UsageCost};
use crate::usage::ledger::{
    LedgerRecord, RECORD_VERSION, RunOrigin, RunOutcome, strip_macos_private,
};
use crate::usage::quota::{QuotaFetch, parse_rfc3339_secs};

/// How long a finished session scan stays fresh before the next open re-scans directory mtimes.
/// Aligned with `quota::CACHE_TTL` so `GET /api/usage` does not enter polling mode just for a
/// session rescan while live quotas are still cached.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// Maximum bytes read from a single JSONL line; longer lines (such as base64 image attachments)
/// are skipped without buffering them whole.
const MAX_LINE_BYTES: usize = if cfg!(test) { 32 << 10 } else { 2 << 20 };

/// Maximum bytes read from a standalone JSON metadata file (`summary.json`, `usage.json`).
const MAX_JSON_FILE_BYTES: u64 = 4 << 20;

/// Grok's proto3 `costUsdTicks` unit: `10^10` ticks per US dollar.
const USD_TICKS_PER_DOLLAR: f64 = 10_000_000_000.0;

#[derive(Clone)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    records: Arc<[LedgerRecord]>,
}

struct ScanState {
    in_flight: bool,
    updated: Option<Instant>,
    records: Arc<Vec<LedgerRecord>>,
    cache: HashMap<PathBuf, FileStamp>,
}

impl Default for ScanState {
    fn default() -> Self {
        Self {
            in_flight: false,
            updated: None,
            records: Arc::new(Vec::new()),
            cache: HashMap::new(),
        }
    }
}

/// Caches and refreshes local CLI session usage records in the background.
pub(crate) struct SessionScanner {
    home: Option<PathBuf>,
    state: Arc<Mutex<ScanState>>,
    #[cfg(test)]
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl SessionScanner {
    /// A disabled scanner that performs no disk I/O (used in benchmarks and unit tests that do
    /// not supply an isolated home directory).
    pub(crate) fn disabled() -> Self {
        Self::new(None)
    }

    /// A scanner rooted at `home` (`$HOME`), or disabled when `None`.
    pub(crate) fn new(home: Option<PathBuf>) -> Self {
        Self {
            home,
            state: Arc::new(Mutex::new(ScanState::default())),
            #[cfg(test)]
            threads: Mutex::new(Vec::new()),
        }
    }

    /// Returns the latest cached CLI session records and whether a background scan is running.
    pub(crate) fn query(&self, fetch: QuotaFetch) -> (Arc<Vec<LedgerRecord>>, bool) {
        let Some(home) = &self.home else {
            return (Arc::new(Vec::new()), false);
        };
        let mut should_scan = false;
        let (records, in_flight) = {
            let mut state = lock(&self.state);
            let stale = state
                .updated
                .is_none_or(|at| matches!(fetch, QuotaFetch::Refresh) || at.elapsed() >= CACHE_TTL);
            if !state.in_flight && stale {
                state.in_flight = true;
                should_scan = true;
            }
            (Arc::clone(&state.records), state.in_flight)
        };
        if should_scan {
            self.spawn_scan(home.clone());
        }
        (records, in_flight)
    }

    fn spawn_scan(&self, home: PathBuf) {
        let shared = Arc::clone(&self.state);
        let spawned = std::thread::Builder::new()
            .name("usage-session-scan".to_owned())
            .spawn(move || {
                let mut cache = std::mem::take(&mut lock(&shared).cache);
                let scanned = scan_home(&home, &mut cache);
                let mut state = lock(&shared);
                state.records = Arc::new(scanned);
                state.cache = cache;
                state.updated = Some(Instant::now());
                state.in_flight = false;
            });
        if let Ok(handle) = spawned {
            #[cfg(test)]
            lock(&self.threads).push(handle);
            #[cfg(not(test))]
            drop(handle);
        } else {
            let mut state = lock(&self.state);
            state.updated = Some(Instant::now());
            state.in_flight = false;
        }
    }

    #[cfg(test)]
    pub(crate) fn join_scans(&self) {
        let handles = std::mem::take(&mut *lock(&self.threads));
        for handle in handles {
            handle.join().expect("session scan thread panicked");
        }
    }
}

impl fmt::Debug for SessionScanner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("SessionScanner")
            .field("enabled", &self.home.is_some())
            .field("in_flight", &state.in_flight)
            .field("records", &state.records.len())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn scan_home(home: &Path, cache: &mut HashMap<PathBuf, FileStamp>) -> Vec<LedgerRecord> {
    let postil_dir = PathBuf::from(normalize_cwd(&home.join(".postil").to_string_lossy()));
    let mut next_cache = HashMap::with_capacity(cache.len());
    let mut out = Vec::new();

    scan_claude_projects(
        &home.join(".claude/projects"),
        &postil_dir,
        cache,
        &mut next_cache,
        &mut out,
    );
    scan_codex_dir(
        &home.join(".codex/sessions"),
        &postil_dir,
        cache,
        &mut next_cache,
        &mut out,
    );
    scan_grok_projects(
        &home.join(".grok/sessions"),
        &postil_dir,
        cache,
        &mut next_cache,
        &mut out,
    );
    scan_antigravity_brain(
        &home.join(".gemini/antigravity-cli/brain"),
        &postil_dir,
        cache,
        &mut next_cache,
        &mut out,
    );

    *cache = next_cache;
    out
}

/// Checks `(modified, len)` on `stamp_file` and reuses `prev`'s parsed records when unchanged.
fn cached_or_parse(
    stamp_file: &Path,
    prev: &mut HashMap<PathBuf, FileStamp>,
    next: &mut HashMap<PathBuf, FileStamp>,
    parse: impl FnOnce(u64) -> Vec<LedgerRecord>,
) -> Arc<[LedgerRecord]> {
    let Ok(meta) = fs::metadata(stamp_file) else {
        return Arc::from([]);
    };
    if !meta.is_file() {
        return Arc::from([]);
    }
    let modified = meta.modified().ok();
    let len = meta.len();
    if let Some((key, hit)) = prev.remove_entry(stamp_file)
        && hit.len == len
        && hit.modified == modified
    {
        let records = Arc::clone(&hit.records);
        next.insert(key, hit);
        return records;
    }
    let fallback_ts = modified
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let records: Arc<[LedgerRecord]> = Arc::from(parse(fallback_ts));
    next.insert(
        stamp_file.to_owned(),
        FileStamp {
            modified,
            len,
            records: Arc::clone(&records),
        },
    );
    records
}

/// Resolves symlinks on `cwd` (or its deepest existing ancestor when a subdirectory was
/// removed) so CLI transcripts match `git rev-parse --show-toplevel` on symlinked paths.
fn normalize_cwd(cwd: &str) -> String {
    if cwd.is_empty() {
        return String::new();
    }
    let path = Path::new(cwd);
    if let Ok(real) = fs::canonicalize(path) {
        return real.to_string_lossy().into_owned();
    }
    for ancestor in path.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() || ancestor == Path::new("/") {
            break;
        }
        if let Ok(real_base) = fs::canonicalize(ancestor)
            && let Ok(rel) = path.strip_prefix(ancestor)
        {
            return real_base.join(rel).to_string_lossy().into_owned();
        }
    }
    cwd.to_owned()
}

/// Whether `cwd` belongs to Postil's own worktrees (`~/.postil`, or external-volume
/// `/Volumes/.../.postil/worktrees/...`) or probe scratch directories (`postil-models`,
/// `postil-quota`).
fn should_skip_cwd(cwd: &str, postil_dir: &Path) -> bool {
    if cwd.is_empty() {
        return false;
    }
    let path = Path::new(cwd);
    if path.starts_with(postil_dir) {
        return true;
    }
    let norm_cwd = Path::new(strip_macos_private(cwd));
    let norm_postil = postil_dir.to_str().map(strip_macos_private).map(Path::new);
    if norm_postil.is_some_and(|base| norm_cwd.starts_with(base)) {
        return true;
    }
    let mut prev_was_postil = false;
    for comp in path.components() {
        let part = comp.as_os_str().to_str();
        if prev_was_postil && part == Some("worktrees") {
            return true;
        }
        prev_was_postil = part == Some(".postil");
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "postil-models" | "postil-quota"))
}

// ---- Claude (`~/.claude/projects/<dir>/*.jsonl`) --------------------------------------------

fn scan_claude_projects(
    projects_dir: &Path,
    postil_dir: &Path,
    prev: &mut HashMap<PathBuf, FileStamp>,
    next: &mut HashMap<PathBuf, FileStamp>,
    out: &mut Vec<LedgerRecord>,
) {
    let Ok(projects) = fs::read_dir(projects_dir) else {
        return;
    };
    for project in projects.flatten() {
        let Ok(kind) = project.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|k| k.is_file()) {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let records = cached_or_parse(&path, prev, next, |fallback_ts| {
                parse_claude_session(&path, postil_dir, fallback_ts)
            });
            out.extend_from_slice(&records);
        }
    }
}

#[derive(Default)]
struct ClaudeAccum {
    cwd: String,
    first_ts: Option<u64>,
    last_ts: Option<u64>,
    tool_calls: u64,
    cost_state: Option<Value>,
    requests: BTreeMap<String, (String, PromptUsage)>,
    anon_seq: u64,
}

fn parse_claude_session(path: &Path, postil_dir: &Path, fallback_ts: u64) -> Vec<LedgerRecord> {
    let mut acc = ClaudeAccum::default();
    let mut skipped = false;
    for_each_jsonl_line(path, |entry| {
        if acc.cwd.is_empty()
            && let Some(cwd) = entry
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            acc.cwd = normalize_cwd(cwd);
            if should_skip_cwd(&acc.cwd, postil_dir) {
                skipped = true;
                return false;
            }
        }
        if let Some(ts) = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_secs)
        {
            acc.first_ts = Some(acc.first_ts.map_or(ts, |prev| prev.min(ts)));
            acc.last_ts = Some(acc.last_ts.map_or(ts, |prev| prev.max(ts)));
        }
        match entry.get("type").and_then(Value::as_str) {
            Some("cost-state") => acc.cost_state = Some(entry),
            Some("assistant") => record_claude_assistant(&entry, &mut acc),
            _ => {}
        }
        true
    });

    if skipped || should_skip_cwd(&acc.cwd, postil_dir) {
        return Vec::new();
    }
    let session_id = file_stem_id(path, "claude");
    if let Some(cost_state) = &acc.cost_state
        && let Some(records) = claude_from_cost_state(cost_state, &acc, &session_id, fallback_ts)
    {
        return records;
    }
    claude_from_requests(acc, &session_id, fallback_ts)
}

fn record_claude_assistant(entry: &Value, acc: &mut ClaudeAccum) {
    let Some(msg) = entry.get("message").filter(|v| v.is_object()) else {
        return;
    };
    if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
        acc.tool_calls += blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .count() as u64;
    }
    let Some(usage) = msg.get("usage").filter(|v| v.is_object()) else {
        return;
    };
    let model = msg
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty() && !m.starts_with('<'))
        .unwrap_or("Default")
        .to_owned();
    let raw_in = u64_of(usage, "input_tokens").unwrap_or(0);
    let cache_r = u64_of(usage, "cache_read_input_tokens").unwrap_or(0);
    let cache_w = u64_of(usage, "cache_creation_input_tokens").unwrap_or(0);
    let out_tok = u64_of(usage, "output_tokens").unwrap_or(0);
    let thought = usage
        .get("output_tokens_details")
        .and_then(|d| u64_of(d, "thinking_tokens"));
    let in_tok = raw_in.saturating_add(cache_r).saturating_add(cache_w);
    let total = in_tok.saturating_add(out_tok);
    if total == 0 {
        return;
    }
    let req_key = entry
        .get("requestId")
        .and_then(Value::as_str)
        .or_else(|| msg.get("id").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map_or_else(
            || {
                acc.anon_seq += 1;
                format!("anon-{}", acc.anon_seq)
            },
            str::to_owned,
        );
    acc.requests.insert(
        req_key,
        (
            model,
            PromptUsage {
                input_tokens: Some(in_tok),
                output_tokens: Some(out_tok),
                cached_read_tokens: (cache_r > 0).then_some(cache_r),
                cached_write_tokens: (cache_w > 0).then_some(cache_w),
                thought_tokens: thought.filter(|&n| n > 0),
                total_tokens: Some(total),
            },
        ),
    );
}

fn claude_from_cost_state(
    state: &Value,
    acc: &ClaudeAccum,
    session_id: &str,
    fallback_ts: u64,
) -> Option<Vec<LedgerRecord>> {
    let models = state.get("modelUsage")?.as_object()?;
    let started_at = u64_of(state, "startTime")
        .map(|ms| ms / 1000)
        .or(acc.first_ts)
        .unwrap_or(fallback_ts);
    let duration = u64_of(state, "totalDuration").map_or_else(
        || acc.last_ts.unwrap_or(started_at).saturating_sub(started_at),
        |ms| ms / 1000,
    );

    let mut out = Vec::new();
    for (model_name, bucket) in models {
        let raw_in = u64_of(bucket, "inputTokens").unwrap_or(0);
        let cache_r = u64_of(bucket, "cacheReadInputTokens").unwrap_or(0);
        let cache_w = u64_of(bucket, "cacheCreationInputTokens").unwrap_or(0);
        let out_tok = u64_of(bucket, "outputTokens").unwrap_or(0);
        let thought = u64_of(bucket, "thinkingTokens").unwrap_or(0);
        let in_tok = raw_in.saturating_add(cache_r).saturating_add(cache_w);
        let total = in_tok.saturating_add(out_tok);
        let cost_usd = bucket
            .get("costUSD")
            .and_then(Value::as_f64)
            .filter(|&c| c.is_finite() && c > 0.0);
        if total == 0 && cost_usd.is_none() {
            continue;
        }
        let secondary = !out.is_empty();
        out.push(LedgerRecord {
            version: RECORD_VERSION,
            run_id: format!("cli:{session_id}"),
            repo: acc.cwd.clone(),
            agent: "Claude".to_owned(),
            model: Some(model_name.clone()),
            origin: RunOrigin::Notes,
            outcome: RunOutcome::Ended,
            started_at,
            ended_at: if secondary {
                started_at
            } else {
                started_at.saturating_add(duration)
            },
            tokens: PromptUsage {
                input_tokens: Some(in_tok),
                output_tokens: Some(out_tok),
                cached_read_tokens: (cache_r > 0).then_some(cache_r),
                cached_write_tokens: (cache_w > 0).then_some(cache_w),
                thought_tokens: (thought > 0).then_some(thought),
                total_tokens: Some(total),
            },
            context_used: None,
            context_size: None,
            cost: cost_usd.map(usd),
            tool_calls: if secondary { 0 } else { acc.tool_calls },
            secondary_model: secondary,
        });
    }
    (!out.is_empty()).then_some(out)
}

fn claude_from_requests(acc: ClaudeAccum, session_id: &str, fallback_ts: u64) -> Vec<LedgerRecord> {
    let mut by_model: BTreeMap<String, PromptUsage> = BTreeMap::new();
    for (model, usage) in acc.requests.into_values() {
        add_prompt_usage(by_model.entry(model).or_default(), &usage);
    }
    let started_at = acc.first_ts.unwrap_or(fallback_ts);
    let ended_at = acc.last_ts.unwrap_or(started_at).max(started_at);
    let mut out = Vec::new();
    for (model, tokens) in by_model {
        let secondary = !out.is_empty();
        out.push(LedgerRecord {
            version: RECORD_VERSION,
            run_id: format!("cli:{session_id}"),
            repo: acc.cwd.clone(),
            agent: "Claude".to_owned(),
            model: Some(model),
            origin: RunOrigin::Notes,
            outcome: RunOutcome::Ended,
            started_at,
            ended_at: if secondary { started_at } else { ended_at },
            tokens,
            context_used: None,
            context_size: None,
            cost: None,
            tool_calls: if secondary { 0 } else { acc.tool_calls },
            secondary_model: secondary,
        });
    }
    out
}

// ---- Codex (`~/.codex/sessions/**/*.jsonl`) -------------------------------------------------

struct CodexWalk<'a> {
    postil_dir: &'a Path,
    prev: &'a mut HashMap<PathBuf, FileStamp>,
    next: &'a mut HashMap<PathBuf, FileStamp>,
    out: &'a mut Vec<LedgerRecord>,
}

fn scan_codex_dir(
    dir: &Path,
    postil_dir: &Path,
    prev: &mut HashMap<PathBuf, FileStamp>,
    next: &mut HashMap<PathBuf, FileStamp>,
    out: &mut Vec<LedgerRecord>,
) {
    let mut walk = CodexWalk {
        postil_dir,
        prev,
        next,
        out,
    };
    walk_codex(dir, 0, &mut walk);
}

fn walk_codex(dir: &Path, depth: u8, walk: &mut CodexWalk<'_>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            walk_codex(&path, depth + 1, walk);
        } else if kind.is_file() && path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let postil_dir = walk.postil_dir;
            let records = cached_or_parse(&path, walk.prev, walk.next, |fallback_ts| {
                parse_codex_session(&path, postil_dir, fallback_ts)
            });
            walk.out.extend_from_slice(&records);
        }
    }
}

fn parse_codex_session(path: &Path, postil_dir: &Path, fallback_ts: u64) -> Vec<LedgerRecord> {
    let mut cwd = String::new();
    let mut skipped = false;
    let mut model: Option<String> = None;
    let mut first_ts: Option<u64> = None;
    let mut last_ts: Option<u64> = None;
    let mut tool_calls = 0_u64;
    let mut last_usage: Option<PromptUsage> = None;

    for_each_jsonl_line(path, |entry| {
        if let Some(ts) = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_secs)
        {
            first_ts = Some(first_ts.map_or(ts, |prev| prev.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |prev| prev.max(ts)));
        }
        let Some(payload) = entry.get("payload").filter(|v| v.is_object()) else {
            return true;
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("session_meta" | "turn_context") => {
                if cwd.is_empty()
                    && let Some(dir) = payload.get("cwd").and_then(Value::as_str)
                {
                    cwd = normalize_cwd(dir);
                    if should_skip_cwd(&cwd, postil_dir) {
                        skipped = true;
                        return false;
                    }
                }
                if let Some(m) = payload
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    model = Some(m.to_owned());
                }
            }
            Some("response_item") => {
                if matches!(
                    payload.get("type").and_then(Value::as_str),
                    Some("function_call" | "custom_tool_call")
                ) {
                    tool_calls += 1;
                }
            }
            Some("event_msg")
                if payload.get("type").and_then(Value::as_str) == Some("token_count") =>
            {
                if let Some(total_obj) = payload
                    .get("info")
                    .and_then(|info| info.get("total_token_usage"))
                    .filter(|v| v.is_object())
                    && let Some(parsed) = parse_codex_token_usage(total_obj)
                {
                    last_usage = Some(parsed);
                }
            }
            _ => {}
        }
        true
    });

    if skipped || should_skip_cwd(&cwd, postil_dir) {
        return Vec::new();
    }
    let Some(tokens) = last_usage else {
        return Vec::new();
    };
    let started_at = first_ts.unwrap_or(fallback_ts);
    let ended_at = last_ts.unwrap_or(started_at).max(started_at);
    vec![LedgerRecord {
        version: RECORD_VERSION,
        run_id: format!("cli:{}", file_stem_id(path, "codex")),
        repo: cwd,
        agent: "Codex".to_owned(),
        model,
        origin: RunOrigin::Notes,
        outcome: RunOutcome::Ended,
        started_at,
        ended_at,
        tokens,
        context_used: None,
        context_size: None,
        cost: None,
        tool_calls,
        secondary_model: false,
    }]
}

fn parse_codex_token_usage(obj: &Value) -> Option<PromptUsage> {
    let in_tok = u64_of(obj, "input_tokens").unwrap_or(0);
    let cache_r = u64_of(obj, "cached_input_tokens").unwrap_or(0);
    let out_tok = u64_of(obj, "output_tokens").unwrap_or(0);
    let thought = u64_of(obj, "reasoning_output_tokens").unwrap_or(0);
    let total = u64_of(obj, "total_tokens").unwrap_or_else(|| in_tok.saturating_add(out_tok));
    (total > 0).then_some(PromptUsage {
        input_tokens: Some(in_tok),
        output_tokens: Some(out_tok),
        cached_read_tokens: (cache_r > 0).then_some(cache_r),
        cached_write_tokens: None,
        thought_tokens: (thought > 0).then_some(thought),
        total_tokens: Some(total),
    })
}

// ---- Grok (`~/.grok/sessions/<encoded-cwd>/<session-id>/`) ----------------------------------

fn scan_grok_projects(
    sessions_dir: &Path,
    postil_dir: &Path,
    prev: &mut HashMap<PathBuf, FileStamp>,
    next: &mut HashMap<PathBuf, FileStamp>,
    out: &mut Vec<LedgerRecord>,
) {
    let Ok(projects) = fs::read_dir(sessions_dir) else {
        return;
    };
    for project in projects.flatten() {
        let Ok(kind) = project.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let encoded_name = project.file_name().to_string_lossy().into_owned();
        let decoded_dir = normalize_cwd(&percent_decode_path(&encoded_name));
        if should_skip_cwd(&decoded_dir, postil_dir) {
            continue;
        }
        let Ok(sessions) = fs::read_dir(project.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            if !session.file_type().is_ok_and(|k| k.is_dir()) {
                continue;
            }
            let session_dir = session.path();
            let usage_file = session_dir.join("usage.json");
            let records = cached_or_parse(&usage_file, prev, next, |fallback_ts| {
                parse_grok_session(&session_dir, &decoded_dir, postil_dir, fallback_ts)
            });
            out.extend_from_slice(&records);
        }
    }
}

fn parse_grok_session(
    session_dir: &Path,
    decoded_dir: &str,
    postil_dir: &Path,
    fallback_ts: u64,
) -> Vec<LedgerRecord> {
    let Some(usage_json) = read_small_json(&session_dir.join("usage.json")) else {
        return Vec::new();
    };
    let summary_json = read_small_json(&session_dir.join("summary.json"));
    let signals_json = read_small_json(&session_dir.join("signals.json"));

    let info_cwd = summary_json
        .as_ref()
        .and_then(|s| {
            s.get("info")
                .and_then(|i| i.get("cwd"))
                .and_then(Value::as_str)
        })
        .filter(|s| !s.is_empty());
    let cwd = if let Some(dir) = info_cwd {
        normalize_cwd(dir)
    } else {
        let git_root = summary_json
            .as_ref()
            .and_then(|s| s.get("git_root_dir").and_then(Value::as_str))
            .filter(|s| !s.is_empty());
        git_root.map_or_else(|| decoded_dir.to_owned(), normalize_cwd)
    };
    if should_skip_cwd(&cwd, postil_dir) {
        return Vec::new();
    }

    let started_at = summary_json
        .as_ref()
        .and_then(|s| s.get("created_at").and_then(Value::as_str))
        .or_else(|| usage_json.get("updatedAt").and_then(Value::as_str))
        .and_then(parse_rfc3339_secs)
        .unwrap_or(fallback_ts);
    let duration = signals_json
        .as_ref()
        .and_then(|s| u64_of(s, "sessionDurationSeconds"))
        .unwrap_or(0);
    let tool_calls = signals_json
        .as_ref()
        .and_then(|s| u64_of(s, "toolCallCount"))
        .unwrap_or(0);
    let session_id = file_stem_id(session_dir, "grok");
    let Some(session_obj) = usage_json.get("session").filter(|v| v.is_object()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    if let Some(by_model) = session_obj.get("modelUsage").and_then(Value::as_object) {
        for (model_name, bucket) in by_model {
            let secondary = !out.is_empty();
            if let Some(rec) = grok_model_record(
                &session_id,
                &cwd,
                Some(model_name.as_str()),
                bucket,
                (started_at, duration, tool_calls, secondary),
            ) {
                out.push(rec);
            }
        }
    }
    if out.is_empty() {
        let model = session_obj.get("primaryModelId").and_then(Value::as_str);
        if let Some(rec) = grok_model_record(
            &session_id,
            &cwd,
            model,
            session_obj,
            (started_at, duration, tool_calls, false),
        ) {
            out.push(rec);
        }
    }
    out
}

fn grok_model_record(
    session_id: &str,
    cwd: &str,
    model: Option<&str>,
    bucket: &Value,
    (started_at, duration, tool_calls, secondary): (u64, u64, u64, bool),
) -> Option<LedgerRecord> {
    let in_tok = u64_of(bucket, "inputTokens").unwrap_or(0);
    let out_tok = u64_of(bucket, "outputTokens").unwrap_or(0);
    let cache_r = u64_of(bucket, "cachedReadTokens").unwrap_or(0);
    let cache_w = u64_of(bucket, "cacheCreationTokens").unwrap_or(0);
    let thought = u64_of(bucket, "reasoningTokens").unwrap_or(0);
    let total = u64_of(bucket, "totalTokens").unwrap_or_else(|| in_tok.saturating_add(out_tok));
    let cost = bucket
        .get("costUsdTicks")
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        })
        .filter(|&ticks| ticks.is_finite() && ticks > 0.0)
        .map(|ticks| usd(ticks / USD_TICKS_PER_DOLLAR));
    if total == 0 && cost.is_none() {
        return None;
    }
    Some(LedgerRecord {
        version: RECORD_VERSION,
        run_id: format!("cli:{session_id}"),
        repo: cwd.to_owned(),
        agent: "Grok".to_owned(),
        model: model.filter(|s| !s.is_empty()).map(str::to_owned),
        origin: RunOrigin::Notes,
        outcome: RunOutcome::Ended,
        started_at,
        ended_at: if secondary {
            started_at
        } else {
            started_at.saturating_add(duration)
        },
        tokens: PromptUsage {
            input_tokens: Some(in_tok),
            output_tokens: Some(out_tok),
            cached_read_tokens: (cache_r > 0).then_some(cache_r),
            cached_write_tokens: (cache_w > 0).then_some(cache_w),
            thought_tokens: (thought > 0).then_some(thought),
            total_tokens: Some(total),
        },
        context_used: None,
        context_size: None,
        cost,
        tool_calls: if secondary { 0 } else { tool_calls },
        secondary_model: secondary,
    })
}

// ---- Antigravity (`~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript.jsonl`)

fn scan_antigravity_brain(
    brain_dir: &Path,
    postil_dir: &Path,
    prev: &mut HashMap<PathBuf, FileStamp>,
    next: &mut HashMap<PathBuf, FileStamp>,
    out: &mut Vec<LedgerRecord>,
) {
    let Ok(conversations) = fs::read_dir(brain_dir) else {
        return;
    };
    for conv in conversations.flatten() {
        if !conv.file_type().is_ok_and(|k| k.is_dir()) {
            continue;
        }
        let conv_dir = conv.path();
        let transcript = conv_dir.join(".system_generated/logs/transcript.jsonl");
        let conv_id = conv.file_name().to_string_lossy().into_owned();
        let records = cached_or_parse(&transcript, prev, next, |fallback_ts| {
            parse_antigravity_transcript(&transcript, &conv_id, postil_dir, fallback_ts)
        });
        out.extend_from_slice(&records);
    }
}

fn parse_antigravity_transcript(
    path: &Path,
    conv_id: &str,
    postil_dir: &Path,
    fallback_ts: u64,
) -> Vec<LedgerRecord> {
    let mut cwd = String::new();
    let mut skipped = false;
    let mut first_ts: Option<u64> = None;
    let mut last_ts: Option<u64> = None;
    let mut tool_calls = 0_u64;
    let mut by_model: BTreeMap<String, PromptUsage> = BTreeMap::new();

    for_each_jsonl_line(path, |entry| {
        let entry_type = entry.get("type").and_then(Value::as_str);
        if cwd.is_empty()
            && matches!(entry_type, Some("USER_INPUT" | "SYSTEM"))
            && let Some(content) = entry.get("content").and_then(Value::as_str)
            && let Some(found) = extract_antigravity_workspace(content)
        {
            cwd = normalize_cwd(&found);
            if should_skip_cwd(&cwd, postil_dir) {
                skipped = true;
                return false;
            }
        }
        if entry_type != Some("PLANNER_RESPONSE") {
            return true;
        }
        if let Some(ts) = entry
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_secs)
        {
            first_ts = Some(first_ts.map_or(ts, |prev| prev.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |prev| prev.max(ts)));
        }
        if let Some(calls) = entry.get("tool_calls").and_then(Value::as_array) {
            tool_calls += calls.len() as u64;
        }
        let in_tok = u64_of(&entry, "input_tokens").unwrap_or(0);
        let cache_r = u64_of(&entry, "cache_read_tokens").unwrap_or(0);
        let out_tok = u64_of(&entry, "output_tokens").unwrap_or(0);
        let total = in_tok.saturating_add(out_tok);
        if total == 0 {
            return true;
        }
        let model = entry
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("Default")
            .to_owned();
        let step_usage = PromptUsage {
            input_tokens: Some(in_tok),
            output_tokens: Some(out_tok),
            cached_read_tokens: (cache_r > 0).then_some(cache_r),
            cached_write_tokens: None,
            thought_tokens: None,
            total_tokens: Some(total),
        };
        add_prompt_usage(by_model.entry(model).or_default(), &step_usage);
        true
    });

    if skipped || should_skip_cwd(&cwd, postil_dir) || by_model.is_empty() {
        return Vec::new();
    }
    let started_at = first_ts.unwrap_or(fallback_ts);
    let ended_at = last_ts.unwrap_or(started_at).max(started_at);
    let mut out = Vec::new();
    for (model, tokens) in by_model {
        let secondary = !out.is_empty();
        out.push(LedgerRecord {
            version: RECORD_VERSION,
            run_id: format!("cli:{conv_id}"),
            repo: cwd.clone(),
            agent: "Antigravity".to_owned(),
            model: Some(model),
            origin: RunOrigin::Notes,
            outcome: RunOutcome::Ended,
            started_at,
            ended_at: if secondary { started_at } else { ended_at },
            tokens,
            context_used: None,
            context_size: None,
            cost: None,
            tool_calls: if secondary { 0 } else { tool_calls },
            secondary_model: secondary,
        });
    }
    out
}

fn extract_antigravity_workspace(content: &str) -> Option<String> {
    if let Some((_, rest)) = content.split_once("Command Working Directory:") {
        let line = rest.lines().next()?.trim();
        if line.starts_with('/') {
            return Some(line.to_owned());
        }
    }
    if let Some((_, rest)) = content.split_once("Active Workspaces:") {
        for line in rest.lines().take(4) {
            if let Some(path) = line.trim().strip_prefix("- ")
                && path.trim().starts_with('/')
            {
                return Some(path.trim().to_owned());
            }
        }
    }
    None
}

// ---- Shared helpers -------------------------------------------------------------------------

fn for_each_jsonl_line(path: &Path, mut visit: impl FnMut(Value) -> bool) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let Ok(meta) = file.metadata() else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut reader)
            .take(MAX_LINE_BYTES as u64)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() == MAX_LINE_BYTES && line.last() != Some(&b'\n') {
            if reader.skip_until(b'\n').is_err() {
                break;
            }
            continue;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&line)
            && !visit(value)
        {
            break;
        }
    }
}

fn read_small_json(path: &Path) -> Option<Value> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_JSON_FILE_BYTES {
        return None;
    }
    let file = File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_JSON_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_JSON_FILE_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn u64_of(obj: &Value, key: &str) -> Option<u64> {
    obj.get(key)?.as_u64()
}

fn add_prompt_usage(acc: &mut PromptUsage, step: &PromptUsage) {
    let sum = |target: &mut Option<u64>, add: Option<u64>| {
        if let Some(n) = add.filter(|&n| n > 0) {
            *target = Some(target.unwrap_or(0).saturating_add(n));
        }
    };
    sum(&mut acc.input_tokens, step.input_tokens);
    sum(&mut acc.output_tokens, step.output_tokens);
    sum(&mut acc.cached_read_tokens, step.cached_read_tokens);
    sum(&mut acc.cached_write_tokens, step.cached_write_tokens);
    sum(&mut acc.thought_tokens, step.thought_tokens);
    sum(&mut acc.total_tokens, step.total_tokens);
}

fn usd(amount: f64) -> UsageCost {
    UsageCost {
        amount,
        currency: "USD".to_owned(),
    }
}

fn file_stem_id(path: &Path, fallback: &str) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

fn percent_decode_path(encoded: &str) -> String {
    if !encoded.contains('%') {
        return String::new();
    }
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%'
            && idx + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[idx + 1]), hex(bytes[idx + 2]))
        {
            out.push((hi << 4) | lo);
            idx += 3;
            continue;
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8(out)
        .ok()
        .filter(|s| s.starts_with('/'))
        .unwrap_or_default()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use crate::usage::ledger::{CostTotal, Range, aggregate};

    fn seed_claude_and_codex_fixtures(root: &Path) {
        let claude_dir = root.join(".claude/projects/-Users-me-repo");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("sess-cost.jsonl"),
            concat!(
                r#"{"type":"user","cwd":"/Users/me/repo/crates/ui","timestamp":"2026-11-26T10:00:00Z"}"#,
                "\n",
                r#"{"type":"cost-state","startTime":1795687200000,"totalDuration":15000,"modelUsage":{"claude-sonnet-4-6":{"inputTokens":100,"cacheReadInputTokens":400,"cacheCreationInputTokens":50,"outputTokens":80,"thinkingTokens":20,"costUSD":0.05},"claude-haiku-4-5":{"inputTokens":10,"outputTokens":5,"costUSD":0.01}}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            claude_dir.join("sess-stream.jsonl"),
            concat!(
                r#"{"type":"assistant","cwd":"/Users/me/repo","requestId":"req_1","timestamp":"2026-11-26T11:00:00Z","message":{"model":"claude-sonnet-4-6","content":[{"type":"thinking"},{"type":"tool_use"}],"usage":{"input_tokens":20,"cache_read_input_tokens":80,"output_tokens":10}}}"#,
                "\n",
                r#"{"type":"assistant","cwd":"/Users/me/repo","requestId":"req_1","timestamp":"2026-11-26T11:00:05Z","message":{"model":"claude-sonnet-4-6","content":[{"type":"text"}],"usage":{"input_tokens":20,"cache_read_input_tokens":80,"output_tokens":30}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let postil_cwd = root.join(".postil/worktrees/R1");
        fs::write(
            claude_dir.join("sess-postil.jsonl"),
            format!(
                r#"{{"type":"cost-state","cwd":"{}","startTime":1795687200000,"modelUsage":{{"sonnet":{{"inputTokens":9999,"outputTokens":9999,"costUSD":9.99}}}}}}"#,
                postil_cwd.display()
            ),
        )
        .unwrap();

        let codex_dir = root.join(".codex/sessions/2026/11/26");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("rollout-1.jsonl"),
            concat!(
                r#"{"timestamp":"2026-11-26T12:00:00Z","type":"session_meta","payload":{"cwd":"/Users/me/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-11-26T12:00:02Z","type":"turn_context","payload":{"cwd":"/Users/me/repo","model":"gpt-5.3-codex"}}"#,
                "\n",
                r#"{"timestamp":"2026-11-26T12:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":200,"output_tokens":50,"reasoning_output_tokens":25,"total_tokens":550}}}}"#,
                "\n",
                r#"{"timestamp":"2026-11-26T12:00:20Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":600,"output_tokens":120,"reasoning_output_tokens":40,"total_tokens":1120}}}}"#,
                "\n",
            ),
        )
        .unwrap();
    }

    #[test]
    fn scans_claude_codex_grok_and_antigravity_and_skips_postil_worktrees() {
        let home = TempDir::new("cli-scanner-home");
        let root = home.path();
        seed_claude_and_codex_fixtures(root);

        // 3. Grok session with stringified `costUsdTicks` and URL-encoded directory fallback.
        let grok_dir = root.join(".grok/sessions/%2FUsers%2Fme%2Frepo/grok-sess-1");
        fs::create_dir_all(&grok_dir).unwrap();
        fs::write(
            grok_dir.join("signals.json"),
            r#"{"sessionDurationSeconds":22,"toolCallCount":3}"#,
        )
        .unwrap();
        fs::write(
            grok_dir.join("usage.json"),
            r#"{
                "updatedAt": "2026-11-26T13:00:00Z",
                "session": {
                    "primaryModelId": "grok-4-fast",
                    "modelUsage": {
                        "grok-4-fast": {
                            "inputTokens": 2000,
                            "outputTokens": 150,
                            "cachedReadTokens": 1500,
                            "reasoningTokens": 60,
                            "totalTokens": 2150,
                            "costUsdTicks": "400000000"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        // 4. Antigravity conversation transcript.
        let agy_logs = root.join(".gemini/antigravity-cli/brain/conv-123/.system_generated/logs");
        fs::create_dir_all(&agy_logs).unwrap();
        fs::write(
            agy_logs.join("transcript.jsonl"),
            concat!(
                r#"{"type":"USER_INPUT","content":"Active Workspaces:\n- /Users/me/repo\nCommand Working Directory: /Users/me/repo\n"}"#,
                "\n",
                r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-26T14:00:00Z","model":"gemini-4-carbon","input_tokens":3000,"cache_read_tokens":2000,"output_tokens":200,"tool_calls":[{"id":"1"},{"id":"2"}]}"#,
                "\n",
                r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-26T14:00:30Z","model":"gemini-4-carbon","input_tokens":4000,"cache_read_tokens":3500,"output_tokens":300,"tool_calls":[]}"#,
                "\n",
            ),
        )
        .unwrap();

        let scanner = SessionScanner::new(Some(root.to_owned()));
        let (_, busy) = scanner.query(QuotaFetch::Cached);
        assert!(busy);
        scanner.join_scans();
        let (records, busy_after) = scanner.query(QuotaFetch::Cached);
        assert!(!busy_after);

        let now = parse_rfc3339_secs("2026-11-26T15:00:00Z").unwrap();
        let (summary, agents) = aggregate(
            records.iter(),
            Some("/Users/me/repo"),
            Range::Day,
            now,
            true,
        );
        // 2 Claude sessions + 1 Codex + 1 Grok + 1 Antigravity = 5 sessions (not 6: Claude's
        // secondary Haiku bucket does not inflate session count).
        assert_eq!(summary.runs, 5, "{summary:?}");
        assert_eq!(agents["Claude"].totals.runs, 2);
        assert_eq!(agents["Claude"].models.len(), 2);
        assert_eq!(agents["Claude"].totals.total_tokens, 630 + 15 + 130);
        assert_eq!(agents["Codex"].totals.total_tokens, 1120);
        assert_eq!(agents["Grok"].totals.total_tokens, 2150);
        assert_eq!(agents["Antigravity"].totals.total_tokens, 7500);
        assert_eq!(agents["Antigravity"].totals.tool_calls, 2);

        let CostTotal::Same(cost) = &summary.cost else {
            panic!("expected USD cost total: {:?}", summary.cost);
        };
        // $0.05 + $0.01 (Claude) + $0.04 (Grok) = $0.10.
        assert!((cost.amount - 0.10).abs() < 1e-9, "{cost:?}");

        assert!(should_skip_cwd(
            "/Volumes/External/.postil/worktrees/repo-1/r1",
            &root.join(".postil")
        ));
        assert!(should_skip_cwd(
            "/private/tmp/postil-quota",
            &root.join(".postil")
        ));
        assert!(!should_skip_cwd("/Users/me/repo", &root.join(".postil")));
    }
}
