//! Pull requests on GitHub, through GitHub's `gh` command (docs/design.md, "Ending a thread").
//! `gh` keeps the engineer's sign-in, finds the repository from the checkout's remotes, and
//! knows GitHub Enterprise hosts; Postil holds no token of its own.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::thread::PullState;

/// A pull request to open with [`create`].
#[derive(Debug)]
pub(crate) struct NewPullRequest<'a> {
    /// The branch it merges into.
    pub(crate) base: &'a str,
    /// The branch it brings, pushed already.
    pub(crate) head: &'a str,
    pub(crate) title: &'a str,
    pub(crate) body: &'a str,
}

/// Opens `new` as a draft pull request with `gh` at `program`, run in the checkout `dir` with
/// `search_path` as `PATH`, and returns its URL. A draft: the engineer reads it on GitHub and
/// marks it ready there.
///
/// # Errors
///
/// [`Error::GitHub`] with `gh`'s message: not signed in, a head branch not on the remote, one
/// with an open pull request already.
pub(crate) fn create(
    program: &Path,
    dir: &Path,
    search_path: &OsStr,
    new: &NewPullRequest<'_>,
) -> Result<String> {
    let args = [
        "pr",
        "create",
        "--draft",
        "--base",
        new.base,
        "--head",
        new.head,
        "--title",
        new.title,
        "--body-file",
        "-",
    ];
    match run(program, dir, search_path, &args, Some(new.body)) {
        // `gh pr create` prints the URL on its last line.
        Ok(printed) => pull_request_url(&printed).ok_or_else(|| Error::GitHub {
            command: "gh pr create".to_owned(),
            message: format!("it printed no pull request URL: {printed}"),
        }),
        // One the record lacks (a write that failed, a quit, another window): gh names it
        // ("a pull request for branch ... already exists:" and the URL), and it is the one.
        Err(Error::GitHub { command, message }) if message.contains("already exists") => {
            pull_request_url(&message).ok_or(Error::GitHub { command, message })
        }
        Err(error) => Err(error),
    }
}

/// The last line of `text` that is a pull request's URL: `https://` and `/pull/`. Only such a
/// line is kept, since the page opens it in the browser.
fn pull_request_url(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("https://") && line.contains("/pull/"))
        .map(str::to_owned)
}

/// The state of the pull request at `url`, read with `gh` as [`create`] runs it.
///
/// # Errors
///
/// [`Error::GitHub`] with `gh`'s message, or for a state it does not know.
pub(crate) fn state(
    program: &Path,
    dir: &Path,
    search_path: &OsStr,
    url: &str,
) -> Result<PullState> {
    let args = ["pr", "view", url, "--json", "state", "--jq", ".state"];
    let printed = run(program, dir, search_path, &args, None)?;
    match printed.trim() {
        "OPEN" => Ok(PullState::Open),
        "MERGED" => Ok(PullState::Merged),
        "CLOSED" => Ok(PullState::Closed),
        other => Err(Error::GitHub {
            command: "gh pr view".to_owned(),
            message: format!("unknown state {other:?}"),
        }),
    }
}

/// An open GitHub issue returned by `gh issue list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GhIssue {
    pub(crate) number: u64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: String,
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) labels: Vec<String>,
    #[serde(default)]
    pub(crate) assignees: Vec<String>,
    #[serde(default)]
    pub(crate) author: String,
    #[serde(default)]
    pub(crate) created_at: String,
    #[serde(default)]
    pub(crate) updated_at: String,
    #[serde(default)]
    pub(crate) comments_count: usize,
}

/// An open GitHub pull request returned by `gh pr list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GhPr {
    pub(crate) number: u64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: String,
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) labels: Vec<String>,
    #[serde(default)]
    pub(crate) assignees: Vec<String>,
    #[serde(default)]
    pub(crate) author: String,
    #[serde(default)]
    pub(crate) head_ref: String,
    #[serde(default)]
    pub(crate) head_oid: String,
    #[serde(default)]
    pub(crate) base_ref: String,
    #[serde(default)]
    pub(crate) is_draft: bool,
    #[serde(default)]
    pub(crate) mergeable: String,
    #[serde(default)]
    pub(crate) review_decision: String,
    #[serde(default)]
    pub(crate) checks_state: String,
    #[serde(default)]
    pub(crate) additions: u64,
    #[serde(default)]
    pub(crate) deletions: u64,
    #[serde(default)]
    pub(crate) changed_files: u64,
    #[serde(default)]
    pub(crate) created_at: String,
    #[serde(default)]
    pub(crate) updated_at: String,
}

