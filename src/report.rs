//! The author's report: the reading order and the checks that the agent which made a run's
//! changes gives at the end of its reply (docs/review-assist.md, "The author's report").
//!
//! The prompt asks for it in words (`notes::REPORT_ASK`), never with an example block, so a
//! reply that quotes the prompt holds nothing that reads as a report. [`read`] takes the last
//! json block that is an object, and keeps what it can use: a step or check it cannot read
//! is left out and counted in [`Report::cut`], never guessed. It reads no file: a step's lines
//! are not checked against the file, and the walkthrough shows a step that names no changed
//! lines (decided with the owner on 2026-11-25).

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::anchor::LineRange;
use crate::review::last_json_block;

/// The most steps a report keeps. Arbitrary: it guards the dialog, not the agent.
const MAX_STEPS: usize = 200;

/// The most checks a report keeps. Arbitrary, as [`MAX_STEPS`].
const MAX_CHECKS: usize = 50;

/// The most characters of a step's `why` a report keeps. Arbitrary: the prompt asks for one
/// line, and a card shows one.
const MAX_WHY: usize = 300;

/// A report as the run log keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Report {
    /// The order to read the changes in: cause before effect.
    pub(crate) steps: Vec<Step>,
    /// The commands the agent says it ran to check its work.
    pub(crate) checks: Vec<Check>,
    /// How many steps and checks the agent wrote that were left out or shortened: a path
    /// that leaves the repository, lines that are not a range, a field missing, a limit
    /// passed.
    pub(crate) cut: usize,
}

/// One step of the reading order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Step {
    /// Relative to the repository root.
    pub(crate) path: String,
    /// On the new side of the diff. `None` for a file that is gone: the step covers the
    /// whole file, whatever lines the agent named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lines: Option<LineRange>,
    pub(crate) why: String,
    pub(crate) risk: Risk,
}

/// How much a step needs the engineer, as the agent rates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Risk {
    /// Needs the engineer's judgment. Also a rating Postil does not know: the safe reading.
    Eyes,
    Routine,
    /// A rename, formatting, generated code.
    Mechanical,
}

/// A command the agent says it ran, and what it says came of it: its claim, never Postil's
/// finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Check {
    pub(crate) command: String,
    pub(crate) result: Claimed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Claimed {
    Passed,
    Failed,
}

/// The report at the end of `reply`, from an agent that worked in `root`: the last json
/// block that is an object, which must have `steps` or `checks`. Each step's path is made
/// relative to `root` as tool paths are, and a step whose path is gone covers the whole
/// file. Asks the file system whether each step's path exists, and reads nothing else.
///
/// # Errors
///
/// Why no report can be read: no json block that is an object, or one with neither `steps`
/// nor `checks`, or with either not a list.
pub(crate) fn read(reply: &str, root: &Path) -> Result<Report, String> {
    // A map and not a struct: serde reads a struct from an array too, by position.
    let block: Map<String, Value> = last_json_block(reply)?;
    let list = |key: &str| match block.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => Ok(Some(items.as_slice())),
        Some(_) => Err(format!("the report's {key} are not a list")),
    };
    let (steps, checks) = match (list("steps")?, list("checks")?) {
        (None, None) => {
            return Err("the reply's last json object has no steps and no checks".to_owned());
        }
        (steps, checks) => (steps.unwrap_or_default(), checks.unwrap_or_default()),
    };

    let mut cut = 0;
    let mut kept_steps = Vec::new();
    for value in steps {
        match step(value, root) {
            Some((step, shortened)) if kept_steps.len() < MAX_STEPS => {
                cut += usize::from(shortened);
                kept_steps.push(step);
            }
            Some(_) | None => cut += 1,
        }
    }
    let mut kept_checks = Vec::new();
    for value in checks {
        match check(value) {
            Some(check) if kept_checks.len() < MAX_CHECKS => kept_checks.push(check),
            Some(_) | None => cut += 1,
        }
    }
    Ok(Report {
        steps: kept_steps,
        checks: kept_checks,
        cut,
    })
}

/// One step, and whether its `why` was shortened; `None` when it cannot be used.
fn step(value: &Value, root: &Path) -> Option<(Step, bool)> {
    let path = repo_path(value.get("path")?.as_str()?, root)?;
    // A file the run deleted has no new side to name lines on.
    let lines = if root.join(&path).symlink_metadata().is_ok() {
        Some(line_range(value.get("lines")?)?)
    } else {
        None
    };
    let why = value.get("why").and_then(Value::as_str).unwrap_or_default();
    let shortened = why.chars().count() > MAX_WHY;
    let why = if shortened {
        let kept: String = why.chars().take(MAX_WHY).collect();
        format!("{kept}…")
    } else {
        why.to_owned()
    };
    let risk = match value.get("risk").and_then(Value::as_str) {
        Some("routine") => Risk::Routine,
        Some("mechanical") => Risk::Mechanical,
        _ => Risk::Eyes,
    };
    let step = Step {
        path,
        lines,
        why,
        risk,
    };
    Some((step, shortened))
}

