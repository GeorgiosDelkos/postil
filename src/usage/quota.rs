//! On-demand background probes for live provider account quotas and rate-limit windows.
//!
//! Each probe runs on a short-lived thread; every subprocess it starts has a hard timeout,
//! and its process group is killed when done, matching `agents::Scanner`. Results are cached
//! briefly in memory so reopening the Usage dialog does not spam provider endpoints, and
//! nothing runs while the dialog is closed. Secrets (OAuth tokens) are piped to
//! `/usr/bin/curl` over stdin (`--config -`), never passed in process arguments.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
#[cfg(test)]
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::acp::METHOD_NOT_FOUND;
use crate::agents;
use crate::usage::now_secs;

/// How long a fetched quota snapshot stays fresh before the next open re-probes it.
/// Arbitrary: provider windows move over minutes and hours, so a minute-old figure is
/// current enough, and closing and reopening the dialog within it sends no request.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Hard limit on one subprocess a probe runs (`security`, `curl`, `codex app-server`, or
/// `grok agent stdio`).
/// A probe runs up to two of them one after the other (Claude: the Keychain read, then the
/// usage request), so a probe can take up to twice this. Arbitrary:
/// long enough for a slow network round trip, short enough that the dialog does not show
/// "Loading" for long.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// How long an in-flight `Loading` entry stays fresh before a later poll replaces its probe.
/// Two [`PROBE_TIMEOUT`]s (the longest a probe should take) plus 4 s of margin, so only a
/// probe that is stuck past its own timeouts is replaced.
const LOADING_TIMEOUT: Duration = Duration::from_secs(2 * PROBE_TIMEOUT.as_secs() + 4);

/// State of a live quota probe for one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum QuotaStatus {
    Loading,
    Ok,
    Unavailable,
    Error,
}

/// One rolling or per-model quota window reported by a provider.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(PartialEq))]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotaWindow {
    pub(crate) label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    /// Percentage of the window consumed, clamped to `0.0..=100.0`.
    pub(crate) used_percent: f64,
    /// Unix epoch seconds when this window resets, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resets_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

/// Extra spend or credit balance reported alongside quota windows.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(PartialEq))]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreditInfo {
    pub(crate) label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) currency: Option<String>,
    pub(crate) unlimited: bool,
}

/// Normalized live quota report for one agent.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(PartialEq))]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotaReport {
    pub(crate) status: QuotaStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plan: Option<String>,
    pub(crate) windows: Vec<QuotaWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) credits: Option<CreditInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fetched_at: Option<u64>,
}

impl QuotaReport {
    pub(crate) fn loading() -> Self {
        Self {
            status: QuotaStatus::Loading,
            plan: None,
            windows: Vec::new(),
            credits: None,
            message: None,
            fetched_at: None,
        }
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: QuotaStatus::Unavailable,
            plan: None,
            windows: Vec::new(),
            credits: None,
            message: Some(message.into()),
            fetched_at: Some(now_secs()),
        }
    }

    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self {
            status: QuotaStatus::Error,
            plan: None,
            windows: Vec::new(),
            credits: None,
            message: Some(message.into()),
            fetched_at: Some(now_secs()),
        }
    }
}

/// Function that probes one built-in agent's live provider quota.
type ProbeFn = Arc<dyn Fn(&str, &OsStr) -> QuotaReport + Send + Sync>;

/// Which cached quota reports [`QuotaPoller::query`] serves and which it probes again.
#[derive(Debug, Clone, Copy)]
pub(crate) enum QuotaFetch {
    /// Serve cached reports; probe only an entry that is missing, older than
    /// [`CACHE_TTL`], or in flight past [`LOADING_TIMEOUT`]. `GET /api/usage`.
    Cached,
    /// Probe every finished entry again, but keep a probe still in flight within
    /// [`LOADING_TIMEOUT`]: its result is the fresh one. `POST /api/usage/refresh`.
    Refresh,
}

/// Manages cached and in-flight provider quota probes across requests.
///
/// # Invariants
///
/// Each cache entry names the id of the latest probe started for it, and only that probe may
/// store its result. Ids are never reused, so a slow probe that a newer one replaced (after
/// [`LOADING_TIMEOUT`], or a Refresh of a finished entry) cannot overwrite the newer result,
/// whichever finishes last.
pub(crate) struct QuotaPoller {
    probe: ProbeFn,
    state: Arc<Mutex<PollerState>>,
    /// How long a probe may stay in flight before the next poll replaces it. Always
    /// [`LOADING_TIMEOUT`] outside tests.
    loading_timeout: Duration,
    /// Every probe thread started, so a test can wait for the probes it started.
    #[cfg(test)]
    threads: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Default)]
struct PollerState {
    /// The id the next probe gets.
    next_probe: u64,
    entries: HashMap<String, CachedQuota>,
}

struct CachedQuota {
    report: QuotaReport,
    updated: Instant,
    /// Id of the latest probe started for this entry.
    probe: u64,
}

impl QuotaPoller {
    /// A poller using the real provider probes. Starts no background work until asked.
    pub(crate) fn live() -> Self {
        Self::new(probe_agent)
    }

    /// A poller whose probes call `probe`, used by tests without network or CLIs.
    pub(crate) fn new(probe: impl Fn(&str, &OsStr) -> QuotaReport + Send + Sync + 'static) -> Self {
        Self {
            probe: Arc::new(probe),
            state: Arc::new(Mutex::new(PollerState::default())),
            loading_timeout: LOADING_TIMEOUT,
            #[cfg(test)]
            threads: Mutex::new(Vec::new()),
        }
    }

    /// Returns the latest quota report for each named agent, and starts background probes
    /// for the entries `fetch` asks to probe again (see [`QuotaFetch`]). Does not wait for
    /// a probe: a probe in flight reports [`QuotaStatus::Loading`].
    pub(crate) fn query(
        &self,
        names: &[&str],
        search_path: &OsStr,
        fetch: QuotaFetch,
    ) -> HashMap<String, QuotaReport> {
        let mut to_spawn: Vec<(String, u64)> = Vec::new();
        let mut results = HashMap::with_capacity(names.len());
        {
            let mut state = lock(&self.state);
            for &name in names {
                let keep = state.entries.get(name).is_some_and(|entry| {
                    let age = entry.updated.elapsed();
                    match entry.report.status {
                        QuotaStatus::Loading => age < self.loading_timeout,
                        QuotaStatus::Ok | QuotaStatus::Unavailable | QuotaStatus::Error => {
                            matches!(fetch, QuotaFetch::Cached) && age < CACHE_TTL
                        }
                    }
                });
                if keep {
                    results.insert(name.to_owned(), state.entries[name].report.clone());
                    continue;
                }

                let probe = state.next_probe;
                state.next_probe += 1;
                let loading = QuotaReport::loading();
                state.entries.insert(
                    name.to_owned(),
                    CachedQuota {
                        report: loading.clone(),
                        updated: Instant::now(),
                        probe,
                    },
                );
                results.insert(name.to_owned(), loading);
                to_spawn.push((name.to_owned(), probe));
            }
        }

        for (name, probe) in to_spawn {
            self.spawn_probe(&name, probe, search_path);
        }
        results
    }

    /// Starts probe number `probe` for agent `name` on its own thread.
    fn spawn_probe(&self, name: &str, probe: u64, search_path: &OsStr) {
        let probe_fn = Arc::clone(&self.probe);
        let shared = Arc::clone(&self.state);
        let path = search_path.to_owned();
        let agent_name = name.to_owned();
        let spawned = std::thread::Builder::new()
            .name(format!("quota-{name}"))
            .spawn(move || {
                let report = probe_fn(&agent_name, &path);
                store(&shared, &agent_name, probe, report);
            });
        match spawned {
            Ok(handle) => {
                #[cfg(test)]
                lock(&self.threads).push(handle);
                // Outside tests nothing waits for a probe: it reports through `store`.
                #[cfg(not(test))]
                drop(handle);
            }
            Err(error) => store(
                &self.state,
                name,
                probe,
                QuotaReport::error(format!("cannot start a quota check thread: {error}")),
            ),
        }
    }

    /// Replaces how long a probe may stay in flight, so a test can start a second probe for
    /// an entry while the first still runs.
    #[cfg(test)]
    fn with_loading_timeout(mut self, timeout: Duration) -> Self {
        self.loading_timeout = timeout;
        self
    }

    /// Takes the handles of every probe thread started so far, in start order.
    #[cfg(test)]
    fn take_probes(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut *lock(&self.threads))
    }

    /// Waits until every probe thread started so far has stored or dropped its result.
    #[cfg(test)]
    pub(crate) fn join_probes(&self) {
        for handle in self.take_probes() {
            handle.join().expect("a quota probe thread panicked");
        }
    }
}

impl fmt::Debug for QuotaPoller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("QuotaPoller")
            .field("probes_started", &state.next_probe)
            .field("cached", &state.entries.len())
            .finish_non_exhaustive()
    }
}

