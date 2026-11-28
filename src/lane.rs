//! Execution lanes, capability-tier stage resolution, and structured prompts/parsers for
//! Issue implementation and Pull Request review workflows.
//!
//! Three lanes match task complexity to agent capability tiers:
//! - [`Lane::Easy`]: trivial changes or small PRs — one agent performs the work directly with no
//!   extra review pass.
//! - [`Lane::Middle`]: standard features or medium PRs — main agent plus an independent review
//!   pass (and judge/fix for issues, or second-opinion synthesis for PRs).
//! - [`Lane::Hard`]: complex or high-risk work — two top-tier Frontier models plan and judge the
//!   plan with additional proposals (optionally pausing at a human approval gate), followed by
//!   implementation, at least two independent reviews, and a final judge.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agents::AgentChoice;
use crate::error::{Error, Result};
use crate::report;
use crate::review::{Finding, MemoryBlock};
use crate::settings::{CapabilityTier, Settings};

/// Execution lane chosen for an issue or pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Lane {
    /// Trivial task: single agent, no extra review.
    Easy,
    /// Standard task: main agent plus independent peer review.
    Middle,
    /// Complex/high-risk task: 2 Frontier models plan + judge plan, then implement + >=2 reviews + final judge.
    Hard,
}

impl Lane {
    /// Human-readable label shown in cards and dialogs.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Easy => "Easy",
            Self::Middle => "Middle",
            Self::Hard => "Hard",
        }
    }
}

/// Whether a lane is implementing an issue or reviewing a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WorkKind {
    Issue,
    PullRequest,
}

/// The role of one stage inside a lane pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum StageKind {
    /// Read-only architecture / implementation planner (or PR context mapper).
    Plan,
    /// Read-only second Frontier model that critiques the plan and adds proposals.
    PlanJudge,
    /// Read-write implementation stage in the thread's worktree.
    Implement,
    /// Read-only independent reviewer proposing structured findings (`F1`, `F2`, ...).
    ReviewPropose,
    /// Final judge: read-write fixer for Issues, read-only verifier/synthesizer for PRs.
    ReviewJudge,
}

impl StageKind {
    /// Short human-readable title for the stage.
    pub(crate) fn label(self, kind: WorkKind, reviewer_index: usize) -> String {
        match (self, kind) {
            (Self::Plan, WorkKind::Issue) => "Architect / Planner".to_owned(),
            (Self::Plan, WorkKind::PullRequest) => "Context & Architecture Audit".to_owned(),
            (Self::PlanJudge, _) => "Plan Judge & Proposals".to_owned(),
            (Self::Implement, _) => "Implementer".to_owned(),
            (Self::ReviewPropose, WorkKind::Issue) => {
                if reviewer_index > 0 {
                    format!("Reviewer {reviewer_index}")
                } else {
                    "Peer Reviewer".to_owned()
                }
            }
            (Self::ReviewPropose, WorkKind::PullRequest) => {
                if reviewer_index > 0 {
                    format!("Deep Audit {reviewer_index}")
                } else {
                    "Primary Reviewer".to_owned()
                }
            }
            (Self::ReviewJudge, WorkKind::Issue) => "Final Judge & Fixer".to_owned(),
            (Self::ReviewJudge, WorkKind::PullRequest) => "Synthesis Judge".to_owned(),
        }
    }
}

/// One resolved stage in a lane pipeline, ready for display or execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResolvedStage {
    pub(crate) kind: StageKind,
    pub(crate) label: String,
    pub(crate) tier: CapabilityTier,
    pub(crate) agent: String,
    pub(crate) model: Option<String>,
    pub(crate) read_only: bool,
    /// True when this stage resolved to a distinct provider or model compared to the stage it
    /// checks/judges (cross-model diversity).
    pub(crate) diverse: bool,
    #[serde(skip_serializing)]
    pub(crate) choice: AgentChoice,
}

/// Structured implementation plan produced by Stage 1 (`Plan`) in Hard Lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PlanDocument {
    pub(crate) summary: String,
    #[serde(default)]
    pub(crate) approach: String,
    #[serde(default)]
    pub(crate) steps: Vec<PlanStep>,
    #[serde(default)]
    pub(crate) invariants: Vec<String>,
    #[serde(default)]
    pub(crate) risks: Vec<String>,
    #[serde(default)]
    pub(crate) tests: Vec<String>,
}

