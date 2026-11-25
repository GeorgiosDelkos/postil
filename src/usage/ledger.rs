//! Durable per-run usage ledger (`<config dir>/usage.jsonl`) and aggregation.
//!
//! Each completed or failed run appends one JSON line under a short exclusive lock on
//! `usage.lock`. A torn line or an entry written by a newer format version is skipped on read,
//! so the report stays readable across crashes and upgrades. Only counters, timestamps, repo
//! path, agent name, and model id are stored — never prompts, notes, or source code.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::files;

/// The ledger record format version this build writes and reads.
pub(crate) const RECORD_VERSION: u32 = 1;

/// Seconds in one day, used for rolling time windows (`Today`, `7d`, `30d`).
const DAY_SECS: u64 = 24 * 60 * 60;

/// Which projects the report aggregates over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Which time window the report aggregates over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Range {
    Today,
    #[serde(rename = "7d")]
    SevenDays,
    #[serde(rename = "30d")]
    ThirtyDays,
    All,
}

impl Range {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "today" => Some(Self::Today),
            "7d" => Some(Self::SevenDays),
            "30d" => Some(Self::ThirtyDays),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// Inclusive lower bound on `started_at` (Unix seconds) for records in this range.
    pub(crate) fn cutoff(self, now: u64) -> u64 {
        match self {
            Self::Today => now.saturating_sub(DAY_SECS),
            Self::SevenDays => now.saturating_sub(7 * DAY_SECS),
            Self::ThirtyDays => now.saturating_sub(30 * DAY_SECS),
            Self::All => 0,
        }
    }
}

/// Where a runner appends finished runs' usage records.
#[derive(Debug, Clone)]
pub(crate) struct LedgerSink {
    pub(crate) file: PathBuf,
    pub(crate) repo: String,
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
    /// `"notes"`, `"follow_up"`, or `"review"`.
    pub(crate) origin: String,
    /// `"ended"`, `"cancelled"`, or `"failed"`.
    pub(crate) status: String,
    pub(crate) started_at: u64,
    pub(crate) ended_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cached_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cached_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thought_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cost_amount: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cost_currency: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: u64,
}

impl LedgerRecord {
    /// Total tokens represented by this record: explicit `total_tokens`, else the sum of
    /// reported token buckets, else `context_used` when only context usage was sent.
    fn effective_total_tokens(&self) -> u64 {
        if let Some(total) = self.total_tokens {
            return total;
        }
        let summed = self.input_tokens.unwrap_or(0)
            + self.output_tokens.unwrap_or(0)
            + self.thought_tokens.unwrap_or(0);
        if summed > 0 {
            return summed;
        }
        self.context_used.unwrap_or(0)
    }

    fn duration_secs(&self) -> u64 {
        self.ended_at.saturating_sub(self.started_at)
    }

    fn model_label(&self) -> &str {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("Default")
    }
}

/// Usage totals for one underlying model under an agent.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelStats {
    pub(crate) model: String,
    pub(crate) runs: u64,
    pub(crate) ended: u64,
    pub(crate) failed: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cached_read_tokens: u64,
    pub(crate) thought_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cost_amount: Option<f64>,
    pub(crate) cost_currency: Option<String>,
    pub(crate) duration_secs: u64,
    pub(crate) tool_calls: u64,
}

/// Aggregated Postil activity for one agent, including its per-model breakdown.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentActivity {
    pub(crate) runs: u64,
    pub(crate) ended: u64,
    pub(crate) failed: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cached_read_tokens: u64,
    pub(crate) thought_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cost_amount: Option<f64>,
    pub(crate) cost_currency: Option<String>,
    pub(crate) duration_secs: u64,
    pub(crate) tool_calls: u64,
    pub(crate) models: Vec<ModelStats>,
}

/// Top-level totals across all agents in the active filter scope and range.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageSummary {
    pub(crate) runs: u64,
    pub(crate) ended: u64,
    pub(crate) failed: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cached_read_tokens: u64,
    pub(crate) thought_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cost_amount: Option<f64>,
    pub(crate) cost_currency: Option<String>,
    pub(crate) duration_secs: u64,
    pub(crate) tool_calls: u64,
}

/// Appends `record` to `file` as one JSON line under a short exclusive lock.
///
/// # Errors
///
/// Returns an I/O error if the directory, lock file, or ledger file cannot be opened or written.
pub(crate) fn append(file: &Path, record: &LedgerRecord) -> io::Result<()> {
    let lock_path = file.with_extension("lock");
    let _guard = files::lock(&lock_path).map_err(|error| match error {
        fs::TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("{} is locked by another Postil window", lock_path.display()),
        ),
        fs::TryLockError::Error(source) => source,
    })?;
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(record).expect("a ledger record always serializes");
    line.push(b'\n');
    let mut out = File::options().create(true).append(true).open(file)?;
    out.write_all(&line)
}

