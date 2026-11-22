//! The `postil` binary's failures that happen before any window opens.

use std::path::PathBuf;
use std::process::{Command, Output};

fn postil(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_postil"))
        .args(args)
        // The assertions quote git's messages, which are translated under other locales.
        .env("LC_ALL", "C")
        .output()
        .expect("the test binary was built by cargo")
}

/// An empty directory in the temporary directory. `name` must differ between tests.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("postil-cli-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn bad_arguments_print_usage_and_exit_2() {
    let output = postil(&["--bench"]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--bench needs a file"), "{stderr}");
    assert!(stderr.contains("usage: postil"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn directory_outside_a_repository_reports_git_and_exits_1() {
    let dir = temp_dir("not-a-repo");

    let output = postil(&[dir.to_str().unwrap()]);
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("postil: git rev-parse"), "{stderr}");
    assert!(stderr.contains("not a git repository"), "{stderr}");
}

#[test]
fn bench_of_a_missing_file_fails_instead_of_timing_an_empty_pane() {
    let dir = temp_dir("bench-missing");
    let init = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["init", "--quiet"])
        .status()
        .unwrap();
    assert!(init.success());

    let output = postil(&[dir.to_str().unwrap(), "--bench", "typo.rs"]);
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("typo.rs"), "{stderr}");
    assert!(
        output.stdout.is_empty(),
        "no report for a file that was never shown"
    );
}
