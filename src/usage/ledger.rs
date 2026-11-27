//! Durable per-run usage ledger (`<config dir>/usage.jsonl`) and aggregation.
//!
//! Each finished run appends one JSON line under a short exclusive lock on `usage.lock`. A
//! line that does not parse, or one written by a newer format version, is skipped on read, so
//! the report stays readable across crashes and upgrades. An append that finds the file
//! ending inside a line (a write cut short by a crash) starts its record on a new line, so a
//! torn line costs only itself. Only counters, timestamps, the repository path, the agent
//! name and the model id are stored: never prompts, notes or source code.
//!
//! Appends are not synced to disk: the ledger holds statistics, and a crash that loses the
//! last few records loses nothing the runs' own logs do not keep.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, Serializer};

use crate::acp::{PromptUsage, UsageCost};
use crate::files;

/// The ledger record format version this build writes and reads.
pub(crate) const RECORD_VERSION: u32 = 1;

/// Seconds in one day, for the rolling time windows (`24h`, `7d`, `30d`).
const DAY_SECS: u64 = 24 * 60 * 60;

/// Which projects the report aggregates over.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Scope {
    Project,
    All,
}

impl Scope {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "project" => Some(Self::Project),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// Which time window the report aggregates over. The windows roll back from now: the page has
/// the local time zone, the process does not, so a calendar day is not offered.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) enum Range {
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    SevenDays,
    #[serde(rename = "30d")]
    ThirtyDays,
    #[serde(rename = "all")]
    All,
}

impl Range {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "24h" => Some(Self::Day),
            "7d" => Some(Self::SevenDays),
            "30d" => Some(Self::ThirtyDays),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// Inclusive lower bound on `started_at` (Unix seconds) for records in this range.
    fn cutoff(self, now: u64) -> u64 {
        match self {
            Self::Day => now.saturating_sub(DAY_SECS),
            Self::SevenDays => now.saturating_sub(7 * DAY_SECS),
            Self::ThirtyDays => now.saturating_sub(30 * DAY_SECS),
            Self::All => 0,
        }
    }
}

/// Where a runner appends finished runs' usage records.
#[derive(Debug)]
pub(crate) struct LedgerSink {
    pub(crate) file: PathBuf,
    pub(crate) repo: String,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunOutcome {
    Ended,
    Cancelled,
    Failed,
}

/// What started a run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunOrigin {
    Notes,
    FollowUp,
    Review,
    Analysis,
    Remember,
}

/// One finished run recorded in `usage.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LedgerRecord {
    pub(crate) version: u32,
    pub(crate) run_id: String,
    pub(crate) repo: String,
    pub(crate) agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    pub(crate) origin: RunOrigin,
    pub(crate) outcome: RunOutcome,
    pub(crate) started_at: u64,
    pub(crate) ended_at: u64,
    /// The token counts of the turn, as the agent reported them on its prompt reply.
    #[serde(flatten)]
    pub(crate) tokens: PromptUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context_size: Option<u64>,
    /// What this run cost, net of what earlier turns on the same kept session cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cost: Option<UsageCost>,
    #[serde(default)]
    pub(crate) tool_calls: u64,
    /// Additional per-model slice of a multi-model CLI session: contributes tokens, cost, and
    /// its own model row without counting as a second session in overall or per-agent totals.
    #[serde(skip)]
    pub(crate) secondary_model: bool,
}

impl LedgerRecord {
    /// The tokens this run spent: the reported total, else the sum of the reported buckets.
    /// Zero when the agent reported no token counts: `context_used` is how full the context
    /// window is, not what the turn spent, and on a kept session it carries earlier turns.
    fn total_tokens(&self) -> u64 {
        let tokens = &self.tokens;
        tokens.total_tokens.unwrap_or_else(|| {
            [
                tokens.input_tokens,
                tokens.output_tokens,
                tokens.thought_tokens,
            ]
            .into_iter()
            .flatten()
            .fold(0, u64::saturating_add)
        })
    }

    fn model_label(&self) -> &str {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("Default")
    }
}

/// Usage totals over a set of runs: the whole report, one agent, or one model.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Totals {
    pub(crate) runs: u64,
    pub(crate) ended: u64,
    pub(crate) failed: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cached_read_tokens: u64,
    pub(crate) cached_write_tokens: u64,
    pub(crate) thought_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cost: CostTotal,
    pub(crate) duration_secs: u64,
    pub(crate) tool_calls: u64,
}