/// Stores `report` as agent `name`'s result when probe number `probe` is still the latest
/// probe for that entry, and drops it otherwise.
fn store(state: &Mutex<PollerState>, name: &str, probe: u64, report: QuotaReport) {
    let mut state = lock(state);
    if let Some(entry) = state.entries.get_mut(name)
        && entry.probe == probe
    {
        entry.report = report;
        entry.updated = Instant::now();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Dispatches a live quota probe for a built-in agent name.
fn probe_agent(name: &str, search_path: &OsStr) -> QuotaReport {
    match name {
        "Claude" => probe_claude(),
        "Codex" => probe_codex(search_path),
        // `agy` keeps its OAuth credentials in its own keyring ACL, and Google's Antigravity
        // Terms disallow third-party extraction or polling of consumer quota endpoints;
        // `agy acp` still reports per-run token usage to the Postil ledger below.
        "Gemini" => QuotaReport::unavailable(
            "Live quota is only shown inside agy (/usage) — see Postil activity below",
        ),
        "Grok" => probe_grok(search_path),
        "OpenCode" => QuotaReport::unavailable(
            "OpenCode routes across multiple providers — see Postil activity below",
        ),
        _ => QuotaReport::unavailable("No provider quota source"),
    }
}

fn probe_claude() -> QuotaReport {
    let creds = match read_claude_credentials() {
        Ok(creds) => creds,
        Err(message) => return QuotaReport::error(message),
    };
    let Some((token, plan)) = creds else {
        if std::env::var_os("ANTHROPIC_API_KEY").is_some_and(|val| !val.is_empty()) {
            return QuotaReport::unavailable("API key mode — no subscription quota window");
        }
        return QuotaReport::unavailable("Not signed in — run `claude` in a terminal to sign in");
    };

    let headers = [
        format!("Authorization: Bearer {token}"),
        "anthropic-beta: oauth-2025-04-20".to_owned(),
        "Accept: application/json".to_owned(),
    ];
    match curl_json(
        "GET",
        "https://api.anthropic.com/api/oauth/usage",
        &headers,
        None,
    ) {
        Ok((200, body)) => parse_claude_quota(&body, plan),
        Ok((401 | 403, _)) => {
            QuotaReport::unavailable("Session expired — run `claude` in a terminal to refresh")
        }
        Ok((status, _)) => {
            QuotaReport::error(format!("Claude usage endpoint returned HTTP {status}"))
        }
        Err(message) => QuotaReport::error(message),
    }
}

/// Claude Code's OAuth token and plan, from the Keychain or else its credentials file.
/// `Ok(None)` means not signed in; an error means the file exists but cannot be read.
fn read_claude_credentials() -> Result<Option<(String, Option<String>)>, String> {
    if let Some(raw) = read_keychain_secret("Claude Code-credentials")
        && let Some(parsed) = parse_claude_credentials(&raw)
    {
        return Ok(Some(parsed));
    }
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(home).join(".claude/.credentials.json");
    Ok(read_credentials_file(&path)?.and_then(|raw| parse_claude_credentials(&raw)))
}

/// Largest credentials file read, in bytes. Arbitrary: the files hold a few tokens as JSON,
/// under 2 KiB, and 64 KiB leaves room for growth while a stray huge file is refused rather
/// than read into memory.
const MAX_CREDENTIALS_BYTES: u64 = 64 * 1024;

/// Reads the credentials file at `path`, or `Ok(None)` when it does not exist. Refuses
/// anything but a regular file of at most [`MAX_CREDENTIALS_BYTES`], so a FIFO or a huge file
/// can neither block the probe nor fill memory.
fn read_credentials_file(path: &Path) -> Result<Option<String>, String> {
    let shown = path.display();
    let check = |metadata: &fs::Metadata| {
        if !metadata.is_file() {
            return Err(format!("{shown} is not a regular file"));
        }
        if metadata.len() > MAX_CREDENTIALS_BYTES {
            return Err(format!(
                "{shown} is larger than {MAX_CREDENTIALS_BYTES} bytes"
            ));
        }
        Ok(())
    };

    // `metadata` does not open the file, so a FIFO is refused before `open` could wait on it
    // for a writer. A FIFO swapped in between the two calls would still block `open`; that
    // takes someone rewriting the user's own home directory during the probe.
    match fs::metadata(path) {
        Ok(metadata) => check(&metadata)?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {shown}: {error}")),
    }
    let file = File::open(path).map_err(|error| format!("cannot read {shown}: {error}"))?;
    // Check what was opened too, in case the path changed after `metadata`.
    check(
        &file
            .metadata()
            .map_err(|error| format!("cannot read {shown}: {error}"))?,
    )?;

    let mut raw = String::new();
    file.take(MAX_CREDENTIALS_BYTES + 1)
        .read_to_string(&mut raw)
        .map_err(|error| format!("cannot read {shown}: {error}"))?;
    if raw.len() as u64 > MAX_CREDENTIALS_BYTES {
        return Err(format!(
            "{shown} is larger than {MAX_CREDENTIALS_BYTES} bytes"
        ));
    }
    Ok(Some(raw))
}

fn parse_claude_credentials(raw: &str) -> Option<(String, Option<String>)> {
    let trimmed = raw.trim();
    let decoded = if trimmed.starts_with('{') {
        None
    } else {
        decode_hex_utf8(trimmed)
    };
    let text = decoded.as_deref().unwrap_or(trimmed);
    let value: Value = serde_json::from_str(text).ok()?;
    let oauth = value.get("claudeAiOauth").unwrap_or(&value);
    let token = oauth
        .get("accessToken")
        .or_else(|| oauth.get("access_token"))
        .and_then(Value::as_str)?
        .trim();
    if token.is_empty() {
        return None;
    }
    let plan = oauth
        .get("subscriptionType")
        .or_else(|| oauth.get("subscription_type"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(title_case);
    Some((token.to_owned(), plan))
}

/// Decodes a hex-encoded UTF-8 string, as macOS `security find-generic-password -w` prints
/// when a Keychain item contains newlines or non-ASCII bytes.
fn decode_hex_utf8(hex: &str) -> Option<String> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    if pairs.is_empty() || !remainder.is_empty() {
        return None;
    }
    let mut bytes = Vec::with_capacity(pairs.len());
    for &[hi, lo] in pairs {
        bytes.push((hex_nibble(hi)? << 4) | hex_nibble(lo)?);
    }
    String::from_utf8(bytes).ok()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_claude_quota(body: &str, plan: Option<String>) -> QuotaReport {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return QuotaReport::error("unreadable reply from Claude usage endpoint");
    };
    let mut windows = Vec::new();
    let specs: [(&str, &str, Option<&str>); 5] = [
        ("five_hour", "5-hour session", None),
        ("seven_day", "7-day all models", None),
        ("seven_day_sonnet", "7-day Sonnet", Some("sonnet")),
        ("seven_day_opus", "7-day Opus", Some("opus")),
        ("seven_day_oauth_apps", "7-day OAuth apps", None),
    ];
    for (key, label, model) in specs {
        let Some(bucket) = json.get(key).filter(|val| val.is_object()) else {
            continue;
        };
        let Some(utilization) = bucket.get("utilization").and_then(Value::as_f64) else {
            continue;
        };
        let resets_at = bucket
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_secs);
        windows.push(QuotaWindow {
            label: label.to_owned(),
            model: model.map(str::to_owned),
            used_percent: clamp_percent(utilization),
            resets_at,
            detail: None,
        });
    }

    let credits = json
        .get("extra_usage")
        .filter(|extra| {
            extra
                .get("is_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .map(|extra| {
            let used = extra
                .get("used_credits")
                .and_then(Value::as_f64)
                .map(|cents| cents / 100.0);
            let limit = extra
                .get("monthly_limit")
                .and_then(Value::as_f64)
                .map(|cents| cents / 100.0);
            CreditInfo {
                label: "Extra usage".to_owned(),
                used,
                limit,
                remaining: match (used, limit) {
                    (Some(u), Some(l)) => Some((l - u).max(0.0)),
                    _ => None,
                },
                currency: Some("USD".to_owned()),
                unlimited: limit.is_none(),
            }
        });

    if windows.is_empty() && credits.is_none() {
        return QuotaReport::unavailable("Claude reported no active quota windows");
    }
    QuotaReport {
        status: QuotaStatus::Ok,
        plan,
        windows,
        credits,
        message: None,
        fetched_at: Some(now_secs()),
    }
}

/// Longest JSON-RPC line read from a quota probe's stdout, newline included; longer lines are
/// skipped so a noisy or misbehaving CLI cannot grow memory without bound. Smaller in tests so
/// oversized-line handling runs cheaply.
const MAX_PROBE_LINE_BYTES: usize = if cfg!(test) { 16 << 10 } else { 1 << 20 };

fn probe_codex(search_path: &OsStr) -> QuotaReport {
    let Some(program) = agents::find_program("codex", search_path) else {
        return QuotaReport::unavailable("Codex CLI not found on PATH");
    };
    let mut child = match Command::new(&program)
        .arg("app-server")
        .env("PATH", search_path)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return QuotaReport::error(format!("could not start codex app-server: {error}"));
        }
    };

    let requests = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","#,
        r#""params":{"clientInfo":{"name":"postil","version":""#,
        env!("CARGO_PKG_VERSION"),
        r#""}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"account/read","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"account/rateLimits/read","params":{}}"#,
        "\n",
    );
    let result = query_stdio_server(
        &mut child,
        "codex app-server",
        requests,
        collect_codex_replies,
    );
    agents::kill_group(child.id());
    drop(child.kill());
    drop(child.wait());

    match result {
        Ok((account, limits)) => parse_codex_quota(account.as_ref(), limits.as_ref()),
        Err(message) => QuotaReport::error(message),
    }
}

