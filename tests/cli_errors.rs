//! The `postil` binary's failures that happen before any window opens.

use std::path::PathBuf;
use std::process::{Command, Output};

/// The `postil` binary, with its configuration (the recent list) in a temporary directory:
/// a test must not write the engineer's own `~/.config/postil`.
fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_postil"));
    command
        .env("XDG_CONFIG_HOME", temp_dir("config"))
        // The assertions quote git's messages, which are translated under other locales.
        .env("LC_ALL", "C");
    command
}

fn postil(args: &[&str]) -> Output {
    command()
        .args(args)
        .output()
        .expect("the test binary was built by cargo")
}

/// A new git repository in the temporary directory. `name` must differ between tests.
fn temp_repo(name: &str) -> PathBuf {
    let dir = temp_dir(name);
    let init = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["init", "--quiet"])
        .status()
        .expect("git runs");
    assert!(init.success());
    dir
}

/// An empty directory in the temporary directory. `name` must differ between tests.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("postil-cli-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the temporary directory is writable");
    dir
}

#[test]
fn missing_git_is_named_with_the_fix_and_exits_1() {
    let output = command()
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
    let dir = temp_repo("bench-missing");

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

// The next tests run a benchmark of a missing file: it fails right after the project opens
// and before any window, so reaching its "typo.rs" error shows the project opened.

#[test]
fn a_plain_folder_is_recognised_whatever_the_language_of_git() {
    let dir = temp_dir("german");

    // German, when git here speaks it: the app must not depend on git's language. A git built
    // without translations, or a machine without the locale, prints English anyway, and this
    // test then proves nothing, so on a laptop it says so and stops. On CI it fails instead:
    // a skip there would leave the property unchecked everywhere.
    let probe = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .arg("rev-parse")
        .env("LC_ALL", "de_DE.UTF-8")
        .output()
        .expect("git runs");
    if String::from_utf8_lossy(&probe.stderr).contains("not a git repository") {
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            std::env::var_os("CI").is_none(),
            "CI's git must translate to German for this test; install one that does"
        );
        eprintln!("skipped: git here does not translate its messages to German");
        return;
    }
    let output = command()
        .env("LC_ALL", "de_DE.UTF-8")
        .arg(&dir)
        .output()
        .expect("the test binary was built by cargo");
    std::fs::remove_dir_all(&dir).unwrap();

    let stderr = String::from_utf8(output.stderr).unwrap();
    let expected = format!("postil: {} is not in a git repository\n", dir.display());
    assert_eq!(stderr, expected);
}

#[test]
fn without_a_home_directory_a_project_still_opens() {
    let dir = temp_repo("no-home");

    let output = command()
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .args([dir.to_str().unwrap(), "--bench", "typo.rs"])
        .output()
        .expect("the test binary was built by cargo");
    std::fs::remove_dir_all(&dir).unwrap();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("typo.rs"), "{stderr}");
}

#[test]
fn the_home_folder_is_never_offered_to_become_a_repository() {
    let home = temp_dir("home");

    let output = command()
        .env("HOME", &home)
        .arg(&home)
        .output()
        .expect("the test binary was built by cargo");
    std::fs::remove_dir_all(&home).unwrap();

    let stderr = String::from_utf8(output.stderr).unwrap();
    let expected = format!(
        "postil: {} is not in a git repository (open a project folder inside it)\n",
        home.display()
    );
    assert_eq!(stderr, expected);
}

#[test]
fn a_git_dir_left_in_the_environment_does_not_redirect_git() {
    let dir = temp_repo("git-dir");

    let output = command()
        .env("GIT_DIR", "/no/such/git/dir")
        .args([dir.to_str().unwrap(), "--bench", "typo.rs"])
        .output()
        .expect("the test binary was built by cargo");
    std::fs::remove_dir_all(&dir).unwrap();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("typo.rs"), "{stderr}");
}

#[test]
fn a_file_given_as_the_folder_is_named_and_exits_1() {
    let dir = temp_dir("file-not-folder");
    let file = dir.join("a.txt");
    std::fs::write(&file, "").expect("the temporary directory is writable");

    let output = postil(&[file.to_str().unwrap()]);
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr,
        format!("postil: {} is not a folder\n", file.display())
    );
}

#[test]
fn a_folder_that_cannot_be_looked_at_keeps_git_s_explanation() {
    use std::os::unix::fs::PermissionsExt;

    // A folder inside a parent nobody may enter: it exists, so it is not "not a folder".
    let parent = temp_dir("locked");
    let inside = parent.join("project");
    std::fs::create_dir_all(&inside).expect("the temporary directory is writable");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000))
        .expect("the test owns the folder");

    let output = postil(&[inside.to_str().unwrap()]);
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))
        .expect("the test owns the folder");
    std::fs::remove_dir_all(&parent).unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("postil: git rev-parse"), "{stderr}");
    assert!(!stderr.contains("is not a folder"), "{stderr}");
}
