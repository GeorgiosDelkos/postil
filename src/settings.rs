//! The engineer's settings, kept in `<config dir>/settings.json` and edited in the settings
//! dialog (`ui/settings.js`).
//!
//! The file is the engineer's own choices, so it is never replaced behind their back. A file
//! that cannot be read, does not parse, or breaks a rule below (a hand edit gone wrong) leaves
//! the defaults in effect and blocks saving: the dialog says why and offers Reset, which
//! writes the defaults over it on purpose. A file from a newer Postil is treated the same way,
//! so running an older build cannot drop the newer build's settings. A missing file is the
//! defaults, and the first save creates it.
//!
//! Format version 2 names the agent notes are sent to, and each reviewer, as a slot: an
//! agent's name and the model it runs (`{"name": "Claude", "model": "opus"}`, `null` for the
//! agent's own default). A version 1 file, where they are plain names, is still read: each
//! name becomes a slot with no model, and the next save writes version 4. Version 3 adds the
//! Analyzer, a slot like the others, which draws the map (docs/review-assist.md, "An analysis
//! run"); a version 2 file has none, and analyses go to the agent notes are sent to. Version 4
//! adds `agentMemory`, whether agents get team memory (docs/review-assist.md, "Milestone 9:
//! team memory"); an older file has it off.
//!
//! The rules, checked when a [`RawSettings`] becomes a [`Settings`] (`TryFrom`), the only way
//! to make one:
//! - the code font size is 11 to 20 px and the tab width 2, 4 or 8;
//! - a custom agent has a name of 1 to 40 characters, unlike any other agent's name
//!   whatever the case, built-in ones included, and a command of plain words (no quotes)
//!   whose program is a name on the search path or an absolute path;
//! - the enabled agents are known agents, each once;
//! - the agent notes are sent to, the analyzer, and every reviewer, is an enabled agent. A
//!   reviewer may be listed more than once: each entry is one independent review;
//! - a slot's model, when set, is 1 to 200 characters with no space at either end and no
//!   control character. Whether the agent offers it is checked when a run starts, against
//!   the list the agent reports then (`acp`, `Starting::open_session`).

use std::fs;
use std::io;
use std::ops::RangeInclusive;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agents::{self, AgentChoice, BUILT_IN};
use crate::error::{Error, Result};
use crate::files::{save_json, version_of};

/// The format version this build writes. It also reads [`V4`], [`V3`], [`V2`] and [`V1`].
const VERSION: u32 = 5;

/// The format before capability tiers and lane/hub settings: read as it is, with defaults.
const V4: u32 = 4;

/// The format before team memory: read as it is, with it off.
const V3: u32 = 3;

/// The format before the analyzer: read as it is, with no analyzer.
const V2: u32 = 2;

/// The format before slots: the agent and the reviewers as plain names ([`upgrade_v1`]).
const V1: u32 = 1;

/// The code font sizes offered, in CSS px. 11 is the smallest Geist Mono size that stays
/// legible on a non-Retina display; 20 fits about 100 columns in the code pane of a 1440 px
/// window.
pub(crate) const FONT_SIZES: RangeInclusive<u8> = 11..=20;

/// The tab widths offered, in columns: the ones in common use.
pub(crate) const TAB_WIDTHS: [u8; 3] = [2, 4, 8];

/// The longest custom agent name, in characters. Arbitrary: it fits one row of the dialog.
const MAX_NAME: usize = 40;

/// The longest model id, in characters. Arbitrary: far above any id an agent reported on
/// 2026-11-26 (the longest, `claude-fable-5-2[1m]`, has 20).
const MAX_MODEL: usize = 200;

/// The window's appearance.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Theme {
    /// Follow the system's light or dark appearance.
    System,
    Light,
    Dark,
}

/// Whether agents get team memory: the `agentMemory` switch, as runs take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TeamMemory {
    Off,
    On,
}

/// Capability tier of an `(Agent, Model)` pair for lane orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CapabilityTier {
    /// Fast / economy models for priority triage and Easy Lane tasks.
    Fast,
    /// Standard workhorse models for day-to-day implementation and peer reviews.
    Standard,
    /// Top-tier Frontier models for Hard Lane planning, plan judging, and final review.
    Frontier,
}

