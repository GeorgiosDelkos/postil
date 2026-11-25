//! Pull requests on GitHub, through GitHub's `gh` command (docs/design.md, "Ending a thread").
//! `gh` keeps the engineer's sign-in, finds the repository from the checkout's remotes, and
//! knows GitHub Enterprise hosts; Postil holds no token of its own.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

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
        Err(Error::GitHub { message, .. })
            if message.contains("already exists") && pull_request_url(&message).is_some() =>
        {
            Ok(pull_request_url(&message).expect("checked by the guard"))
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
