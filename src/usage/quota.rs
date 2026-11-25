//! On-demand background probes for live provider account quotas and rate-limit windows.
//!
//! Each probe runs on a short-lived thread with a hard timeout and kills its process group
//! when done, matching `agents::Scanner`. Results are cached briefly in memory so reopening
//! the Usage dialog does not spam provider endpoints, and nothing runs while the dialog is
//! closed. Secrets (OAuth tokens) are piped to `/usr/bin/curl` over stdin (`--config -`),
//! never passed in process arguments.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::agents;
use crate::usage::now_secs;

/// How long a fetched quota snapshot stays fresh before the next open re-probes it.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Hard limit on any single provider probe (subprocess or HTTP).
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Maximum time an in-flight `Loading` entry stays fresh before a subsequent poll may retry it.
const LOADING_TIMEOUT: Duration = Duration::from_secs(20);

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
#[derive(Debug, Clone, PartialEq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// Manages cached and in-flight provider quota probes across requests.
#[derive(Clone)]
pub(crate) struct QuotaPoller {
    probe: ProbeFn,
    state: Arc<Mutex<PollerState>>,
}

#[derive(Default)]
struct PollerState {
    generation: u64,
    entries: HashMap<String, CachedQuota>,
}

struct CachedQuota {
    report: QuotaReport,
    updated: Instant,
    generation: u64,
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
        }
    }

    /// Returns the latest quota report for each named agent, starting background probes for
    /// any agent whose cache entry is missing or stale.
    pub(crate) fn snapshot(
        &self,
        names: &[&str],
        search_path: &OsStr,
    ) -> HashMap<String, QuotaReport> {
        self.query(names, search_path, false)
    }

    /// Invalidates cached quota reports, bumps the generation so in-flight older probes are
    /// dropped, and starts fresh probes for all named agents.
    pub(crate) fn refresh(
        &self,
        names: &[&str],
        search_path: &OsStr,
    ) -> HashMap<String, QuotaReport> {
        self.query(names, search_path, true)
    }

    fn query(
        &self,
        names: &[&str],
        search_path: &OsStr,
        force: bool,
    ) -> HashMap<String, QuotaReport> {
        let mut to_spawn: Vec<(String, u64)> = Vec::new();
        let mut results = HashMap::with_capacity(names.len());
        {
            let mut state = lock(&self.state);
            if force {
                state.generation += 1;
                state.entries.clear();
            }
            let generation = state.generation;
            for &name in names {
                let fresh = state.entries.get(name).is_some_and(|entry| {
                    entry.generation == generation
                        && match entry.report.status {
                            QuotaStatus::Loading => entry.updated.elapsed() < LOADING_TIMEOUT,
                            QuotaStatus::Ok | QuotaStatus::Unavailable | QuotaStatus::Error => {
                                entry.updated.elapsed() < CACHE_TTL
                            }
                        }
                });
                if fresh {
                    results.insert(name.to_owned(), state.entries[name].report.clone());
                } else {
                    let loading = QuotaReport::loading();
                    state.entries.insert(
                        name.to_owned(),
                        CachedQuota {
                            report: loading.clone(),
                            updated: Instant::now(),
                            generation,
                        },
                    );
                    results.insert(name.to_owned(), loading);
                    to_spawn.push((name.to_owned(), generation));
                }
            }
        }

        let path = search_path.to_owned();
        for (name, generation) in to_spawn {
            let probe = Arc::clone(&self.probe);
            let shared = Arc::clone(&self.state);
            let path = path.clone();
            let agent_name = name.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("quota-{name}"))
                .spawn(move || {
                    let report = probe(&agent_name, &path);
                    let mut state = lock(&shared);
                    if state.generation == generation {
                        state.entries.insert(
                            agent_name,
                            CachedQuota {
                                report,
                                updated: Instant::now(),
                                generation,
                            },
                        );
                    }
                });
            if spawned.is_err() {
                let mut state = lock(&self.state);
                if state.generation == generation {
                    state.entries.insert(
                        name,
                        CachedQuota {
                            report: QuotaReport::error("could not start quota check thread"),
                            updated: Instant::now(),
                            generation,
                        },
                    );
                }
            }
        }

        results
    }
}

impl fmt::Debug for QuotaPoller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("QuotaPoller")
            .field("generation", &state.generation)
            .field("cached", &state.entries.len())
            .finish_non_exhaustive()
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
        "Gemini" => probe_gemini(),
        "Grok" => QuotaReport::unavailable("Live quota is not exposed by the Grok CLI yet"),
        "OpenCode" => QuotaReport::unavailable(
            "OpenCode routes across multiple providers — see Postil activity below",
        ),
        _ => QuotaReport::unavailable("No provider quota source"),
    }
}