/// One concrete step inside [`PlanDocument`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PlanStep {
    #[serde(default)]
    pub(crate) path: Option<String>,
    pub(crate) change: String,
}

/// Structured judgment produced by Stage 2 (`PlanJudge`) in Hard Lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PlanJudgeResult {
    /// `"approve"` or `"revise"`.
    pub(crate) verdict: String,
    #[serde(default)]
    pub(crate) critique: String,
    #[serde(default)]
    pub(crate) proposals: Vec<PlanProposal>,
}

/// One concrete addition or revision proposed by the Plan Judge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PlanProposal {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) detail: String,
    /// `"must"`, `"should"`, or `"optional"`.
    #[serde(default = "default_priority")]
    pub(crate) priority: String,
}

fn default_priority() -> String {
    "should".to_owned()
}

/// Resolves the ordered stages for `lane` and `kind` from `settings`'s capability tiers.
///
/// Falls back cleanly when a specific tier has no explicit entries:
/// - `Fast` -> `Standard` -> `Frontier` -> `default_agent`
/// - `Standard` -> `Frontier` -> `Fast` -> `default_agent`
/// - `Frontier` -> `Standard` -> `Fast` -> `default_agent`
///
/// # Errors
///
/// [`Error::BadRequest`] when no enabled agent is available in any tier or as the default agent.
#[expect(
    clippy::too_many_lines,
    reason = "maps all six (lane, work_kind) matrices directly so each lane's stages read top-to-bottom"
)]
pub(crate) fn resolve_stages(
    settings: &Settings,
    lane: Lane,
    kind: WorkKind,
) -> Result<Vec<ResolvedStage>> {
    let pick = |tier: CapabilityTier, avoid: Option<&AgentChoice>| -> Result<(AgentChoice, bool)> {
        let pool = tier_pool(settings, tier)?;
        Ok(pick_from_pool(&pool, avoid))
    };

    let make = |stage_kind: StageKind,
                tier: CapabilityTier,
                read_only: bool,
                reviewer_index: usize,
                avoid: Option<&AgentChoice>|
     -> Result<ResolvedStage> {
        let (choice, diverse) = pick(tier, avoid)?;
        Ok(ResolvedStage {
            kind: stage_kind,
            label: stage_kind.label(kind, reviewer_index),
            tier,
            agent: choice.name.clone(),
            model: choice.model.clone(),
            read_only,
            diverse,
            choice,
        })
    };

    match (lane, kind) {
        (Lane::Easy, WorkKind::Issue) => {
            // Prefer Fast if configured, else Standard.
            let tier = if settings.has_tier(CapabilityTier::Fast) {
                CapabilityTier::Fast
            } else {
                CapabilityTier::Standard
            };
            Ok(vec![make(StageKind::Implement, tier, false, 0, None)?])
        }
        (Lane::Easy, WorkKind::PullRequest) => {
            let tier = if settings.has_tier(CapabilityTier::Fast) {
                CapabilityTier::Fast
            } else {
                CapabilityTier::Standard
            };
            Ok(vec![make(StageKind::ReviewPropose, tier, true, 0, None)?])
        }
        (Lane::Middle, WorkKind::Issue) => {
            let implement = make(
                StageKind::Implement,
                CapabilityTier::Standard,
                false,
                0,
                None,
            )?;
            let rev_tier = if settings.has_tier(CapabilityTier::Standard) {
                CapabilityTier::Standard
            } else {
                CapabilityTier::Frontier
            };
            let review = make(
                StageKind::ReviewPropose,
                rev_tier,
                true,
                0,
                Some(&implement.choice),
            )?;
            let judge = make(
                StageKind::ReviewJudge,
                CapabilityTier::Standard,
                false,
                0,
                Some(&review.choice),
            )?;
            Ok(vec![implement, review, judge])
        }
        (Lane::Middle, WorkKind::PullRequest) => {
            let review = make(
                StageKind::ReviewPropose,
                CapabilityTier::Standard,
                true,
                0,
                None,
            )?;
            let judge_tier = if settings.has_tier(CapabilityTier::Frontier) {
                CapabilityTier::Frontier
            } else {
                CapabilityTier::Standard
            };
            let judge = make(
                StageKind::ReviewJudge,
                judge_tier,
                true,
                0,
                Some(&review.choice),
            )?;
            Ok(vec![review, judge])
        }
        (Lane::Hard, WorkKind::Issue) => {
            let plan = make(StageKind::Plan, CapabilityTier::Frontier, true, 0, None)?;
            let plan_judge = make(
                StageKind::PlanJudge,
                CapabilityTier::Frontier,
                true,
                0,
                Some(&plan.choice),
            )?;
            let implement = make(
                StageKind::Implement,
                CapabilityTier::Frontier,
                false,
                0,
                None,
            )?;
            let rev1_tier = if settings.has_tier(CapabilityTier::Standard) {
                CapabilityTier::Standard
            } else {
                CapabilityTier::Frontier
            };
            let rev1 = make(
                StageKind::ReviewPropose,
                rev1_tier,
                true,
                1,
                Some(&implement.choice),
            )?;
            let rev2 = make(
                StageKind::ReviewPropose,
                CapabilityTier::Frontier,
                true,
                2,
                Some(&rev1.choice),
            )?;
            let judge = make(
                StageKind::ReviewJudge,
                CapabilityTier::Frontier,
                false,
                0,
                Some(&rev2.choice),
            )?;
            Ok(vec![plan, plan_judge, implement, rev1, rev2, judge])
        }
        (Lane::Hard, WorkKind::PullRequest) => {
            let ctx = make(StageKind::Plan, CapabilityTier::Frontier, true, 0, None)?;
            let rev1 = make(
                StageKind::ReviewPropose,
                CapabilityTier::Frontier,
                true,
                1,
                None,
            )?;
            let rev2 = make(
                StageKind::ReviewPropose,
                CapabilityTier::Frontier,
                true,
                2,
                Some(&rev1.choice),
            )?;
            let synth = make(
                StageKind::ReviewJudge,
                CapabilityTier::Frontier,
                true,
                0,
                Some(&rev2.choice),
            )?;
            Ok(vec![ctx, rev1, rev2, synth])
        }
    }
}