/// Ordered `(Agent, Model)` slots grouped by capability tier.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub(crate) struct ModelTiers {
    pub(crate) fast: Vec<AgentSlot>,
    pub(crate) standard: Vec<AgentSlot>,
    pub(crate) frontier: Vec<AgentSlot>,
}

/// Preferences governing execution lanes and the multi-repo Hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub(crate) struct LaneSettings {
    /// Whether Hard Lane pauses after Stage 2 (Plan Judge) for human approval before coding.
    pub(crate) hard_human_gate: bool,
    /// Whether the Hub automatically runs cached Tier-1 AI triage on newly synced items.
    pub(crate) auto_triage: bool,
}

impl Default for LaneSettings {
    fn default() -> Self {
        Self {
            hard_human_gate: true,
            auto_triage: true,
        }
    }
}

/// The settings in effect, each rule in the module docs kept: made only from a
/// [`RawSettings`] that passes them, so settings that break one never reach the file, the
/// page, or a run. Serialized as the [`RawSettings`] it holds.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Settings(RawSettings);

/// The settings as the page sends them and the file stores them, before the rules are
/// checked. A field missing from the file takes its default, so a file written by hand may
/// name only what it changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub(crate) struct RawSettings {
    theme: Theme,
    /// Code text size in CSS px; the line height follows it (`ui/app.js`).
    code_font_size: u8,
    /// Columns per tab character in the code view.
    tab_width: u8,
    /// Whether a start without a folder opens the most recent project instead of the
    /// welcome screen.
    reopen_last_project: bool,

    /// Agents the engineer added, beside the built-in ones.
    custom_agents: Vec<CustomAgent>,
    /// The names of the agents offered for sending and reviewing. A built-in agent that is
    /// enabled but no longer installed stays listed, marked missing.
    enabled_agents: Vec<String>,
    /// The agent notes are sent to, and its model.
    agent: Option<AgentSlot>,
    /// The agent that analyzes the repository for the map, and its model; `None` for the
    /// agent notes are sent to, with its model.
    analyzer: Option<AgentSlot>,
    /// The agents that review the agent's work, in order, each with its model.
    reviewers: Vec<AgentSlot>,

    /// Whether agents get team memory (hippius-mem). Off by default: it is turned on only
    /// once hippius-mem is ready (`site.rs`, `save_settings`), which a file does not know.
    agent_memory: bool,

    /// Model capability tiers (Fast, Standard, Frontier) used by execution lanes.
    model_tiers: ModelTiers,

    /// Execution lane and Hub triage preferences.
    lanes: LaneSettings,
}

/// A place in the workflow: the agent notes are sent to, or one reviewer. One type for both,
/// so the two cannot drift apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentSlot {
    /// The agent's name, an enabled agent.
    name: String,
    /// The model id to set on the agent's session; `None` (`null`, or missing in a file
    /// written by hand) keeps the agent's own default. Always written, so the file and the
    /// page share one shape.
    #[serde(default)]
    model: Option<String>,
}

impl Settings {
    pub(crate) fn theme(&self) -> Theme {
        self.0.theme
    }

    pub(crate) fn reopen_last_project(&self) -> bool {
        self.0.reopen_last_project
    }

    pub(crate) fn team_memory(&self) -> TeamMemory {
        if self.0.agent_memory {
            TeamMemory::On
        } else {
            TeamMemory::Off
        }
    }

    pub(crate) fn custom_agents(&self) -> &[CustomAgent] {
        &self.0.custom_agents
    }

    /// Whether the agent called `name` is turned on in the settings.
    pub(crate) fn is_enabled(&self, name: &str) -> bool {
        self.0.enabled_agents.iter().any(|enabled| enabled == name)
    }

    /// The agent notes are sent to, with its command and model.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when none is chosen.
    pub(crate) fn default_agent(&self) -> Result<AgentChoice> {
        let slot = self.0.agent.as_ref().ok_or_else(|| {
            Error::BadRequest("choose the agent notes are sent to in Settings".to_owned())
        })?;
        self.choice(&slot.name, slot.model.clone())
    }

