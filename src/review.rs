//! Reviews: a chain of reviewer agents checks a run's work (docs/design.md, "Reviews").
//!
//! The reviewers are the settings' ordered list. Every reviewer but the last is read-only (as
//! far as its tool calls ask permission; `run.rs` checks its edits as it ends) and proposes
//! findings, ending its reply with them as a JSON block; the last judges all the
//! findings and makes the changes it accepts, ending with one line per finding. A single
//! reviewer only proposes. Decided with the owner on 2026-11-23.
//!
//! This module holds what a stage is, the prompts, and the reading of findings. The runner
//! (`run.rs`) starts each stage as a run of its own, in order.

use std::fmt::{self, Write as _};
use std::ops::Range;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::report::{self, Report, Risk, Step};

/// Where a review run stands: it reviews run `of` as reviewer `reviewer` (from 1) of `reviewers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Stage {
    pub(crate) of: String,
    pub(crate) reviewer: usize,
    pub(crate) reviewers: usize,
}

impl Stage {
    /// Whether this reviewer judges the findings and makes the changes: the last of two or
    /// more. Every other reviewer proposes, read-only.
    pub(crate) fn judges(&self) -> bool {
        self.reviewers > 1 && self.reviewer == self.reviewers
    }
}

/// A finding a proposing reviewer made. `id` (`F1`, `F2`, ...) counts across the chain's
/// proposers, so the judge answers each by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Finding {
    pub(crate) id: String,
    /// The note it is about (`N3`), when the reviewer says.
    pub(crate) note: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) line: Option<u64>,
    pub(crate) text: String,
    /// The change the reviewer proposes.
    pub(crate) fix: Option<String>,
}

/// What is reviewed: the run's prompt (the notes as the agent got them), its reply, the
/// diffs its edits made, in order, and the author's report when it gave one. Its `Debug`
/// shows sizes and paths only, never the text: notes and code stay out of debug output, as
/// for a run.
pub(crate) struct Work {
    pub(crate) prompt: String,
    pub(crate) reply: String,
    pub(crate) diffs: Vec<Diff>,
    pub(crate) report: Option<Report>,
}

/// One edit of a file: its text before (none for a new file) and after.
#[derive(Debug)]
pub(crate) struct Diff {
    pub(crate) path: String,
    pub(crate) before: Option<String>,
    pub(crate) after: String,
}

impl fmt::Debug for Work {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Work")
            .field("prompt_len", &self.prompt.len())
            .field("reply_len", &self.reply.len())
            .field(
                "diffs",
                &self.diffs.iter().map(|diff| &diff.path).collect::<Vec<_>>(),
            )
            .field(
                "report_steps",
                &self.report.as_ref().map(|report| report.steps.len()),
            )
            .finish()
    }
}

/// The prompt of a proposing reviewer.
pub(crate) fn propose_prompt(work: &Work) -> String {
    let mut text = String::from(
        "You are reviewing work another coding agent did in this repository. Do not change \
         any file: you may only read and search.\n\n",
    );
    context(&mut text, work);
    rated_low(&mut text, work);
    text.push_str(
        "Review the changes against the notes: wrong behaviour, a note not addressed, a change \
         the notes did not ask for, missing tests. Read the files as they are now where you \
         need more context.\n\nEnd your reply with every finding as a JSON array in one code \
         block fenced as json: one object per finding, with the fields note (the note id it is \
         about, such as N1, or null), path, line, finding (what is wrong) and fix (what to \
         change). An empty array when you find nothing.\n",
    );
    text
}

