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
//!   whatever the case, built-in ones included, and a command of plain words (no quotes)
//!   whose program is a name on the search path or an absolute path;
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Theme {
    /// Follow the system's light or dark appearance.
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
            // Destructured, so a change to the offered widths fails to compile here until
            // the message follows it.
            let [narrow, middle, wide] = TAB_WIDTHS;
            return Err(format!(
                "the tab width must be {narrow}, {middle} or {wide}, not {}",
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
            let Some(program) = agents::program_of(&agent.command) else {
                return Err(format!("the agent {name:?} needs a command"));
            };
            // A relative path would be looked up from Postil's working directory (`/` when
            // started from the Finder) but run from the project's, so it names two files.
            if program.contains('/') && !program.starts_with('/') {
                return Err(format!(
                    "the program of {name:?} must be a name on the search path or an absolute \
                     path, not {program}"
                ));
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
    /// A fingerprint of the file's text ([`revision_of`]), empty when there is no file or
    /// it cannot be read. A save names the revision it started from.
    pub(crate) revision: String,
}

/// Reads the settings from `file`. Never fails: a file that cannot be used gives the defaults
/// and the reason (see the module docs).
pub(crate) fn load(file: &Path) -> Loaded {
    let (read, revision) = read(file);
    match read {
        Ok(settings) => Loaded {
            settings: settings.unwrap_or_default(),
            problem: None,
            revision,
        },
        Err(problem) => Loaded {
            settings: Settings::default(),
            problem: Some(format!(
                "{problem}. The defaults are in use, and saving is off until the file is fixed \
                 or reset."
            )),
            revision,
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
    match serde_json::from_str::<VersionOnly>(json) {
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
    let stored: SettingsFile<Settings> = serde_json::from_str(json)
        .map_err(|error| format!("{shown} is not valid settings JSON: {error}"))?;
    stored
        .settings
        .check()
        .map_err(|problem| format!("{shown} breaks a rule: {problem}"))?;
    Ok(stored.settings)
}

/// Writes `settings` to `file`, if the file is still at `revision`: the one the change
/// started from. Another window, or a hand edit, may have changed it since, and saving the
/// whole settings over that would drop the other change without a word. The check and the
/// write are not one step: within a process saves run on the main thread one at a time, but
/// two Postil processes saving in the same instant can both pass it, and the later wins.
///
/// # Errors
///
/// - [`Error::InvalidSettings`] when `settings` breaks a rule; nothing is written.
/// - [`Error::SettingsChanged`] when the file is no longer at `revision`; nothing is
///   written.
/// - [`Error::SettingsLocked`] when the file on disk cannot be used (see the module docs);
///   nothing is written.
/// - [`Error::WriteSettings`] when the file cannot be written.
pub(crate) fn save(file: &Path, settings: &Settings, revision: &str) -> Result<()> {
    settings.check().map_err(Error::InvalidSettings)?;
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
    let mut json = serde_json::to_vec_pretty(&stored).expect("settings always serialize");
    json.push(b'\n');
    write_atomically(file, &json).map_err(|source| Error::WriteSettings {
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
}
