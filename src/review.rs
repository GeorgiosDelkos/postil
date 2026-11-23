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

use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// What is reviewed: the run's prompt (the notes as the agent got them), its reply, and the
/// diffs its edits made, in order. Its `Debug` shows sizes and paths only, never the text:
/// notes and code stay out of debug output, as for a run.
pub(crate) struct Work {
    pub(crate) prompt: String,
    pub(crate) reply: String,
    pub(crate) diffs: Vec<Diff>,
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

/// The reviewed work, as every reviewer reads it.
fn context(text: &mut String, work: &Work) {
    write!(
        text,
        "The engineer's notes, as the agent received them:\n\n{}\n\nThe agent's reply:\n\n{}\n\n",
        work.prompt.trim_end(),
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

/// The findings in a proposer's reply: the last ```` ```json ```` block that starts a line and
/// holds an array, of objects, numbered from `F{first}` on. A field of the wrong type is left
/// out; an entry without its text is skipped.
///
/// # Errors
///
/// Why the findings cannot be read: no JSON block, or none that is an array (the last
/// block's problem is given).
pub(crate) fn findings_in(reply: &str, first: usize) -> Result<Vec<Finding>, String> {
    const FENCE: &str = "```json";
    // A fence opens a block only at the start of a line, after blanks at most. JSON strings
    // cannot hold a raw line break, so a fence that a finding's text quotes never starts a
    // line, and is passed over here. From the last block backwards: a block
    // after the findings that is not an array (an example, say) does not hide them.
    let opens_block = |at: usize| {
        let before = reply[..at].trim_end_matches([' ', '\t']);
        before.is_empty() || before.ends_with('\n')
    };
    let mut problem = None;
    for (start, _) in reply
        .rmatch_indices(FENCE)
        .filter(|(at, _)| opens_block(*at))
    {
        match array_after(&reply[start + FENCE.len()..]) {
            Ok(entries) => return Ok(numbered(&entries, first)),
            Err(error) => {
                problem.get_or_insert(error);
            }
        }
    }
    Err(problem.unwrap_or_else(|| "the reply has no ```json block of findings".to_owned()))
}

/// The JSON array at the start of `body`, after any whitespace. It ends where the JSON says,
/// not at the next fence: a finding's text can quote code in a fence of its own.
fn array_after(body: &str) -> Result<Vec<Value>, String> {
    serde_json::Deserializer::from_str(body)
        .into_iter::<Vec<Value>>()
        .next()
        .ok_or("the ```json block of findings is empty")?
        .map_err(|error| format!("its findings are not a JSON array: {error}"))
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

    fn work() -> Work {
        Work {
            prompt: "[N1] src/a.rs:2\n> one();\nrename one\n".to_owned(),
            reply: "Renamed it.\n\nN1: renamed one to two".to_owned(),
            diffs: vec![Diff {
                path: "src/a.rs".to_owned(),
                before: Some("    one();\n".to_owned()),
                after: "    two();\n".to_owned(),
            }],
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
                .contains("not a JSON array")
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