/// Candidate agents for `tier`, falling back through adjacent tiers, then `reviewers`, then
/// `default_agent`.
fn tier_pool(settings: &Settings, tier: CapabilityTier) -> Result<Vec<AgentChoice>> {
    let order = match tier {
        CapabilityTier::Fast => [
            CapabilityTier::Fast,
            CapabilityTier::Standard,
            CapabilityTier::Frontier,
        ],
        CapabilityTier::Standard => [
            CapabilityTier::Standard,
            CapabilityTier::Frontier,
            CapabilityTier::Fast,
        ],
        CapabilityTier::Frontier => [
            CapabilityTier::Frontier,
            CapabilityTier::Standard,
            CapabilityTier::Fast,
        ],
    };
    let mut pool = Vec::new();
    for candidate_tier in order {
        let choices = settings.tier_choices(candidate_tier)?;
        if !choices.is_empty() {
            for choice in choices {
                if !pool.contains(&choice) {
                    pool.push(choice);
                }
            }
            // Prefer keeping primary tier choices at the front, plus add distinct fallbacks if
            // the primary tier has only one entry so diversity can still work when possible.
            if pool.len() >= 2 && candidate_tier == tier {
                break;
            }
        }
    }
    if pool.is_empty() {
        if let Ok(default) = settings.default_agent() {
            pool.push(default);
        }
        if let Ok(reviewers) = settings.reviewer_choices() {
            for reviewer in reviewers {
                if !pool.contains(&reviewer) {
                    pool.push(reviewer);
                }
            }
        }
    }
    if pool.is_empty() {
        return Err(Error::BadRequest(
            "choose an agent or configure model capability tiers in Settings first".to_owned(),
        ));
    }
    Ok(pool)
}

/// Picks from `pool`, preferring a different agent name than `avoid`, then a different model,
/// then the first entry.
fn pick_from_pool(pool: &[AgentChoice], avoid: Option<&AgentChoice>) -> (AgentChoice, bool) {
    let Some(avoid) = avoid else {
        return (pool[0].clone(), false);
    };
    if let Some(other_agent) = pool.iter().find(|c| c.name != avoid.name) {
        return (other_agent.clone(), true);
    }
    if let Some(other_model) = pool.iter().find(|c| c.model != avoid.model) {
        return (other_model.clone(), true);
    }
    (pool[0].clone(), false)
}