/// Reads every valid v1 record from `file`. Missing, unreadable, torn, or newer-version lines
/// are skipped so the report never fails on a partial crash write.
pub(crate) fn read(file: &Path) -> Vec<LedgerRecord> {
    let Ok(bytes) = fs::read(file) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for line in bytes.split(|&byte| byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<LedgerRecord>(line) else {
            continue;
        };
        if record.version == RECORD_VERSION {
            records.push(record);
        }
    }
    records
}

/// Filters `records` by `scope`, `repo`, and `range` relative to `now`, and aggregates totals
/// overall and per agent -> per model.
pub(crate) fn aggregate(
    records: &[LedgerRecord],
    scope: Scope,
    repo: Option<&str>,
    range: Range,
    now: u64,
) -> (UsageSummary, BTreeMap<String, AgentActivity>) {
    let cutoff = range.cutoff(now);
    let mut summary = Totals::default();
    let mut by_agent: BTreeMap<String, (Totals, BTreeMap<String, Totals>)> = BTreeMap::new();

    for record in records {
        if record.started_at < cutoff {
            continue;
        }
        if scope == Scope::Project && repo.is_some_and(|root| record.repo != root) {
            continue;
        }
        if scope == Scope::Project && repo.is_none() {
            continue;
        }
        summary.add(record);
        let (agent_totals, models) = by_agent.entry(record.agent.clone()).or_default();
        agent_totals.add(record);
        models
            .entry(record.model_label().to_owned())
            .or_default()
            .add(record);
    }

    let activities = by_agent
        .into_iter()
        .map(|(agent, (totals, models))| {
            let mut model_rows: Vec<ModelStats> = models
                .into_iter()
                .map(|(model, row)| row.into_model_stats(model))
                .collect();
            model_rows.sort_by(|a, b| {
                b.runs
                    .cmp(&a.runs)
                    .then_with(|| b.total_tokens.cmp(&a.total_tokens))
                    .then_with(|| a.model.cmp(&b.model))
            });
            (agent, totals.into_agent_activity(model_rows))
        })
        .collect();

    (summary.into_summary(), activities)
}

/// Accumulator shared across summary, agent, and model levels.
#[derive(Default)]
struct Totals {
    runs: u64,
    ended: u64,
    failed: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_read_tokens: u64,
    thought_tokens: u64,
    total_tokens: u64,
    cost: CostAccum,
    duration_secs: u64,
    tool_calls: u64,
}

#[derive(Default)]
enum CostAccum {
    #[default]
    None,
    Same {
        amount: f64,
        currency: String,
    },
    /// Different currencies were seen in the same bucket, so no single sum is meaningful.
    Mixed,
}

impl CostAccum {
    fn add(&mut self, amount: Option<f64>, currency: Option<&str>) {
        let (Some(amount), Some(currency)) = (amount, currency) else {
            return;
        };
        if !amount.is_finite() || amount < 0.0 || currency.is_empty() {
            return;
        }
        match self {
            Self::None => {
                *self = Self::Same {
                    amount,
                    currency: currency.to_owned(),
                };
            }
            Self::Same {
                amount: total,
                currency: existing,
            } if existing == currency => {
                *total += amount;
            }
            Self::Same { .. } | Self::Mixed => {
                *self = Self::Mixed;
            }
        }
    }

    fn finish(self) -> (Option<f64>, Option<String>) {
        match self {
            Self::Same { amount, currency } => (Some(amount), Some(currency)),
            Self::None | Self::Mixed => (None, None),
        }
    }
}