impl Totals {
    fn add(&mut self, record: &LedgerRecord, count_session: bool) {
        if count_session {
            self.runs += 1;
            match record.outcome {
                RunOutcome::Ended => self.ended += 1,
                RunOutcome::Failed => self.failed += 1,
                RunOutcome::Cancelled => {}
            }
        }
        let add = |total: &mut u64, count: Option<u64>| {
            *total = total.saturating_add(count.unwrap_or(0));
        };
        add(&mut self.input_tokens, record.tokens.input_tokens);
        add(&mut self.output_tokens, record.tokens.output_tokens);
        add(
            &mut self.cached_read_tokens,
            record.tokens.cached_read_tokens,
        );
        add(
            &mut self.cached_write_tokens,
            record.tokens.cached_write_tokens,
        );
        add(&mut self.thought_tokens, record.tokens.thought_tokens);
        add(&mut self.total_tokens, Some(record.total_tokens()));
        if count_session {
            add(
                &mut self.duration_secs,
                Some(record.ended_at.saturating_sub(record.started_at)),
            );
            add(&mut self.tool_calls, Some(record.tool_calls));
        }
        self.cost.add(record.cost.as_ref());
    }
}

/// The summed cost of a set of runs. Serialized as the sum, or `null` when no run reported
/// a cost or the runs reported different currencies, which have no single sum.
#[derive(Debug, Default)]
pub(crate) enum CostTotal {
    #[default]
    None,
    Same(UsageCost),
    Mixed,
}

impl CostTotal {
    fn add(&mut self, cost: Option<&UsageCost>) {
        let Some(cost) = cost else {
            return;
        };
        if !cost.amount.is_finite() || cost.amount < 0.0 || cost.currency.is_empty() {
            return;
        }
        match self {
            Self::None => *self = Self::Same(cost.clone()),
            Self::Same(total) if total.currency == cost.currency => total.amount += cost.amount,
            Self::Same(_) | Self::Mixed => *self = Self::Mixed,
        }
    }
}

impl Serialize for CostTotal {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Same(cost) => serializer.serialize_some(cost),
            Self::None | Self::Mixed => serializer.serialize_none(),
        }
    }
}

/// Usage totals for one model under an agent.
#[derive(Debug, Serialize)]
pub(crate) struct ModelStats {
    pub(crate) model: String,
    #[serde(flatten)]
    pub(crate) totals: Totals,
}

/// One agent's totals, with its per-model breakdown.
#[derive(Debug, Default, Serialize)]
pub(crate) struct AgentActivity {
    #[serde(flatten)]
    pub(crate) totals: Totals,
    pub(crate) models: Vec<ModelStats>,
}

/// Appends `record` to `file` as one JSON line under a short exclusive lock.
///
/// Waits up to `files::lock`'s limit for another window's append. Does not sync the file
/// (see the module docs).
///
/// # Errors
///
/// Returns an I/O error if the lock file or the ledger cannot be opened, read or written.
pub(crate) fn append(file: &Path, record: &LedgerRecord) -> io::Result<()> {
    let lock_path = file.with_extension("lock");
    let _guard = files::lock(&lock_path).map_err(|error| match error {
        fs::TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("{} is locked by another Postil window", lock_path.display()),
        ),
        fs::TryLockError::Error(source) => source,
    })?;
    let mut line = serde_json::to_vec(record).expect("a ledger record always serializes");
    line.push(b'\n');
    let mut out = File::options()
        .create(true)
        .read(true)
        .append(true)
        .open(file)?;
    // A file that does not end in a newline ends in a torn line; starting this record on a
    // new line keeps it from being joined to that line and skipped with it.
    let len = out.metadata()?.len();
    if len > 0 {
        let mut last = [0];
        out.read_exact_at(&mut last, len - 1)?;
        if last != *b"\n" {
            line.insert(0, b'\n');
        }
    }
    out.write_all(&line)
}

/// Reads every valid v1 record from `file`; a missing file has none. Torn and newer-version
/// lines are skipped, so the report never fails on a partial crash write.
///
/// # Errors
///
/// Returns the I/O error if the file exists but cannot be read.
pub(crate) fn read(file: &Path) -> io::Result<Vec<LedgerRecord>> {
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(bytes
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<LedgerRecord>(line).ok())
        .filter(|record| record.version == RECORD_VERSION)
        .collect())
}

/// Maps legacy built-in agent names in `usage.jsonl` to their current catalog name when no
/// custom agent claims the legacy name.
pub(crate) fn canonical_agent_name(name: &str, remap_legacy_gemini: bool) -> &str {
    if remap_legacy_gemini && name == "Gemini" {
        "Antigravity"
    } else {
        name
    }
}

