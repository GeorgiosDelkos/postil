//! The `postil` command: opens the review window for a repository.

use std::error::Error as _;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: postil [DIR] [--bench FILE]

  DIR           a directory inside a git repository (default: the current directory)
  --bench FILE  open FILE (relative to the repository root), scroll through it,
                print launch and frame timings, and exit";

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
struct Args {
    dir: PathBuf,
    bench: Option<String>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut dir = None;
    let mut bench = None;

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--bench" {
            let file = args.next().ok_or("--bench needs a file")?;
            if bench.replace(file).is_some() {
                return Err("--bench given twice".to_owned());
            }
        } else if arg.starts_with('-') {
            return Err(format!("unknown option {arg}"));
        } else if dir.replace(PathBuf::from(&arg)).is_some() {
            return Err(format!("unexpected second directory {arg}"));
        }
    }

    Ok(Args {
        dir: dir.unwrap_or_else(|| PathBuf::from(".")),
        bench,
    })
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "main is the command's one output function: usage, bench report, errors"
)]
fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(problem) => {
            eprintln!("postil: {problem}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let outcome = match &args.bench {
        Some(file) => postil::bench(&args.dir, file).map(|report| {
            println!("launch_to_paint_ms {}", report.launch_to_paint.as_millis());
            println!("page {}", report.page);
        }),
        None => postil::run(&args.dir),
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprint!("postil: {error}");
            let mut source = error.source();
            while let Some(cause) = source {
                eprint!(": {cause}");
                source = cause.source();
            }
            eprintln!();
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn defaults_to_the_current_directory() {
        let expected = Args {
            dir: PathBuf::from("."),
            bench: None,
        };
        assert_eq!(parse(&[]).unwrap(), expected);
    }

    #[test]
    fn takes_a_directory_and_a_bench_file_in_either_order() {
        let expected = Args {
            dir: PathBuf::from("repo"),
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