/// The prompt of the judging reviewer, given every proposer's findings.
pub(crate) fn judge_prompt(work: &Work, findings: &[Finding]) -> String {
    let mut text = String::from(
        "You are the last reviewer of work another coding agent did in this repository. \
         Earlier reviewers, who could not change files, proposed the findings below. Judge \
         each against the code as it is now: accept it when it is right and worth fixing, and \
         make the change; reject it when it is wrong or not worth it. Change nothing else.\n\n",
    );
    context(&mut text, work);
    text.push_str("The findings:\n\n");
    for finding in findings {
        text.push_str(&finding.id);
        if let Some(note) = &finding.note {
            write!(text, " [{note}]").expect("writing to a String cannot fail");
        }
        match (&finding.path, finding.line) {
            (Some(path), Some(line)) => {
                write!(text, " {path}:{line}").expect("writing to a String cannot fail");
            }
            (Some(path), None) => {
                write!(text, " {path}").expect("writing to a String cannot fail");
            }
            (None, _) => {}
        }
        write!(text, ": {}", finding.text).expect("writing to a String cannot fail");
        if let Some(fix) = &finding.fix {
            write!(text, " Proposed fix: {fix}").expect("writing to a String cannot fail");
        }
        text.push('\n');
    }
    text.push_str(
        "\nWhen you finish, write one line per finding, `F1: accepted - what you changed` or \
         `F2: rejected - why`.\n",
    );
    text
}

/// The steps the author's report rated as needing no judgment, for a proposer to check: the
/// engineer reads those last, so a rating too low is a finding (docs/review-assist.md, "The
/// reviewer checks the ratings"). Nothing when the report has none, or there is no report.
fn rated_low(text: &mut String, work: &Work) {
    let low: Vec<(&Step, &str)> = work
        .report
        .iter()
        .flat_map(|report| &report.steps)
        .filter_map(|step| match step.risk {
            Risk::Eyes => None,
            Risk::Routine => Some((step, "routine")),
            Risk::Mechanical => Some((step, "mechanical")),
        })
        .collect();
    if low.is_empty() {
        return;
    }
    text.push_str("The agent's report rates these changes as needing no judgment:\n\n");
    for (step, risk) in low {
        match step.lines {
            Some(lines) => writeln!(
                text,
                "- {}:{}-{} ({risk})",
                step.path, lines.start, lines.end
            ),
            None => writeln!(text, "- {} ({risk})", step.path),
        }
        .expect("writing to a String cannot fail");
    }
    text.push_str("\nReport as a finding any of them that does need the engineer's judgment.\n\n");
}

/// The reviewed work, as every reviewer reads it. The prompt is quoted without its ask for
/// the author's report ([`report::ASK`]): a reviewer that obeyed it would end with that object
/// instead of its findings.
fn context(text: &mut String, work: &Work) {
    let notes = work
        .prompt
        .replace(report::ASK, "")
        .replace(report::ASK_AGAIN, "");
    write!(
        text,
        "The engineer's notes, as the agent received them:\n\n{}\n\nThe agent's reply:\n\n{}\n\n",
        notes.trim_end(),
        work.reply.trim(),
    )
    .expect("writing to a String cannot fail");
    if work.diffs.is_empty() {
        // Edits made through a command report no diff, so "no file changed" could be false.
        text.push_str("No edit diffs were reported; read the files to see their state.\n\n");
        return;
    }
    text.push_str("What the agent changed, each edit's text before and after:\n\n");
    for diff in &work.diffs {
        writeln!(text, "{}, before:", diff.path).expect("writing to a String cannot fail");
        match &diff.before {
            Some(before) => {
                writeln!(text, "```\n{}\n```", before.trim_end())
                    .expect("writing to a String cannot fail");
            }
            None => text.push_str("(a new file)\n"),
        }
        writeln!(text, "after:\n```\n{}\n```\n", diff.after.trim_end())
            .expect("writing to a String cannot fail");
    }
}

/// The findings in a proposer's reply: the last ```` ```json ```` block that holds an array
/// ([`last_json_block`]), of objects, numbered from `F{first}` on. A field of the wrong type is
/// left out; an entry without its text is skipped.
///
/// # Errors
///
/// As [`last_json_block`]. The page shows the problem after "A reviewer's findings could not be
/// read", so it does not repeat that.
pub(crate) fn findings_in(reply: &str, first: usize) -> Result<Vec<Finding>, String> {
    let entries: Vec<Value> = last_json_block(reply)?;
    Ok(numbered(&entries, first))
}

/// The value of the last ```` ```json ```` block in `reply` that starts a line and parses as a
/// `T`. The value ends where the JSON says, not at the next fence, so a string in it can quote
/// code in a fence of its own.
///
/// # Errors
///
/// Why no block could be read: the reply has none, or none parses as a `T` (the last block's
/// problem is given).
pub(crate) fn last_json_block<T: DeserializeOwned>(reply: &str) -> Result<T, String> {
    last_json_block_in(reply).map(|(value, _)| value)
}

