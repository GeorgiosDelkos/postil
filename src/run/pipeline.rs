//! Multi-stage Lane Pipeline orchestration (`Easy`, `Middle`, `Hard`) for implementing GitHub
//! Issues and reviewing Pull Requests inside isolated Postil worktrees.
//!
//! Each pipeline belongs to one thread (keyed by its first run id, e.g., `R12`), persists its
//! state in `<git dir>/postil/pipelines.json` so Plan Gates and stage history survive restarts,
//! and executes every stage as a real Postil [`Run`] in the thread's worktree.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::agents::{self, AgentChoice};
use crate::error::{Error, Result, error_chain};
use crate::files::{Stored, is_newer_version, load_versioned, save_json_relaxed};
use crate::github::{self, PrReviewComment, PrReviewVerdict};
use crate::lane::{
    self, Lane, PlanDocument, PlanJudgeResult, PlanProposal, ResolvedStage, StageKind, TaskContext,
    WorkKind,
};
use crate::memory::{HubMemoryHit, HubRecallRequest};
use crate::notes::Prompt;
use crate::review::{self, Finding, MemoryBlock, MemoryNote, Work};
use crate::run::log::{Origin, Run};
use crate::run::{Content, Conversation, NewRun, Place, Runner, lock};
use crate::settings::{CapabilityTier, Settings, TeamMemory};
use crate::thread::Mode;

const PIPELINES_VERSION: u32 = 1;

/// Overall status of a Lane pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PipelineStatus {
    Running,
    AwaitingGate,
    Completed,
    Failed,
    Stopped,
}

/// Execution status of an individual stage inside a pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum StageStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Skipped,
}

/// Persisted record of one stage inside a [`PipelineRun`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PipelineStageRecord {
    pub(crate) index: usize,
    pub(crate) kind: StageKind,
    pub(crate) label: String,
    pub(crate) tier: CapabilityTier,
    pub(crate) agent: String,
    #[serde(default)]
    pub(crate) model: Option<String>,
    pub(crate) command: String,
    pub(crate) read_only: bool,
    #[serde(default)]
    pub(crate) diverse: bool,
    #[serde(default)]
    pub(crate) run_id: Option<String>,
    pub(crate) status: StageStatus,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

/// Record of a PR review submitted to GitHub from Postil.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrSubmitted {
    pub(crate) verdict: PrReviewVerdict,
    pub(crate) comments_count: usize,
    pub(crate) submitted_at: u64,
}

/// Full persisted state of one Lane pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PipelineRun {
    /// Thread & first run id (`R12`).
    pub(crate) id: String,
    pub(crate) lane: Lane,
    pub(crate) kind: WorkKind,
    pub(crate) number: u64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) labels: Vec<String>,
    #[serde(default)]
    pub(crate) base_branch: Option<String>,
    #[serde(default)]
    pub(crate) head_branch: Option<String>,
    /// Cached PR diff for PR review pipelines.
    #[serde(default)]
    pub(crate) pr_diff: Option<String>,
    pub(crate) human_gate: bool,
    pub(crate) use_memory: bool,
    pub(crate) status: PipelineStatus,
    /// Index in `stages` currently running or waiting at the gate.
    pub(crate) current_stage: usize,
    pub(crate) stages: Vec<PipelineStageRecord>,
    #[serde(default)]
    pub(crate) plan: Option<PlanDocument>,
    #[serde(default)]
    pub(crate) plan_reply: Option<String>,
    #[serde(default)]
    pub(crate) plan_judge: Option<PlanJudgeResult>,
    #[serde(default)]
    pub(crate) accepted_proposals: Vec<String>,
    #[serde(default)]
    pub(crate) extra_instructions: Option<String>,
    #[serde(default)]
    pub(crate) findings: Vec<Finding>,
    #[serde(default)]
    pub(crate) memories: Vec<HubMemoryHit>,
    #[serde(default)]
    pub(crate) implement_run_id: Option<String>,
    /// Superseded run IDs (e.g. from prior Plan Gate revisions) kept so their logs aren't purged.
    #[serde(default)]
    pub(crate) history_run_ids: Vec<String>,
    #[serde(default)]
    pub(crate) pr_review_submitted: Option<PrSubmitted>,
    #[serde(default)]
    pub(crate) created_at: u64,
}

impl PipelineRun {
    fn task_context(&self) -> TaskContext {
        TaskContext {
            kind: self.kind,
            number: self.number,
            title: self.title.clone(),
            body: self.body.clone(),
            url: self.url.clone(),
            labels: self.labels.clone(),
            base_branch: self.base_branch.clone(),
            head_branch: self.head_branch.clone(),
            diff: self.pr_diff.clone(),
        }
    }