// ---- Claude -------------------------------------------------------------------------------

fn probe_claude() -> QuotaReport {
    let creds = read_claude_credentials();
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

fn read_claude_credentials() -> Option<(String, Option<String>)> {
    if let Some(raw) = read_keychain_secret("Claude Code-credentials")
        && let Some(parsed) = parse_claude_credentials(&raw)
    {
        return Some(parsed);
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    let path = PathBuf::from(home).join(".claude/.credentials.json");
    let raw = fs::read_to_string(path).ok()?;
    parse_claude_credentials(&raw)
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
    if hex.len() < 2 || !hex.len().is_multiple_of(2) {
        return None;
    }
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
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

// ---- Codex --------------------------------------------------------------------------------

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

    let pid = child.id();
    let result = (|| {
        let mut stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let requests = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"postil","version":"0.1.0"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"account/read","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"account/rateLimits/read","params":{}}"#,
            "\n",
        );
        if stdin.write_all(requests.as_bytes()).is_err() || stdin.flush().is_err() {
            return None;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader = std::thread::Builder::new()
            .name("codex-quota-read".to_owned())
            .spawn(move || {
                let mut init_error = None;
                let mut account = None;
                let mut limits = None;
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    match msg.get("id").and_then(Value::as_u64) {
                        Some(1) if msg.get("error").is_some() => {
                            init_error = msg
                                .get("error")
                                .and_then(|err| err.get("message"))
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .or_else(|| Some("codex app-server refused initialize".to_owned()));
                            break;
                        }
                        Some(2) => account = Some(msg),
                        Some(3) => limits = Some(msg),
                        _ => {}
                    }
                    if account.is_some() && limits.is_some() {
                        break;
                    }
                }
                drop(sender.send((init_error, account, limits)));
            });
        reader.ok()?;
        receiver.recv_timeout(PROBE_TIMEOUT).ok()
    })();

    agents::kill_group(pid);
    drop(child.kill());
    drop(child.wait());

    let Some((init_error, account, limits)) = result else {
        return QuotaReport::error("Codex quota probe timed out");
    };
    if let Some(error) = init_error {
        return QuotaReport::error(error);
    }
    parse_codex_quota(account.as_ref(), limits.as_ref())
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
        return QuotaReport::unavailable(message);
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

// ---- Gemini -------------------------------------------------------------------------------

enum GeminiCreds {
    Token(String),
    Expired,
    Missing,
}

fn probe_gemini() -> QuotaReport {
    let token = match read_gemini_creds(now_secs()) {
        GeminiCreds::Token(token) => token,
        GeminiCreds::Expired => {
            return QuotaReport::unavailable(
                "Session expired — run `gemini` in a terminal to refresh",
            );
        }
        GeminiCreds::Missing => {
            if std::env::var_os("GEMINI_API_KEY").is_some_and(|val| !val.is_empty()) {
                return QuotaReport::unavailable("API key mode — no subscription quota window");
            }
            return QuotaReport::unavailable(
                "Not signed in — run `gemini` in a terminal to sign in",
            );
        }
    };

    let headers = [
        format!("Authorization: Bearer {token}"),
        "Content-Type: application/json".to_owned(),
        "Accept: application/json".to_owned(),
    ];
    let load_body = r#"{"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}"#;
    let (project, plan) = match curl_json(
        "POST",
        "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist",
        &headers,
        Some(load_body),
    ) {
        Ok((200, body)) => parse_gemini_load(&body),
        Ok((401 | 403, _)) => {
            return QuotaReport::unavailable(
                "Session expired or tier ineligible — run `gemini` in a terminal",
            );
        }
        Ok((status, _)) => {
            return QuotaReport::error(format!("Gemini loadCodeAssist returned HTTP {status}"));
        }
        Err(message) => return QuotaReport::error(message),
    };

    let quota_body = match project {
        Some(project) => serde_json::json!({ "project": project }).to_string(),
        None => "{}".to_owned(),
    };
    match curl_json(
        "POST",
        "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota",
        &headers,
        Some(&quota_body),
    ) {
        Ok((200, body)) => parse_gemini_quota(&body, plan),
        Ok((401 | 403, _)) => {
            QuotaReport::unavailable("Gemini quota unavailable for this account tier")
        }
        Ok((status, _)) => {
            QuotaReport::error(format!("Gemini retrieveUserQuota returned HTTP {status}"))
        }
        Err(message) => QuotaReport::error(message),
    }
}

fn read_gemini_creds(now: u64) -> GeminiCreds {
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return GeminiCreds::Missing;
    };
    let path = PathBuf::from(home).join(".gemini/oauth_creds.json");
    let Ok(raw) = fs::read_to_string(path) else {
        return GeminiCreds::Missing;
    };
    parse_gemini_creds(&raw, now)
}

fn parse_gemini_creds(raw: &str, now: u64) -> GeminiCreds {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return GeminiCreds::Missing;
    };
    let Some(token) = value
        .get("access_token")
        .or_else(|| value.get("accessToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
    else {
        return GeminiCreds::Missing;
    };
    if let Some(expiry) = value
        .get("expiry_date")
        .or_else(|| value.get("expiryDate"))
        .and_then(Value::as_u64)
    {
        // Google OAuth writes milliseconds since epoch; handle seconds too.
        let expiry_secs = if expiry >= 10_000_000_000 {
            expiry / 1000
        } else {
            expiry
        };
        if expiry_secs <= now {
            return GeminiCreds::Expired;
        }
    }
    GeminiCreds::Token(token.to_owned())
}

fn parse_gemini_load(body: &str) -> (Option<String>, Option<String>) {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return (None, None);
    };
    let project = json
        .get("cloudaicompanionProject")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let plan = json
        .get("paidTier")
        .or_else(|| json.get("currentTier"))
        .and_then(|tier| tier.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    (project, plan)
}

fn parse_gemini_quota(body: &str, plan: Option<String>) -> QuotaReport {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return QuotaReport::error("unreadable reply from Gemini quota endpoint");
    };
    let Some(buckets) = json.get("buckets").and_then(Value::as_array) else {
        return QuotaReport::unavailable("Gemini reported no quota buckets");
    };
    // Deduplicate per modelId, keeping the tightest (lowest remainingFraction) bucket.
    let mut by_model: HashMap<String, (f64, Option<u64>)> = HashMap::new();
    for bucket in buckets {
        let Some(model_id) = bucket.get("modelId").and_then(Value::as_str) else {
            continue;
        };
        let Some(remaining) = bucket.get("remainingFraction").and_then(Value::as_f64) else {
            continue;
        };
        let resets_at = bucket
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_secs);
        by_model
            .entry(model_id.to_owned())
            .and_modify(|entry| {
                if remaining < entry.0 {
                    *entry = (remaining, resets_at);
                }
            })
            .or_insert((remaining, resets_at));
    }

    let mut windows: Vec<QuotaWindow> = by_model
        .into_iter()
        .map(|(model_id, (remaining, resets_at))| QuotaWindow {
            label: model_id.clone(),
            model: Some(model_id),
            used_percent: clamp_percent((1.0 - remaining) * 100.0),
            resets_at,
            detail: None,
        })
        .collect();
    windows.sort_by(|a, b| a.label.cmp(&b.label));

    if windows.is_empty() {
        return QuotaReport::unavailable("Gemini reported no model quota buckets");
    }
    QuotaReport {
        status: QuotaStatus::Ok,
        plan,
        windows,
        credits: None,
        message: None,
        fetched_at: Some(now_secs()),
    }
}