/// Whether a record or CLI session at `session_repo` belongs inside `project_root`. Compares
/// path components (treating macOS `/private/var` and `/private/tmp` aliases identically to
/// `/var` and `/tmp`) so `/repo/sub` matches `/repo` while `/repo-other` does not.
pub(crate) fn repo_matches_project(session_repo: &str, project_root: &str) -> bool {
    if session_repo == project_root || Path::new(session_repo).starts_with(Path::new(project_root))
    {
        return true;
    }
    let norm_session = strip_macos_private(session_repo);
    let norm_root = strip_macos_private(project_root);
    Path::new(norm_session).starts_with(Path::new(norm_root))
}

pub(crate) fn strip_macos_private(path: &str) -> &str {
    if path == "/private/var"
        || path.starts_with("/private/var/")
        || path == "/private/tmp"
        || path.starts_with("/private/tmp/")
    {
        &path["/private".len()..]
    } else {
        path
    }
}

/// Totals the `records` that started in `range` (relative to `now`), overall and per agent,
/// then per model. `repo` keeps only that repository's records (including subdirectories);
/// `None` keeps every one.
pub(crate) fn aggregate<'a>(
    records: impl IntoIterator<Item = &'a LedgerRecord>,
    repo: Option<&str>,
    range: Range,
    now: u64,
    remap_legacy_gemini: bool,
) -> (Totals, BTreeMap<String, AgentActivity>) {
    let cutoff = range.cutoff(now);
    let mut summary = Totals::default();
    let mut by_agent: BTreeMap<&'a str, (Totals, BTreeMap<&'a str, Totals>)> = BTreeMap::new();

    let kept = records.into_iter().filter(|record| {
        record.started_at >= cutoff
            && repo.is_none_or(|root| repo_matches_project(&record.repo, root))
    });
    for record in kept {
        let count_session = !record.secondary_model;
        summary.add(record, count_session);
        let agent_name = canonical_agent_name(&record.agent, remap_legacy_gemini);
        let (agent_totals, models) = by_agent.entry(agent_name).or_default();
        agent_totals.add(record, count_session);
        models
            .entry(record.model_label())
            .or_default()
            .add(record, true);
    }

    let activities = by_agent
        .into_iter()
        .map(|(agent, (totals, models))| {
            let mut models: Vec<ModelStats> = models
                .into_iter()
                .map(|(model, totals)| ModelStats {
                    model: model.to_owned(),
                    totals,
                })
                .collect();
            models.sort_by(|a, b| {
                b.totals
                    .runs
                    .cmp(&a.totals.runs)
                    .then_with(|| b.totals.total_tokens.cmp(&a.totals.total_tokens))
                    .then_with(|| a.model.cmp(&b.model))
            });
            (agent.to_owned(), AgentActivity { totals, models })
        })
        .collect();

    (summary, activities)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn sample(
        id: &str,
        repo: &str,
        agent: &str,
        model: Option<&str>,
        started_at: u64,
    ) -> LedgerRecord {
        LedgerRecord {
            version: RECORD_VERSION,
            run_id: id.to_owned(),
            repo: repo.to_owned(),
            agent: agent.to_owned(),
            model: model.map(str::to_owned),
            origin: RunOrigin::Notes,
            outcome: RunOutcome::Ended,
            started_at,
            ended_at: started_at + 30,
            tokens: PromptUsage {
                cached_read_tokens: Some(10),
                ..PromptUsage::default()
            },
            context_used: None,
            context_size: None,
            cost: None,
            tool_calls: 2,
            secondary_model: false,
        }
    }

    fn with_usage(
        mut record: LedgerRecord,
        input: u64,
        output: u64,
        cost: Option<f64>,
    ) -> LedgerRecord {
        record.tokens.input_tokens = Some(input);
        record.tokens.output_tokens = Some(output);
        record.cost = cost.map(|amount| UsageCost {
            amount,
            currency: "USD".to_owned(),
        });
        record
    }

    #[test]
    fn append_and_read_skip_torn_and_future_version_lines() {
        let dir = TempDir::new("usage-ledger");
        let file = dir.path().join("usage.jsonl");
        let first = with_usage(
            sample("R1", "/a", "Claude", Some("sonnet"), 1000),
            100,
            20,
            Some(0.1),
        );
        append(&file, &first).unwrap();
        let mut raw = fs::read_to_string(&file).unwrap();
        raw.push_str("{broken json\n");
        let newer = serde_json::json!({
            "version": 99, "runId": "R2", "repo": "/a", "agent": "Claude", "origin": "notes",
            "outcome": "ended", "startedAt": 1000, "endedAt": 1010,
        });
        raw.push_str(&format!("{newer}\n"));
        fs::write(&file, raw).unwrap();
        let second = with_usage(sample("R3", "/b", "Codex", None, 2000), 50, 10, None);
        append(&file, &second).unwrap();

        let read_back = read(&file).unwrap();
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[0].run_id, "R1");
        assert_eq!(read_back[1].run_id, "R3");
    }

    #[test]
    fn an_append_after_a_torn_last_line_loses_only_that_line() {
        let dir = TempDir::new("usage-ledger-torn");
        let file = dir.path().join("usage.jsonl");
        append(&file, &sample("R1", "/a", "Claude", None, 1000)).unwrap();
        append(&file, &sample("R2", "/a", "Claude", None, 1001)).unwrap();
        // A crash in the middle of writing R2: the file ends inside its line.
        let whole = fs::read(&file).unwrap();
        let torn = whole.len() - 10;
        fs::write(&file, &whole[..torn]).unwrap();

        append(&file, &sample("R3", "/a", "Claude", None, 1002)).unwrap();

        let ids: Vec<_> = read(&file)
            .unwrap()
            .into_iter()
            .map(|record| record.run_id)
            .collect();
        assert_eq!(ids, ["R1", "R3"]);
    }

    #[test]
    fn an_append_after_a_zero_filled_tail_keeps_every_whole_record() {
        let dir = TempDir::new("usage-ledger-zeros");
        let file = dir.path().join("usage.jsonl");
        append(&file, &sample("R1", "/a", "Claude", None, 1000)).unwrap();
        append(&file, &sample("R2", "/a", "Claude", None, 1001)).unwrap();
        // A crash after the file grew but before its data reached the disk: the new length
        // reads back as zero bytes.
        let mut whole = fs::read(&file).unwrap();
        whole.extend([0; 16]);
        fs::write(&file, &whole).unwrap();

        append(&file, &sample("R3", "/a", "Claude", None, 1002)).unwrap();

        let ids: Vec<_> = read(&file)
            .unwrap()
            .into_iter()
            .map(|record| record.run_id)
            .collect();
        assert_eq!(ids, ["R1", "R2", "R3"]);
    }

    #[test]
    fn a_missing_ledger_is_empty_but_an_unreadable_one_is_an_error() {
        let dir = TempDir::new("usage-ledger-unreadable");
        assert!(read(&dir.path().join("usage.jsonl")).unwrap().is_empty());
        // A directory where the file should be: it exists, and reading it fails.
        let error = read(dir.path()).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn each_range_keeps_the_runs_started_at_or_after_its_cutoff() {
        // Spelled out rather than `DAY_SECS`, so a wrong constant fails this test.
        let day = 86_400;
        let now = 100 * day;
        let started = [
            now - day,
            now - day - 1,
            now - 7 * day,
            now - 7 * day - 1,
            now - 30 * day,
            now - 30 * day - 1,
        ];
        let records: Vec<_> = started
            .iter()
            .map(|&at| sample("R", "/a", "Claude", None, at))
            .collect();
        let runs = |range| aggregate(&records, None, range, now, true).0.runs;
        let parsed = |value| Range::parse(value).expect("a known range");
        assert_eq!(runs(parsed("24h")), 1);
        assert_eq!(runs(parsed("7d")), 3);
        assert_eq!(runs(parsed("30d")), 5);
        assert_eq!(runs(parsed("all")), 6);
        assert!(Range::parse("1y").is_none());
    }

    #[test]
    fn totals_count_each_outcome_and_skip_costs_that_cannot_be_summed() {
        let cost = |amount: f64, currency: &str| {
            Some(UsageCost {
                amount,
                currency: currency.to_owned(),
            })
        };
        let records: Vec<_> = [
            (RunOutcome::Ended, cost(0.0, "USD")),
            (RunOutcome::Ended, cost(-1.0, "USD")),
            (RunOutcome::Failed, cost(f64::NAN, "USD")),
            (RunOutcome::Failed, cost(f64::INFINITY, "USD")),
            (RunOutcome::Failed, cost(1.0, "")),
            (RunOutcome::Cancelled, None),
        ]
        .into_iter()
        .map(|(outcome, cost)| LedgerRecord {
            outcome,
            cost,
            ..sample("R", "/a", "Claude", None, 1000)
        })
        .collect();

        let (summary, _) = aggregate(&records, None, Range::All, 2000, true);
        assert_eq!((summary.runs, summary.ended, summary.failed), (6, 2, 3));
        // Only the zero cost is valid: it still makes the total known, unlike no cost at all.
        let CostTotal::Same(total) = &summary.cost else {
            panic!("a zero cost counts: {:?}", summary.cost);
        };
        assert!(total.amount.abs() < 1e-9, "{total:?}");
        assert_eq!(total.currency, "USD");
    }

    #[test]
    fn tokens_count_only_what_the_agent_reported_spending() {
        let mut record = sample("R1", "/a", "Claude", None, 1000);
        record.context_used = Some(40_000);
        assert_eq!(record.total_tokens(), 0, "context fill is not tokens spent");
        record.tokens.thought_tokens = Some(5);
        record.tokens.output_tokens = Some(7);
        assert_eq!(record.total_tokens(), 12);
        record.tokens.total_tokens = Some(100);
        assert_eq!(record.total_tokens(), 100);
    }

    #[test]
    fn aggregate_filters_by_repo_and_range_and_groups_models() {
        let now = 100 * DAY_SECS;
        let mut cached_writes = with_usage(
            sample("R2", "/proj-a", "Claude", Some("sonnet"), now - 200),
            200,
            40,
            Some(0.20),
        );
        cached_writes.tokens.cached_write_tokens = Some(5);
        let records = vec![
            with_usage(
                sample("R1", "/proj-a", "Claude", Some("sonnet"), now - 100),
                100,
                50,
                Some(0.10),
            ),
            cached_writes,
            with_usage(
                sample("R3", "/proj-a", "Claude", Some("opus"), now - 3 * DAY_SECS),
                500,
                100,
                Some(0.50),
            ),
            with_usage(
                sample("R4", "/proj-b", "Codex", Some("gpt-5-codex"), now - 500),
                300,
                60,
                None,
            ),
            with_usage(
                sample(
                    "R5",
                    "/proj-a",
                    "Claude",
                    Some("sonnet"),
                    now - 40 * DAY_SECS,
                ),
                999,
                1,
                Some(1.0),
            ),
        ];

        let (summary, agents) = aggregate(&records, Some("/proj-a"), Range::SevenDays, now, true);
        assert_eq!(summary.runs, 3);
        assert_eq!(summary.total_tokens, 990);
        assert_eq!(summary.cached_write_tokens, 5);
        let CostTotal::Same(cost) = &summary.cost else {
            panic!("one currency sums: {:?}", summary.cost);
        };
        assert!((cost.amount - 0.80).abs() < 1e-9);
        assert_eq!(cost.currency, "USD");
        let claude = &agents["Claude"];
        assert_eq!(claude.models.len(), 2);
        assert_eq!(claude.models[0].model, "sonnet");
        assert_eq!(claude.models[0].totals.runs, 2);
        assert_eq!(claude.models[1].model, "opus");
        assert_eq!(claude.models[1].totals.runs, 1);

        let (all_day, day_agents) = aggregate(&records, None, Range::Day, now, true);
        assert_eq!(all_day.runs, 3);
        assert!(day_agents.contains_key("Claude"));
        assert!(day_agents.contains_key("Codex"));

        let (all_time, _) = aggregate(&records, None, Range::All, now, true);
        assert_eq!(all_time.runs, 5);

        // Subdirectory sessions match the project root, while sibling prefixes and legacy
        // "Gemini" agent records normalize cleanly (unless a custom "Gemini" agent exists).
        let sub_and_legacy = [
            sample(
                "R6",
                "/proj-a/crates/core",
                "Gemini",
                Some("neon"),
                now - 10,
            ),
            sample("R7", "/proj-ab", "Gemini", None, now - 10),
        ];
        let (sub_sum, sub_agents) =
            aggregate(&sub_and_legacy, Some("/proj-a"), Range::Day, now, true);
        assert_eq!(sub_sum.runs, 1);
        assert!(sub_agents.contains_key("Antigravity"));
        assert!(!sub_agents.contains_key("Gemini"));

        let (_, custom_gemini) =
            aggregate(&sub_and_legacy, Some("/proj-a"), Range::Day, now, false);
        assert!(custom_gemini.contains_key("Gemini"));
        assert!(!custom_gemini.contains_key("Antigravity"));
    }

    #[test]
    fn costs_in_different_currencies_have_no_sum() {
        let mut euros = with_usage(sample("R2", "/a", "Claude", None, 1001), 1, 1, Some(0.5));
        euros.cost.as_mut().unwrap().currency = "EUR".to_owned();
        let records = [
            with_usage(sample("R1", "/a", "Claude", None, 1000), 1, 1, Some(0.5)),
            euros,
        ];
        let (summary, _) = aggregate(&records, None, Range::All, 2000, true);
        assert!(matches!(summary.cost, CostTotal::Mixed));
        assert_eq!(
            serde_json::to_value(&summary).unwrap()["cost"],
            serde_json::Value::Null
        );
    }
}