    /// The agent called `name`, with its command: a one-off choice in the send picker, or the
    /// agent a follow-up starts when no live agent of the run is kept. It runs the default
    /// slot's model when it is the default agent, and its own default model otherwise.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when it is not enabled.
    pub(crate) fn chosen_agent(&self, name: &str) -> Result<AgentChoice> {
        let model = self
            .0
            .agent
            .as_ref()
            .filter(|slot| slot.name == name)
            .and_then(|slot| slot.model.clone());
        self.choice(name, model)
    }

    /// The agent that analyzes the repository, with its command and model: the analyzer, or
    /// the agent notes are sent to when no analyzer is chosen.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when neither is chosen.
    pub(crate) fn analyzer(&self) -> Result<AgentChoice> {
        match &self.0.analyzer {
            Some(slot) => self.choice(&slot.name, slot.model.clone()),
            None => self.default_agent(),
        }
    }

    /// The reviewers, in order, each with its command and model.
    ///
    /// # Errors
    ///
    /// None in practice: every reviewer is an enabled agent (`check_choices`). The `Result`
    /// is [`Settings::choice`]'s.
    pub(crate) fn reviewer_choices(&self) -> Result<Vec<AgentChoice>> {
        self.0
            .reviewers
            .iter()
            .map(|slot| self.choice(&slot.name, slot.model.clone()))
            .collect()
    }

    /// Whether `tier` has at least one configured `(Agent, Model)` entry.
    pub(crate) fn has_tier(&self, tier: CapabilityTier) -> bool {
        !self.tier_slots(tier).is_empty()
    }

    /// The raw slots assigned to capability `tier`.
    fn tier_slots(&self, tier: CapabilityTier) -> &[AgentSlot] {
        match tier {
            CapabilityTier::Fast => &self.0.model_tiers.fast,
            CapabilityTier::Standard => &self.0.model_tiers.standard,
            CapabilityTier::Frontier => &self.0.model_tiers.frontier,
        }
    }

    /// The resolved `(Agent, Model)` choices configured for capability `tier`.
    ///
    /// # Errors
    ///
    /// As [`Settings::choice`].
    pub(crate) fn tier_choices(&self, tier: CapabilityTier) -> Result<Vec<AgentChoice>> {
        self.tier_slots(tier)
            .iter()
            .map(|slot| self.choice(&slot.name, slot.model.clone()))
            .collect()
    }

    /// Execution lane and Hub triage preferences.
    pub(crate) fn lanes(&self) -> &LaneSettings {
        &self.0.lanes
    }

    /// The command of the agent called `name`, when it is a custom one.
    pub(crate) fn custom_command(&self, name: &str) -> Option<&str> {
        self.custom_agents()
            .iter()
            .find(|agent| agent.name == name)
            .map(|agent| agent.command.as_str())
    }

    /// The agent called `name` running `model`, with its command.
    ///
    /// # Errors
    ///
    /// [`Error::BadRequest`] when it is not enabled.
    fn choice(&self, name: &str, model: Option<String>) -> Result<AgentChoice> {
        if !self.0.enabled_agents.iter().any(|enabled| enabled == name) {
            return Err(Error::BadRequest(format!(
                "turn {name} on in Settings before sending to it"
            )));
        }
        let built_in = BUILT_IN
            .iter()
            .find(|agent| agent.name == name)
            .map(|agent| agent.command);
        let command = built_in
            .or_else(|| self.custom_command(name))
            .expect("an enabled agent is a known one: the settings passed check_choices");
        Ok(AgentChoice {
            name: name.to_owned(),
            command: command.to_owned(),
            model,
        })
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::try_from(RawSettings::default()).expect("the default settings keep every rule")
    }
}

impl TryFrom<RawSettings> for Settings {
    /// The rule `raw` breaks, in words for the dialog.
    type Error = String;

    /// Checks the rules in the module docs.
    fn try_from(raw: RawSettings) -> std::result::Result<Self, Self::Error> {
        if !FONT_SIZES.contains(&raw.code_font_size) {
            return Err(format!(
                "the code font size must be {} to {} px, not {}",
                FONT_SIZES.start(),
                FONT_SIZES.end(),
                raw.code_font_size
            ));
        }
        if !TAB_WIDTHS.contains(&raw.tab_width) {
            // Destructured, so a change to the offered widths fails to compile here until
            // the message follows it.
            let [narrow, middle, wide] = TAB_WIDTHS;
            return Err(format!(
                "the tab width must be {narrow}, {middle} or {wide}, not {}",
                raw.tab_width
            ));
        }
        raw.check_custom_agents()?;
        raw.check_choices()?;
        Ok(Self(raw))
    }
}