fn check(value: &Value) -> Option<Check> {
    let command = value.get("command")?.as_str()?.trim();
    let result = match value.get("result")?.as_str()? {
        "passed" => Claimed::Passed,
        "failed" => Claimed::Failed,
        _ => return None,
    };
    (!command.is_empty()).then(|| Check {
        command: command.to_owned(),
        result,
    })
}

/// `raw` relative to `root`, as the run log makes tool paths relative (`run/log.rs`,
/// `relative`); `None` when it is empty or leaves the repository. A path outside `root` comes
/// back from `strip_prefix` unchanged, so one that is still absolute, or climbs with `..`, is
/// outside.
fn repo_path(raw: &str, root: &Path) -> Option<String> {
    let path = Path::new(raw).strip_prefix(root).unwrap_or(Path::new(raw));
    let outside = path.components().any(|part| {
        matches!(
            part,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    let path = path.to_str()?;
    (!outside && !path.is_empty()).then(|| path.to_owned())
}

/// `[first, last]`, 1-based with `first <= last`.
fn line_range(value: &Value) -> Option<LineRange> {
    let [start, end] = value.as_array()?.as_slice() else {
        return None;
    };
    let start = usize::try_from(start.as_u64()?).ok()?;
    let end = usize::try_from(end.as_u64()?).ok()?;
    (1 <= start && start <= end).then_some(LineRange { start, end })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing::TempDir;

    /// An agent's reply that ends with `block`.
    fn reply(block: &Value) -> String {
        format!("N1: done\n\n```json\n{block}\n```\n")
    }

    /// A repository root holding `a.txt`.
    fn root(name: &str) -> TempDir {
        let dir = TempDir::new(name);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        dir
    }

    /// The step `raw` reads as, or `None` when it is left out.
    fn one_step(raw: &Value, root: &Path) -> Option<Step> {
        let report = read(&reply(&json!({ "steps": [raw] })), root).unwrap();
        assert_eq!(
            report.cut,
            1 - report.steps.len(),
            "a step is kept or counted"
        );
        report.steps.into_iter().next()
    }

    #[test]
    fn a_step_is_kept_only_with_a_path_in_the_repository_and_lines_that_are_a_range() {
        let dir = root("report-steps");
        let root = dir.path();
        let absolute = root.join("a.txt").to_string_lossy().into_owned();
        let kept = |path: &str, lines: Option<(usize, usize)>| {
            Some(Step {
                path: path.to_owned(),
                lines: lines.map(|(start, end)| LineRange { start, end }),
                why: "w".to_owned(),
                risk: Risk::Routine,
            })
        };
        let cases = [
            (
                "a range",
                json!({"path": "a.txt", "lines": [1, 2]}),
                kept("a.txt", Some((1, 2))),
            ),
            (
                "one line",
                json!({"path": "a.txt", "lines": [3, 3]}),
                kept("a.txt", Some((3, 3))),
            ),
            // Not checked against the file: the walkthrough shows a step that names nothing.
            (
                "past the end",
                json!({"path": "a.txt", "lines": [8, 9]}),
                kept("a.txt", Some((8, 9))),
            ),
            (
                "absolute inside",
                json!({"path": absolute, "lines": [1, 1]}),
                kept("a.txt", Some((1, 1))),
            ),
            (
                "a deleted file",
                json!({"path": "gone.txt", "lines": [5, 9]}),
                kept("gone.txt", None),
            ),
            (
                "a deleted file, no lines",
                json!({"path": "gone.txt"}),
                kept("gone.txt", None),
            ),
            ("climbs out", json!({"path": "../x", "lines": [1, 1]}), None),
            (
                "climbs out midway",
                json!({"path": "src/../../x", "lines": [1, 1]}),
                None,
            ),
            (
                "absolute outside",
                json!({"path": "/etc/x", "lines": [1, 1]}),
                None,
            ),
            ("empty path", json!({"path": "", "lines": [1, 1]}), None),
            ("no path", json!({"lines": [1, 1]}), None),
            ("reversed", json!({"path": "a.txt", "lines": [2, 1]}), None),
            ("line zero", json!({"path": "a.txt", "lines": [0, 1]}), None),
            ("negative", json!({"path": "a.txt", "lines": [-1, 1]}), None),
            ("one number", json!({"path": "a.txt", "lines": [1]}), None),
            (
                "not numbers",
                json!({"path": "a.txt", "lines": ["1", "2"]}),
                None,
            ),
            ("no lines on a file there", json!({"path": "a.txt"}), None),
        ];
        for (case, mut raw, expected) in cases {
            raw["why"] = json!("w");
            raw["risk"] = json!("routine");
            assert_eq!(one_step(&raw, root), expected, "{case}");
        }
    }

    #[test]
    fn a_rating_postil_does_not_know_is_eyes_and_a_long_why_is_cut_and_counted() {
        let dir = root("report-fields");
        let risk = |risk: Value| {
            one_step(
                &json!({"path": "a.txt", "lines": [1, 1], "risk": risk}),
                dir.path(),
            )
            .unwrap()
            .risk
        };
        assert_eq!(risk(json!("routine")), Risk::Routine);
        assert_eq!(risk(json!("mechanical")), Risk::Mechanical);
        assert_eq!(risk(json!("eyes")), Risk::Eyes);
        assert_eq!(risk(json!("trivial")), Risk::Eyes, "unknown");
        assert_eq!(risk(Value::Null), Risk::Eyes);

        let long = "x".repeat(MAX_WHY + 1);
        let block = json!({"steps": [{"path": "a.txt", "lines": [1, 1], "why": long}]});
        let report = read(&reply(&block), dir.path()).unwrap();
        assert_eq!(report.steps[0].why, format!("{}…", "x".repeat(MAX_WHY)));
        assert_eq!(report.cut, 1, "kept, and counted as shortened");
        let block =
            json!({"steps": [{"path": "a.txt", "lines": [1, 1], "why": "x".repeat(MAX_WHY)}]});
        assert_eq!(
            read(&reply(&block), dir.path()).unwrap().cut,
            0,
            "at the limit"
        );
    }

    #[test]
    fn checks_need_a_command_and_a_claim_and_both_lists_stop_at_their_limits() {
        let dir = root("report-checks");
        let block = json!({"checks": [
            {"command": " cargo test ", "result": "passed"},
            {"command": "cargo build", "result": "failed"},
            {"command": "cargo run", "result": "maybe"},
            {"command": "  ", "result": "passed"},
            {"result": "passed"},
        ]});
        let report = read(&reply(&block), dir.path()).unwrap();
        assert_eq!(
            report.checks,
            [
                Check {
                    command: "cargo test".to_owned(),
                    result: Claimed::Passed
                },
                Check {
                    command: "cargo build".to_owned(),
                    result: Claimed::Failed
                },
            ]
        );
        assert_eq!((report.steps.len(), report.cut), (0, 3));

        let step = json!({"path": "a.txt", "lines": [1, 1]});
        let check = json!({"command": "c", "result": "passed"});
        let block = json!({
            "steps": vec![step; MAX_STEPS + 2],
            "checks": vec![check; MAX_CHECKS + 1],
        });
        let report = read(&reply(&block), dir.path()).unwrap();
        assert_eq!(
            (report.steps.len(), report.checks.len(), report.cut),
            (MAX_STEPS, MAX_CHECKS, 3)
        );
    }

    #[test]
    fn a_reply_with_no_report_says_why() {
        let dir = root("report-none");
        let problem = |reply: &str| read(reply, dir.path()).unwrap_err();
        assert!(problem("Done, no block.").contains("no ```json block"));
        assert!(
            problem("```json\n[1, 2]\n```").contains("does not parse"),
            "an array is no report"
        );
        assert!(problem("```json\n{\"x\": 1}\n```").contains("no steps and no checks"));
        assert!(problem("```json\n{\"steps\": \"a\"}\n```").contains("steps are not a list"));
        assert!(problem("```json\n{\"checks\": {}}\n```").contains("checks are not a list"));
        // An object with only one of the lists is a report.
        let report = read("```json\n{\"steps\": []}\n```", dir.path()).unwrap();
        assert_eq!((report.steps.len(), report.checks.len()), (0, 0));
    }

    /// Replies of random shapes, from a seeded generator so a failure repeats: `read` never
    /// panics, and every step it keeps has lines `1 <= start <= end` or none.
    #[test]
    fn no_reply_panics_and_every_kept_range_is_ordered() {
        let dir = root("report-random");
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move |below: u64| {
            // xorshift64: a generator with no crate, enough to vary the shapes.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % below
        };
        for _ in 0..2000 {
            let mut pick = |values: &[Value]| {
                let index = usize::try_from(next(values.len() as u64)).expect("below the length");
                values[index].clone()
            };
            let numbers = [
                json!(0),
                json!(1),
                json!(7),
                json!(-3),
                json!(u64::MAX),
                json!(1.5),
                json!("2"),
                Value::Null,
            ];
            let lines = json!([pick(&numbers), pick(&numbers)]);
            let paths = [
                json!("a.txt"),
                json!("gone.txt"),
                json!("../a.txt"),
                json!("/a.txt"),
                json!(""),
                json!(7),
            ];
            let step =
                json!({"path": pick(&paths), "lines": pick(&[lines, json!([]), json!("1-2")])});
            let block = pick(&[
                json!({"steps": [step]}),
                json!({"steps": step}),
                json!([step]),
                json!({"checks": [step]}),
            ]);
            let Ok(report) = read(&reply(&block), dir.path()) else {
                continue;
            };
            for step in report.steps {
                if let Some(lines) = step.lines {
                    assert!(1 <= lines.start && lines.start <= lines.end, "{block}");
                }
            }
        }
    }
}