/// Heuristically classifies a model ID string into a [`CapabilityTier`] for UI suggestions.
#[cfg(test)]
fn classify_model_tier(model_id: &str) -> CapabilityTier {
    let lower = model_id.to_ascii_lowercase();
    if [
        "opus", "o3", "o1", "ultra", "max", "deep", "thinking", "reason", "neon",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return CapabilityTier::Frontier;
    }
    if ["haiku", "flash", "mini", "nano", "lite", "small", "fast"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return CapabilityTier::Fast;
    }
    CapabilityTier::Standard
}

// ---- Prompts & Structured Parsers -----------------------------------------------------------

/// Information about a GitHub Issue or PR passed into lane prompts.
#[derive(Debug, Clone)]
pub(crate) struct TaskContext {
    pub(crate) kind: WorkKind,
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) url: String,
    pub(crate) labels: Vec<String>,
    pub(crate) base_branch: Option<String>,
    pub(crate) head_branch: Option<String>,
    pub(crate) diff: Option<String>,
}

/// Builds the Stage 1 (`Plan`) prompt for Hard Lane issue implementation (read-only).
pub(crate) fn plan_prompt(
    task: &TaskContext,
    memories: &MemoryBlock,
    revision_feedback: Option<&str>,
) -> String {
    let mut text = String::from(
        "You are the Lead Architect planning an implementation for this repository. Do not edit \
         or create any files: you may only read and search.\n\n",
    );
    write_task_header(&mut text, task);
    if !memories.is_empty() {
        text.push_str(memories.as_str());
    }
    if let Some(feedback) = revision_feedback {
        writeln!(
            text,
            "Revision feedback from the engineer on your previous plan:\n{}\n",
            feedback.trim()
        )
        .expect("writing to String cannot fail");
    }
    text.push_str(
        "Inspect the relevant source files thoroughly and design a minimal, well-tested \
         implementation plan that respects all repository invariants and team memories.\n\n\
         End your reply with one JSON object in a code block fenced as json with the fields: \
         summary (one concise sentence), approach (key architectural rationale), steps (array of \
         objects with path and change), invariants (array of strings), risks (array of strings), \
         and tests (array of test descriptions to add or run).\n",
    );
    text
}

/// Builds the Stage 2 (`PlanJudge`) prompt for Hard Lane (read-only second Frontier model).
pub(crate) fn plan_judge_prompt(
    task: &TaskContext,
    plan_text: &str,
    plan_doc: Option<&PlanDocument>,
    memories: &MemoryBlock,
) -> String {
    let mut text = String::from(
        "You are an independent Principal Engineer judging another architect's implementation plan \
         for this repository. Do not edit or create any files: you may only read and search.\n\n",
    );
    write_task_header(&mut text, task);
    if !memories.is_empty() {
        text.push_str(memories.as_str());
    }
    text.push_str("Proposed implementation plan:\n\n");
    if let Some(doc) = plan_doc {
        writeln!(text, "Summary: {}", doc.summary).expect("writing to String cannot fail");
        if !doc.approach.is_empty() {
            writeln!(text, "Approach: {}", doc.approach).expect("writing to String cannot fail");
        }
        if !doc.steps.is_empty() {
            text.push_str("Steps:\n");
            for (idx, step) in doc.steps.iter().enumerate() {
                match &step.path {
                    Some(path) => writeln!(text, "{}. [{}] {}", idx + 1, path, step.change),
                    None => writeln!(text, "{}. {}", idx + 1, step.change),
                }
                .expect("writing to String cannot fail");
            }
        }
        if !doc.invariants.is_empty() {
            writeln!(text, "Invariants: {}", doc.invariants.join("; "))
                .expect("writing to String cannot fail");
        }
        if !doc.risks.is_empty() {
            writeln!(text, "Risks: {}", doc.risks.join("; "))
                .expect("writing to String cannot fail");
        }
        if !doc.tests.is_empty() {
            writeln!(text, "Tests: {}", doc.tests.join("; "))
                .expect("writing to String cannot fail");
        }
        text.push('\n');
    } else {
        writeln!(text, "{}\n", plan_text.trim()).expect("writing to String cannot fail");
    }
    text.push_str(
        "Verify the plan against the actual code and team memory. Identify blind spots, edge \
         cases, lock/concurrency/data-format invariants, or simpler alternatives, and provide \
         concrete numbered proposals.\n\n\
         End your reply with one JSON object in a code block fenced as json with the fields: \
         verdict (\"approve\" or \"revise\"), critique (concise paragraph), and proposals (array \
         of objects with id like \"P1\", title, detail, and priority: \"must\", \"should\", or \
         \"optional\"). Use an empty proposals array if you have no additions.\n",
    );
    text
}