impl RawSettings {
    fn check_custom_agents(&self) -> std::result::Result<(), String> {
        let mut names: Vec<&str> = BUILT_IN.iter().map(|agent| agent.name).collect();
        for agent in &self.custom_agents {
            let name = agent.name.as_str();
            if name.trim().is_empty() || name.trim() != name {
                return Err(format!(
                    "an agent name cannot be empty or start or end with a space: {name:?}"
                ));
            }
            if name.chars().count() > MAX_NAME {
                return Err(format!(
                    "an agent name can be at most {MAX_NAME} characters: {name:?}"
                ));
            }
            if names
                .iter()
                .any(|known| known.to_lowercase() == name.to_lowercase())
            {
                return Err(format!("there is already an agent called {name:?}"));
            }
            check_agent_command(name, &agent.command)?;
            names.push(name);
        }
        Ok(())
    }

    fn check_choices(&self) -> std::result::Result<(), String> {
        let known = |name: &str| {
            BUILT_IN.iter().any(|agent| agent.name == name)
                || self.custom_agents.iter().any(|agent| agent.name == name)
        };
        for (index, name) in self.enabled_agents.iter().enumerate() {
            if !known(name) {
                return Err(format!("there is no agent called {name:?} to enable"));
            }
            if self.enabled_agents[..index].contains(name) {
                return Err(format!("{name:?} is enabled twice"));
            }
        }
        let enabled = |slot: &AgentSlot| self.enabled_agents.contains(&slot.name);
        if let Some(agent) = &self.agent {
            if !enabled(agent) {
                return Err(format!(
                    "notes cannot go to {:?}: it is not an enabled agent",
                    agent.name
                ));
            }
            check_model(agent)?;
        }
        if let Some(analyzer) = &self.analyzer {
            if !enabled(analyzer) {
                return Err(format!(
                    "{:?} cannot analyze: it is not an enabled agent",
                    analyzer.name
                ));
            }
            check_model(analyzer)?;
        }
        for reviewer in &self.reviewers {
            if !enabled(reviewer) {
                return Err(format!(
                    "{:?} cannot review: it is not an enabled agent",
                    reviewer.name
                ));
            }
            check_model(reviewer)?;
        }
        for (tier_name, slots) in [
            ("fast", &self.model_tiers.fast),
            ("standard", &self.model_tiers.standard),
            ("frontier", &self.model_tiers.frontier),
        ] {
            for slot in slots {
                if !enabled(slot) {
                    return Err(format!(
                        "{:?} cannot be assigned to the {tier_name} tier: it is not an enabled agent",
                        slot.name
                    ));
                }
                check_model(slot)?;
            }
        }
        Ok(())
    }
}

/// Checks the command of the agent called `name`: plain words (no quotes, split on
/// whitespace as it is) whose program is a name on the search path or an absolute path. The
/// rule for a custom agent's command, also applied to the wizard's unsaved custom agents
/// before their models are asked for.
pub(crate) fn check_agent_command(name: &str, command: &str) -> std::result::Result<(), String> {
    let Some(program) = agents::program_of(command) else {
        return Err(format!("the agent {name:?} needs a command"));
    };
    // A relative path would be looked up from Postil's working directory (`/` when
    // started from the Finder) but run from the project's, so it names two files.
    if program.contains('/') && !program.starts_with('/') {
        return Err(format!(
            "the program of {name:?} must be a name on the search path or an absolute path, \
             not {program}"
        ));
    }
    if command.contains(['"', '\'', '\\']) {
        return Err(format!(
            "the command of {name:?} cannot hold quotes or backslashes: it is split on spaces \
             as it is"
        ));
    }
    Ok(())
}