    fn memory_block(&self) -> MemoryBlock {
        let notes: Vec<MemoryNote> = self
            .memories
            .iter()
            .map(|hit| MemoryNote {
                id: hit.id.clone(),
                kind: hit.kind.clone().unwrap_or_else(|| "context".to_owned()),
                author: "team".to_owned(),
                summary: hit.summary.clone(),
                body: hit.summary.clone(),
            })
            .collect();
        MemoryBlock::new(notes)
    }

    fn stage_choice(&self, index: usize) -> Option<AgentChoice> {
        let stage = self.stages.get(index)?;
        Some(AgentChoice {
            name: stage.agent.clone(),
            command: stage.command.clone(),
            model: stage.model.clone(),
        })
    }

    fn accepted_proposal_items(&self) -> Vec<PlanProposal> {
        let Some(judge) = &self.plan_judge else {
            return Vec::new();
        };
        judge
            .proposals
            .iter()
            .filter(|p| self.accepted_proposals.contains(&p.id))
            .cloned()
            .collect()
    }
}

/// Per-stage agent/model override sent when launching a customized lane roster.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StageOverride {
    pub(crate) agent: String,
    #[serde(default)]
    pub(crate) model: Option<String>,
}

/// Request payload to start a new Lane pipeline (`POST /api/lane/start`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StartPipelineRequest {
    pub(crate) lane: Lane,
    #[serde(default)]
    pub(crate) base_lane: Option<Lane>,
    #[serde(default)]
    pub(crate) stage_overrides: Vec<StageOverride>,
    pub(crate) kind: WorkKind,
    pub(crate) number: u64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) labels: Vec<String>,
    #[serde(default)]
    pub(crate) base_branch: Option<String>,
    #[serde(default)]
    pub(crate) head_branch: Option<String>,
    #[serde(default)]
    pub(crate) human_gate: Option<bool>,
    #[serde(default)]
    pub(crate) memories: Vec<HubMemoryHit>,
}

/// Request payload to approve a paused Hard Lane Plan Gate (`POST /api/lane/gate/approve`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApproveGateRequest {
    pub(crate) pipeline: String,
    #[serde(default)]
    pub(crate) accepted_proposals: Vec<String>,
    #[serde(default)]
    pub(crate) extra_instructions: Option<String>,
}

/// Request payload to ask the Hard Lane Planner to revise its plan (`POST /api/lane/gate/revise`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReviseGateRequest {
    pub(crate) pipeline: String,
    pub(crate) feedback: String,
}

/// Request payload to submit a PR review to GitHub (`POST /api/pr/submit-review`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SubmitPrReviewRequest {
    #[serde(default)]
    pub(crate) pipeline: Option<String>,
    pub(crate) number: u64,
    pub(crate) verdict: PrReviewVerdict,
    #[serde(default)]
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) comments: Vec<PrReviewComment>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelinesFile {
    version: u32,
    pipelines: Vec<PipelineRun>,
}

/// Loads persisted pipelines from `<git dir>/postil/pipelines.json`.
/// Any pipeline that was still `Running` when Postil exited or crashed is marked `Stopped` so it
/// is never stranded permanently in `Running`.
pub(super) fn load_pipelines(file: &Path) -> HashMap<String, PipelineRun> {
    match load_versioned::<PipelinesFile>(file, PIPELINES_VERSION) {
        Stored::Current(stored) => {
            let mut changed = false;
            let mut map = HashMap::with_capacity(stored.pipelines.len());
            for mut p in stored.pipelines {
                if p.status == PipelineStatus::Running {
                    p.status = PipelineStatus::Stopped;
                    for stage in &mut p.stages {
                        if stage.status == StageStatus::Running {
                            stage.status = StageStatus::Failed;
                            stage.error = Some("interrupted when Postil closed".to_owned());
                        }
                    }
                    changed = true;
                }
                map.insert(p.id.clone(), p);
            }
            if changed {
                save_pipelines(file, &map);
            }
            map
        }
        Stored::Newer | Stored::Nothing => HashMap::new(),
    }
}

fn save_pipelines(file: &Path, map: &HashMap<String, PipelineRun>) {
    if is_newer_version(file, PIPELINES_VERSION) {
        return;
    }
    let mut list: Vec<PipelineRun> = map.values().cloned().collect();
    list.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    let stored = PipelinesFile {
        version: PIPELINES_VERSION,
        pipelines: list,
    };
    drop(save_json_relaxed(file, &stored));
}