/// [`last_json_block`], and where in `reply` the block is: from its fence to the end of its
/// closing fence, or of the value when no fence closes it on a line after it.
///
/// # Errors
///
/// As [`last_json_block`].
pub(crate) fn last_json_block_in<T: DeserializeOwned>(
    reply: &str,
) -> Result<(T, Range<usize>), String> {
    const FENCE: &str = "```json";
    // A fence opens a block only at the start of a line, after blanks at most. JSON strings
    // cannot hold a raw line break, so a fence that a string quotes never starts a line, and
    // is passed over here. From the last block backwards: a block after the wanted one that is
    // not a `T` (an example, say) does not hide it.
    let opens_block = |at: usize| {
        let before = reply[..at].trim_end_matches([' ', '\t']);
        before.is_empty() || before.ends_with('\n')
    };
    let mut problem = None;
    for (start, _) in reply
        .rmatch_indices(FENCE)
        .filter(|(at, _)| opens_block(*at))
    {
        let body_start = start + FENCE.len();
        let mut values = serde_json::Deserializer::from_str(&reply[body_start..]).into_iter::<T>();
        match values.next() {
            Some(Ok(value)) => {
                let value_end = body_start + values.byte_offset();
                return Ok((value, start..block_end(reply, value_end)));
            }
            Some(Err(error)) => {
                problem.get_or_insert_with(|| {
                    format!("the reply's last ```json block does not parse: {error}")
                });
            }
            None => {
                problem.get_or_insert_with(|| "the reply's last ```json block is empty".to_owned());
            }
        }
    }
    Err(problem.unwrap_or_else(|| "the reply has no ```json block".to_owned()))
}

/// Where a block whose value ends at `value_end` ends: after its closing fence when only
/// blanks come between the two and the fence ends its line, else at the value.
fn block_end(reply: &str, value_end: usize) -> usize {
    let rest = &reply[value_end..];
    let fence_at = rest.len() - rest.trim_start().len();
    let after_fence = &rest[fence_at..];
    let closes = after_fence.strip_prefix("```").is_some_and(|tail| {
        let line = tail.split('\n').next().unwrap_or_default();
        line.trim().is_empty()
    });
    if closes {
        value_end + fence_at + "```".len()
    } else {
        value_end
    }
}