/// Checks a slot's model id, when it has one (module docs). The id travels to the agent as
/// it is, so a stray space or newline from a hand edit would name a model no agent offers.
fn check_model(slot: &AgentSlot) -> std::result::Result<(), String> {
    let Some(model) = &slot.model else {
        return Ok(());
    };
    let length = model.chars().count();
    if length == 0
        || length > MAX_MODEL
        || model.trim() != model
        || model.chars().any(char::is_control)
    {
        return Err(format!(
            "the model of {:?} must be 1 to {MAX_MODEL} characters, with no space at either \
             end and no control character: {model:?}",
            slot.name
        ));
    }
    Ok(())
}

impl Default for RawSettings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            // The size the code view had before it was a setting.
            code_font_size: 13,
            tab_width: 4,
            reopen_last_project: false,
            custom_agents: Vec::new(),
            enabled_agents: Vec::new(),
            agent: None,
            analyzer: None,
            reviewers: Vec::new(),
            agent_memory: false,
            model_tiers: ModelTiers::default(),
            lanes: LaneSettings::default(),
        }
    }
}

/// An agent the engineer added.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustomAgent {
    pub(crate) name: String,
    /// The command that starts it as an ACP server: a program and its arguments, split on
    /// whitespace.
    pub(crate) command: String,
}

/// The file: read with `S` = [`RawSettings`], written with `S` = `&Settings`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile<S> {
    version: u32,
    settings: S,
}

/// The settings in effect, and why the file was not used when it was not.
#[derive(Debug, Clone)]
pub(crate) struct Loaded {
    pub(crate) settings: Settings,
    /// Set when the file exists but cannot be used; saving is blocked until Reset.
    pub(crate) problem: Option<String>,
    /// A fingerprint of the file's text ([`revision_of`]), empty when there is no file or
    /// it cannot be read. A save names the revision it started from.
    pub(crate) revision: String,
    /// Whether no settings file exists yet, so the startup wizard opens on first launch.
    pub(crate) first_run: bool,
}

/// Reads the settings from `file`. Never fails: a file that cannot be used gives the defaults
/// and the reason (see the module docs).
pub(crate) fn load(file: &Path) -> Loaded {
    let (read, revision) = read(file);
    match read {
        Ok(Some(settings)) => Loaded {
            settings,
            problem: None,
            revision,
            first_run: false,
        },
        Ok(None) => Loaded {
            settings: Settings::default(),
            problem: None,
            revision,
            first_run: true,
        },
        Err(problem) => Loaded {
            settings: Settings::default(),
            problem: Some(format!(
                "{problem}. The defaults are in use, and saving is off until the file is fixed \
                 or reset."
            )),
            revision,
            first_run: false,
        },
    }
}

/// The file's settings (`None` when there is no file) or why the file cannot be used, and
/// its revision.
fn read(file: &Path) -> (std::result::Result<Option<Settings>, String>, String) {
    match fs::read_to_string(file) {
        Ok(json) => {
            let revision = revision_of(&json);
            (parse(file, &json).map(Some), revision)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (Ok(None), String::new()),
        Err(error) => (
            Err(format!("{} cannot be read: {error}", file.display())),
            String::new(),
        ),
    }
}

/// A fingerprint of the file's text: FNV-1a, 64 bits, in hex. Not a defence against anyone;
/// it tells two versions of a small file apart, and is the same in every build, so two
/// Postil versions open at once agree on it.
fn revision_of(json: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let hash = json.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    });
    format!("{hash:016x}")
}

fn parse(file: &Path, json: &str) -> std::result::Result<Settings, String> {
    let shown = file.display();
    // `None` when the field is missing or the text does not parse: the parse below says why.
    let version = version_of(json.as_bytes()).ok();
    match version {
        Some(version) if version > VERSION => {
            return Err(format!(
                "{shown} was written by a newer Postil (format version {version})"
            ));
        }
        Some(version) if ![VERSION, V4, V3, V2, V1].contains(&version) => {
            return Err(format!("{shown} has an unknown format version {version}"));
        }
        _ => {}
    }
    let unreadable =
        |error: serde_json::Error| format!("{shown} is not valid settings JSON: {error}");
    let mut stored: SettingsFile<RawSettings> = if version == Some(V1) {
        let mut value: Value = serde_json::from_str(json).map_err(unreadable)?;
        upgrade_v1(&mut value);
        serde_json::from_value(value).map_err(unreadable)?
    } else {
        // Read from the text, not through a `Value`, so serde's message names the line.
        serde_json::from_str(json).map_err(unreadable)?
    };
    stored.settings.upgrade_legacy_agent_names();
    Settings::try_from(stored.settings)
        .map_err(|problem| format!("{shown} breaks a rule: {problem}"))
}