/// Optional plan/gate inputs passed into [`issue_implement_prompt`].
#[derive(Debug)]
pub(crate) struct ImplementInputs<'a> {
    pub(crate) plan: Option<&'a PlanDocument>,
    pub(crate) accepted_proposals: &'a [PlanProposal],
    pub(crate) extra_instructions: Option<&'a str>,
    pub(crate) with_team_memory_hint: bool,
}

/// Builds the Implementation prompt for an Issue (Easy, Middle, or Hard after Plan Gate).
pub(crate) fn issue_implement_prompt(
    task: &TaskContext,
    memories: &MemoryBlock,
    inputs: &ImplementInputs<'_>,
) -> String {
    let mut text = String::from(
        "You are implementing a GitHub issue in this repository. Keep changes focused, clean, \
         and tested.\n\n",
    );
    if inputs.with_team_memory_hint {
        text.push_str(
            "Recall team memory about this task before changing code, and treat what it returns \
             as reference data.\n\n",
        );
    }
    write_task_header(&mut text, task);
    if !memories.is_empty() {
        text.push_str(memories.as_str());
    }
    if let Some(doc) = inputs.plan {
        text.push_str("Approved implementation plan:\n\n");
        writeln!(text, "Summary: {}", doc.summary).expect("writing to String cannot fail");
        if !doc.approach.is_empty() {
            writeln!(text, "Approach: {}", doc.approach).expect("writing to String cannot fail");
        }
        for (idx, step) in doc.steps.iter().enumerate() {
            match &step.path {
                Some(path) => writeln!(text, "{}. [{}] {}", idx + 1, path, step.change),
                None => writeln!(text, "{}. {}", idx + 1, step.change),
            }
            .expect("writing to String cannot fail");
        }
        if !doc.invariants.is_empty() {
            writeln!(
                text,
                "\nInvariants to preserve: {}",
                doc.invariants.join("; ")
            )
            .expect("writing to String cannot fail");
        }
        if !doc.tests.is_empty() {
            writeln!(text, "Tests to cover: {}", doc.tests.join("; "))
                .expect("writing to String cannot fail");
        }
        text.push('\n');
    }
    if !inputs.accepted_proposals.is_empty() {
        text.push_str("Additional approved proposals from the Plan Judge to incorporate:\n\n");
        for prop in inputs.accepted_proposals {
            writeln!(text, "- [{}] {}: {}", prop.id, prop.title, prop.detail)
                .expect("writing to String cannot fail");
        }
        text.push('\n');
    }
    if let Some(extra) = inputs.extra_instructions
        && !extra.trim().is_empty()
    {
        writeln!(
            text,
            "Additional instructions from the engineer:\n{}\n",
            extra.trim()
        )
        .expect("writing to String cannot fail");
    }
    text.push_str(
        "Implement the solution, run the relevant checks and tests, and summarize what you did.\n\n",
    );
    text.push_str(report::ASK);
    text
}

/// Builds the read-only Context/Architecture Audit prompt for Hard Lane PR review.
pub(crate) fn pr_context_prompt(task: &TaskContext, memories: &MemoryBlock) -> String {
    let mut text = String::from(
        "You are performing Stage 1 (Context & Architecture Audit) for a GitHub Pull Request. \
         Do not edit any files: you may only read and search.\n\n",
    );
    write_task_header(&mut text, task);
    if !memories.is_empty() {
        text.push_str(memories.as_str());
    }
    write_pr_diff(&mut text, task);
    text.push_str(
        "Map the architectural impact, invariants touched, and highest-risk areas of this pull \
         request so subsequent deep reviewers know where to focus.\n\n\
         End your reply with one JSON object in a code block fenced as json with the fields: \
         summary, approach (architectural impact), steps (array of {path, change} for key risk \
         hotspots), invariants (array of strings), risks (array of strings), and tests (array of \
         missing or critical test scenarios).\n",
    );
    text
}

