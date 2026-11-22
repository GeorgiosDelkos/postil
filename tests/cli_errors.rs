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
    std::fs::create_dir_all(&dir).expect("the temporary directory is writable");
    dir
}

#[test]
fn missing_git_is_named_with_the_fix_and_exits_1() {
    let output = Command::new(env!("CARGO_BIN_EXE_postil"))
        .arg(std::env::temp_dir())
        .env("PATH", "")
        .output()
        .expect("the test binary was built by cargo");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("cannot run git (is it installed and on PATH?)"),
        "{stderr}"
    );
}

#[test]
fn bad_arguments_print_usage_and_exit_2() {
    let output = postil(&["--bench"]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--bench needs a file"), "{stderr}");
    assert!(stderr.contains("usage: postil"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}

#[test]
fn directory_outside_a_repository_is_named_and_exits_1() {
    let dir = temp_dir("not-a-repo");

    let output = postil(&[dir.to_str().unwrap()]);
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let expected = format!("postil: {} is not in a git repository\n", dir.display());
    assert_eq!(stderr, expected);
}

#[test]
fn a_path_that_is_not_a_folder_is_named_and_exits_1() {
    let output = postil(&["/no/such/folder"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr, "postil: /no/such/folder is not a folder\n");
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