impl Totals {
    fn add(&mut self, record: &LedgerRecord) {
        self.runs += 1;
        match record.status.as_str() {
            "ended" => self.ended += 1,
            "failed" => self.failed += 1,
            _ => {}
        }
        self.input_tokens = self
            .input_tokens
            .saturating_add(record.input_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(record.output_tokens.unwrap_or(0));
        self.cached_read_tokens = self
            .cached_read_tokens
            .saturating_add(record.cached_read_tokens.unwrap_or(0));
        self.thought_tokens = self
            .thought_tokens
            .saturating_add(record.thought_tokens.unwrap_or(0));
        self.total_tokens = self
            .total_tokens
            .saturating_add(record.effective_total_tokens());
        self.cost
            .add(record.cost_amount, record.cost_currency.as_deref());
        self.duration_secs = self.duration_secs.saturating_add(record.duration_secs());
        self.tool_calls = self.tool_calls.saturating_add(record.tool_calls);
    }

    fn into_model_stats(self, model: String) -> ModelStats {
        let (cost_amount, cost_currency) = self.cost.finish();
        ModelStats {
            model,
            runs: self.runs,
            ended: self.ended,
            failed: self.failed,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_read_tokens: self.cached_read_tokens,
            thought_tokens: self.thought_tokens,
            total_tokens: self.total_tokens,
            cost_amount,
            cost_currency,
            duration_secs: self.duration_secs,
            tool_calls: self.tool_calls,
        }
    }

    fn into_agent_activity(self, models: Vec<ModelStats>) -> AgentActivity {
        let (cost_amount, cost_currency) = self.cost.finish();
        AgentActivity {
            runs: self.runs,
            ended: self.ended,
            failed: self.failed,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_read_tokens: self.cached_read_tokens,
            thought_tokens: self.thought_tokens,
            total_tokens: self.total_tokens,
            cost_amount,
            cost_currency,
            duration_secs: self.duration_secs,
            tool_calls: self.tool_calls,
            models,
        }
    }

    fn into_summary(self) -> UsageSummary {
        let (cost_amount, cost_currency) = self.cost.finish();
        UsageSummary {
            runs: self.runs,
            ended: self.ended,
            failed: self.failed,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_read_tokens: self.cached_read_tokens,
            thought_tokens: self.thought_tokens,
            total_tokens: self.total_tokens,
            cost_amount,
            cost_currency,
            duration_secs: self.duration_secs,
            tool_calls: self.tool_calls,
        }
    }
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
            origin: "notes".to_owned(),
            status: "ended".to_owned(),
            started_at,
            ended_at: started_at + 30,
            input_tokens: None,
            output_tokens: None,
            cached_read_tokens: Some(10),
            cached_write_tokens: None,
            thought_tokens: None,
            total_tokens: None,
            context_used: None,
            context_size: None,
            cost_amount: None,
            cost_currency: None,
            tool_calls: 2,
        }
    }

    fn with_usage(
        mut record: LedgerRecord,
        input: u64,
        output: u64,
        cost: Option<f64>,
    ) -> LedgerRecord {
        record.input_tokens = Some(input);
        record.output_tokens = Some(output);
        record.cost_amount = cost;
        record.cost_currency = cost.map(|_| "USD".to_owned());
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
        raw.push_str(
            r#"{"version":99,"runId":"R2","repo":"/a","agent":"Claude","origin":"notes","status":"ended","startedAt":1000,"endedAt":1010}"#,
        );
        raw.push('\n');
        fs::write(&file, raw).unwrap();
        let second = with_usage(sample("R3", "/b", "Codex", None, 2000), 50, 10, None);
        append(&file, &second).unwrap();

        let read_back = read(&file);
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[0].run_id, "R1");
        assert_eq!(read_back[1].run_id, "R3");
    }

    #[test]
    fn aggregate_filters_by_scope_and_range_and_groups_models() {
        let now = 100 * DAY_SECS;
        let records = vec![
            with_usage(
                sample("R1", "/proj-a", "Claude", Some("sonnet"), now - 100),
                100,
                50,
                Some(0.10),
            ),
            with_usage(
                sample("R2", "/proj-a", "Claude", Some("sonnet"), now - 200),
                200,
                40,
                Some(0.20),
            ),
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

        let (summary, agents) = aggregate(
            &records,
            Scope::Project,
            Some("/proj-a"),
            Range::SevenDays,
            now,
        );
        assert_eq!(summary.runs, 3);
        assert_eq!(summary.total_tokens, 990);
        assert!((summary.cost_amount.unwrap() - 0.80).abs() < 1e-9);
        assert_eq!(summary.cost_currency.as_deref(), Some("USD"));
        let claude = &agents["Claude"];
        assert_eq!(claude.models.len(), 2);
        assert_eq!(claude.models[0].model, "sonnet");
        assert_eq!(claude.models[0].runs, 2);
        assert_eq!(claude.models[1].model, "opus");
        assert_eq!(claude.models[1].runs, 1);

        let (all_today, today_agents) =
            aggregate(&records, Scope::All, Some("/proj-a"), Range::Today, now);
        assert_eq!(all_today.runs, 3);
        assert!(today_agents.contains_key("Claude"));
        assert!(today_agents.contains_key("Codex"));

        let (all_time, _) = aggregate(&records, Scope::All, None, Range::All, now);
        assert_eq!(all_time.runs, 5);
    }
}