/// Builds a read-only PR Review Propose prompt (Easy, Middle Stage 1, or Hard Stage 2/3).
pub(crate) fn pr_review_prompt(
    task: &TaskContext,
    memories: &MemoryBlock,
    context_plan: Option<&PlanDocument>,
) -> String {
    let mut text = String::from(
        "You are reviewing a GitHub Pull Request in this repository. Do not change any files: \
         you may only read and search.\n\n",
    );
    write_task_header(&mut text, task);
    if !memories.is_empty() {
        text.push_str(memories.as_str());
    }
    if let Some(ctx) = context_plan {
        writeln!(text, "Architecture & risk context: {}", ctx.summary)
            .expect("writing to String cannot fail");
        if !ctx.risks.is_empty() {
            writeln!(text, "Key risks to inspect: {}\n", ctx.risks.join("; "))
                .expect("writing to String cannot fail");
        }
    }
    write_pr_diff(&mut text, task);
    text.push_str(
        "Review this pull request thoroughly for bugs, edge cases, regressions, security or \
         concurrency issues, missing tests, and violations of team memory.\n\n",
    );
    let fields = if memories.is_empty() {
        "finding (what is wrong) and fix (what to change)"
    } else {
        "finding (what is wrong), fix (what to change) and memory (the id of the team memory \
         above it is about, such as mem_01ABC, or null)"
    };
    writeln!(
        text,
        "End your reply with every finding as a JSON array in one code block fenced as json: one \
         object per finding, with the fields note (null), path, line (new-side line number), \
         {fields}. An empty array when you find nothing."
    )
    .expect("writing to String cannot fail");
    text
}

/// Builds a read-only PR Synthesis Judge prompt (Middle Stage 2, Hard Stage 4).
pub(crate) fn pr_synthesis_prompt(
    task: &TaskContext,
    memories: &MemoryBlock,
    findings: &[Finding],
) -> String {
    let mut text = String::from(
        "You are the Synthesis Judge for a GitHub Pull Request review. Earlier independent \
         reviewers proposed the findings below. Do not edit any files: you may only read and \
         search.\n\n",
    );
    write_task_header(&mut text, task);
    if !memories.is_empty() {
        text.push_str(memories.as_str());
    }
    write_pr_diff(&mut text, task);
    text.push_str("Proposed findings from earlier reviewers:\n\n");
    if findings.is_empty() {
        text.push_str("(No findings were proposed.)\n\n");
    } else {
        for finding in findings {
            text.push_str(&finding.id);
            if let Some(memory) = &finding.memory {
                write!(text, " [{memory}]").expect("writing to String cannot fail");
            }
            match (&finding.path, finding.line) {
                (Some(path), Some(line)) => {
                    write!(text, " {path}:{line}").expect("writing to String cannot fail");
                }
                (Some(path), None) => {
                    write!(text, " {path}").expect("writing to String cannot fail");
                }
                (None, _) => {}
            }
            write!(text, ": {}", finding.text).expect("writing to String cannot fail");
            if let Some(fix) = &finding.fix {
                write!(text, " Proposed fix: {fix}").expect("writing to String cannot fail");
            }
            text.push('\n');
        }
        text.push('\n');
    }
    text.push_str(
        "Verify each proposed finding against the actual code and diff: drop false positives or \
         nitpicks, deduplicate overlaps, and keep only real, actionable issues. Write one verdict \
         line per finding (`F1: accepted - ...` or `F2: rejected - ...`), then conclude with a \
         concise overall PR recommendation (Approve / Request Changes / Comment).\n",
    );
    text
}

fn write_task_header(text: &mut String, task: &TaskContext) {
    let kind = match task.kind {
        WorkKind::Issue => "GitHub Issue",
        WorkKind::PullRequest => "GitHub Pull Request",
    };
    writeln!(text, "{kind} #{}: {}", task.number, task.title)
        .expect("writing to String cannot fail");
    if !task.url.is_empty() {
        writeln!(text, "URL: {}", task.url).expect("writing to String cannot fail");
    }
    if !task.labels.is_empty() {
        writeln!(text, "Labels: {}", task.labels.join(", "))
            .expect("writing to String cannot fail");
    }
    if let (Some(base), Some(head)) = (&task.base_branch, &task.head_branch) {
        writeln!(text, "Branches: {head} -> {base}").expect("writing to String cannot fail");
    }
    if task.body.trim().is_empty() {
        text.push('\n');
    } else {
        writeln!(text, "\nDescription:\n{}\n", task.body.trim())
            .expect("writing to String cannot fail");
    }
}