/// The findings `entries` hold, numbered from `F{first}` on.
fn numbered(entries: &[Value], first: usize) -> Vec<Finding> {
    let text = |entry: &Value, key: &str| entry[key].as_str().map(str::to_owned);
    entries
        .iter()
        .filter_map(|entry| {
            let finding = text(entry, "finding").or_else(|| text(entry, "text"))?;
            Some((entry, finding))
        })
        .enumerate()
        .map(|(index, (entry, finding))| Finding {
            id: format!("F{}", first + index),
            note: text(entry, "note"),
            path: text(entry, "path"),
            line: entry["line"].as_u64(),
            text: finding,
            fix: text(entry, "fix"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::LineRange;

    fn work() -> Work {
        Work {
            prompt: format!(
                "Address every note.\n{}\n[N1] src/a.rs:2\n> one();\nrename one\n",
                report::ASK
            ),
            reply: "Renamed it.\n\nN1: renamed one to two".to_owned(),
            diffs: vec![Diff {
                path: "src/a.rs".to_owned(),
                before: Some("    one();\n".to_owned()),
                after: "    two();\n".to_owned(),
            }],
            report: Some(Report {
                steps: vec![
                    step("src/a.rs", Some((2, 2)), Risk::Mechanical),
                    step("src/b.rs", None, Risk::Routine),
                    step("src/c.rs", Some((1, 4)), Risk::Eyes),
                ],
                checks: Vec::new(),
                cut: 0,
                block: String::new(),
            }),
        }
    }

    fn step(path: &str, lines: Option<(usize, usize)>, risk: Risk) -> Step {
        Step {
            path: path.to_owned(),
            lines: lines.map(|(start, end)| LineRange { start, end }),
            why: "secret why".to_owned(),
            risk,
        }
    }

    #[test]
    fn debug_output_shows_the_work_s_sizes_and_paths_never_its_text() {
        let shown = format!("{:?}", work());
        assert!(shown.contains("src/a.rs"), "{shown}");
        for secret in ["rename one", "renamed one to two", "two();"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[test]
    fn only_the_last_of_two_or_more_reviewers_judges() {
        let stage = |reviewer, reviewers| Stage {
            of: "R1".to_owned(),
            reviewer,
            reviewers,
        };
        assert!(!stage(1, 1).judges(), "a single reviewer only proposes");
        assert!(!stage(1, 2).judges());
        assert!(stage(2, 2).judges());
        assert!(!stage(2, 3).judges());
        assert!(stage(3, 3).judges());
    }

    #[test]
    fn a_proposer_sees_the_notes_the_reply_and_the_changes_and_is_told_not_to_edit() {
        let prompt = propose_prompt(&work());
        assert!(prompt.contains("Do not change any file"), "{prompt}");
        assert!(prompt.contains("rename one"), "{prompt}");
        assert!(prompt.contains("N1: renamed one to two"), "{prompt}");
        assert!(
            prompt.contains("src/a.rs, before:\n```\n    one();\n```"),
            "{prompt}"
        );
        assert!(prompt.contains("after:\n```\n    two();\n```"), "{prompt}");
        // The format in words: an example block would be read back as findings by anything
        // that quotes the prompt.
        assert!(!prompt.contains("```json"), "{prompt}");
        assert!(prompt.contains("fenced as json"), "{prompt}");

        let empty = Work {
            diffs: Vec::new(),
            ..work()
        };
        assert!(propose_prompt(&empty).contains("No edit diffs were reported"));
    }

    /// The reviewer checks the author's ratings: the changes it rated as needing no judgment
    /// are named, the others are not.
    #[test]
    fn a_proposer_is_asked_to_check_the_changes_the_author_rated_low() {
        let prompt = propose_prompt(&work());
        assert!(
            prompt.contains(
                "rates these changes as needing no judgment:\n\n- src/a.rs:2-2 (mechanical)\n\
                 - src/b.rs (routine)\n"
            ),
            "{prompt}"
        );
        assert!(!prompt.contains("src/c.rs"), "an eyes step: {prompt}");
        assert!(
            prompt.contains("Report as a finding any of them"),
            "{prompt}"
        );

        // The author was asked for a report; a reviewer told the same would end with an object
        // and not its findings.
        let follow_up = Work {
            prompt: format!("and more\n\n{}", report::ASK_AGAIN),
            ..work()
        };
        for quoted in [propose_prompt(&work()), judge_prompt(&follow_up, &[])] {
            assert!(!quoted.contains("end your reply with"), "{quoted}");
        }
        assert!(propose_prompt(&work()).contains("Address every note.\n\n[N1]"));

        let no_report = Work {
            report: None,
            ..work()
        };
        assert!(!propose_prompt(&no_report).contains("needing no judgment"));
        assert!(
            !judge_prompt(&work(), &[]).contains("needing no judgment"),
            "the judge judges findings"
        );
    }

    #[test]
    fn the_judge_gets_every_finding_by_id_and_says_what_to_answer() {
        let findings = findings_in(
            "```json\n[{\"note\": \"N1\", \"path\": \"src/a.rs\", \"line\": 2, \"finding\": \
             \"two is a bad name\", \"fix\": \"call it renamed\"}, \
             {\"finding\": \"no test\"}]\n```",
            1,
        )
        .unwrap();
        let prompt = judge_prompt(&work(), &findings);
        assert!(
            prompt
                .contains("F1 [N1] src/a.rs:2: two is a bad name Proposed fix: call it renamed\n"),
            "{prompt}"
        );
        assert!(prompt.contains("F2: no test\n"), "{prompt}");
        assert!(
            prompt.contains("`F1: accepted - what you changed`"),
            "{prompt}"
        );
    }

    #[test]
    fn findings_are_read_from_the_last_json_block_and_numbered_on() {
        let reply = "Here is an example:\n```json\n[{\"finding\": \"not this\"}]\n```\nThe \
                     findings:\n```json\n[{\"finding\": \"a\", \"line\": \"7\"}, {\"text\": \
                     \"b\", \"note\": null}, {\"fix\": \"no text\"}]\n```\nDone.";
        let findings = findings_in(reply, 3).unwrap();
        assert_eq!(
            findings
                .iter()
                .map(|finding| (finding.id.as_str(), finding.text.as_str()))
                .collect::<Vec<_>>(),
            [("F3", "a"), ("F4", "b")],
            "the last block; an entry without text skipped"
        );
        assert_eq!(
            findings[0].line, None,
            "a line that is not a number is left out"
        );
        assert_eq!(findings[1].note, None);
        assert_eq!(findings_in("```json\n[]\n```", 1), Ok(Vec::new()));
        let fenced = findings_in(
            "```json\n[{\"finding\": \"x\", \"fix\": \"```rust\\nfoo()\\n```\"}]\n```",
            1,
        )
        .unwrap();
        assert_eq!(
            fenced[0].fix.as_deref(),
            Some("```rust\nfoo()\n```"),
            "a fence inside a fix"
        );
        assert!(
            findings_in("no block", 1)
                .unwrap_err()
                .contains("no ```json block")
        );
        assert!(
            findings_in("```json\n{\"finding\": \"x\"}\n```", 1)
                .unwrap_err()
                .contains("does not parse: invalid type: map, expected a sequence")
        );
    }

    /// A report is an object, where findings are an array.
    #[test]
    fn the_last_json_block_reads_an_object_and_passes_over_a_later_array() {
        #[derive(Debug, PartialEq, Eq, Deserialize)]
        struct Probe {
            steps: Vec<u32>,
        }
        let reply = "```json\n{\"steps\": [1]}\n```\nThen:\n```json\n{\"steps\": [2]}\n```\n\
                     ```json\n[3]\n```\nDone.";
        assert_eq!(last_json_block(reply), Ok(Probe { steps: vec![2] }));
        assert!(
            last_json_block::<Probe>("```json\n\"no\"\n```")
                .unwrap_err()
                .contains("expected struct Probe")
        );
    }

    /// Replies that quote a ```` ```json ```` fence inside a finding, or put other blocks
    /// around the findings. Each case gives the texts of the findings read, or a part of the
    /// error.
    #[test]
    fn a_fence_quoted_in_a_finding_or_another_block_does_not_hide_the_findings() {
        let found = |texts: &[&str]| -> Result<Vec<String>, String> {
            Ok(texts.iter().map(|text| (*text).to_owned()).collect())
        };
        let cases = [
            (
                // What follows the quoted fence parses as an empty array: read from there, the
                // findings would be lost without a word.
                "a quoted fence followed by an empty array",
                "```json\n[{\"finding\": \"a\", \"fix\": \"write ```json [] ``` here\"}]\n```",
                found(&["a"]),
            ),
            (
                "a quoted fence followed by text that is not JSON",
                "```json\n[{\"finding\": \"a\", \"fix\": \"```json\\n{\\\"x\\\": 1}\\n```\"}, \
                 {\"text\": \"b\"}]\n```",
                found(&["a", "b"]),
            ),
            (
                "an example block that is not an array after the findings",
                "```json\n[{\"finding\": \"a\"}]\n```\nFor example:\n```json\n{\"x\": 1}\n```",
                found(&["a"]),
            ),
            (
                "a block indented with spaces and a tab",
                "Findings:\n \t```json\n  [{\"finding\": \"a\"}]\n  ```",
                found(&["a"]),
            ),
            (
                "a fence inside a line opens no block",
                "I answer in ```json [{\"finding\": \"a\"}]",
                Err("no ```json block".to_owned()),
            ),
            (
                "no block holds an array: the last block's problem",
                "```json\n{\"x\": 1}\n```\n```json\n\n```",
                // The first block's problem would be "invalid type: map".
                Err("expected value".to_owned()),
            ),
        ];
        for (case, reply, expected) in cases {
            let read: Result<Vec<String>, String> = findings_in(reply, 1)
                .map(|findings| findings.into_iter().map(|finding| finding.text).collect());
            match (read, expected) {
                (Ok(read), Ok(expected)) => assert_eq!(read, expected, "{case}"),
                (Err(read), Err(expected)) => assert!(read.contains(&expected), "{case}: {read}"),
                (read, expected) => panic!("{case}: read {read:?}, expected {expected:?}"),
            }
        }
    }
}
