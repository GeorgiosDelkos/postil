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
//! The rules ([`Settings::check`]):
//! - the code font size is 11 to 20 px and the tab width 2, 4 or 8;
//! - a custom agent has a name of 1 to 40 characters, unlike any other agent's name
//!   whatever the case, built-in ones included, and a command of plain words (no quotes);
//! - the enabled agents are known agents, each once;
//! - the agent notes are sent to, and every reviewer, is an enabled agent. A reviewer may be
//!   listed more than once: each entry is one independent review.

use std::fs;
use std::io;
use std::ops::RangeInclusive;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::agents::{self, BUILT_IN};
use crate::error::{Error, Result};
use crate::files::write_atomically;

/// The format version this build reads and writes.
const VERSION: u32 = 1;

/// The code font sizes offered, in CSS px. 11 is the smallest Geist Mono size that stays
/// legible on a non-Retina display; 20 fits about 100 columns in the code pane of a 1440 px
/// window.
pub(crate) const FONT_SIZES: RangeInclusive<u8> = 11..=20;

/// The tab widths offered, in columns: the ones in common use.
pub(crate) const TAB_WIDTHS: [u8; 3] = [2, 4, 8];

/// The longest custom agent name, in characters. Arbitrary: it fits one row of the dialog.
const MAX_NAME: usize = 40;

/// The window's appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Theme {
    /// Follow the system's light or dark appearance.
    #[default]
    System,
    Light,
    Dark,
}

/// The settings, as the page sends them and the file stores them. A field missing from the
/// file takes its default, so a file written by hand may name only what it changes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub(crate) struct Settings {
    pub(crate) theme: Theme,
    /// Code text size in CSS px; the line height follows it (`ui/app.js`).
    pub(crate) code_font_size: u8,
    /// Columns per tab character in the code view.
    pub(crate) tab_width: u8,
    /// Whether a start without a folder opens the most recent project instead of the
    /// welcome screen.
    pub(crate) reopen_last_project: bool,

    /// Agents the engineer added, beside the built-in ones.
    pub(crate) custom_agents: Vec<CustomAgent>,
    /// The names of the agents offered for sending and reviewing. A built-in agent that is
    /// enabled but no longer installed stays listed, marked missing.
    pub(crate) enabled_agents: Vec<String>,
    /// The agent notes are sent to.
    pub(crate) agent: Option<String>,
    /// The agents that review the agent's work, in order.
    pub(crate) reviewers: Vec<String>,
}

impl Default for Settings {
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
            reviewers: Vec::new(),
        }
    }
}

/// An agent the engineer added.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustomAgent {
    pub(crate) name: String,
    /// The command that starts it as an ACP server: a program and its arguments, split on
    /// whitespace.
    pub(crate) command: String,
}

/// The file: read with `S` = [`Settings`], written with `S` = `&Settings`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile<S> {
    version: u32,
    settings: S,
}

/// The one field every version of the file has; other fields are ignored here on purpose.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// The settings in effect, and why the file was not used when it was not.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub(crate) settings: Settings,
    /// Set when the file exists but cannot be used; saving is blocked until Reset.
    pub(crate) problem: Option<String>,
}

impl Settings {
    /// Checks the rules in the module docs. The error says which rule failed, in words for
    /// the dialog.
    pub(crate) fn check(&self) -> std::result::Result<(), String> {
        if !FONT_SIZES.contains(&self.code_font_size) {
            return Err(format!(
                "the code font size must be {} to {} px, not {}",
                FONT_SIZES.start(),
                FONT_SIZES.end(),
                self.code_font_size
            ));
        }
        if !TAB_WIDTHS.contains(&self.tab_width) {
            return Err(format!(
                "the tab width must be 2, 4 or 8, not {}",
                self.tab_width
            ));
        }
        self.check_custom_agents()?;
        self.check_choices()
    }

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
            if agents::program_of(&agent.command).is_none() {
                return Err(format!("the agent {name:?} needs a command"));
            }
            if agent.command.contains(['"', '\'', '\\']) {
                return Err(format!(
                    "the command of {name:?} cannot hold quotes or backslashes: it is split on \
                     spaces as it is"
                ));
            }
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
        let enabled = |name: &String| self.enabled_agents.contains(name);
        if let Some(agent) = self.agent.as_ref().filter(|agent| !enabled(agent)) {
            return Err(format!(
                "notes cannot go to {agent:?}: it is not an enabled agent"
            ));
        }
        if let Some(reviewer) = self.reviewers.iter().find(|reviewer| !enabled(reviewer)) {
            return Err(format!(
                "{reviewer:?} cannot review: it is not an enabled agent"
            ));
        }
        Ok(())
    }
}

/// Reads the settings from `file`. Never fails: a file that cannot be used gives the defaults
/// and the reason (see the module docs).
pub(crate) fn load(file: &Path) -> Loaded {
    match read(file) {
        Ok(settings) => Loaded {
            settings: settings.unwrap_or_default(),
            problem: None,
        },
        Err(problem) => Loaded {
            settings: Settings::default(),
            problem: Some(format!(
                "{problem}. The defaults are in use, and saving is off until the file is fixed \
                 or reset."
            )),
        },
    }
}

/// The file's settings, `None` when there is no file, or why the file cannot be used.
fn read(file: &Path) -> std::result::Result<Option<Settings>, String> {
    let shown = file.display();
    let json = match fs::read_to_string(file) {
        Ok(json) => json,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{shown} cannot be read: {error}")),
    };
    match serde_json::from_str::<VersionOnly>(&json) {
        Ok(VersionOnly { version }) if version > VERSION => {
            return Err(format!(
                "{shown} was written by a newer Postil (format version {version})"
            ));
        }
        Ok(VersionOnly { version }) if version != VERSION => {
            return Err(format!("{shown} has an unknown format version {version}"));
        }
        _ => {}
    }
    let stored: SettingsFile<Settings> = serde_json::from_str(&json)
        .map_err(|error| format!("{shown} is not valid settings JSON: {error}"))?;
    stored
        .settings
        .check()
        .map_err(|problem| format!("{shown} breaks a rule: {problem}"))?;
    Ok(Some(stored.settings))
}

/// Writes `settings` to `file`.
///
/// # Errors
///
/// - [`Error::InvalidSettings`] when `settings` breaks a rule; nothing is written.
/// - [`Error::SettingsLocked`] when the file on disk cannot be used (see the module docs);
///   nothing is written.
/// - [`Error::WriteSettings`] when the file cannot be written.
pub(crate) fn save(file: &Path, settings: &Settings) -> Result<()> {
    settings.check().map_err(Error::InvalidSettings)?;
    if let Err(problem) = read(file) {
        return Err(Error::SettingsLocked { problem });
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
    let mut json = serde_json::to_vec_pretty(&stored).expect("settings always serialize");
    json.push(b'\n');
    write_atomically(file, &json).map_err(|source| Error::WriteSettings {
        path: file.to_owned(),
        source,
    })
}