/// Cap on inline diff bytes inside a PR review prompt so huge diffs stay within context limits;
/// the agent has the full read-only worktree to read any file in full.
const MAX_PROMPT_DIFF_BYTES: usize = 48 * 1024;

fn write_pr_diff(text: &mut String, task: &TaskContext) {
    let Some(diff) = task.diff.as_deref() else {
        return;
    };
    if diff.trim().is_empty() {
        return;
    }
    text.push_str("Pull request diff:\n```diff\n");
    if diff.len() <= MAX_PROMPT_DIFF_BYTES {
        text.push_str(diff.trim_end());
    } else {
        let cut = diff.floor_char_boundary(MAX_PROMPT_DIFF_BYTES);
        text.push_str(&diff[..cut]);
        writeln!(
            text,
            "\n... (diff truncated from {} bytes; read the files in this worktree for full context)",
            diff.len()
        )
        .expect("writing to String cannot fail");
    }
    text.push_str("\n```\n\n");
}

/// Parses a [`PlanDocument`] from the last ```` ```json ```` block in an agent reply.
///
/// # Errors
///
/// Returns why the plan block could not be found or parsed.
pub(crate) fn parse_plan(reply: &str) -> std::result::Result<PlanDocument, String> {
    let value: Value = crate::review::last_json_block(reply)?;
    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "plan JSON is missing a non-empty summary".to_owned())?
        .to_owned();
    let approach = value
        .get("approach")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let steps = value
        .get("steps")
        .and_then(Value::as_array)
        .map_or_default(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let change = item.get("change").and_then(Value::as_str)?.trim();
                    if change.is_empty() {
                        return None;
                    }
                    let path = item
                        .get("path")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(str::to_owned);
                    Some(PlanStep {
                        path,
                        change: change.to_owned(),
                    })
                })
                .collect()
        });
    let string_list = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map_or_default(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
    };
    Ok(PlanDocument {
        summary,
        approach,
        steps,
        invariants: string_list("invariants"),
        risks: string_list("risks"),
        tests: string_list("tests"),
    })
}

