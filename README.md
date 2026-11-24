# Postil

[![test](https://github.com/GeorgiosDelkos/postil/actions/workflows/test.yml/badge.svg)](https://github.com/GeorgiosDelkos/postil/actions/workflows/test.yml)
[![check](https://github.com/GeorgiosDelkos/postil/actions/workflows/check.yml/badge.svg)](https://github.com/GeorgiosDelkos/postil/actions/workflows/check.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Postil is a macOS app for reviewing code with a coding agent. You open a local git repository,
read the code, write notes on line ranges, and send the notes to an agent over the
[Agent Client Protocol](https://agentclientprotocol.com) (ACP). You watch the agent work, see
what it changed, and send follow-ups or more notes on the changes, all in one window.

A postil is a note written in the margin of a text.

![Postil's changes view: the diff a run made to two files, with the note it addressed under the
changed lines, and the Run tab on the right.](docs/screenshot.png)

Postil is a reader with a notes tray and an agent panel. It is not an editor, a terminal or a
pull request tool: the agent edits, and you read and write notes.

> **Status:** early (0.1.0). macOS only. The app is built from source, and can be signed and
> notarized with your own Developer ID (see [Build and run](#build-and-run)); there is no
> published release yet.

## Contents

- [How it works](#how-it-works)
- [Requirements](#requirements)
- [Build and run](#build-and-run)
- [Agents](#agents)
- [Keyboard](#keyboard)
- [Where Postil keeps its data](#where-postil-keeps-its-data)
- [Development](#development)
- [Project layout](#project-layout)
- [License](#license)

## How it works

1. **Open** a repository: `postil .`, the Open Folder dialog, a recent project, or a folder
   dropped on the window. A folder that is not a repository is offered `git init`.
2. **Read** the code: a file tree, a fuzzy finder, and a code pane with syntax colors for Rust,
   JavaScript, TypeScript, Python, Go, JSON and TOML.
3. **Note** a line range: select lines and press `c` (or right-click, Add note), type, and
   press `Cmd-Enter`. A note keeps the text of its lines, so it follows that text when the file
   changes.
4. **Send** notes to an agent. The send step shows the exact prompt the agent will receive.
   A single note can also be sent from its card or its dialog.
5. **Watch** the run in the Run tab: the agent's plan, every tool call, its questions and its
   reply. Postil allows the agent's tool calls itself; only a question the agent asks waits for
   you.
6. **Verify** the result:
   - **Changes:** a run's dialog shows what the run changed as a diff per file, with your notes
     beside the changed lines. Postil snapshots the working tree before and after each run as a
     git tree, without touching your index, branch or stash.
   - **Follow-ups:** ask the same agent for more from the run's dialog. The follow-up goes to
     the same conversation while the agent is still running.
   - **Reviews:** send a run's work to a chain of reviewer agents. Every reviewer but the last
     proposes findings without changing files; the last judges them and makes the changes it
     accepts.
   - **Resolve** a note when you are done with it, or write a new note on the change itself.

Every view uses small cards; a card opens a dialog with everything about its note or run.

## Requirements

- macOS (the app uses the system WebKit through [wry](https://github.com/tauri-apps/wry)).
- [Rust](https://rustup.rs). The toolchain is pinned in `rust-toolchain.toml` (1.100.0);
  rustup installs it on the first build.
- git, on the `PATH`.
- [Node.js](https://nodejs.org), for the page's tests and for the agents started with `npx`.
- At least one ACP agent (see [Agents](#agents)).

## Build and run

```sh
git clone https://github.com/GeorgiosDelkos/postil.git
cd postil
cargo run --release -- /path/to/a/repository
```

`postil` with no folder shows the welcome screen with the recent projects.

```text
usage: postil [DIR] [--bench FILE]
```

To build a macOS app bundle with the icon:

```sh
scripts/bundle.sh
open target/release/Postil.app
```

To build it and install it in `/Applications` (or `~/Applications` when `/Applications` is not
writable), moving an older copy to the Trash:

```sh
scripts/install.sh
```

To sign the app with a Developer ID certificate and notarize it, so it opens on other Macs
without a Gatekeeper warning, store your App Store Connect API key in the keychain once (keep
the `.p8` file outside the repository, and delete it after):

```sh
xcrun notarytool store-credentials postil \
  --key /path/to/AuthKey_XXXXXXXXXX.p8 --key-id XXXXXXXXXX --issuer xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

Then set these, and `scripts/install.sh` signs and notarizes before it installs
(`scripts/sign.sh` signs a bundle on its own):

```sh
export POSTIL_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)"
export POSTIL_NOTARY_PROFILE=postil
```

Without `POSTIL_NOTARY_PROFILE` it signs only.

Releases are built in CI: pushing a tag `vX.Y.Z` that matches the version in `Cargo.toml` runs
`.github/workflows/release.yml`, which signs and notarizes the app with the secrets of the
repository's `release` environment and publishes a zip of it. Running the workflow by hand is a
dry run with no release.

## Agents

Postil talks to any agent that speaks ACP over stdio. It detects these on the `PATH` and lists
them in Settings (`Cmd-,`):

| Agent    | Command it runs                                   |
| -------- | ------------------------------------------------- |
| Claude   | `npx -y @agentclientprotocol/claude-agent-acp`    |
| Codex    | `npx -y @zed-industries/codex-acp`                |
| Gemini   | `gemini --acp`                                    |
| Grok     | `grok agent stdio`                                |
| OpenCode | `opencode acp`                                    |

You can add any other agent by name and command in Settings. Settings also choose the agent
notes go to by default and the reviewers, in order, for a review. Each agent signs in on its
own: if a run fails at start, run the agent's command in a terminal once.

## Keyboard

| Keys                 | Action                                                    |
| -------------------- | --------------------------------------------------------- |
| `Cmd-O`              | Open a folder                                             |
| `Cmd-P`              | Find a file                                               |
| `Cmd-\`              | Focus mode: the code alone, centered                      |
| `c`                  | Write a note on the selected lines                        |
| `Cmd-Enter`          | Save a note; in a dialog, send it or save the edit        |
| `Cmd-Shift-Enter`    | Open the send step; press again to send                   |
| `Esc`                | Stop the run; close a dialog, the send step or the view   |
| `a`, `Shift-a`, `r`  | Answer a permission request Postil could not answer       |
| `Cmd-,`              | Settings                                                  |

## Where Postil keeps its data

Postil writes nothing into your working tree. Its files are plain JSON, so a script or an agent
can read and write them too.

| File                                   | What it holds                                  |
| -------------------------------------- | ---------------------------------------------- |
| `<git dir>/postil/review.json`         | The notes of the working tree                  |
| `<git dir>/postil/runs/R<n>.jsonl`     | Each run's log: its prompt and every event     |
| `<git dir>/postil/folders.json`        | The folders open in the file list              |
| `<git dir>/postil/snapshot/index`      | The index Postil writes snapshots through      |
| `~/.config/postil/settings.json`       | Agents, the default agent, reviewers, the look |
| `~/.config/postil/layout.json`         | The widths of the panes                        |
| `~/.config/postil/recent.json`         | The recent projects                            |

`<git dir>` is the repository's `.git` directory, or the worktree's own git directory for a
linked worktree, so each worktree keeps its own notes. `~/.config` is `$XDG_CONFIG_HOME` when
that is set. Snapshots are git objects no ref holds, so `git gc` may prune them after its
prune period (two weeks by default).

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked          # Rust tests, and the page's tests through Node
node --test ui/              # the page's tests alone
```

[prek](https://github.com/j178/prek) (`cargo install --locked prek`)
runs CI's lints (rustfmt, clippy, rustdoc, oxlint, oxfmt) on every commit from
`.pre-commit-config.yaml`, once `prek install` has been run in the clone.

The Rust tests run end to end: they drive the app's HTTP routes against real git repositories
in temporary directories and a fake ACP agent (`tests/fake-agent.mjs`) over real pipes. One
test runs a real agent when `POSTIL_REAL_AGENT` is set.

The page's end-to-end tests (`tests/e2e/page.test.js`) click through the page in Playwright's
WebKit while a run streams, against real sites served over HTTP on localhost by
`src/site/tests/page_e2e.rs`. They catch clicks lost to a node the page replaced while it drew
the run again, which the page's unit tests cannot. They need Playwright and its WebKit build,
installed once into `tests/e2e/`, the one place in the repository with npm packages:

```sh
(cd tests/e2e && npm ci --ignore-scripts && npx --no-install playwright-core install webkit)
cargo test --locked page_end_to_end -- --ignored
```

Before a pull request is marked ready, run mutation testing over its changes and kill or
explain each surviving mutant:

```sh
git diff origin/main...HEAD -- src > /tmp/pr.diff
cargo mutants --in-diff /tmp/pr.diff
```

CI runs formatting, clippy and the tests on macOS for every pull request that is ready for
review, and `cargo deny` (advisories, licenses, sources) when the dependencies change and every
night. Every day it also runs the tests on the nightly toolchain, and after `cargo update` with
deprecations denied (`.github/workflows/scheduled.yml`).

[`docs/design.md`](docs/design.md) is the design: the loop, the data structures, the ACP
integration, runs, reviews, follow-ups, the changes view, and the decisions with their reasons.
Read it before a change of any size.

## Project layout

| Path                   | What it is                                                         |
| ---------------------- | ------------------------------------------------------------------ |
| `src/main.rs`          | The `postil` command                                               |
| `src/shell.rs`         | The window, the webview, and the thread that answers the page      |
| `src/site.rs`, `src/site/` | The HTTP routes the page calls, and their tests                |
| `src/acp.rs`, `src/acp/` | The ACP client: starting an agent, prompts, permissions, questions |
| `src/run.rs`, `src/run/` | Runs: sending notes, the run log, follow-ups, snapshots          |
| `src/review.rs`        | Review chains: prompts and findings                                |
| `src/notes.rs`         | Notes and `review.json`                                            |
| `src/repo.rs`          | git: the file list, snapshots, diffs                               |
| `src/agents.rs`        | Detecting the agents on the `PATH`                                 |
| `src/settings.rs`      | Settings and `settings.json`                                       |
| `ui/`                  | The page: plain HTML, CSS and JavaScript modules, no build step    |
| `tests/`               | The fake ACP agent and the command-line and page tests             |
| `scripts/bundle.sh`    | Builds `Postil.app`                                                |
| `scripts/install.sh`   | Builds `Postil.app` and installs it in `/Applications`             |
| `scripts/sign.sh`      | Signs `Postil.app` with a Developer ID, and notarizes it           |

## License

MIT. See [LICENSE](LICENSE).