impl RawSettings {
    /// Renames the built-in `"Gemini"` slot name (used before the `agy` catalog entry took its
    /// own name `"Antigravity"`) when no custom agent claims `"Gemini"` or `"Antigravity"`.
    fn upgrade_legacy_agent_names(&mut self) {
        let custom_clash = self.custom_agents.iter().any(|agent| {
            agent.name.eq_ignore_ascii_case("Gemini")
                || agent.name.eq_ignore_ascii_case("Antigravity")
        });
        if custom_clash {
            return;
        }
        let mut upgraded = Vec::with_capacity(self.enabled_agents.len());
        for name in self.enabled_agents.drain(..) {
            let mapped = if name == "Gemini" {
                "Antigravity".to_owned()
            } else {
                name
            };
            if !upgraded.contains(&mapped) {
                upgraded.push(mapped);
            }
        }
        self.enabled_agents = upgraded;
        let slots = self
            .agent
            .iter_mut()
            .chain(self.analyzer.iter_mut())
            .chain(self.reviewers.iter_mut())
            .chain(self.model_tiers.fast.iter_mut())
            .chain(self.model_tiers.standard.iter_mut())
            .chain(self.model_tiers.frontier.iter_mut());
        for slot in slots {
            if slot.name == "Gemini" {
                "Antigravity".clone_into(&mut slot.name);
            }
        }
    }
}

/// Rewrites a version 1 file, as parsed, into version 2's shape, which version 4 reads: the
/// agent and each reviewer, plain names there, become slots with no model. Anything else is
/// left as it is, so a value of another type still fails the parse that follows, with serde's
/// message.
fn upgrade_v1(file: &mut Value) {
    let slot = |value: &mut Value| {
        if let Value::String(name) = value {
            *value = serde_json::json!({ "name": name, "model": null });
        }
    };
    let Some(settings) = file.get_mut("settings") else {
        return;
    };
    if let Some(agent) = settings.get_mut("agent") {
        slot(agent);
    }
    if let Some(Value::Array(reviewers)) = settings.get_mut("reviewers") {
        reviewers.iter_mut().for_each(slot);
    }
}

/// Writes `settings` to `file`, if the file is still at `revision`: the one the change
/// started from. Another window, or a hand edit, may have changed it since, and saving the
/// whole settings over that would drop the other change without a word. The check and the
/// write are not one step: within a process saves run on the request thread one at a time, but
/// two Postil processes saving in the same instant can both pass it, and the later wins.
///
/// # Errors
///
/// - [`Error::SettingsChanged`] when the file is no longer at `revision`; nothing is
///   written.
/// - [`Error::SettingsLocked`] when the file on disk cannot be used (see the module docs);
///   nothing is written.
/// - [`Error::WriteSettings`] when the file cannot be written.
pub(crate) fn save(file: &Path, settings: &Settings, revision: &str) -> Result<()> {
    let (read, current) = read(file);
    if current != revision {
        return Err(Error::SettingsChanged);
    }
    if let Err(problem) = read {
        return Err(Error::SettingsLocked(problem));
    }
    write(file, settings)
}

/// Writes the defaults to `file`, over whatever it holds.
///
/// # Errors
///
/// [`Error::WriteSettings`] when the file cannot be written.
pub(crate) fn reset(file: &Path) -> Result<()> {
    write(file, &Settings::default())
}