/// One inline comment submitted as part of a pull request review.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrReviewComment {
    pub(crate) path: String,
    pub(crate) line: u64,
    pub(crate) body: String,
}

/// Overall review verdict submitted to GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PrReviewVerdict {
    Comment,
    Approve,
    RequestChanges,
}

impl PrReviewVerdict {
    fn api_event(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }

    fn cli_flag(self) -> &'static str {
        match self {
            Self::Comment => "--comment",
            Self::Approve => "--approve",
            Self::RequestChanges => "--request-changes",
        }
    }
}

/// Lists up to 50 open issues for the repository in `dir` via `gh issue list`.
///
/// # Errors
///
/// [`Error::GitHub`] when `gh` fails or returns unreadable JSON.
pub(crate) fn list_issues(program: &Path, dir: &Path, search_path: &OsStr) -> Result<Vec<GhIssue>> {
    let args = [
        "issue",
        "list",
        "--state",
        "open",
        "--limit",
        "50",
        "--json",
        "number,title,body,url,labels,assignees,author,createdAt,updatedAt,comments",
    ];
    let stdout = run(program, dir, search_path, &args, None)?;
    parse_issues_json(&stdout).map_err(|message| Error::GitHub {
        command: "gh issue list".to_owned(),
        message,
    })
}

/// Lists up to 50 open pull requests for the repository in `dir` via `gh pr list`.
///
/// # Errors
///
/// [`Error::GitHub`] when `gh` fails or returns unreadable JSON.
pub(crate) fn list_prs(program: &Path, dir: &Path, search_path: &OsStr) -> Result<Vec<GhPr>> {
    let args = [
        "pr",
        "list",
        "--state",
        "open",
        "--limit",
        "50",
        "--json",
        "number,title,body,url,labels,assignees,author,headRefName,headRefOid,baseRefName,isDraft,mergeable,reviewDecision,statusCheckRollup,additions,deletions,changedFiles,createdAt,updatedAt",
    ];
    let stdout = run(program, dir, search_path, &args, None)?;
    parse_prs_json(&stdout).map_err(|message| Error::GitHub {
        command: "gh pr list".to_owned(),
        message,
    })
}

/// Reads the unified diff of pull request `number` via `gh pr diff`.
///
/// # Errors
///
/// [`Error::GitHub`] when `gh` fails.
pub(crate) fn pr_diff(
    program: &Path,
    dir: &Path,
    search_path: &OsStr,
    number: u64,
) -> Result<String> {
    let num = number.to_string();
    let args = ["pr", "diff", &num];
    run(program, dir, search_path, &args, None)
}

/// Parameters for submitting a pull request review via [`submit_pr_review`].
#[derive(Debug)]
pub(crate) struct PrReviewSubmission<'a> {
    pub(crate) number: u64,
    pub(crate) verdict: PrReviewVerdict,
    pub(crate) body: &'a str,
    pub(crate) comments: &'a [PrReviewComment],
}

/// Submits a pull request review (with optional inline comments) to GitHub via `gh`.
///
/// # Errors
///
/// [`Error::GitHub`] when `gh` fails.
pub(crate) fn submit_pr_review(
    program: &Path,
    dir: &Path,
    search_path: &OsStr,
    review: &PrReviewSubmission<'_>,
) -> Result<()> {
    if review.comments.is_empty() {
        let num = review.number.to_string();
        let args = [
            "pr",
            "review",
            &num,
            review.verdict.cli_flag(),
            "--body-file",
            "-",
        ];
        let fallback_body = if review.body.trim().is_empty() {
            match review.verdict {
                PrReviewVerdict::Approve => "Approved via Postil.",
                PrReviewVerdict::RequestChanges => "Changes requested via Postil.",
                PrReviewVerdict::Comment => "Reviewed via Postil.",
            }
        } else {
            review.body
        };
        drop(run(program, dir, search_path, &args, Some(fallback_body))?);
        return Ok(());
    }
    let slug = run(
        program,
        dir,
        search_path,
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ],
        None,
    )?;
    let slug = slug.trim();
    if slug.is_empty() || !slug.contains('/') {
        return Err(Error::GitHub {
            command: "gh repo view".to_owned(),
            message: format!("could not determine owner/repo slug: {slug:?}"),
        });
    }
    let api_comments: Vec<Value> = review
        .comments
        .iter()
        .map(|c| {
            serde_json::json!({
                "path": c.path,
                "line": c.line,
                "side": "RIGHT",
                "body": c.body,
            })
        })
        .collect();
    let payload = serde_json::json!({
        "event": review.verdict.api_event(),
        "body": review.body,
        "comments": api_comments,
    });
    let endpoint = format!("repos/{slug}/pulls/{}/reviews", review.number);
    let args = ["api", &endpoint, "--method", "POST", "--input", "-"];
    let input = payload.to_string();
    drop(run(program, dir, search_path, &args, Some(&input))?);
    Ok(())
}