/// Sends `requests` to a running JSON-RPC stdio server and collects its replies within
/// [`PROBE_TIMEOUT`]. Ignores `BrokenPipe` on write so a child that exits immediately still
/// has its stdout/EOF read deterministically. Does not stop the child.
fn query_stdio_server<T: Send + 'static>(
    child: &mut Child,
    server_name: &'static str,
    requests: &str,
    collect: impl FnOnce(ChildStdout) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let mut stdin = child
        .stdin
        .take()
        .expect("stdio quota probe starts child with piped stdin");
    let stdout = child
        .stdout
        .take()
        .expect("stdio quota probe starts child with piped stdout");
    if let Err(error) = stdin
        .write_all(requests.as_bytes())
        .and_then(|()| stdin.flush())
        && error.kind() != ErrorKind::BrokenPipe
    {
        return Err(format!("cannot write to {server_name}: {error}"));
    }

    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("quota-stdio-read".to_owned())
        .spawn(move || drop(sender.send(collect(stdout))))
        .map_err(|error| format!("cannot start a thread to read {server_name}: {error}"))?;
    receiver.recv_timeout(PROBE_TIMEOUT).map_err(|_| {
        format!(
            "{server_name} did not answer within {} s",
            PROBE_TIMEOUT.as_secs()
        )
    })?
}

/// Reads bounded JSON-RPC response lines (ignoring notifications and server-initiated requests
/// carrying `"method"`) until `on_reply` returns `Ok(true)`, an error, or EOF.
fn read_jsonrpc_replies(
    stdout: impl Read,
    mut on_reply: impl FnMut(u64, Value) -> Result<bool, String>,
) -> Result<(), String> {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut reader)
            .take(MAX_PROBE_LINE_BYTES as u64)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() == MAX_PROBE_LINE_BYTES && line.last() != Some(&b'\n') {
            if reader.skip_until(b'\n').is_err() {
                break;
            }
            continue;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if msg.get("method").is_some() {
            continue;
        }
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if on_reply(id, msg)? {
            break;
        }
    }
    Ok(())
}

/// Extracts an `initialize` error message when `"error"` is present and non-null.
fn rpc_init_error(msg: &Value, server_name: &str) -> Option<String> {
    let err = msg.get("error").filter(|v| !v.is_null())?;
    Some(
        err.get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(
                || format!("{server_name} refused initialize"),
                bounded_provider_text,
            ),
    )
}

/// Reads `codex app-server` replies until both the account and the rate-limit reply arrived,
/// the output ends, or `initialize` failed.
fn collect_codex_replies(stdout: ChildStdout) -> Result<(Option<Value>, Option<Value>), String> {
    let mut account = None;
    let mut limits = None;
    read_jsonrpc_replies(stdout, |id, msg| {
        match id {
            1 => {
                if let Some(err) = rpc_init_error(&msg, "codex app-server") {
                    return Err(err);
                }
            }
            2 => account = Some(msg),
            3 => limits = Some(msg),
            _ => {}
        }
        Ok(account.is_some() && limits.is_some())
    })?;
    Ok((account, limits))
}

fn parse_codex_quota(account: Option<&Value>, limits: Option<&Value>) -> QuotaReport {
    let auth_mode = account
        .and_then(|msg| msg.get("result"))
        .and_then(|res| {
            res.get("authMode")
                .or_else(|| res.get("account").and_then(|acc| acc.get("type")))
        })
        .and_then(Value::as_str);
    let plan = account
        .and_then(|msg| msg.get("result"))
        .and_then(|res| {
            res.get("planType")
                .or_else(|| res.get("account").and_then(|acc| acc.get("planType")))
        })
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(title_case);

    if matches!(auth_mode, Some("apikey" | "apiKey" | "api_key")) {
        return QuotaReport::unavailable("API key mode — no subscription quota window");
    }

    let Some(limits_msg) = limits else {
        return QuotaReport::unavailable("Codex did not return rate limit data");
    };
    if let Some(error) = limits_msg.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("sign in with `codex` in a terminal");
        return QuotaReport::unavailable(bounded_provider_text(message));
    }
    let Some(result) = limits_msg.get("result") else {
        return QuotaReport::unavailable("Codex did not return rate limit data");
    };

    let mut windows = Vec::new();
    let mut credits = None;

    if let Some(primary_bucket) = result.get("rateLimits").filter(|v| v.is_object()) {
        collect_codex_bucket(primary_bucket, None, &mut windows);
        credits = parse_codex_credits(primary_bucket);
    }
    if let Some(by_id) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for (limit_id, bucket) in by_id {
            if limit_id == "codex" && !windows.is_empty() {
                continue;
            }
            let model_name = bucket
                .get("limitName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(limit_id.as_str());
            collect_codex_bucket(bucket, Some(model_name), &mut windows);
            if credits.is_none() {
                credits = parse_codex_credits(bucket);
            }
        }
    }

    if windows.is_empty() && credits.is_none() {
        return QuotaReport::unavailable("Not signed in — run `codex` in a terminal to sign in");
    }
    QuotaReport {
        status: QuotaStatus::Ok,
        plan,
        windows,
        credits,
        message: None,
        fetched_at: Some(now_secs()),
    }
}

fn collect_codex_bucket(bucket: &Value, model: Option<&str>, out: &mut Vec<QuotaWindow>) {
    for (field, fallback_label) in [("primary", "5-hour window"), ("secondary", "7-day window")] {
        let Some(win) = bucket.get(field).filter(|v| v.is_object()) else {
            continue;
        };
        let Some(used) = win
            .get("usedPercent")
            .or_else(|| win.get("used_percent"))
            .and_then(Value::as_f64)
        else {
            continue;
        };
        let resets_at = win
            .get("resetsAt")
            .or_else(|| win.get("resets_at"))
            .and_then(Value::as_u64);
        let mins = win
            .get("windowDurationMins")
            .or_else(|| win.get("window_minutes"))
            .and_then(Value::as_u64);
        let window_name = match mins {
            Some(m) if m >= 60 * 24 => format!("{}-day window", m / (60 * 24)),
            Some(m) if m >= 60 => format!("{}-hour window", m / 60),
            Some(m) if m > 0 => format!("{m}-min window"),
            _ => fallback_label.to_owned(),
        };
        let label = match model {
            Some(name) => format!("{name} ({window_name})"),
            None => window_name,
        };
        out.push(QuotaWindow {
            label,
            model: model.map(str::to_owned),
            used_percent: clamp_percent(used),
            resets_at,
            detail: None,
        });
    }
}

fn parse_codex_credits(bucket: &Value) -> Option<CreditInfo> {
    let credits = bucket.get("credits").filter(|v| v.is_object())?;
    let has_credits = credits
        .get("hasCredits")
        .or_else(|| credits.get("has_credits"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let unlimited = credits
        .get("unlimited")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !has_credits && !unlimited {
        return None;
    }
    let balance = credits.get("balance").and_then(|val| {
        val.as_f64()
            .or_else(|| val.as_str().and_then(|s| s.parse::<f64>().ok()))
    });
    Some(CreditInfo {
        label: "Credits".to_owned(),
        used: None,
        limit: None,
        remaining: balance,
        currency: None,
        unlimited,
    })
}

fn probe_grok(search_path: &OsStr) -> QuotaReport {
    let Some(program) = agents::find_program("grok", search_path) else {
        return QuotaReport::unavailable("Grok CLI not found on PATH");
    };
    let cwd = std::env::temp_dir().join("postil-quota");
    if let Err(error) = fs::create_dir_all(&cwd) {
        return QuotaReport::error(format!("cannot make the folder {}: {error}", cwd.display()));
    }
    let mut child = match Command::new(&program)
        .args(["agent", "stdio"])
        .current_dir(&cwd)
        .env("PATH", search_path)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return QuotaReport::error(format!("could not start grok agent stdio: {error}"));
        }
    };

    let requests = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","#,
        r#""params":{"protocolVersion":1,"clientCapabilities":"#,
        r#"{"fs":{"readTextFile":false,"writeTextFile":false},"#,
        r#""terminal":false,"elicitation":{"form":{}}}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"_x.ai/billing","params":{}}"#,
        "\n",
    );
    let result = query_stdio_server(&mut child, "grok agent stdio", requests, collect_grok_reply);
    agents::kill_group(child.id());
    drop(child.kill());
    drop(child.wait());

    let has_api_key = std::env::var_os("XAI_API_KEY").is_some_and(|val| !val.is_empty());
    match result {
        Ok(billing) => parse_grok_quota(billing.as_ref(), has_api_key),
        Err(message) => QuotaReport::error(message),
    }
}