fn apply_stage_output(
    pipeline: &mut PipelineRun,
    current: usize,
    ended_id: &str,
    reply_text: &str,
) -> bool {
    match pipeline.stages[current].kind {
        StageKind::Plan => {
            pipeline.plan_reply = Some(reply_text.to_owned());
            match lane::parse_plan(reply_text) {
                Ok(doc) => pipeline.plan = Some(doc),
                Err(err) => pipeline.stages[current].error = Some(err),
            }
            false
        }
        StageKind::PlanJudge => {
            match lane::parse_plan_judge(reply_text) {
                Ok(judged) => {
                    // Default-select `must` and `should` proposals for convenience at the gate.
                    pipeline.accepted_proposals = judged
                        .proposals
                        .iter()
                        .filter(|p| p.priority != "optional")
                        .map(|p| p.id.clone())
                        .collect();
                    pipeline.plan_judge = Some(judged);
                }
                Err(err) => pipeline.stages[current].error = Some(err),
            }
            pipeline.human_gate
        }
        StageKind::Implement => {
            pipeline.implement_run_id = Some(ended_id.to_owned());
            false
        }
        StageKind::ReviewPropose => {
            let mem_block = pipeline.memory_block();
            let next_num = pipeline.findings.len() + 1;
            match review::findings_in(reply_text, next_num, &mem_block) {
                Ok(found) => pipeline.findings.extend(found),
                Err(err) => pipeline.stages[current].error = Some(err),
            }
            false
        }
        StageKind::ReviewJudge => {
            if pipeline.kind == WorkKind::PullRequest {
                let verdicts = lane::parse_pr_verdicts(reply_text);
                if !verdicts.is_empty() {
                    pipeline.findings.retain(|f| {
                        verdicts
                            .get(&f.id.to_ascii_uppercase())
                            .copied()
                            .unwrap_or(true)
                    });
                }
            }
            false
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Runner {
    /// Lists every pipeline for this repository, newest first.
    pub(crate) fn pipelines(&self) -> Vec<PipelineRun> {
        let mut list: Vec<PipelineRun> = lock(&self.pipelines).values().cloned().collect();
        list.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        list
    }

    /// Returns one pipeline by its root thread id.
    pub(crate) fn pipeline(&self, id: &str) -> Option<PipelineRun> {
        lock(&self.pipelines).get(id).cloned()
    }

    /// Run ids owned by any pipeline (so `delete_runs_without_notes` keeps their logs).
    pub(super) fn pipeline_run_ids(&self) -> HashSet<String> {
        let guard = lock(&self.pipelines);
        let mut ids = HashSet::new();
        for p in guard.values() {
            ids.insert(p.id.clone());
            for run_id in &p.history_run_ids {
                ids.insert(run_id.clone());
            }
            for stage in &p.stages {
                if let Some(run_id) = &stage.run_id {
                    ids.insert(run_id.clone());
                }
            }
        }
        ids
    }

    /// Returns the root thread id if `run_id` is any stage of a pipeline (so `thread_of`
    /// resolves stage runs `R2..Rn` back to the pipeline's root worktree thread `R1`).
    pub(super) fn thread_of_pipeline_run(&self, run_id: &str) -> Option<String> {
        let guard = lock(&self.pipelines);
        for p in guard.values() {
            if p.id == run_id
                || p.history_run_ids.iter().any(|id| id == run_id)
                || p.stages
                    .iter()
                    .any(|st| st.run_id.as_deref() == Some(run_id))
            {
                return Some(p.id.clone());
            }
        }
        None
    }

    /// Whether any pipeline is currently running (including inter-stage transitions).
    pub(super) fn has_running_pipeline(&self) -> bool {
        lock(&self.pipelines)
            .values()
            .any(|p| p.status == PipelineStatus::Running)
    }

    /// Marks every `Running` pipeline as `Stopped` when the project or app closes.
    pub(super) fn stop_all_pipelines(&self) {
        let mut map = lock(&self.pipelines);
        let mut changed = false;
        for pipeline in map.values_mut() {
            if pipeline.status == PipelineStatus::Running {
                pipeline.status = PipelineStatus::Stopped;
                if let Some(stage) = pipeline.stages.get_mut(pipeline.current_stage)
                    && stage.status == StageStatus::Running
                {
                    stage.status = StageStatus::Failed;
                    stage.error = Some("stopped: Postil closed the project".to_owned());
                }
                changed = true;
            }
        }
        if changed {
            save_pipelines(&self.repo.pipelines_file(), &map);
        }
    }

    /// Fails a running pipeline stage when `Runner::start` or `Runner::launch` fails before the
    /// agent ever enters `on_event` (e.g., worktree creation, spawn, or session open error).
    pub(super) fn fail_pipeline_run(&self, thread_id: &str, run_id: &str, error: String) {
        let mut map = lock(&self.pipelines);
        let Some(pipeline) = map.get_mut(thread_id) else {
            return;
        };
        if pipeline.status != PipelineStatus::Running {
            return;
        }
        let current = pipeline.current_stage;
        if pipeline
            .stages
            .get(current)
            .and_then(|s| s.run_id.as_deref())
            != Some(run_id)
        {
            return;
        }
        pipeline.status = PipelineStatus::Failed;
        pipeline.stages[current].status = StageStatus::Failed;
        pipeline.stages[current].error = Some(error);
        save_pipelines(&self.repo.pipelines_file(), &map);
    }

    /// Starts a new Lane pipeline (`Easy`, `Middle`, or `Hard`) for a GitHub Issue or PR.
    ///
    /// # Errors
    ///
    /// Returns errors from stage resolution, git `HEAD` lookup, or launching the first stage.
    #[expect(
        clippy::too_many_lines,
        reason = "coordinates stage resolution, PR diff fetch, memory recall, launch, and persistence in one flow"
    )]
    pub(crate) fn start_pipeline(
        self: &Arc<Self>,
        request: StartPipelineRequest,
        settings: &Settings,
    ) -> Result<PipelineRun> {
        if request.title.trim().is_empty() {
            return Err(Error::BadRequest(
                "pipeline task title cannot be blank".to_owned(),
            ));
        }
        let template_lane = match request.lane {
            Lane::Custom => match request.base_lane {
                Some(Lane::Custom) | None => Lane::Middle,
                Some(other) => other,
            },
            other => other,
        };
        let mut resolved: Vec<ResolvedStage> =
            lane::resolve_stages(settings, template_lane, request.kind)?;
        if !request.stage_overrides.is_empty() {
            if request.stage_overrides.len() != resolved.len() {
                return Err(Error::BadRequest(format!(
                    "stageOverrides length {} does not match {} stage(s) for {} lane",
                    request.stage_overrides.len(),
                    resolved.len(),
                    template_lane.label(),
                )));
            }
            for (stage, ov) in resolved.iter_mut().zip(request.stage_overrides.iter()) {
                let model = ov
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(ToOwned::to_owned);
                let choice = settings.choice(&ov.agent, model)?;
                stage.agent.clone_from(&choice.name);
                stage.model.clone_from(&choice.model);
                stage.choice = choice;
            }
            for idx in 1..resolved.len() {
                let prev = &resolved[idx - 1];
                let curr_diverse =
                    resolved[idx].agent != prev.agent || resolved[idx].model != prev.model;
                resolved[idx].diverse = curr_diverse;
            }
        }
        let fork = self.repo.head()?.ok_or(Error::NoCommit)?;
        let team_memory = settings.team_memory();
        let human_gate = request
            .human_gate
            .unwrap_or(settings.lanes().hard_human_gate);

        // Fetch PR diff when reviewing a PR via `gh pr diff`; fail explicitly rather than
        // auditing a local checkout without the PR diff.
        let pr_diff = if request.kind == WorkKind::PullRequest {
            let search_path = self.agents.wait();
            let gh = agents::find_program("gh", &search_path).ok_or(Error::NoGh)?;
            Some(github::pr_diff(
                &gh,
                self.repo.root(),
                &search_path,
                request.number,
            )?)
        } else {
            None
        };

        // Enrich with hippius-mem recalls only when team memory is explicitly enabled.
        let memories = if team_memory == TeamMemory::On {
            if !request.memories.is_empty() {
                request.memories
            } else if self.memory.is_ready() {
                let repo_name = self.repo.memory_name();
                let query = format!("{} {}", request.title, request.labels.join(" "));
                self.memory.recall_for_hub_repo(&HubRecallRequest {
                    repo_dir: self.repo.root(),
                    repo_name: repo_name.as_deref(),
                    query: &query,
                    limit: 6,
                    wait_for_check: false,
                })
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        let stages: Vec<PipelineStageRecord> = resolved
            .iter()
            .enumerate()
            .map(|(index, s)| PipelineStageRecord {
                index,
                kind: s.kind,
                label: s.label.clone(),
                tier: s.tier,
                agent: s.agent.clone(),
                model: s.model.clone(),
                command: s.choice.command.clone(),
                read_only: s.read_only,
                diverse: s.diverse,
                run_id: None,
                status: if index == 0 {
                    StageStatus::Running
                } else {
                    StageStatus::Pending
                },
                error: None,
            })
            .collect();

        let mut draft = PipelineRun {
            id: String::new(),
            lane: request.lane,
            kind: request.kind,
            number: request.number,
            title: request.title.clone(),
            body: request.body,
            url: request.url,
            labels: request.labels,
            base_branch: request.base_branch,
            head_branch: request.head_branch,
            pr_diff,
            human_gate,
            use_memory: team_memory == TeamMemory::On,
            status: PipelineStatus::Running,
            current_stage: 0,
            stages,
            plan: None,
            plan_reply: None,
            plan_judge: None,
            accepted_proposals: Vec::new(),
            extra_instructions: None,
            findings: Vec::new(),
            memories,
            implement_run_id: None,
            history_run_ids: Vec::new(),
            pr_review_submitted: None,
            created_at: now_secs(),
        };

        let first_prompt = self.build_stage_prompt(&draft, 0, None);
        let first_stage = &draft.stages[0];
        let first_agent = draft
            .stage_choice(0)
            .expect("pipeline always has at least one stage");
        let thread_mode = match draft.kind {
            WorkKind::Issue => Mode::Work,
            WorkKind::PullRequest => Mode::Read,
        };
        let thread_title = match draft.kind {
            WorkKind::Issue => format!("issue-{} {}", draft.number, draft.title),
            WorkKind::PullRequest => format!("pr-{} {}", draft.number, draft.title),
        };
        let stage_read_only = first_stage.read_only;

        let new = NewRun {
            agent: first_agent,
            memory: team_memory,
            origin: Origin::Notes,
            conversation: Conversation::Fresh,
            review: None,
            review_recall: None,
            stage_read_only,
            place: Place::New {
                fork,
                title: thread_title,
                mode: thread_mode,
            },
        };

        // Register the pipeline *before* `launch` spawns the agent thread so a fast-exiting
        // agent cannot race ahead of `self.pipelines` registration in `advance_pipeline`.
        let mut registered_id: Option<String> = None;
        let launch_result = self.launch(self.slots(), new, |run_id| {
            let root_id = run_id.to_owned();
            draft.id.clone_from(&root_id);
            draft.stages[0].run_id = Some(root_id.clone());
            if draft.stages[0].kind == StageKind::Implement {
                draft.implement_run_id = Some(root_id.clone());
            }
            {
                let mut map = lock(&self.pipelines);
                map.insert(root_id.clone(), draft.clone());
                save_pipelines(&self.repo.pipelines_file(), &map);
            }
            registered_id = Some(root_id);
            Ok(Content {
                notes: Vec::new(),
                prompt: Prompt {
                    text: first_prompt,
                    files: Vec::new(),
                },
            })
        });
        if let Err(error) = launch_result {
            if let Some(root_id) = registered_id {
                let mut map = lock(&self.pipelines);
                map.remove(&root_id);
                save_pipelines(&self.repo.pipelines_file(), &map);
            }
            return Err(error);
        }

        (self.notify)();
        Ok(draft)
    }

    /// Approves a paused Hard Lane Plan Gate and launches Stage 3 (`Implement`).
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] if the pipeline does not exist or is not waiting at the gate.
    pub(crate) fn approve_pipeline_gate(
        self: &Arc<Self>,
        request: ApproveGateRequest,
    ) -> Result<PipelineRun> {
        let (prior, next_index) = {
            let mut map = lock(&self.pipelines);
            let pipeline = map.get_mut(&request.pipeline).ok_or_else(|| {
                Error::BadRequest(format!("no pipeline with id {:?}", request.pipeline))
            })?;
            if pipeline.status != PipelineStatus::AwaitingGate {
                return Err(Error::BadRequest(format!(
                    "pipeline {} is not waiting at the Plan Gate",
                    request.pipeline
                )));
            }
            let prior = pipeline.clone();
            pipeline.accepted_proposals = request.accepted_proposals;
            pipeline.extra_instructions = request.extra_instructions;
            pipeline.status = PipelineStatus::Running;
            let next = pipeline.current_stage + 1;
            if next >= pipeline.stages.len() {
                pipeline.status = PipelineStatus::Completed;
                let snapshot = pipeline.clone();
                save_pipelines(&self.repo.pipelines_file(), &map);
                return Ok(snapshot);
            }
            pipeline.current_stage = next;
            pipeline.stages[next].status = StageStatus::Running;
            save_pipelines(&self.repo.pipelines_file(), &map);
            (prior, next)
        };
        if let Err(error) = self.launch_next_pipeline_stage(&request.pipeline, next_index, None) {
            let mut map = lock(&self.pipelines);
            map.insert(request.pipeline, prior);
            save_pipelines(&self.repo.pipelines_file(), &map);
            return Err(error);
        }
        Ok(self
            .pipeline(&request.pipeline)
            .expect("pipeline exists after launch"))
    }

    /// Sends feedback back to Stage 1 (`Plan`) from a paused Hard Lane Plan Gate.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] if the pipeline is not waiting at the gate or feedback is blank.
    pub(crate) fn revise_pipeline_gate(
        self: &Arc<Self>,
        request: &ReviseGateRequest,
    ) -> Result<PipelineRun> {
        if request.feedback.trim().is_empty() {
            return Err(Error::BadRequest(
                "provide revision feedback for the planner".to_owned(),
            ));
        }
        let prior = {
            let mut map = lock(&self.pipelines);
            let pipeline = map.get_mut(&request.pipeline).ok_or_else(|| {
                Error::BadRequest(format!("no pipeline with id {:?}", request.pipeline))
            })?;
            if pipeline.status != PipelineStatus::AwaitingGate {
                return Err(Error::BadRequest(format!(
                    "pipeline {} is not waiting at the Plan Gate",
                    request.pipeline
                )));
            }
            let prior = pipeline.clone();
            pipeline.status = PipelineStatus::Running;
            pipeline.current_stage = 0;
            pipeline.plan = None;
            pipeline.plan_reply = None;
            pipeline.plan_judge = None;
            pipeline.accepted_proposals.clear();
            pipeline.extra_instructions = None;
            pipeline.findings.clear();
            for (idx, stage) in pipeline.stages.iter_mut().enumerate() {
                if let Some(old_run) = stage.run_id.take()
                    && !pipeline.history_run_ids.contains(&old_run)
                {
                    pipeline.history_run_ids.push(old_run);
                }
                stage.status = if idx == 0 {
                    StageStatus::Running
                } else {
                    StageStatus::Pending
                };
                stage.error = None;
            }
            save_pipelines(&self.repo.pipelines_file(), &map);
            prior
        };
        if let Err(error) =
            self.launch_next_pipeline_stage(&request.pipeline, 0, Some(&request.feedback))
        {
            let mut map = lock(&self.pipelines);
            map.insert(request.pipeline.clone(), prior);
            save_pipelines(&self.repo.pipelines_file(), &map);
            return Err(error);
        }
        Ok(self
            .pipeline(&request.pipeline)
            .expect("pipeline exists after revise"))
    }

    /// Submits a PR review (selected findings + engineer notes) to GitHub via `gh`.
    ///
    /// # Errors
    ///
    /// [`Error::NoGh`] or [`Error::GitHub`] when `gh` is missing or fails.
    pub(crate) fn submit_pr_review(&self, request: &SubmitPrReviewRequest) -> Result<()> {
        let search_path = self.agents.wait();
        let gh = agents::find_program("gh", &search_path).ok_or(Error::NoGh)?;
        github::submit_pr_review(
            &gh,
            self.repo.root(),
            &search_path,
            &github::PrReviewSubmission {
                number: request.number,
                verdict: request.verdict,
                body: &request.body,
                comments: &request.comments,
            },
        )?;
        if let Some(pid) = &request.pipeline {
            let mut map = lock(&self.pipelines);
            if let Some(pipeline) = map.get_mut(pid) {
                pipeline.pr_review_submitted = Some(PrSubmitted {
                    verdict: request.verdict,
                    comments_count: request.comments.len(),
                    submitted_at: now_secs(),
                });
                save_pipelines(&self.repo.pipelines_file(), &map);
            }
        }
        (self.notify)();
        Ok(())
    }

    /// Marks any active pipeline in thread `thread_id` as Stopped when a run is cancelled.
    pub(super) fn stop_pipeline(&self, thread_id: &str) {
        let mut map = lock(&self.pipelines);
        if let Some(pipeline) = map.get_mut(thread_id)
            && pipeline.status == PipelineStatus::Running
        {
            pipeline.status = PipelineStatus::Stopped;
            if let Some(stage) = pipeline.stages.get_mut(pipeline.current_stage)
                && stage.status == StageStatus::Running
            {
                stage.status = StageStatus::Failed;
                stage.error = Some("stopped by engineer".to_owned());
            }
            save_pipelines(&self.repo.pipelines_file(), &map);
        }
    }

    /// Called on the ended agent's reader thread after a run finishes; if the run belongs to a
    /// live [`PipelineRun`], parses its structured output and advances to the next stage (or
    /// pauses at the Hard Lane Plan Gate).
    pub(super) fn advance_pipeline(
        self: &Arc<Self>,
        thread_id: &str,
        ended: &Arc<Run>,
        ended_well: bool,
    ) {
        let (ended_id, reply_text) = {
            let state = ended.state();
            (state.header.id.clone(), state.reply())
        };

        let next_step: Option<usize> = {
            let mut map = lock(&self.pipelines);
            let Some(pipeline) = map.get_mut(thread_id) else {
                return;
            };
            if pipeline.status != PipelineStatus::Running {
                return;
            }
            let current = pipeline.current_stage;
            if pipeline
                .stages
                .get(current)
                .and_then(|s| s.run_id.as_deref())
                != Some(ended_id.as_str())
            {
                return;
            }
            if !ended_well {
                pipeline.status = PipelineStatus::Failed;
                pipeline.stages[current].status = StageStatus::Failed;
                pipeline.stages[current].error = Some("stage did not complete cleanly".to_owned());
                save_pipelines(&self.repo.pipelines_file(), &map);
                (self.notify)();
                return;
            }

            pipeline.stages[current].status = StageStatus::Completed;
            if apply_stage_output(pipeline, current, &ended_id, &reply_text) {
                pipeline.status = PipelineStatus::AwaitingGate;
                save_pipelines(&self.repo.pipelines_file(), &map);
                (self.notify)();
                return;
            }

            let mut next = current + 1;
            // Skip write-capable Final Judge & Fixer for Issues when reviewers found zero issues.
            if next < pipeline.stages.len()
                && pipeline.kind == WorkKind::Issue
                && pipeline.stages[next].kind == StageKind::ReviewJudge
                && pipeline.findings.is_empty()
            {
                pipeline.stages[next].status = StageStatus::Skipped;
                next += 1;
            }
            if next >= pipeline.stages.len() {
                pipeline.status = PipelineStatus::Completed;
                save_pipelines(&self.repo.pipelines_file(), &map);
                (self.notify)();
                return;
            }
            pipeline.current_stage = next;
            pipeline.stages[next].status = StageStatus::Running;
            save_pipelines(&self.repo.pipelines_file(), &map);
            Some(next)
        };

        if let Some(next_index) = next_step
            && let Err(error) = self.launch_next_pipeline_stage(thread_id, next_index, None)
        {
            let mut map = lock(&self.pipelines);
            if let Some(pipeline) = map.get_mut(thread_id) {
                pipeline.status = PipelineStatus::Failed;
                if let Some(stage) = pipeline.stages.get_mut(next_index) {
                    stage.status = StageStatus::Failed;
                    stage.error = Some(error_chain(&error));
                }
                save_pipelines(&self.repo.pipelines_file(), &map);
            }
            (self.notify)();
        }
    }

    fn launch_next_pipeline_stage(
        self: &Arc<Self>,
        thread_id: &str,
        stage_index: usize,
        revision_feedback: Option<&str>,
    ) -> Result<()> {
        let pipeline = self
            .pipeline(thread_id)
            .ok_or_else(|| Error::BadRequest(format!("pipeline {thread_id} no longer exists")))?;
        let stage = pipeline
            .stages
            .get(stage_index)
            .ok_or_else(|| Error::BadRequest(format!("stage {stage_index} out of range")))?;
        let choice = pipeline
            .stage_choice(stage_index)
            .expect("stage_index validated above");
        let prompt_text = self.build_stage_prompt(&pipeline, stage_index, revision_feedback);
        let record = self.open_thread(thread_id)?;
        let team_memory = if pipeline.use_memory {
            TeamMemory::On
        } else {
            TeamMemory::Off
        };
        let stage_read_only = stage.read_only;

        let new = NewRun {
            agent: choice,
            memory: team_memory,
            origin: Origin::Notes,
            conversation: Conversation::Fresh,
            review: None,
            review_recall: None,
            stage_read_only,
            place: Place::Open(record),
        };

        // Record `stage.run_id` inside `launch`'s content closure (before `start()` spawns the
        // agent thread) so `advance_pipeline` always observes it even if the agent exits rapidly.
        let mut recorded_run_id: Option<String> = None;
        let launch_result = self.launch(self.slots(), new, |run_id| {
            let owned = run_id.to_owned();
            {
                let mut map = lock(&self.pipelines);
                if let Some(stored) = map.get_mut(thread_id)
                    && let Some(st) = stored.stages.get_mut(stage_index)
                {
                    st.run_id = Some(owned.clone());
                    if st.kind == StageKind::Implement {
                        stored.implement_run_id = Some(owned.clone());
                    }
                    save_pipelines(&self.repo.pipelines_file(), &map);
                }
            }
            recorded_run_id = Some(owned);
            Ok(Content {
                notes: Vec::new(),
                prompt: Prompt {
                    text: prompt_text,
                    files: Vec::new(),
                },
            })
        });
        if let Err(error) = launch_result {
            if let Some(run_id) = recorded_run_id {
                let mut map = lock(&self.pipelines);
                if let Some(stored) = map.get_mut(thread_id)
                    && let Some(st) = stored.stages.get_mut(stage_index)
                    && st.run_id.as_deref() == Some(run_id.as_str())
                {
                    st.run_id = None;
                    save_pipelines(&self.repo.pipelines_file(), &map);
                }
            }
            return Err(error);
        }
        (self.notify)();
        Ok(())
    }

    fn build_stage_prompt(
        &self,
        pipeline: &PipelineRun,
        stage_index: usize,
        revision_feedback: Option<&str>,
    ) -> String {
        let task = pipeline.task_context();
        let mem_block = pipeline.memory_block();
        let kind = pipeline.stages[stage_index].kind;
        match (pipeline.kind, kind) {
            (WorkKind::Issue, StageKind::Plan) => {
                lane::plan_prompt(&task, &mem_block, revision_feedback)
            }
            (WorkKind::Issue, StageKind::PlanJudge) => lane::plan_judge_prompt(
                &task,
                pipeline.plan_reply.as_deref().unwrap_or(""),
                pipeline.plan.as_ref(),
                &mem_block,
            ),
            (WorkKind::Issue, StageKind::Implement) => {
                let accepted = pipeline.accepted_proposal_items();
                lane::issue_implement_prompt(
                    &task,
                    &mem_block,
                    &lane::ImplementInputs {
                        plan: pipeline.plan.as_ref(),
                        accepted_proposals: &accepted,
                        extra_instructions: pipeline.extra_instructions.as_deref(),
                        with_team_memory_hint: pipeline.use_memory,
                    },
                )
            }
            (WorkKind::Issue, StageKind::ReviewPropose) => {
                let work = self.work_for_issue_review(pipeline, mem_block);
                review::propose_prompt(&work)
            }
            (WorkKind::Issue, StageKind::ReviewJudge) => {
                let work = self.work_for_issue_review(pipeline, mem_block);
                review::judge_prompt(&work, &pipeline.findings)
            }
            (WorkKind::PullRequest, StageKind::Plan) => lane::pr_context_prompt(&task, &mem_block),
            (WorkKind::PullRequest, StageKind::ReviewPropose) => {
                lane::pr_review_prompt(&task, &mem_block, pipeline.plan.as_ref())
            }
            (
                WorkKind::PullRequest,
                StageKind::ReviewJudge | StageKind::Implement | StageKind::PlanJudge,
            ) => lane::pr_synthesis_prompt(&task, &mem_block, &pipeline.findings),
        }
    }

    fn work_for_issue_review(&self, pipeline: &PipelineRun, memories: MemoryBlock) -> Work {
        let impl_id = pipeline.implement_run_id.as_deref().unwrap_or(&pipeline.id);
        if let Ok(run) = self.run_by_id(impl_id) {
            let mut work = run.state().work();
            if work.memories.is_empty() && !memories.is_empty() {
                work.memories = memories;
            }
            return work;
        }
        Work {
            prompt: format!(
                "Issue #{}: {}\n\n{}",
                pipeline.number, pipeline.title, pipeline.body
            ),
            reply: String::new(),
            diffs: Vec::new(),
            report: None,
            memories,
        }
    }

    /// Prefilled PR title and body when opening a draft PR from an Issue pipeline thread.
    pub(super) fn pipeline_pr_text(&self, thread_id: &str) -> Option<(String, String)> {
        let pipeline = self.pipeline(thread_id)?;
        if pipeline.kind != WorkKind::Issue {
            return None;
        }
        let title = format!("{} (#{})", pipeline.title, pipeline.number);
        let mut body = format!(
            "Closes #{}\n\nImplemented in Postil ({} Lane, thread {}).\n",
            pipeline.number,
            pipeline.lane.label(),
            pipeline.id
        );
        if let Some(plan) = &pipeline.plan {
            body.push_str(&format!("\n### Summary\n{}\n", plan.summary));
        }
        Some((title, body))
    }
}
