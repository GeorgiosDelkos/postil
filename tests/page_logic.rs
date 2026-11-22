//! Runs the page's JavaScript unit tests (`ui/*.test.js`) with Node's built-in test runner.
//!
//! The page has no npm packages, so `node --test` is the whole toolchain. Node expands the glob
//! itself; this was checked on Node.js 26.13 and not on older versions. Running it from
//! here keeps `cargo test` the one command that checks everything.

use std::process::Command;

#[test]
fn page_logic_tests_pass_under_node() {
    let output = Command::new("node")
        .args(["--test", "ui/*.test.js"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("node is on PATH: the page tests need Node.js (checked with 26.13)");

    assert!(
        output.status.success(),
        "node --test ui/ failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