fn write(file: &Path, settings: &Settings) -> Result<()> {
    let stored = SettingsFile {
        version: VERSION,
        settings,
    };
    save_json(file, &stored).map_err(|source| Error::WriteSettings {
        path: file.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_revision_is_fnv_1a_so_every_build_agrees_on_it() {
        // FNV-1a 64-bit of "" and "a" in the FNV reference test suite (test_fnv.c by Landon
        // Curt Noll, the fnv1a_64 vectors).
        assert_eq!(revision_of(""), "cbf29ce484222325");
        assert_eq!(revision_of("a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn the_file_is_pretty_json_ending_in_a_newline_byte_for_byte() {
        let dir = crate::testing::TempDir::new("settings-bytes");
        let file = dir.path().join("settings.json");
        let raw: RawSettings = serde_json::from_value(serde_json::json!({
            "theme": "dark",
            "codeFontSize": 14,
            "tabWidth": 2,
            "reopenLastProject": true,
            "customAgents": [{ "name": "Local", "command": "local-agent --acp" }],
            "enabledAgents": ["Claude", "Local"],
            "agent": { "name": "Local" },
            "analyzer": { "name": "Claude", "model": "sonnet" },
            "reviewers": [
                { "name": "Claude", "model": "opus" },
                { "name": "Claude", "model": null },
            ],
            "agentMemory": true,
        }))
        .unwrap();
        let settings = Settings::try_from(raw).unwrap();

        save(&file, &settings, "").unwrap();

        let expected = r#"{
  "version": 5,
  "settings": {
    "theme": "dark",
    "codeFontSize": 14,
    "tabWidth": 2,
    "reopenLastProject": true,
    "customAgents": [
      {
        "name": "Local",
        "command": "local-agent --acp"
      }
    ],
    "enabledAgents": [
      "Claude",
      "Local"
    ],
    "agent": {
      "name": "Local",
      "model": null
    },
    "analyzer": {
      "name": "Claude",
      "model": "sonnet"
    },
    "reviewers": [
      {
        "name": "Claude",
        "model": "opus"
      },
      {
        "name": "Claude",
        "model": null
      }
    ],
    "agentMemory": true,
    "modelTiers": {
      "fast": [],
      "standard": [],
      "frontier": []
    },
    "lanes": {
      "hardHumanGate": true,
      "autoTriage": true
    }
  }
}
"#;
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    }

    #[test]
    fn legacy_gemini_slots_in_settings_file_upgrade_to_antigravity_unless_custom() {
        let dir = crate::testing::TempDir::new("settings-legacy-gemini");
        let file = dir.path().join("settings.json");
        fs::write(
            &file,
            r#"{
                "version": 2,
                "settings": {
                    "enabledAgents": ["Claude", "Gemini"],
                    "agent": { "name": "Gemini", "model": "gemini-4-neon" },
                    "reviewers": [{ "name": "Gemini", "model": null }]
                }
            }"#,
        )
        .unwrap();
        let loaded = load(&file);
        assert!(loaded.problem.is_none(), "{loaded:?}");
        assert!(loaded.settings.is_enabled("Antigravity"));
        assert!(!loaded.settings.is_enabled("Gemini"));
        assert_eq!(loaded.settings.default_agent().unwrap().name, "Antigravity");
        assert_eq!(
            loaded.settings.reviewer_choices().unwrap()[0].name,
            "Antigravity"
        );

        // The analyzer slot, new in version 4, is renamed too: left as "Gemini" it would name
        // an agent that is no longer enabled, and the file would fail to load.
        fs::write(
            &file,
            r#"{
                "version": 4,
                "settings": {
                    "enabledAgents": ["Claude", "Gemini"],
                    "agent": { "name": "Claude", "model": null },
                    "analyzer": { "name": "Gemini", "model": "gemini-4-carbon" }
                }
            }"#,
        )
        .unwrap();
        let analyzer = load(&file);
        assert!(analyzer.problem.is_none(), "{analyzer:?}");
        assert_eq!(analyzer.settings.analyzer().unwrap().name, "Antigravity");

        // A user-defined custom agent named "Gemini" keeps its own name untouched.
        fs::write(
            &file,
            r#"{
                "version": 2,
                "settings": {
                    "customAgents": [{ "name": "Gemini", "command": "my-gemini --acp" }],
                    "enabledAgents": ["Gemini"],
                    "agent": { "name": "Gemini", "model": null }
                }
            }"#,
        )
        .unwrap();
        let custom = load(&file);
        assert!(custom.problem.is_none(), "{custom:?}");
        assert!(custom.settings.is_enabled("Gemini"));
        assert_eq!(custom.settings.default_agent().unwrap().name, "Gemini");
    }
}