/// Reads `grok agent stdio` lines until the `_x.ai/billing` reply (`id: 2`) arrives, stdout
/// ends, or `initialize` (`id: 1`) returned an error.
fn collect_grok_reply(stdout: ChildStdout) -> Result<Option<Value>, String> {
    let mut billing = None;
    read_jsonrpc_replies(stdout, |id, msg| match id {
        1 => match rpc_init_error(&msg, "grok agent stdio") {
            Some(err) => Err(err),
            None => Ok(false),
        },
        2 => {
            billing = Some(msg);
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(billing)
}

fn parse_grok_quota(billing: Option<&Value>, has_api_key: bool) -> QuotaReport {
    let Some(billing_msg) = billing else {
        return QuotaReport::unavailable("Grok did not return billing data");
    };
    if let Some(error) = billing_msg.get("error").filter(|v| !v.is_null()) {
        if error.get("code").and_then(Value::as_i64) == Some(METHOD_NOT_FOUND) {
            return QuotaReport::unavailable("Update Grok CLI (`grok update`) to check live quota");
        }
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Not signed in — run `grok login` in a terminal");
        return QuotaReport::unavailable(bounded_provider_text(message));
    }
    let Some(result) = billing_msg.get("result").filter(|v| v.is_object()) else {
        return QuotaReport::unavailable("Grok did not return billing data");
    };

    let plan = ["subscription_tier", "subscriptionTier"]
        .into_iter()
        .find_map(|key| result.get(key).and_then(Value::as_str))
        .map(|s| s.replace(['\r', '\n'], " "))
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| title_case(&bounded_provider_text(s)));

    let mut windows = Vec::new();
    let mut credits = None;
    if let Some(config) = result.get("config").filter(|v| v.is_object()) {
        // Only emit a window when `creditUsagePercent` is explicitly present: omitting it is
        // not 0% consumption.
        let used = ["creditUsagePercent", "credit_usage_percent"]
            .into_iter()
            .find_map(|key| config.get(key).and_then(Value::as_f64));
        if let Some(used) = used {
            let period = ["currentPeriod", "current_period"]
                .into_iter()
                .find_map(|key| config.get(key).filter(|v| v.is_object()));
            let label =
                grok_period_label(period.and_then(|p| p.get("type")).and_then(Value::as_str));
            let resets_at = period
                .and_then(|p| p.get("end").and_then(Value::as_str))
                .or_else(|| config.get("billingPeriodEnd").and_then(Value::as_str))
                .and_then(parse_rfc3339_secs);
            windows.push(QuotaWindow {
                label: label.to_owned(),
                model: None,
                used_percent: clamp_percent(used),
                resets_at,
                detail: None,
            });
        }
        credits = parse_grok_credits(config);
    }

    if windows.is_empty() && credits.is_none() {
        let message = if plan.is_none() && has_api_key {
            "API key mode — no subscription quota window"
        } else {
            "Grok reported no active quota windows"
        };
        return QuotaReport {
            plan,
            ..QuotaReport::unavailable(message)
        };
    }
    QuotaReport {
        status: QuotaStatus::Ok,
        plan,
        windows,
        credits,
        message: None,
        fetched_at: Some(now_secs()),
    }
}

fn grok_period_label(period_type: Option<&str>) -> &'static str {
    match period_type {
        Some("USAGE_PERIOD_TYPE_DAILY") => "Daily window",
        Some("USAGE_PERIOD_TYPE_WEEKLY") => "Weekly window",
        Some("USAGE_PERIOD_TYPE_MONTHLY") => "Monthly window",
        _ => "Usage window",
    }
}

fn parse_grok_credits(config: &Value) -> Option<CreditInfo> {
    let amount = |keys: &[&str]| keys.iter().find_map(|key| cents_usd(config.get(*key)));
    if let Some(limit) = amount(&["onDemandCap", "on_demand_cap"]).filter(|&cap| cap > 0.0) {
        let used = amount(&["onDemandUsed", "on_demand_used"]).unwrap_or(0.0);
        return Some(CreditInfo {
            label: "On-demand".to_owned(),
            used: Some(used),
            limit: Some(limit),
            remaining: Some((limit - used).max(0.0)),
            currency: Some("USD".to_owned()),
            unlimited: false,
        });
    }
    let remaining = amount(&["prepaidBalance", "prepaid_balance"]).filter(|&bal| bal > 0.0)?;
    Some(CreditInfo {
        label: "Prepaid credits".to_owned(),
        used: None,
        limit: None,
        remaining: Some(remaining),
        currency: Some("USD".to_owned()),
        unlimited: false,
    })
}

/// Reads a `{ "val": <cents> }` billing wrapper into US dollars.
fn cents_usd(field: Option<&Value>) -> Option<f64> {
    let val = field?.get("val")?;
    let cents = val
        .as_f64()
        .or_else(|| val.as_str().and_then(|s| s.trim().parse::<f64>().ok()))?;
    (cents.is_finite() && cents >= 0.0).then_some(cents / 100.0)
}

/// Reads a generic password secret from the macOS Keychain, bounded by [`PROBE_TIMEOUT`].
#[cfg(target_os = "macos")]
fn read_keychain_secret(service: &str) -> Option<String> {
    let output = run_timed(
        Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service, "-w"])
            .stdin(Stdio::null()),
        None,
        PROBE_TIMEOUT,
    )
    // The credentials file is the fallback, whatever stopped the Keychain read.
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let secret = String::from_utf8(output.stdout).ok()?;
    let trimmed = secret.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(not(target_os = "macos"))]
fn read_keychain_secret(_service: &str) -> Option<String> {
    None
}