fn parse_issues_json(stdout: &str) -> std::result::Result<Vec<GhIssue>, String> {
    let raw: Vec<Value> = serde_json::from_str(stdout).map_err(|e| format!("invalid JSON: {e}"))?;
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        let Some(number) = item.get("number").and_then(Value::as_u64) else {
            continue;
        };
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let body = item
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let url = item
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let labels = item
            .get("labels")
            .and_then(Value::as_array)
            .map_or_default(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            });
        let assignees = item
            .get("assignees")
            .and_then(Value::as_array)
            .map_or_default(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("login").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            });
        let author = item
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let created_at = item
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let updated_at = item
            .get("updatedAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let comments_count = item
            .get("comments")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        out.push(GhIssue {
            number,
            title,
            body,
            url,
            labels,
            assignees,
            author,
            created_at,
            updated_at,
            comments_count,
        });
    }
    Ok(out)
}

fn parse_prs_json(stdout: &str) -> std::result::Result<Vec<GhPr>, String> {
    let raw: Vec<Value> = serde_json::from_str(stdout).map_err(|e| format!("invalid JSON: {e}"))?;
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        let Some(number) = item.get("number").and_then(Value::as_u64) else {
            continue;
        };
        let str_field = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let labels = item
            .get("labels")
            .and_then(Value::as_array)
            .map_or_default(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            });
        let assignees = item
            .get("assignees")
            .and_then(Value::as_array)
            .map_or_default(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("login").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            });
        let author = item
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let checks_state = summarize_checks(item.get("statusCheckRollup"));
        out.push(GhPr {
            number,
            title: str_field("title"),
            body: str_field("body"),
            url: str_field("url"),
            labels,
            assignees,
            author,
            head_ref: str_field("headRefName"),
            head_oid: str_field("headRefOid"),
            base_ref: str_field("baseRefName"),
            is_draft: item
                .get("isDraft")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            mergeable: str_field("mergeable"),
            review_decision: str_field("reviewDecision"),
            checks_state,
            additions: item.get("additions").and_then(Value::as_u64).unwrap_or(0),
            deletions: item.get("deletions").and_then(Value::as_u64).unwrap_or(0),
            changed_files: item
                .get("changedFiles")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            created_at: str_field("createdAt"),
            updated_at: str_field("updatedAt"),
        });
    }
    Ok(out)
}

fn summarize_checks(rollup: Option<&Value>) -> String {
    let Some(items) = rollup.and_then(Value::as_array) else {
        return String::new();
    };
    if items.is_empty() {
        return String::new();
    }
    let mut saw_pending = false;
    let mut saw_pass = false;
    for entry in items {
        let state = entry
            .get("conclusion")
            .or_else(|| entry.get("state"))
            .or_else(|| entry.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match state {
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" => {
                return "failing".to_owned();
            }
            "PENDING" | "IN_PROGRESS" | "QUEUED" | "EXPECTED" => saw_pending = true,
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => saw_pass = true,
            _ => {}
        }
    }
    if saw_pending {
        "pending".to_owned()
    } else if saw_pass {
        "passing".to_owned()
    } else {
        String::new()
    }
}

/// Runs `gh` with `args` in `dir`, writing `input` to its standard input, and returns what it
/// printed.
fn run(
    program: &Path,
    dir: &Path,
    search_path: &OsStr,
    args: &[&str],
    input: Option<&str>,
) -> Result<String> {
    let command_line = format!("gh {}", args[..2].join(" "));
    let failed = |message: String| Error::GitHub {
        command: command_line.clone(),
        message,
    };
    let mut child = Command::new(program)
        .args(args)
        .current_dir(dir)
        // Credential helpers and `git`, which gh runs, are found by the login shell's PATH.
        .env("PATH", search_path)
        // Never a question on a terminal there is none of: gh fails and says why instead.
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("NO_COLOR", "1")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| failed(error.to_string()))?;
    let output = std::thread::scope(|scope| {
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            // On a thread of its own, as `repo.rs` writes to git: gh may exit before it reads
            // the body (not signed in), which fails the write, and its exit status and message
            // then say why. The pipe closes when `stdin` drops, which ends gh's read.
            scope.spawn(move || drop(stdin.write_all(input.as_bytes())));
        }
        child.wait_with_output()
    })
    .map_err(|error| failed(error.to_string()))?;
    if !output.status.success() {
        return Err(failed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