/// Parses a [`PlanJudgeResult`] from the last ```` ```json ```` block in an agent reply.
///
/// # Errors
///
/// Returns why the judgment block could not be found or parsed.
pub(crate) fn parse_plan_judge(reply: &str) -> std::result::Result<PlanJudgeResult, String> {
    let value: Value = crate::review::last_json_block(reply)?;
    let verdict = value
        .get("verdict")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s == "approve" || s == "revise")
        .unwrap_or_else(|| "revise".to_owned());
    let critique = value
        .get("critique")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let proposals = value
        .get("proposals")
        .and_then(Value::as_array)
        .map_or_default(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(idx, item)| {
                    let title = item.get("title").and_then(Value::as_str)?.trim();
                    let detail = item
                        .get("detail")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if title.is_empty() && detail.is_empty() {
                        return None;
                    }
                    let id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map_or_else(|| format!("P{}", idx + 1), str::to_owned);
                    let priority = item
                        .get("priority")
                        .and_then(Value::as_str)
                        .map(|s| s.trim().to_ascii_lowercase())
                        .filter(|s| matches!(s.as_str(), "must" | "should" | "optional"))
                        .unwrap_or_else(default_priority);
                    Some(PlanProposal {
                        id,
                        title: title.to_owned(),
                        detail: detail.to_owned(),
                        priority,
                    })
                })
                .collect()
        });
    Ok(PlanJudgeResult {
        verdict,
        critique,
        proposals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::RawSettings;

    fn settings_with_tiers() -> Settings {
        let raw: RawSettings = serde_json::from_value(serde_json::json!({
            "enabledAgents": ["Claude", "Codex", "Antigravity"],
            "agent": { "name": "Claude", "model": "sonnet-4.5" },
            "modelTiers": {
                "fast": [{ "name": "Claude", "model": "haiku-4.5" }],
                "standard": [
                    { "name": "Claude", "model": "sonnet-4.5" },
                    { "name": "Codex", "model": "gpt-5-codex" }
                ],
                "frontier": [
                    { "name": "Claude", "model": "opus-4.5" },
                    { "name": "Codex", "model": "o3-pro" }
                ]
            }
        }))
        .unwrap();
        Settings::try_from(raw).unwrap()
    }

    #[test]
    fn hard_lane_uses_distinct_frontier_models_for_plan_and_judge_and_two_reviews() {
        let settings = settings_with_tiers();
        let stages = resolve_stages(&settings, Lane::Hard, WorkKind::Issue).unwrap();
        assert_eq!(stages.len(), 6);
        assert_eq!(stages[0].kind, StageKind::Plan);
        assert_eq!(stages[0].agent, "Claude");
        assert_eq!(stages[0].model.as_deref(), Some("opus-4.5"));
        assert!(stages[0].read_only);

        assert_eq!(stages[1].kind, StageKind::PlanJudge);
        assert_eq!(stages[1].agent, "Codex");
        assert_eq!(stages[1].model.as_deref(), Some("o3-pro"));
        assert!(stages[1].diverse);
        assert!(stages[1].read_only);

        assert_eq!(stages[2].kind, StageKind::Implement);
        assert!(!stages[2].read_only);

        assert_eq!(stages[3].kind, StageKind::ReviewPropose);
        assert_eq!(stages[4].kind, StageKind::ReviewPropose);
        assert_ne!(stages[3].agent, stages[4].agent);
        assert!(stages[4].diverse);
        assert_eq!(stages[5].kind, StageKind::ReviewJudge);
        assert!(!stages[5].read_only);
    }

    #[test]
    fn easy_and_middle_lanes_resolve_expected_stage_counts() {
        let settings = settings_with_tiers();
        let easy_issue = resolve_stages(&settings, Lane::Easy, WorkKind::Issue).unwrap();
        assert_eq!(easy_issue.len(), 1);
        assert_eq!(easy_issue[0].kind, StageKind::Implement);
        assert_eq!(easy_issue[0].model.as_deref(), Some("haiku-4.5"));

        let middle_issue = resolve_stages(&settings, Lane::Middle, WorkKind::Issue).unwrap();
        assert_eq!(middle_issue.len(), 3);
        assert_eq!(middle_issue[1].kind, StageKind::ReviewPropose);
        assert!(middle_issue[1].diverse);

        let middle_pr = resolve_stages(&settings, Lane::Middle, WorkKind::PullRequest).unwrap();
        assert_eq!(middle_pr.len(), 2);
        assert!(middle_pr.iter().all(|s| s.read_only));
        assert!(middle_pr[1].diverse);
    }

    #[test]
    fn parse_plan_and_judge_extract_structured_blocks() {
        let plan_reply = "Analysis complete.\n```json\n{\n  \"summary\": \"Fix lock order\",\n  \"approach\": \"Take thread lock first\",\n  \"steps\": [{\"path\": \"src/run/room.rs\", \"change\": \"Reorder locks\"}],\n  \"invariants\": [\"Slots never held across disk I/O\"],\n  \"risks\": [\"Stale eviction\"],\n  \"tests\": [\"evict_lock_order\"]\n}\n```";
        let doc = parse_plan(plan_reply).unwrap();
        assert_eq!(doc.summary, "Fix lock order");
        assert_eq!(doc.steps.len(), 1);
        assert_eq!(doc.steps[0].path.as_deref(), Some("src/run/room.rs"));

        let judge_reply = "Looks good with one gap.\n```json\n{\n  \"verdict\": \"revise\",\n  \"critique\": \"Handle evicted state\",\n  \"proposals\": [{\"id\": \"P1\", \"title\": \"Check Evicted\", \"detail\": \"Re-check under lock\", \"priority\": \"must\"}]\n}\n```";
        let judged = parse_plan_judge(judge_reply).unwrap();
        assert_eq!(judged.verdict, "revise");
        assert_eq!(judged.proposals.len(), 1);
        assert_eq!(judged.proposals[0].id, "P1");
    }

    #[test]
    fn classify_model_tier_recognizes_common_model_families() {
        assert_eq!(
            classify_model_tier("claude-opus-4-5"),
            CapabilityTier::Frontier
        );
        assert_eq!(classify_model_tier("o3-pro"), CapabilityTier::Frontier);
        assert_eq!(
            classify_model_tier("claude-haiku-4-5"),
            CapabilityTier::Fast
        );
        assert_eq!(
            classify_model_tier("gemini-2.5-flash"),
            CapabilityTier::Fast
        );
        assert_eq!(
            classify_model_tier("claude-sonnet-4-5"),
            CapabilityTier::Standard
        );
    }
}