/// Calls `/usr/bin/curl` with URL and headers supplied over stdin (`--config -`) so bearer
/// tokens never appear in process arguments. Returns `(http_status, body)`.
fn curl_json(
    method: &str,
    url: &str,
    headers: &[String],
    body: Option<&str>,
) -> std::result::Result<(u16, String), String> {
    const STATUS_MARK: &str = "\n__POSTIL_HTTP_STATUS__:";
    let mut config = String::new();
    config.push_str(&format!("url = {}\n", curl_quote(url)));
    config.push_str(&format!("request = {}\n", curl_quote(method)));
    for header in headers {
        config.push_str(&format!("header = {}\n", curl_quote(header)));
    }
    if let Some(body) = body {
        config.push_str(&format!("data-raw = {}\n", curl_quote(body)));
    }

    // One second under the hard kill, so curl normally stops itself and exits with code 28
    // ("the provider did not answer in time") before `run_timed` kills it.
    let curl_max_time = PROBE_TIMEOUT
        .saturating_sub(Duration::from_secs(1))
        .as_secs()
        .to_string();
    let output = run_timed(
        Command::new("/usr/bin/curl")
            .args([
                // curl honours `--disable` only as its first argument. It skips the user's
                // `~/.curlrc`, which could log the Authorization header to a file
                // (`--trace-ascii`) or change the reply's shape (`--include`).
                "--disable",
                "--silent",
                "--max-time",
                &curl_max_time,
                "--write-out",
                &format!("{STATUS_MARK}%{{http_code}}"),
                "--config",
                "-",
            ])
            .stdin(Stdio::piped()),
        Some(config.as_bytes()),
        PROBE_TIMEOUT,
    )?;
    // `run_timed` discards curl's stderr, which can echo request details; the page gets a
    // fixed message per failure instead.
    if !output.status.success() {
        return Err(curl_failure(output.status.code()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some((response_body, status_text)) = stdout.rsplit_once(STATUS_MARK) else {
        return Err("curl gave no HTTP status for the quota request".to_owned());
    };
    let status = status_text
        .trim()
        .parse::<u16>()
        .map_err(|_| "curl gave no HTTP status for the quota request".to_owned())?;
    Ok((status, response_body.to_owned()))
}

/// A fixed message for curl's exit `code` (`None` when a signal stopped it). The codes are
/// the ones `man curl` lists under EXIT CODES.
fn curl_failure(code: Option<i32>) -> String {
    match code {
        Some(6) => "cannot resolve the provider's host: check the network".to_owned(),
        Some(7) => "cannot connect to the provider: check the network".to_owned(),
        Some(28) => "the provider did not answer in time".to_owned(),
        Some(35 | 60) => "secure connection to the provider failed".to_owned(),
        Some(code) => format!("quota request failed (curl exit code {code})"),
        None => "quota request was stopped by a signal".to_owned(),
    }
}

/// `value` as one double-quoted curl config value: `\` and `"` are escaped, and a line break
/// becomes a space, so a token cannot end the value or start a new option line.
fn curl_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' => out.push(' '),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Runs `cmd` in its own process group with optional stdin bytes and a hard `timeout`,
/// killing the entire process group if the timeout elapses. The error says which step
/// failed: starting the program, writing its stdin, or waiting for it.
fn run_timed(
    cmd: &mut Command,
    stdin_bytes: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .process_group(0)
        .stdout(Stdio::piped())
        // Discarded: no caller shows it, and curl's can echo request details.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start {program}: {error}"))?;
    let pid = child.id();
    if let (Some(bytes), Some(mut stdin)) = (stdin_bytes, child.stdin.take())
        && let Err(error) = stdin.write_all(bytes)
    {
        agents::kill_group(pid);
        drop(child.wait());
        return Err(format!("cannot write to {program}: {error}"));
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let waiter = std::thread::Builder::new()
        .name("quota-wait".to_owned())
        .spawn(move || drop(sender.send(child.wait_with_output())));
    if let Err(error) = waiter {
        agents::kill_group(pid);
        return Err(format!(
            "cannot start a thread to wait for {program}: {error}"
        ));
    }
    let Ok(output) = receiver.recv_timeout(timeout) else {
        agents::kill_group(pid);
        return Err(format!("{program} timed out after {timeout:?}"));
    };
    output.map_err(|error| format!("cannot read from {program}: {error}"))
}

/// Most characters of provider-written text a quota message keeps. Arbitrary: about two
/// lines of the quota card, enough for an error sentence, and it bounds what the cache holds
/// and the page shows when a provider sends a long reply.
const MAX_PROVIDER_TEXT_CHARS: usize = 160;

/// `text` cut to [`MAX_PROVIDER_TEXT_CHARS`] characters, ending in "…" when it was cut.
fn bounded_provider_text(text: &str) -> String {
    let mut chars = text.chars();
    let kept: String = chars.by_ref().take(MAX_PROVIDER_TEXT_CHARS).collect();
    if chars.next().is_some() {
        format!("{kept}…")
    } else {
        kept
    }
}

fn clamp_percent(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    (value.clamp(0.0, 100.0) * 10.0).round() / 10.0
}

fn title_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(chars).collect(),
    }
}

/// Parses an RFC 3339 timestamp (`YYYY-MM-DDTHH:MM:SS(.frac)?(Z|[+-]HH:MM)`) into Unix
/// seconds. Returns `None` on invalid input or dates before 1970-01-01.
fn parse_rfc3339_secs(input: &str) -> Option<u64> {
    let bytes = input.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b't')
    {
        return None;
    }
    let year: i64 = input.get(0..4)?.parse().ok()?;
    let month: u32 = input.get(5..7)?.parse().ok()?;
    let day: u32 = input.get(8..10)?.parse().ok()?;
    let hour: u32 = input.get(11..13)?.parse().ok()?;
    let minute: u32 = input.get(14..16)?.parse().ok()?;
    let second: u32 = input.get(17..19)?.parse().ok()?;
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut rest = input.get(19..)?;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits = after_dot.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &after_dot[digits..];
    }
    let offset_secs: i64 = if rest == "Z" || rest == "z" {
        0
    } else if rest.len() == 6 && rest.as_bytes()[3] == b':' {
        let sign: i64 = match rest.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let off_h: i64 = rest.get(1..3)?.parse().ok()?;
        let off_m: i64 = rest.get(4..6)?.parse().ok()?;
        if off_h > 23 || off_m > 59 {
            return None;
        }
        sign * (off_h * 3600 + off_m * 60)
    } else {
        return None;
    };

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second)
        - offset_secs;
    u64::try_from(secs).ok()
}