// ---- Subprocess helpers (macOS security & curl) -------------------------------------------

/// Reads a generic password secret from the macOS Keychain, bounded by [`PROBE_TIMEOUT`].
#[cfg(target_os = "macos")]
fn read_keychain_secret(service: &str) -> Option<String> {
    let output = run_timed(
        Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service, "-w"])
            .stdin(Stdio::null()),
        None,
        PROBE_TIMEOUT,
    )?;
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

    let output = run_timed(
        Command::new("/usr/bin/curl")
            .args([
                "--silent",
                "--show-error",
                "--max-time",
                "7",
                "--write-out",
                &format!("{STATUS_MARK}%{{http_code}}"),
                "--config",
                "-",
            ])
            .stdin(Stdio::piped()),
        Some(config.as_bytes()),
        PROBE_TIMEOUT,
    )
    .ok_or_else(|| "quota request timed out".to_owned())?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some((response_body, status_text)) = stdout.rsplit_once(STATUS_MARK) else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            "quota request failed".to_owned()
        } else {
            stderr
        });
    };
    let status = status_text
        .trim()
        .parse::<u16>()
        .map_err(|_| "could not read HTTP status from curl".to_owned())?;
    Ok((status, response_body.to_owned()))
}

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
/// killing the entire process group if the timeout elapses.
fn run_timed(
    cmd: &mut Command,
    stdin_bytes: Option<&[u8]>,
    timeout: Duration,
) -> Option<std::process::Output> {
    let mut child = cmd
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let pid = child.id();
    if let (Some(bytes), Some(mut stdin)) = (stdin_bytes, child.stdin.take()) {
        drop(stdin.write_all(bytes));
        drop(stdin);
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let waiter = std::thread::Builder::new()
        .name("quota-wait".to_owned())
        .spawn(move || {
            let output = child.wait_with_output().ok();
            drop(sender.send(output));
        });
    if waiter.is_err() {
        agents::kill_group(pid);
        return None;
    }
    if let Ok(output) = receiver.recv_timeout(timeout) {
        output
    } else {
        agents::kill_group(pid);
        None
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
        || !(1..=31).contains(&day)
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
                    "primary": { "usedPercent": 21.0, "windowDurationMins": 300, "resetsAt": 1_795_620_000 },
                    "secondary": { "usedPercent": 48.0, "windowDurationMins": 10080, "resetsAt": 1_796_000_000 },
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

    #[test]
    fn parses_gemini_retrieve_user_quota_buckets() {
        let body = r#"{
            "buckets": [
                { "modelId": "gemini-2.5-pro", "remainingFraction": 0.88, "resetTime": "2026-11-26T00:00:00Z" },
                { "modelId": "gemini-2.5-pro", "remainingFraction": 0.75, "resetTime": "2026-11-26T00:00:00Z" },
                { "modelId": "gemini-2.5-flash", "remainingFraction": 0.96, "resetTime": "2026-11-26T00:00:00Z" }
            ]
        }"#;
        let report = parse_gemini_quota(body, Some("Google AI Pro".to_owned()));
        assert_eq!(report.status, QuotaStatus::Ok);
        assert_eq!(report.plan.as_deref(), Some("Google AI Pro"));
        assert_eq!(report.windows.len(), 2);
        assert_eq!(report.windows[0].label, "gemini-2.5-flash");
        assert!((report.windows[0].used_percent - 4.0).abs() < 1e-6);
        assert_eq!(report.windows[1].label, "gemini-2.5-pro");
        assert!((report.windows[1].used_percent - 25.0).abs() < 1e-6);
    }

    #[test]
    fn poller_drops_stale_probe_results_after_refresh() {
        let (release, gate) = mpsc::channel::<()>();
        let (entered, started) = mpsc::channel::<()>();
        let gate = Mutex::new(Some((gate, entered)));
        let poller = QuotaPoller::new(move |name, _| {
            let first = lock(&gate).take();
            match first {
                Some((gate, entered)) => {
                    entered.send(()).unwrap();
                    gate.recv().unwrap();
                    QuotaReport::unavailable(format!("old-{name}"))
                }
                None => QuotaReport::unavailable(format!("new-{name}")),
            }
        });

        let initial = poller.snapshot(&["Claude"], OsStr::new(""));
        assert_eq!(initial["Claude"].status, QuotaStatus::Loading);
        started.recv().unwrap();

        let _ = poller.refresh(&["Claude"], OsStr::new(""));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = poller.snapshot(&["Claude"], OsStr::new(""));
            if snap["Claude"].status != QuotaStatus::Loading {
                assert_eq!(snap["Claude"].message.as_deref(), Some("new-Claude"));
                break;
            }
            assert!(Instant::now() < deadline, "new probe did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let final_snap = poller.snapshot(&["Claude"], OsStr::new(""));
        assert_eq!(
            final_snap["Claude"].message.as_deref(),
            Some("new-Claude"),
            "older probe result was dropped"
        );
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
    fn parse_gemini_creds_detects_expired_token() {
        let now = 1_795_000_000;
        let expired = r#"{"access_token":"tok","expiry_date":1794000000000}"#;
        assert!(matches!(
            parse_gemini_creds(expired, now),
            GeminiCreds::Expired
        ));
        let fresh = r#"{"access_token":"tok","expiry_date":1796000000000}"#;
        assert!(matches!(parse_gemini_creds(fresh, now), GeminiCreds::Token(t) if t == "tok"));
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
                "      printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"userAgent\":\"test\"}}\\n'\n",
                "      ;;\n",
                "    *'\"method\":\"initialized\"'*)\n",
                "      saw_initialized=1\n",
                "      ;;\n",
                "    *'\"method\":\"account/read\"'*)\n",
                "      printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"authMode\":\"chatgpt\",\"planType\":\"plus\"}}\\n'\n",
                "      ;;\n",
                "    *'\"method\":\"account/rateLimits/read\"'*)\n",
                "      if [ \"$saw_initialized\" = \"1\" ]; then\n",
                "        printf '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":33.0,\"windowDurationMins\":300,\"resetsAt\":1800000000}}}}\\n'\n",
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
}
