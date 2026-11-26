//! The `postil` command: opens the review window, on a repository or on the welcome screen.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
usage: postil [DIR] [--bench FILE]

  DIR           a directory inside a git repository; without it, the window opens on the
                welcome screen (open a folder, or a recent project)
  --bench FILE  open FILE (relative to the root of DIR's repository, or of the current
                directory's), scroll through it, print launch and frame timings, and exit";

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
struct Args {
    dir: Option<PathBuf>,
    bench: Option<String>,
}

/// Reads the arguments as the system gives them: a directory may be any bytes, and only the
/// bench file, which the page receives as JSON, must be UTF-8.
fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Args, String> {
    let mut dir = None;
    let mut bench = None;

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let shown = arg.to_string_lossy();
        if arg == "--bench" {
            let file = args.next().ok_or("--bench needs a file")?;
            let file = file.into_string().map_err(|file| {
                format!("--bench needs a UTF-8 file name, not {}", file.display())
            })?;
            if bench.replace(file).is_some() {
                return Err("--bench given twice".to_owned());
            }
        } else if shown.starts_with('-') {
            return Err(format!("unknown option {shown}"));
        } else if dir.replace(PathBuf::from(&arg)).is_some() {
            return Err(format!("unexpected second directory {shown}"));
        }
    }

    Ok(Args { dir, bench })
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "main is the command's one output function: usage, bench report, errors"
)]
fn main() -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(problem) => {
            eprintln!("postil: {problem}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let outcome = match &args.bench {
        // A benchmark needs a project: the current directory's when none is given.
        Some(file) => {
            let dir = args.dir.as_deref().unwrap_or(Path::new("."));
            postil::bench(dir, file).map(|report| {
                println!("launch_to_paint_ms {}", report.launch_to_paint.as_millis());
                println!("page {}", report.page);
                report.over_budget()
            })
        }
        None => postil::run(args.dir.as_deref()).map(|()| None),
    };

    match outcome {
        Ok(None) => ExitCode::SUCCESS,
        // The report is printed above, so the run that missed is on record.
        Ok(Some(missed)) => {
            eprintln!("postil: bench: {missed}");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("postil: {}", postil::error_chain(&error));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn without_a_directory_there_is_no_project_to_open() {
        let expected = Args {
            dir: None,
            bench: None,
        };
        assert_eq!(parse(&[]).unwrap(), expected);
    }

    #[test]
    fn takes_a_directory_and_a_bench_file_in_either_order() {
        let expected = Args {
            dir: Some(PathBuf::from("repo")),
            bench: Some("src/a.rs".to_owned()),
        };
        assert_eq!(parse(&["repo", "--bench", "src/a.rs"]).unwrap(), expected);
        assert_eq!(parse(&["--bench", "src/a.rs", "repo"]).unwrap(), expected);
    }

    #[test]
    fn rejects_what_it_does_not_understand() {
        assert_eq!(parse(&["--bench"]).unwrap_err(), "--bench needs a file");
        assert_eq!(parse(&["--wat"]).unwrap_err(), "unknown option --wat");
        assert_eq!(
            parse(&["--bench", "a", "--bench", "b"]).unwrap_err(),
            "--bench given twice"
        );
        assert_eq!(
            parse(&["a", "b"]).unwrap_err(),
            "unexpected second directory b"
        );
    }
}