/// The number of days in `month` (1 to 12) of `year` in the Gregorian calendar.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 for a Gregorian civil date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = i64::from(month) + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_parses_utc_fractional_and_offsets() {
        assert_eq!(parse_rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_secs("2026-11-25T12:30:45Z"),
            Some(1_795_609_845)
        );
        assert_eq!(
            parse_rfc3339_secs("2026-11-25T14:30:45.123456+02:00"),
            Some(1_795_609_845)
        );
        assert_eq!(parse_rfc3339_secs("not-a-timestamp"), None);
    }

    /// Formats Unix seconds `secs`, shown at UTC offset `offset_mins`, as RFC 3339. Finds the
    /// date by searching with `days_from_civil`, so it shares no calendar code with the
    /// parser's `days_in_month`.
    fn format_rfc3339(secs: i64, offset_mins: i64, fraction: bool) -> String {
        let local = secs + offset_mins * 60;
        let days = local.div_euclid(86_400);
        let time = local.rem_euclid(86_400);
        let mut year = 1969;
        while days_from_civil(year + 1, 1, 1) <= days {
            year += 1;
        }
        let mut month = 1;
        while month < 12 && days_from_civil(year, month + 1, 1) <= days {
            month += 1;
        }
        let day = days - days_from_civil(year, month, 1) + 1;
        let (hour, minute, second) = (time / 3600, time / 60 % 60, time % 60);
        let sign = if offset_mins < 0 { '-' } else { '+' };
        let (off_h, off_m) = (offset_mins.abs() / 60, offset_mins.abs() % 60);
        let frac = if fraction { ".250" } else { "" };
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{frac}\
             {sign}{off_h:02}:{off_m:02}"
        )
    }

    #[test]
    fn rfc3339_round_trips_a_spread_of_timestamps() {
        // A fixed linear congruential generator (Knuth's MMIX constants), so the spread is
        // the same on every run.
        let mut state: u64 = 0x5eed;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 33
        };
        // 2100-01-01T00:00:00Z: past any reset time a provider reports today.
        let end = 4_102_444_800_u64;
        for case in 0..2_000 {
            let secs = i64::try_from(next() % end).unwrap();
            let offset_mins = i64::try_from(next() % (2 * 14 * 60 + 1)).unwrap() - 14 * 60;
            let offset_mins = if secs + offset_mins * 60 < 0 {
                0
            } else {
                offset_mins
            };
            let text = format_rfc3339(secs, offset_mins, case % 2 == 0);
            assert_eq!(
                parse_rfc3339_secs(&text),
                Some(u64::try_from(secs).unwrap()),
                "{text}"
            );
        }
    }

    #[test]
    fn rfc3339_rejects_a_day_past_the_end_of_its_month() {
        assert_eq!(parse_rfc3339_secs("2026-02-31T00:00:00Z"), None);
        // Every month end from 1970 to 2100, so leap years (2000, 2028) and century
        // non-leap years (2100) are covered.
        for year in 1970..2101 {
            for month in 1..=12 {
                let (next_year, next_month) = if month == 12 {
                    (year + 1, 1)
                } else {
                    (year, month + 1)
                };
                let length =
                    days_from_civil(next_year, next_month, 1) - days_from_civil(year, month, 1);
                let last = format!("{year:04}-{month:02}-{length:02}T00:00:00Z");
                let past = format!("{year:04}-{month:02}-{:02}T00:00:00Z", length + 1);
                assert!(parse_rfc3339_secs(&last).is_some(), "{last}");
                assert_eq!(parse_rfc3339_secs(&past), None, "{past}");
            }
        }
    }

    #[test]
    fn parses_claude_usage_payload_with_model_windows_and_extra_usage() {
        let body = r#"{
            "five_hour": { "utilization": 62.4, "resets_at": "2026-11-25T15:00:00Z" },
            "seven_day": { "utilization": 31.0, "resets_at": "2026-11-30T00:00:00Z" },
            "seven_day_opus": { "utilization": 14.5, "resets_at": "2026-11-30T00:00:00Z" },
            "extra_usage": { "is_enabled": true, "used_credits": 420, "monthly_limit": 5000 }
        }"#;
        let report = parse_claude_quota(body, Some("Max".to_owned()));
        assert_eq!(report.status, QuotaStatus::Ok);
        assert_eq!(report.plan.as_deref(), Some("Max"));
        assert_eq!(report.windows.len(), 3);
        assert_eq!(report.windows[0].label, "5-hour session");
        assert!((report.windows[0].used_percent - 62.4).abs() < 1e-6);
        assert_eq!(report.windows[2].model.as_deref(), Some("opus"));
        let credits = report.credits.unwrap();
        assert!((credits.used.unwrap() - 4.20).abs() < 1e-6);
        assert!((credits.limit.unwrap() - 50.0).abs() < 1e-6);
    }

    #[test]
    fn parses_codex_app_server_rate_limits() {
        let account = serde_json::json!({
            "id": 2,
            "result": { "authMode": "chatgpt", "planType": "plus" }
        });
        let limits = serde_json::json!({
            "id": 3,
            "result": {
                "rateLimits": {
                    "primary": {
                        "usedPercent": 21.0, "windowDurationMins": 300, "resetsAt": 1_795_620_000
                    },
                    "secondary": {
                        "usedPercent": 48.0, "windowDurationMins": 10080, "resetsAt": 1_796_000_000
                    },
                    "credits": { "hasCredits": true, "unlimited": false, "balance": "12.50" }
                }
            }
        });
        let report = parse_codex_quota(Some(&account), Some(&limits));
        assert_eq!(report.status, QuotaStatus::Ok);
        assert_eq!(report.plan.as_deref(), Some("Plus"));
        assert_eq!(report.windows.len(), 2);
        assert_eq!(report.windows[0].label, "5-hour window");
        assert_eq!(report.windows[1].label, "7-day window");
        assert!((report.credits.unwrap().remaining.unwrap() - 12.5).abs() < 1e-6);
    }

    /// A probe whose first call waits until `release` is sent to it, and that reports which
    /// call each result came from.
    fn gated_poller() -> (QuotaPoller, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (release, gate) = mpsc::channel::<()>();
        let (entered, started) = mpsc::channel::<()>();
        let gate = Mutex::new(Some((gate, entered)));
        let calls = Mutex::new(0_u32);
        let poller = QuotaPoller::new(move |_, _| {
            let call = {
                let mut calls = lock(&calls);
                *calls += 1;
                *calls
            };
            let first = lock(&gate).take();
            if let Some((gate, entered)) = first {
                entered.send(()).unwrap();
                gate.recv().unwrap();
            }
            QuotaReport::unavailable(format!("call {call}"))
        });
        (poller, started, release)
    }

    /// Claude's report from one `query` of `poller`.
    fn claude(poller: &QuotaPoller, fetch: QuotaFetch) -> QuotaReport {
        let mut reports = poller.query(&["Claude"], OsStr::new(""), fetch);
        reports
            .remove("Claude")
            .expect("query answers for every name")
    }

    #[test]
    fn poller_drops_the_result_of_a_probe_that_a_newer_one_replaced() {
        let (poller, started, release) = gated_poller();
        let poller = poller.with_loading_timeout(Duration::ZERO);

        assert_eq!(
            claude(&poller, QuotaFetch::Cached).status,
            QuotaStatus::Loading
        );
        started.recv().unwrap();
        // The first probe is still in flight, and a zero loading timeout lets this poll
        // start a second one.
        let _ = claude(&poller, QuotaFetch::Cached);
        let mut probes = poller.take_probes();
        assert_eq!(probes.len(), 2);
        probes.pop().unwrap().join().unwrap();

        release.send(()).unwrap();
        probes.pop().unwrap().join().unwrap();
        assert_eq!(
            claude(&poller, QuotaFetch::Cached).message.as_deref(),
            Some("call 2"),
            "the replaced first probe finished last and must not overwrite the second"
        );
    }

    #[test]
    fn refresh_keeps_a_probe_in_flight_and_reprobes_a_finished_entry() {
        let (poller, started, release) = gated_poller();

        let _ = claude(&poller, QuotaFetch::Cached);
        started.recv().unwrap();
        let during = claude(&poller, QuotaFetch::Refresh);
        assert_eq!(during.status, QuotaStatus::Loading);
        release.send(()).unwrap();
        poller.join_probes();
        let first = claude(&poller, QuotaFetch::Cached);
        assert_eq!(first.message.as_deref(), Some("call 1"));

        let after = claude(&poller, QuotaFetch::Refresh);
        assert_eq!(after.status, QuotaStatus::Loading);
        poller.join_probes();
        let second = claude(&poller, QuotaFetch::Cached);
        assert_eq!(second.message.as_deref(), Some("call 2"));
    }

    #[test]
    fn parse_claude_credentials_decodes_hex_keychain_output() {
        use std::fmt::Write as _;

        let json = r#"{"claudeAiOauth":{"accessToken":"sk-ant-test","subscriptionType":"max"}}"#;
        let mut hex = String::with_capacity(json.len() * 2);
        for byte in json.bytes() {
            write!(hex, "{byte:02x}").unwrap();
        }
        let parsed = parse_claude_credentials(&hex).expect("hex-encoded keychain JSON parses");
        assert_eq!(parsed.0, "sk-ant-test");
        assert_eq!(parsed.1.as_deref(), Some("Max"));
    }

    #[test]
    fn hex_decoding_takes_either_case_and_refuses_partial_input() {
        assert_eq!(decode_hex_utf8("4a6b").as_deref(), Some("Jk"));
        assert_eq!(decode_hex_utf8("0x4A6B").as_deref(), Some("Jk"));
        assert_eq!(decode_hex_utf8("7E").as_deref(), Some("~"));
        assert_eq!(decode_hex_utf8(""), None);
        assert_eq!(decode_hex_utf8("4"), None);
        assert_eq!(decode_hex_utf8("4a6"), None);
        assert_eq!(decode_hex_utf8("4g"), None);
        assert_eq!(decode_hex_utf8("ff"), None, "not UTF-8");
    }

    #[test]
    fn rfc3339_refuses_each_malformed_field_and_takes_each_boundary() {
        for valid in [
            "2026-11-25t12:30:45z",
            "2026-12-31T23:59:60Z",
            "2026-11-25T12:30:45+23:59",
            "2026-11-25T12:30:45-00:30",
        ] {
            assert!(parse_rfc3339_secs(valid).is_some(), "{valid}");
        }
        for invalid in [
            "2026-11-25T12:30:4Z",
            "2026/11-25T12:30:45Z",
            "2026-11/25T12:30:45Z",
            "2026-11-25 12:30:45Z",
            "2026-11-25T12-30:45Z",
            "2026-11-25T12:30-45Z",
            "2026-00-25T12:30:45Z",
            "2026-13-25T12:30:45Z",
            "2026-11-00T12:30:45Z",
            "2026-11-25T24:30:45Z",
            "2026-11-25T12:60:45Z",
            "2026-11-25T12:30:61Z",
            "2026-11-25T12:30:45.Z",
            "2026-11-25T12:30:45",
            "2026-11-25T12:30:45ZZ",
            "2026-11-25T12:30:45*02:00",
            "2026-11-25T12:30:45+02-00",
            "2026-11-25T12:30:45+24:00",
            "2026-11-25T12:30:45+02:60",
        ] {
            assert_eq!(parse_rfc3339_secs(invalid), None, "{invalid}");
        }
        assert_eq!(
            parse_rfc3339_secs("1970-01-01T00:00:01+00:01"),
            None,
            "before 1970 in UTC"
        );
    }

    #[test]
    fn claude_extra_usage_alone_is_a_quota_with_what_remains() {
        let extra = |enabled: bool| {
            serde_json::json!({
                "extra_usage": { "is_enabled": enabled, "used_credits": 420, "monthly_limit": 5000 }
            })
            .to_string()
        };
        let report = parse_claude_quota(&extra(true), None);
        assert_eq!(report.status, QuotaStatus::Ok);
        assert_eq!(report.windows.len(), 0);
        let credits = report.credits.unwrap();
        assert!((credits.remaining.unwrap() - 45.8).abs() < 1e-6);
        assert!(!credits.unlimited);

        let report = parse_claude_quota(&extra(false), None);
        assert_eq!(report.status, QuotaStatus::Unavailable);
    }

    #[test]
    fn codex_names_each_window_by_its_length() {
        let window =
            |mins: u64| serde_json::json!({ "usedPercent": 1.0, "windowDurationMins": mins });
        let labels = |bucket: Value| {
            let mut out = Vec::new();
            collect_codex_bucket(&bucket, None, &mut out);
            out.into_iter().map(|win| win.label).collect::<Vec<_>>()
        };
        assert_eq!(
            labels(serde_json::json!({ "primary": window(45), "secondary": window(2880) })),
            ["45-min window", "2-day window"]
        );
        assert_eq!(
            labels(serde_json::json!({ "primary": window(90), "secondary": window(0) })),
            ["1-hour window", "7-day window"]
        );
    }

    #[test]
    fn codex_lists_per_model_buckets_but_not_a_second_copy_of_the_main_one() {
        let bucket = |used: f64| serde_json::json!({ "primary": { "usedPercent": used } });
        let by_id = serde_json::json!({
            "codex": bucket(10.0),
            "gpt-5": { "limitName": "GPT-5", "primary": { "usedPercent": 20.0 } },
            "o3": { "limitName": "", "primary": { "usedPercent": 30.0 } },
        });
        let labels = |result: Value| {
            let limits = serde_json::json!({ "result": result });
            parse_codex_quota(None, Some(&limits))
                .windows
                .into_iter()
                .map(|win| win.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(serde_json::json!({ "rateLimits": bucket(10.0), "rateLimitsByLimitId": by_id })),
            [
                "5-hour window",
                "GPT-5 (5-hour window)",
                "o3 (5-hour window)"
            ]
        );
        assert_eq!(
            labels(serde_json::json!({ "rateLimitsByLimitId": by_id })),
            [
                "codex (5-hour window)",
                "GPT-5 (5-hour window)",
                "o3 (5-hour window)"
            ]
        );
    }

    #[test]
    fn codex_credits_show_when_held_or_unlimited() {
        let credits = |has: bool, unlimited: bool| {
            parse_codex_credits(&serde_json::json!({
                "credits": { "hasCredits": has, "unlimited": unlimited }
            }))
        };
        assert!(credits(false, false).is_none());
        assert!(credits(true, false).is_some_and(|info| !info.unlimited));
        assert!(credits(false, true).is_some_and(|info| info.unlimited));
    }

    #[test]
    fn probe_codex_talks_json_rpc_over_stdio_to_app_server_script() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::testing::TempDir::new("codex-app-server");
        let script = dir.path().join("codex");
        fs::write(
            &script,
            concat!(
                "#!/bin/sh\n",
                "saw_initialized=0\n",
                "while IFS= read -r line; do\n",
                "  case \"$line\" in\n",
                "    *'\"method\":\"initialize\"'*)\n",
                "      printf '{\"jsonrpc\":\"2.0\",\"id\":1,",
                "\"result\":{\"userAgent\":\"test\"}}\\n'\n",
                "      ;;\n",
                "    *'\"method\":\"initialized\"'*)\n",
                "      saw_initialized=1\n",
                "      ;;\n",
                "    *'\"method\":\"account/read\"'*)\n",
                "      printf '{\"jsonrpc\":\"2.0\",\"id\":2,",
                "\"result\":{\"authMode\":\"chatgpt\",\"planType\":\"plus\"}}\\n'\n",
                "      ;;\n",
                "    *'\"method\":\"account/rateLimits/read\"'*)\n",
                "      if [ \"$saw_initialized\" = \"1\" ]; then\n",
                "        printf '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"rateLimits\":",
                "{\"primary\":{\"usedPercent\":33.0,\"windowDurationMins\":300,",
                "\"resetsAt\":1800000000}}}}\\n'\n",
                "      fi\n",
                "      ;;\n",
                "  esac\n",
                "done\n",
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let report = probe_codex(dir.path().as_os_str());
        assert_eq!(report.status, QuotaStatus::Ok, "{report:?}");
        assert_eq!(report.plan.as_deref(), Some("Plus"));
        assert_eq!(report.windows.len(), 1);
        assert!((report.windows[0].used_percent - 33.0).abs() < 1e-6);
    }

    #[test]
    fn probe_codex_reports_the_error_initialize_returned() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::testing::TempDir::new("codex-init-error");
        let script = dir.path().join("codex");
        fs::write(
            &script,
            concat!(
                "#!/bin/sh\n",
                "read -r line\n",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"message\":\"too old\"}}\\n'\n",
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let report = probe_codex(dir.path().as_os_str());
        assert_eq!(report.status, QuotaStatus::Error);
        assert_eq!(report.message.as_deref(), Some("too old"));
    }

    #[test]
    fn a_failed_curl_request_reports_a_fixed_message_not_curl_output() {
        // Nothing listens on port 1, so curl exits with code 7 and an error on stderr.
        let error = curl_json("GET", "http://127.0.0.1:1/", &[], None).unwrap_err();
        assert_eq!(error, "cannot connect to the provider: check the network");
    }

    #[test]
    fn codex_error_text_is_cut_to_the_bound() {
        let at_bound = "é".repeat(MAX_PROVIDER_TEXT_CHARS);
        let over = format!("{at_bound}x");
        let limits = serde_json::json!({ "id": 3, "error": { "message": over } });
        let report = parse_codex_quota(None, Some(&limits));
        assert_eq!(report.message, Some(format!("{at_bound}…")));

        let limits = serde_json::json!({ "id": 3, "error": { "message": at_bound } });
        let report = parse_codex_quota(None, Some(&limits));
        assert_eq!(report.message.as_deref(), Some(at_bound.as_str()));
    }

    #[test]
    fn curl_quote_keeps_a_hostile_token_inside_one_config_value() {
        let token = "a\"b\\c\nurl = \"https://evil.example\"\r";
        let quoted = curl_quote(token);
        assert_eq!(quoted, r#""a\"b\\c url = \"https://evil.example\" ""#);
        assert!(
            !quoted.contains(['\n', '\r']),
            "a newline would start a new option"
        );
    }

    #[test]
    fn run_timed_says_whether_the_program_did_not_start_or_timed_out() {
        let missing = run_timed(
            &mut Command::new("/nonexistent/postil-test-program"),
            None,
            PROBE_TIMEOUT,
        )
        .unwrap_err();
        assert!(
            missing.starts_with("cannot start /nonexistent/"),
            "{missing}"
        );

        let slow = run_timed(
            Command::new("/bin/sh").args(["-c", "sleep 5"]),
            None,
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert_eq!(slow, "/bin/sh timed out after 50ms");
    }

    #[test]
    fn a_missing_credentials_file_is_not_signed_in_but_an_unreadable_one_is_an_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::testing::TempDir::new("quota-creds");
        assert_eq!(
            read_credentials_file(&dir.path().join("absent.json")),
            Ok(None)
        );
        let unreadable = dir.path().join("unreadable.json");
        fs::write(&unreadable, "{}").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let error = read_credentials_file(&unreadable).unwrap_err();
        assert!(error.starts_with("cannot read "), "{error}");
        // A path through a file fails before the file is opened, and is not "missing" either.
        let through_a_file = unreadable.join("credentials.json");
        let error = read_credentials_file(&through_a_file).unwrap_err();
        assert!(error.starts_with("cannot read "), "{error}");
    }

    #[test]
    fn credentials_files_are_read_only_when_regular_and_small() {
        let dir = crate::testing::TempDir::new("quota-creds-kinds");
        let small = dir.path().join("small.json");
        fs::write(&small, r#"{"access_token":"tok"}"#).unwrap();
        assert_eq!(
            read_credentials_file(&small),
            Ok(Some(r#"{"access_token":"tok"}"#.to_owned()))
        );

        let limit = usize::try_from(MAX_CREDENTIALS_BYTES).unwrap();
        let at_limit = dir.path().join("at-limit.json");
        fs::write(&at_limit, vec![b' '; limit]).unwrap();
        assert!(read_credentials_file(&at_limit).is_ok_and(|raw| raw.unwrap().len() == limit));
        let huge = dir.path().join("huge.json");
        fs::write(&huge, vec![b' '; limit + 1]).unwrap();
        let error = read_credentials_file(&huge).unwrap_err();
        assert!(error.ends_with("is larger than 65536 bytes"), "{error}");

        // Opening a FIFO with no writer would block forever, so this test hangs if the kind
        // check is missing.
        let fifo = dir.path().join("fifo.json");
        let made = Command::new("/usr/bin/mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        let error = read_credentials_file(&fifo).unwrap_err();
        assert!(error.ends_with("is not a regular file"), "{error}");
        let error = read_credentials_file(dir.path()).unwrap_err();
        assert!(error.ends_with("is not a regular file"), "{error}");
    }

    #[test]
    fn parses_grok_weekly_quota_credits_and_missing_percent() {
        let weekly = serde_json::json!({
            "id": 2,
            "error": null,
            "result": {
                "config": {
                    "creditUsagePercent": 12.0,
                    "currentPeriod": {
                        "type": "USAGE_PERIOD_TYPE_WEEKLY",
                        "start": "2026-11-24T08:37:24.596441+00:00",
                        "end": "2026-12-01T08:37:24.596441+00:00"
                    },
                    "onDemandCap": { "val": 0 },
                    "onDemandUsed": { "val": 0 },
                    "prepaidBalance": { "val": 0 },
                    "isUnifiedBillingUser": true
                },
                "subscription_tier": "SuperGrok Heavy"
            }
        });
        let report = parse_grok_quota(Some(&weekly), false);
        assert_eq!(report.status, QuotaStatus::Ok);
        assert_eq!(report.plan.as_deref(), Some("SuperGrok Heavy"));
        assert_eq!(report.windows.len(), 1);
        assert_eq!(report.windows[0].label, "Weekly window");
        assert!((report.windows[0].used_percent - 12.0).abs() < 1e-6);
        assert!(report.windows[0].resets_at.is_some());
        assert!(report.credits.is_none());

        // On-demand budget cap and used cents convert to USD; omitting creditUsagePercent does
        // not fabricate a 0% window.
        let on_demand = serde_json::json!({
            "id": 2,
            "result": {
                "config": {
                    "on_demand_cap": { "val": 5000 },
                    "on_demand_used": { "val": 1250 },
                    "prepaid_balance": { "val": 900 }
                },
                "subscription_tier": null,
                "subscriptionTier": "  free\ntier  "
            }
        });
        let report = parse_grok_quota(Some(&on_demand), false);
        assert_eq!(report.status, QuotaStatus::Ok);
        assert_eq!(report.plan.as_deref(), Some("Free tier"));
        assert_eq!(report.windows.len(), 0);
        let credits = report.credits.unwrap();
        assert_eq!(credits.label, "On-demand");
        assert_eq!(credits.currency.as_deref(), Some("USD"));
        assert!(!credits.unlimited);
        assert!((credits.used.unwrap() - 12.5).abs() < 1e-6);
        assert!((credits.limit.unwrap() - 50.0).abs() < 1e-6);
        assert!((credits.remaining.unwrap() - 37.5).abs() < 1e-6);

        // Over-limit on-demand spend clamps remaining to 0; prepaid credits parse trimmed strings.
        let over = serde_json::json!({
            "result": { "config": { "onDemandCap": { "val": 1000 }, "onDemandUsed": { "val": 1500 } } }
        });
        let over_credits = parse_grok_quota(Some(&over), false).credits.unwrap();
        assert!((over_credits.remaining.unwrap()).abs() < 1e-6);

        let prepaid = serde_json::json!({
            "result": { "config": { "prepaidBalance": { "val": " 2500 " } } }
        });
        let prepaid_credits = parse_grok_quota(Some(&prepaid), false).credits.unwrap();
        assert_eq!(prepaid_credits.label, "Prepaid credits");
        assert_eq!(prepaid_credits.currency.as_deref(), Some("USD"));
        assert!(!prepaid_credits.unlimited);
        assert!((prepaid_credits.remaining.unwrap() - 25.0).abs() < 1e-6);
    }

    #[test]
    fn grok_quota_handles_fallbacks_errors_and_api_key_modes() {
        assert_eq!(
            parse_grok_quota(None, false).message.as_deref(),
            Some("Grok did not return billing data")
        );
        assert_eq!(
            parse_grok_quota(Some(&serde_json::json!({ "id": 2, "result": null })), false)
                .message
                .as_deref(),
            Some("Grok did not return billing data")
        );

        let unsupported = serde_json::json!({
            "id": 2,
            "error": { "code": METHOD_NOT_FOUND, "message": "Method not found" }
        });
        assert!(
            parse_grok_quota(Some(&unsupported), false)
                .message
                .unwrap()
                .contains("grok update")
        );

        let blank_err =
            serde_json::json!({ "id": 2, "error": { "code": -32000, "message": "  " } });
        assert_eq!(
            parse_grok_quota(Some(&blank_err), false).message.as_deref(),
            Some("Not signed in — run `grok login` in a terminal")
        );
        let long_msg = "x".repeat(MAX_PROVIDER_TEXT_CHARS + 5);
        let auth_err =
            serde_json::json!({ "id": 2, "error": { "code": -32000, "message": long_msg } });
        assert_eq!(
            parse_grok_quota(Some(&auth_err), false)
                .message
                .unwrap()
                .chars()
                .count(),
            MAX_PROVIDER_TEXT_CHARS + 1
        );

        // Null primary fields fall back to snake_case / billingPeriodEnd; percent is clamped;
        // overlong plan names are bounded.
        let fallback = serde_json::json!({
            "result": {
                "subscription_tier": "p".repeat(MAX_PROVIDER_TEXT_CHARS + 10),
                "config": {
                    "creditUsagePercent": null,
                    "credit_usage_percent": 140.0,
                    "currentPeriod": null,
                    "current_period": { "type": "USAGE_PERIOD_TYPE_MONTHLY", "end": null },
                    "billingPeriodEnd": "2026-12-31T00:00:00Z"
                }
            }
        });
        let report = parse_grok_quota(Some(&fallback), false);
        assert_eq!(report.windows[0].label, "Monthly window");
        assert!((report.windows[0].used_percent - 100.0).abs() < 1e-6);
        assert!(report.windows[0].resets_at.is_some());
        assert_eq!(
            report.plan.unwrap().chars().count(),
            MAX_PROVIDER_TEXT_CHARS + 1
        );
        assert_eq!(grok_period_label(Some("OTHER")), "Usage window");

        // Empty windows/credits matrix over (plan, has_api_key).
        let empty_no_plan = serde_json::json!({ "result": { "config": {} } });
        assert_eq!(
            parse_grok_quota(Some(&empty_no_plan), false)
                .message
                .as_deref(),
            Some("Grok reported no active quota windows")
        );
        assert_eq!(
            parse_grok_quota(Some(&empty_no_plan), true)
                .message
                .as_deref(),
            Some("API key mode — no subscription quota window")
        );
        let empty_with_plan = serde_json::json!({
            "result": { "subscription_tier": "SuperGrok", "config": {} }
        });
        let with_plan = parse_grok_quota(Some(&empty_with_plan), true);
        assert_eq!(with_plan.plan.as_deref(), Some("SuperGrok"));
        assert_eq!(
            with_plan.message.as_deref(),
            Some("Grok reported no active quota windows")
        );
    }

    #[test]
    fn cents_usd_and_probe_agent_boundaries() {
        assert_eq!(cents_usd(None), None);
        assert_eq!(cents_usd(Some(&serde_json::json!({}))), None);
        assert_eq!(cents_usd(Some(&serde_json::json!({ "val": 0 }))), Some(0.0));
        assert_eq!(cents_usd(Some(&serde_json::json!({ "val": -1 }))), None);
        assert_eq!(cents_usd(Some(&serde_json::json!({ "val": "NaN" }))), None);
        assert_eq!(cents_usd(Some(&serde_json::json!({ "val": "inf" }))), None);

        let empty_dir = crate::testing::TempDir::new("quota-empty-path");
        let empty = empty_dir.path().as_os_str();
        assert_eq!(
            probe_agent("Grok", empty).message.as_deref(),
            Some("Grok CLI not found on PATH")
        );
        assert!(
            probe_agent("Gemini", empty)
                .message
                .unwrap()
                .contains("agy")
        );
        assert!(
            probe_agent("OpenCode", empty)
                .message
                .unwrap()
                .contains("OpenCode")
        );
        assert_eq!(
            probe_agent("Other", empty).message.as_deref(),
            Some("No provider quota source")
        );
    }

    #[test]
    fn probe_grok_talks_json_rpc_over_stdio_and_skips_noise_and_requests() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::testing::TempDir::new("grok-agent-stdio");
        let script = dir.path().join("grok");
        fs::write(
            &script,
            concat!(
                "#!/bin/sh\n",
                "if [ \"$1\" != \"agent\" ] || [ \"$2\" != \"stdio\" ]; then exit 2; fi\n",
                "case \"$PWD\" in *postil-quota) ;; *) exit 3 ;; esac\n",
                "while IFS= read -r line; do\n",
                "  case \"$line\" in\n",
                "    *'\"method\":\"initialize\"'*)\n",
                "      printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1},\"error\":null}\\n'\n",
                "      printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"fs/read_text_file\",\"params\":{}}\\n'\n",
                "      printf '%020000d\\n' 0\n",
                "      ;;\n",
                "    *'\"method\":\"_x.ai/billing\"'*)\n",
                "      printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"config\":",
                "{\"creditUsagePercent\":27.5,\"currentPeriod\":{\"type\":\"USAGE_PERIOD_TYPE_DAILY\",",
                "\"end\":\"2026-12-01T00:00:00Z\"}},\"subscription_tier\":\"SuperGrok\"}}\\n'\n",
                "      ;;\n",
                "  esac\n",
                "done\n",
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let report = probe_grok(dir.path().as_os_str());
        assert_eq!(report.status, QuotaStatus::Ok, "{report:?}");
        assert_eq!(report.plan.as_deref(), Some("SuperGrok"));
        assert_eq!(report.windows.len(), 1);
        assert_eq!(report.windows[0].label, "Daily window");
        assert!((report.windows[0].used_percent - 27.5).abs() < 1e-6);
    }

    #[test]
    fn probe_grok_handles_init_refusal_and_fast_exit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::testing::TempDir::new("grok-init-err");
        let script = dir.path().join("grok");
        fs::write(
            &script,
            "#!/bin/sh\nread -r _\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"message\":\"   \"}}\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let report = probe_grok(dir.path().as_os_str());
        assert_eq!(report.status, QuotaStatus::Error);
        assert_eq!(
            report.message.as_deref(),
            Some("grok agent stdio refused initialize")
        );

        // Immediate exit closes stdin before or during write (`BrokenPipe` safe) -> Unavailable.
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let fast = probe_grok(dir.path().as_os_str());
        assert_eq!(fast.status, QuotaStatus::Unavailable);
        assert_eq!(
            fast.message.as_deref(),
            Some("Grok did not return billing data")
        );
    }
}
