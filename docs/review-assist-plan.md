# Postil: review assist, implementation plan

Status: 2026-11-25. Nothing here is built. This plan turns `docs/review-assist.md` (the
design) into pull requests. It was written against the code at `53442a6`; every function and
line it names was read there. `b7286bc` (#50, Open pull request) landed after it: line numbers
in `src/notes.rs`, `src/run.rs`, `src/site.rs`, `src/thread.rs` and `ui/run-dialog.js` moved,
and `threads.json` went to version 3. Where the code contradicts the design, section "Corrections to
the design" says which one changes.

How to read a step: **Changes** names the files and functions, **Edge cases** lists the
inputs and states the step must handle, **Tests** lists each test with the bug it catches. A
step is one pull request unless it says otherwise.

## Corrections to the design

Found while reading the code. Each is applied to `docs/review-assist.md` with this plan.

1. **Read-only runs get a thread each, not one shared reading worktree.** The run machinery
   is keyed by thread everywhere: `Slots.by_thread`, `Pending.thread`, `on_event`'s slot
   lookup, the per-thread lock that counts live runs, `undo_proposer` (it resets to
   `thread.branch` and `thread.tip`), `thread::open` (it refuses a branch outside `postil/`),
   and `free_disk` (it counts `threads.json`). A shared detached worktree needs a second
   variant of each. A read-only thread reuses all of them: eviction and the caps then work
   with no new code. Follow-ups need one change: `launch` takes read-only from the thread's
   mode, since `Runner::follow_up` carries nothing of the kind (step 7.6). The cost
   accepted: one `git worktree add` per ask (no build folders are cloned for a read-only
   thread).
2. **Evidence needs data the log does not keep.** `ToolCall` in `src/acp/wire.rs` declares no
   `rawInput`, so an execute call's command line is dropped at parse, and no exit status is
   kept: only `status` completed or failed. Step 7.2 keeps the command and the exit code:
   spike S1 found that Grok reports a non-zero exit as `completed`.
3. **A read-only run that edits is not marked failed.** Today a proposing reviewer that edits
   ends `Ended` with a problem text, and its worktree is reset. Read-only runs keep that
   behavior. What changes per kind: an analysis stores nothing, an ask shows its answer under
   the problem notice.
4. **`g d` is dropped.** The code view has no caret, only whole-line selections. Jump to
   definition is `Cmd`-click, Go to Definition in the right-click menu, and the voice command.
5. **The forced update runs only when history moved forward.** Comparing any two `HEAD`s
   would start an update on every branch switch. It runs when `checked` is an ancestor of
   `HEAD` (a commit, a merge, a pull). After a switch, a rebase or a reset the diagram shows
   "the checkout moved to another history" with Update, and `checked` moves when it is
   pressed.
6. **Memory calls never run on the request thread.** `page-requests` is one thread
   (`src/shell.rs:345`), so a slow `recall` would freeze the page. Memory uses the
   `Scanner` pattern (`src/agents.rs:88`): a worker and a pending reply. `Scanner` sends the
   page no nudge: the settings dialog asks again every 250 ms (`ui/settings-dialog.js:23`),
   and the only nudge is `RunChanged`, which syncs runs alone. How the page learns a result
   is in is open ("What this plan leaves open", 7).
7. **Findings are drawn beside changed lines.** The changes view shows only notes today. The
   memory flag and a disputed risk rating both need a finding beside its line (step 7.5).

## Spikes before any code

Each answers a question the design left unchecked. A spike is a throwaway script or a manual
run; its result is written into `docs/review-assist.md`, "Open questions", and it gates the
step named.

| Spike | Question | Gates | If the answer is bad |
|---|---|---|---|
| S1 | For Claude and Grok: what does an execute tool call carry (`title`, `rawInput`, `rawOutput`), and is a non-zero exit `status: failed`? | 7.2, 7.4 | Evidence shows "ran" instead of "passed", from `status` alone. Done: Grok's is `completed`; the exit code is kept instead (design, "Open questions", 5). |
| S2 | Do Claude and Grok end a turn with the report block when the prompt asks in words? 10 runs each. | 7.3 | Under 8 of 10: the report is asked for in a second short turn of the same session. Done: 10 of 10 each (design, "Open questions", 11). |
| S3 | `hippius-mem`: the tool names and argument schemas (`tools/list`), `doctor`'s exit codes and output, whether `recall` touches the network, whether any call can prompt on a terminal, and which repository a server started in `~/.postil/worktrees/...` binds. | 9.1 to 9.5 | Wrong repository: Postil passes the repository explicitly where the tool takes one (`recall` has `repo`). |
| S4 | `@dagrejs/dagre` at a release at least two weeks old: is there one browser file with a global, its size, and the layout time for 200 nodes in WKWebView? | 8.3 | Over 50 ms or no browser build: a layered layout of our own, limited to 60 nodes. |
| S5 | `cargo deny check` with `tree-sitter-tags`, `objc2-speech`, `objc2-avf-audio` added. | 7.7, 10.1 | A license outside `deny.toml`'s list: ask the owner before adding an exception. |
| S6 | Speech: does `supportsOnDeviceRecognition` hold for the development Mac's languages, and what does a denied permission return? | 10.1 | No on-device model for a language: dictation reports it, as designed. |
| S7 | ACP schema at the commit `src/acp/wire.rs` pins: the stdio form of `mcpServers`, and whether an agent may refuse it. | 9.3 | Refusal is possible: `session/new` is retried once with none, and the run says so. |

## Rules for every step

- **Formats.** A new field is a new version, written in the same pull request as the code
  that reads it. Every version bump updates the byte-for-byte test of that file and the test
  that a newer version is refused.

  | File | Now | Steps that bump it |
  |---|---|---|
  | Run log (`LOG_VERSION`, `src/run/log.rs:80`) | 4 | 7.2 (5), 7.3 (6), 7.6 (7), 8.2 (8), 9.4 (9), 9.5 (10) |
  | `review.json` (`VERSION`, `src/notes.rs:70`) | 4 | 7.6 (5), 9.4 (6) |
  | `threads.json` (`THREADS_VERSION`, `src/thread.rs:60`) | 3 | 7.6 (4) |
  | `settings.json` (`VERSION`, `src/settings.rs:42`) | 2 | 8.2 (3), 9.1 (4) |
  | `layout.json` | 1 | 8.3 (2) |
  | `analysis.json` | none | 8.1 (1) |

  `settings.json` is at version 2 since #60 made the agent and reviewers `{name, model}` slots:
  `settings::parse` (`src/settings.rs:461`) reads version 1 through `upgrade_v1` and refuses
  every other older version. Step 8.2 adds version 2 to what it reads.
  `files::load_versioned` drops older versions too (`src/files.rs:44`), and `layout::load`
  then falls back to the default widths: step 8.3 reads a version 1 `layout.json` itself,
  with `map` defaulted, and tests `a_version_1_layout_keeps_its_widths`.
- **A new run log entry kind** touches a fixed list: the `Entry` variant, `RunState::apply`
  (`log.rs:767`), `RunView` when the page needs it, and on the page the redraw keys
  `threadKey` and `noteDialogKey` (`ui/status.js:330`, `:375`) and the card signature
  (`ui/run-tab.js:92`). `foldRun` drops unknown kinds silently, so a missed place shows as a
  dialog that does not redraw, not as an error. Each such step has a test for the redraw key.
- **A new `ui/*.js` module** needs a row in `PAGE` (`src/site.rs:33`); two tests in
  `src/site/tests/files.rs` fail without it. **A new route** needs a row in `ROUTES`
  (`src/site/tests/routes.rs:6`). **A new `Error` variant** needs an arm in `json_or_error`
  (`src/site.rs:1133`).
- **Colors.** A new color is a variable declared in both palettes (`ui/app-css.test.js`).
- **Tests.** End to end first: through `Site::respond` with the fake agent and real `git`,
  and through Playwright for clicks. The WebKit tests hang inside the Claude Code sandbox:
  run `cargo test page_end_to_end -- --ignored` outside it. The e2e bridge hard-wires the
  `flood` scenario and three files (`src/site/tests/page_e2e.rs:143`); step 7.3 makes
  `POST /new` take a scenario and files, and later steps use that.
- **Before `gh pr ready`:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, `node --test ui/`, the e2e run, `cargo mutants --in-diff` once, the
  `rust-style-reviewer`, and the review of the step's tier. Steps that change a persisted
  format, concurrency or `unsafe` are tier 2; the rest are tier 1.
- **Docs.** Each step moves its part of `docs/review-assist.md` from "designed" to "built" and
  amends `docs/design.md` where the design says it changes a rule.

## Milestone 7: reviewer aids

### 7.1 Shared pieces, no behavior change

**Changes**

- New `src/anchor.rs`: `LineRange`, `locate`, `locate_lines` move from `src/notes.rs:571`,
  `:699`, `:706` as `pub(crate)`. `notes.rs` uses them. The property tests move with them.
- `src/review.rs`: `findings_in` (`:171`) scans for the last ```` ```json ```` fence that
  opens a line and parses an array. Factor the scan into
  `last_json_block<T: DeserializeOwned>(reply: &str) -> Result<T, String>`; `findings_in`
  calls it.
- `src/files.rs`: `lock(file: &Path) -> io::Result<File>`, the retry loop from
  `notes::lock` (`notes.rs:739`), with the caller mapping the error. `notes::lock` calls it.
- `ui/run-dialog.js`: export `fillMarkdown` and `inlineView` (`:783`, `:795`) from
  `ui/dialog-parts.js`, for the node, memory and answer dialogs.

**Tests.** No new behavior: the existing suites pass unchanged. One new unit test for
`last_json_block` with an object, which catches a refactor that still only accepts arrays.

### 7.2 Keep an execute call's command (run log 5)

**Changes**

- `src/acp/wire.rs`: `ToolCall` and `ToolCallUpdate` (`:259`, `:276`) gain
  `raw_input` and `raw_output: Option<serde_json::Value>`.
- `src/run/log.rs`: `Entry::Tool` and `Entry::ToolUpdate` gain `command: Option<String>`
  and `exit: Option<i64>`, filled in `entry_of` (`:1080`) only for execute calls, by the
  rules S1 settled: `rawInput.command` when it is a string (no agent sent an array); the exit
  code from Grok's `rawOutput.exit_code` or Claude's leading `Exit code N` line, only from a
  report that the call finished, and Claude's text only from a failed one. An update need not
  name its call's kind (Claude names it only on the call, Grok only on an update), so
  `RunState` keeps each call's last known kind. `LOG_VERSION` 5. The exit code was added
  after S1, by the owner's choice on 2026-11-25.
- `ui/run.js`: `foldMore` merges `command` and `exit` into the tool item.
- `tests/fake-agent.mjs`: scenario `checks`, each agent's shapes as S1 saw them: Claude's
  (kind on the call, command in updates, a failure as `failed`), Grok's (kind on an update,
  `exit_code` 0 while running, a failure as `completed`), a call reported whole, a read whose
  input names a command, an edit after the first.

**Edge cases**

- `rawInput` missing, null, a string, an object with no `command`, or a blank one: `command`
  is `None`, and a blank one does not replace the call's.
- A command over 4 KiB is cut at a character boundary, its length noted: a heredoc can be
  megabytes, and the log is read whole.
- An update that carries `rawInput` after the call did: the later one wins.
- Grok's `exit_code` 0 on an update while the command runs is not kept; Claude's
  `Exit code N` text on a completed call is output, not an exit code.
- Version 4 logs read with neither field.

**Tests** (as built in #54)

- `an_execute_call_keeps_its_command_and_exit_code_and_no_other_call_does` (site): catches
  either field dropped at parse, an update read by its own kind only (Claude's command lost),
  a running call's exit 0 kept, Claude's text read from a completed call, a read's input kept,
  and a byte slice that panics inside a character.
- `a_version_4_log_reads_without_commands`, and the refused-version test moved to 6.
- `foldRun` test: an update's command and exit replace the call's, an exit of 0 included.

### 7.3 The author's report (run log 6)

**Changes**

- `src/notes.rs`, `prompt` (`:283`): one paragraph, in words, asks for a last block fenced as
  json with `steps` and `checks`. `src/run.rs`, the kept-agent follow-up text (`:591`) and
  `fresh_follow_up` (`:2217`) gain a one-line reminder, since a follow-up's prompt is the
  engineer's words alone.
- New `src/report.rs`: `Report`, `Step`, `Risk`, `Check`, and
  `read(reply: &str, root: &Path) -> Result<Report, String>` over `last_json_block`.
- `src/run.rs`, `on_event` (`:1954`): for a run that ended well whose header has no
  `review` (and, from 7.6, is not read-only), read the report and
  `run.record(Entry::Report { .. })` after the `Slots` lock is dropped, beside
  `record_end_snapshot` (`:2033`) and before `record_event` logs `Ended`, which drops the log
  handle. Not inside the `slots()` block where `decide` runs: reading the report may read
  files, and that lock is the whole runner's. The reply is `run.state().reply()`, as
  `end_stage` reads it.
- `src/run/log.rs`: `Entry::Report { report: Option<Report>, problem: Option<String> }`,
  `LOG_VERSION` 6. No `RunView` field: the page reads the last entry of kind `report`, as it
  reads `findings` (`ui/status.js:270`).
- `src/review.rs`, `context` (`:137`): the reviewers' prompt includes the report, and
  `propose_prompt` asks for a finding on a hunk rated too low.
- `src/site/tests/page_e2e.rs`: `POST /new` takes `scenario` and `files`. Not in 7.3 as
  built (#55's description): no 7.3 test runs the bridge. 7.4 added `scenario`, for its e2e
  test; `files` comes with the first step whose e2e test needs files of its own (7.5 or 7.7).
- `tests/fake-agent.mjs`: `report`, `report-bad`, `report-none`, `report-twice`.

**Edge cases**

- No block, or one that does not parse: `report: None` with the problem; the run is not
  failed and the page says the report is missing.
- A run that was stopped, failed, cut at `max_tokens` or refused: no report is read.
- Text after the block, or two json blocks: the last one that parses as an object wins. A
  reply that quotes the prompt's instructions cannot match, because the prompt holds no
  example block.
- A step whose `path` is absolute or inside the worktree: made relative as `Place` paths are
  (`log.rs:1125`). A path that leaves the repository is dropped and counted: after
  `relative`, one that is still absolute or has a `..` component (`relative` returns a path
  outside the root unchanged). The unit table holds `../x`, `src/../../x` and `/etc/x`.
- A deleted file has no new side: `lines` stays required and is ignored when the path no
  longer exists, so the step covers the whole file and the S2 wording stays (owner,
  2026-11-25). `lines` reversed or zero: the step is dropped and counted. `report::read`
  reads no file: lines past the end are not checked there, and the walkthrough (7.5) lists
  a step that names no changed lines (owner, 2026-11-25). Whether a path exists is one
  `lstat` per step.
- An unknown `risk`: `eyes`. The safe reading of a rating Postil does not know.
- Limits: 200 steps, 50 checks, 300 characters of `why`: more is cut and counted. The numbers
  are arbitrary and guard the dialog, not the agent.
- The reply still holds the block: the run dialog's `replyPart` (`ui/run-dialog.js:391`)
  draws the reply without it, through a pure `withoutReport(text)` that finds the block by
  `last_json_block`'s rule (a fence that starts a line, the last that parses), and only when
  the run has a report entry: a reply with a json block and no report keeps the block.
- `agentSaid` (`ui/status.js:85`) reads `N3: ...` lines: a report after them must not change
  a note's Addressed status.
- The run dialog tells a kept agent's follow-up from a fresh agent's by
  `turn.prompt !== turn.followUp.text` (`ui/run-dialog.js:453`, and `threadKey`). A reminder
  appended to a kept agent's prompt makes every follow-up read as a fresh agent's, so that
  check must change with the reminder.

**Tests**

- `a_run_ends_with_its_report` (site, `report`): catches the hook missing for ordinary runs.
- `a_review_stage_logs_findings_and_no_report`: catches the new branch also firing for stages.
- `a_reply_with_no_report_ends_the_run_well`, `a_bad_report_is_named_and_the_run_ends_well`.
- `the_last_report_block_wins` (`report-twice`).
- `a_stopped_run_reads_no_report`: catches a half-written block read from a cancelled turn.
- Unit, `report::read`: a table of the path and line cases above; a property test that no
  input panics and that every kept step's range is ordered.
- `a_follow_up_asks_for_a_report_again`: catches the reminder missing from the kept-agent path.
- Page: `withoutReport` (block last, block absent, two blocks, text after the block: spike S2
  saw it in 3 of 10 Grok runs, a later block that is not an object), `threadKey` changes
  when a report arrives, `agentSaid` with a report after the note lines.

### 7.4 Evidence on the run

**Changes**

- `ui/run.js`: `evidence(report, items) -> [{command, claim, state, item}]`, pure.
  States: `passed`, `failed`, `stale`, `unfinished`, `not-seen`. A non-zero `exit` is
  `failed` whatever the item's status (Grok ends a failure as `completed`). A claim matches
  the last execute item whose command equals it after trimming and collapsing spaces; with no
  command kept, whose title equals it or equals it wrapped in backticks.
- `ui/run.js`, `foldMore`: a tool item gains `endedAt`, the index of the entry that gave it
  a final status. Items keep the order calls started, so without it an edit started before a
  check and finished after it could not mark the check stale.
- `ui/run-dialog.js`, `runDetail` (`:212`): a Checks section after Steps, each row opening
  its step (`openDetail({kind: "step", ...})`), then the run's other commands.
- `ui/run.js`, `toolRow`: a step with a non-zero `exit` shows as failed. It reads `status`
  only today, so Grok's failed command would show done in Steps and failed in Checks.
- `ui/run-tab.js`: one icon on the card for the worst state, added to the card's signature
  (`:92`) or the card will not redraw.

**Edge cases**

- The same command run three times: the last run is the evidence.
- A command still pending or in progress when the turn ended: `unfinished`.
- An edit, delete or move call whose `endedAt` is after the check's: `stale`. A formatter that
  rewrites files is an execute call Postil cannot see into: accepted, and the section's hint
  says "an edit after it" so the claim is exact.
- The claim says passed and the log says failed: the log's state shows, with "the author
  says passed" beside it.
- A check that failed and was edited after shows failed. The card's worst state, in order:
  failed, not seen, stale, unfinished, passed (owner, 2026-11-25). The card ranks the checks
  the latest turn's report claims: the other commands an agent tried do not mark it.
- Each turn has its own Checks under its own Steps, the run's and each follow-up's (owner,
  2026-11-25).
- No report: the section lists the commands the run ran, with no claims.
- A run with no command: "No command was run".

**Tests** (`ui/run.test.js`): one per state; the last of repeated commands wins; a claim that
differs only in spacing matches; a claim that is a prefix of a command does not (catches
`cargo test` matching `cargo test --no-run`); stale needs an edit after, not before; an edit
started before the check and finished after it is stale. One e2e: the Checks row opens its
step.

### 7.5 Walkthrough, risk, and findings in the changes view

**Changes**

- `ui/patch.js`: a hunk keeps `oldStart`, `oldCount`, `newStart`, `newCount`, parsed at `:41`
  and dropped today. New pure `walkthrough(files, steps) -> {groups, rest}`: each step with
  the hunks its lines overlap, and the hunks no step covers.
- `ui/changes-view.js`: `fileView` (`:77`) draws hunks and lines as flat siblings. Split out
  `hunkView(file, hunk)` with a wrapper element, so a hunk can be drawn alone, folded, and
  labeled. `draw` (`:54`) takes an order: Walkthrough or Files. `show` takes the run's report
  and findings from its caller (`ui/app.js:123`).
- Findings beside lines: `findingsByLine(file, findings)` in `patch.js`, drawn as cards under
  the line as notes are (`:100`).
- Keys `n` and `p` in the view's keydown (`:152`), which handles only Escape today. The
  header controls sit in the pane's `.pane-header`, outside `#changes-view`
  (`ui/index.html:39`), so each gives focus back to the view after acting.
- A second header control, All or Eyes first (the design, "Guided walkthrough and risk"):
  it reorders steps and never hides one.

**Edge cases**

- A hunk two steps overlap: it belongs to the first, and the second says "shown in step N".
- A step no hunk overlaps (the agent named unchanged lines): listed at the end as "names no
  changed lines", so a wrong report is visible.
- A hunk with no new-side lines (a deleted file, or a modified file emptied, `+0,0`): a step
  matches it by path alone.
- A binary file has no hunks: one row per file in whichever group names its path.
- The 400-line limit (`LINES_SHOWN`, `:14`) is per file today. In walkthrough order it is
  per hunk group, with the same "Show the other N lines".
- `mechanical` hunks start folded. A note or a finding on a folded hunk opens it: nothing a
  reviewer wrote is hidden.
- A finding on a `routine` or `mechanical` hunk shows the hunk as `eyes`, whatever the report
  said: `Finding` gets no field for a dispute (owner, 2026-11-25).
- A finding with a line outside every hunk, or no line: listed under the file's header; in
  walkthrough order, in the group of the first step that names its file, else under "Not in
  the walkthrough". A finding with no path: at the top of the view.
- A thread of several runs keeps Files order (design). The control is disabled there with a
  hint, not hidden.
- `n` at the last step and `p` at the first do nothing, and neither fires with a modifier
  held. (No composer opens inside the view: a click on a line number closes it first,
  `ui/changes-view.js:145`.)
- Reduce Motion: `n` and `p` jump without the scroll spring.

**Tests**

- `ui/patch.test.js`: hunk ranges for added, deleted, emptied (`+0,0`) and one-line files
  (`@@ -0,0 +1 @@`);
  `walkthrough` for each case above; a property test that every hunk is in exactly one group
  or in `rest` (catches a hunk lost or drawn twice).
- e2e (`report` scenario): `n` moves to step 2, and still does after a click on Walkthrough;
  a folded hunk opens on click; a click on a
  line number inside a walkthrough hunk opens the composer (the existing behavior, which the
  restructure can break).

### 7.6 Read-only threads and Ask (run log 7, `review.json` 5, `threads.json` 4)

**Changes**

- `src/thread.rs`: `Thread` gains `mode: Mode { Work, Read }` (version 4; versions 1 to 3 read
  as `Work`). `thread::make` (`:171`) skips the cloned build folders for `Read`. Apply and Open
  pull request are refused for `Read` (400).
- `src/run/log.rs`: the header gains `readOnly: bool` (version 7).
- `src/run.rs`: `read_only` (`:1978`) becomes `header.read_only || non-judging stage`.
  `undo_proposer` (`:2158`) keys on the same value, and its notice says "the agent changed
  files" for a run that is not a review stage. New `Runner::ask(agent, id)`: a run of one
  note in a new `Read` thread. `launch` sets the header's `readOnly` from the thread's mode,
  not from `Runner::ask`: `Runner::follow_up` carries no such value, and a follow-up to an
  ask would otherwise get edit rights and commit to the thread's branch. `launch` takes no
  snapshot when `readOnly`, or the run dialog would offer Changes for an ask
  (`ui/run-dialog.js:309`).
- The undo notice of a run that is not a review stage is kept only in `RunState.problem`
  (`run.rs:2019`), which is never logged, so a restart loses it. How it is logged is open
  ("What this plan leaves open", 10).
- `src/notes.rs`: `Note.intent: Intent { Change, Ask }` (version 5; older notes are
  `Change`). `StoredNote`, the hand-written `Serialize` (`:828`), `NewNote`. A second prompt
  for an ask: answer the question about the quoted lines, change nothing. `notes::prompt`
  refuses a note with intent `Ask` (400), as it refuses a resolved one: both
  `POST /api/run` and `GET /api/run/preview` build their prompt there (`run.rs:446`, `:475`).
  `Runner::ask` builds its own prompt and does not call it.
- `src/site.rs`: `POST /api/notes` takes `intent`; `POST /api/run/ask {note}`.
- Page: the composer's ask mode (`q`, and Ask in `codeActions`, `ui/code-view.js:490`);
  `runStatus` (`ui/status.js:44`) gives `answered` to an ask whose run ended well; the note
  dialog shows the reply as the answer, with Make it a note. One predicate, `isAsk(note)`,
  keeps asks out of everything that sends: `sendable` (`ui/run.js:8`), the send step's
  checkboxes and send buttons and its open counts (`ui/notes-tray.js:179`, `:199`, `:246`,
  `:276`), and the note dialog's Send (`ui/note-dialog.js:360`). A `Read` thread's run dialog
  has no Apply, Open pull request or Review (`ui/run-dialog.js:348`, `:411`, `:566`).
- The ask composer shows the uncommitted-changes warning that #52 built for the send step
  (`Repo::uncommitted`, `PreviewView.uncommitted`), read the same way.

**Edge cases**

- The cap of 3 live runs is reached: 409, and the composer keeps the question. Whether the
  note was saved before the refusal is open ("What this plan leaves open", 11).
- A repository with no commit: 422 with the message a send gets.
- Uncommitted edits in the checkout: the composer says the agent reads the last commit, and
  when the asked file is among them, that the lines may differ.
- The agent offers no reject option for an edit (`auto_answer` returns `None`,
  `run.rs:2078`): the request waits for the engineer, as today.
- The agent edits without asking: the worktree is reset, and the answer shows under the
  problem notice.
- An ask on a file or a folder: the same path, no snippet, as path notes.
- Resolve or Delete on an ask ends its thread and frees its worktree: `change_note` and
  `delete_note` call `Runner::end_thread(thread, Discard)` for an ask's note; today neither
  reaches the runner (`site.rs:759`, `:770`). A delete during a live run succeeds today and
  its log goes at the next delete (`a_live_run_is_kept_when_its_note_is_deleted_...`,
  `tests/e2e/page.test.js:112`); what a delete or resolve of a live ask does is open ("What
  this plan leaves open", 9).
- An ask's thread is evicted when idle like any thread; a follow-up then gets a fresh agent
  with the history (`fresh_follow_up`).
- Review on an ask's run is refused (400): there is no work to review.
- The send step never ticks an ask, and a `POST /api/run` naming one is refused, so a stale
  page cannot send it as a change request.
- Another window sends the same ask: the note is marked sent once the agent takes the turn;
  the second is refused when the note is no longer open.
- An ask's status with no `N1:` line in the reply is `answered`, not No word.

**Tests**

- `an_ask_runs_read_only_in_a_thread_of_its_own` (site, a new scenario `asks-to-edit` that
  asks permission for an edit, then a read, and says each answer: `reviewer-edits` asks for
  nothing): catches an ask given edit rights.
- `a_follow_up_of_an_ask_is_read_only`: catches a follow-up that edits and commits.
- `an_ask_logs_no_report`, and a page test that a reply with a json block and no report
  keeps it.
- `an_ask_that_edits_is_undone_and_says_so` (`reviewer-edits`), and that the notice survives
  reopening the project.
- `a_send_refuses_an_ask`, `a_preview_refuses_an_ask`,
  `the_prompt_of_an_ask_asks_for_no_change`.
- `resolving_an_ask_frees_its_worktree`, `a_read_thread_clones_no_build_folders`,
  `a_read_thread_cannot_be_applied`.
- `a_version_4_review_reads_every_note_as_a_change`, the byte-for-byte tests of both files,
  and `a_version_3_record_reads_every_thread_as_work`. `thread::read` lists the versions it
  reads (`Ok(1 | 2 | THREADS_VERSION)`, `src/thread.rs:836`): version 3 joins that list, and
  `threads.json` has no byte-for-byte test yet, so this step adds one.
- `an_ask_at_the_cap_is_refused` (with `hang`).
- Page: `runStatus` for an ask (ended, stopped, failed), `isAsk` in the send step's counts,
  `groupNotes`.
- e2e: `q` on a selection opens the composer in ask mode and `Cmd-Enter` shows Answered.

### 7.7 Jump to definition

**Changes**

- `Cargo.toml`: `tree-sitter-tags` 0.27.2, justified in `docs/design.md`, "Dependencies",
  with the measured crate count and binary size.
- `src/highlight.rs`: `Grammar` (`:46`) gains `tags: Option<fn() -> TagsConfiguration>`,
  set for Rust, JavaScript, TypeScript, TSX, Python and Go. `MAX_HIGHLIGHTED` becomes
  `pub(crate)`.
- New `src/definitions.rs`: `Index { by_name: HashMap<String, Vec<Definition>>, seen:
  HashMap<String, Stamp> }`, built on a thread named `index-definitions` started by
  `Site::open` (`site.rs:527`), owned by `Project`, and shared the way `Scanner` shares its
  result.
- `GET /api/definitions?name=x&from=path`: the ranked list, or `{state: "building"}`. The
  request only reads the index. When the window regains focus or a run ends,
  `index-definitions` re-stats the files and reparses those whose size or mtime changed, as
  the design says; a lookup meanwhile answers from the index as it was. Reparsing inside the
  request would hold `page-requests`, the one thread that answers the page
  (`src/shell.rs:344`), for a whole pull's worth of files.
- Page: a `mousedown` with `metaKey` on `.c` in `ui/code-view.js`; Go to Definition in
  `codeActions`, whose contextmenu handler finds the word at the event's point and passes it
  (`codeActions(line, word)`; it gets the line alone today, `:486`), shown only when there
  is a word; `Cmd-[` and `Cmd-]` over a history of at most 50 places.

**Edge cases**

- The `Cmd`-mousedown must not start a text drag (`code-view.js:318`) or clear the selection
  on mouseup (`:341`): it calls `preventDefault` and is skipped by both, as `.add-note` is.
- The word under the pointer: `caretRangeFromPoint`, widened over identifier characters
  (Unicode `ID_Continue`, and `$` for JavaScript). A click on whitespace, punctuation, a
  string (`.s`) or a keyword (`.k`) does nothing; a click in a comment (`.m`) is open
  ("What this plan leaves open", 8).
- One definition and it is the clicked line: a toast "this is the definition".
- Several: ranked same file, same folder, then by path; at most 50 listed, with the count.
- The index is still building: the page asks again until it is built, by the mechanism
  open item 7 settles (no nudge exists for it today).
- Files over 2 MB (`MAX_HIGHLIGHTED`), files that are not UTF-8, and languages without a
  tags query are skipped; a click in a `.json` file says "no definitions for this file type".
- A file deleted or renamed since it was indexed: its entries go at the next refresh.
- A repository of 50,000 files: the first build and a refresh are measured in the pull
  request.
- A history entry whose file is gone is skipped. Opening another project clears the history.
- The changes view and the prompt preview do not jump in version 1.

**Tests**

- Unit: definitions found per language from a fixture file each (catches a tags query not
  wired for a grammar); two functions of one name both listed; the ranking.
- Site: `a_definition_follows_an_edit` (write, ask, edit the file, ask again), `a_deleted_
  file_leaves_no_definitions`, `a_name_with_no_definition_is_an_empty_list`.
- e2e: `Cmd`-click jumps and leaves the selection as it was; `Cmd-[` goes back.

## Milestone 8: the map

### 8.1 The analysis file and citation checking (`analysis.json` 1)

Rust only, no agent and no page.

**Changes**

- New `src/analysis.rs`: the types of the design (`Analysis`, `Diagram`, `Node`, `NodeKind`,
  `Edge`, `CodeRef`, `RefState`), `load`, `save` under `analysis.lock` (`files::lock`),
  `Repo::analysis_file()`.
- `check(cited: Cited, text: &str) -> CodeRef`: the citation rule (the stated line equals
  `first`; else the only equal line; else `Unverified`), then the lines become the snippet.
- `place(analysis, repo)`: every `Found` anchor through `anchor::locate`; called on each read.
- `GET /api/analysis`: the placed analysis, or `null`.

**Edge cases**

- The file is missing: `null`. It does not parse, has unknown fields, or a newer version: the
  problem is reported with the path and the file is never written over; the diagram pane
  offers Start over, which moves it to the Trash.
- A cited path that is absolute, has `..`, or names nothing: `Unverified`. The path rule is
  the notes' (`docs/design.md`, "Data structures").
- A cited folder, or a file with no `lines`: a path target, found while the path exists.
- `lines` past the end of the file, reversed or zero: `Unverified`.
- `first` empty or only spaces: it holds only at the stated line, never by search, since a
  blank line is never unique.
- `first` equal to several lines and none at the stated line: `Unverified`. The nearest is
  not taken, for the reason notes give.
- Carriage returns: compared after `str::lines`, which drops them.
- A file over `MAX_READ` (20 MB) or not UTF-8: `Unverified`, with the reason kept for the
  node's dialog.
- Placing reads each cited file once per request. Measured in the pull request on an
  analysis of 200 anchors over 60 files; over 10 ms, results are kept per path and stat.

**Tests**

- Property tests for `check`: a citation that is right is `Found` at its lines; one shifted
  by inserted lines above is `Found` at the new lines; `check` never returns a range outside
  the file (catches an off-by-one at the last line).
- Unit table: each edge case above.
- Site: `an_analysis_reads_back_placed` (write a file by hand, edit a cited file, read:
  the anchor moved; delete the lines, read: `Outdated`); `an_unreadable_analysis_is_
  reported_and_kept`; the byte-for-byte test; `a_newer_analysis_is_refused`.

### 8.2 Analysis runs (run log 8, settings 3)

**Changes**

- `src/settings.rs`: `analyzer: Option<String>` with `check_choices` requiring it enabled and
  an accessor that falls back to `agent`; `VERSION` 3, and `parse` (`:461`) reads version 2.
  `ui/settings.js`: `usage` (`:21`) counts the analyzer, so it cannot be disabled while set.
- `src/run/log.rs`: header `analysis: {kind, of}` and `Origin::Analysis` (version 8).
- `src/analysis.rs`: the prompts (top level, deeper, flow, update), and
  `merge(reply) -> Result<Summary>`: read the block, check every citation against the run's
  worktree, number ids, write under the lock.
- `src/run.rs`: `Runner::analyze(request)`: a `Read` thread, ended as the run ends. The
  merge runs in `on_event` where the report is read, before the thread is freed.
- Routes: `POST /api/analysis {kind, of?, question?}`, `POST /api/analysis/update`,
  `DELETE /api/analysis?diagram=D3`.
- `tests/fake-agent.mjs`: `analysis` (one right citation, one shifted, one wrong, an edge to
  an unknown node), `analysis-empty`, `analysis-edits`.

**Edge cases**

- **Ids.** The agent names nodes with keys of its own; Postil assigns `D<n>.<m>`. Two nodes
  with one key: the second is dropped and counted.
- An edge to or from an unknown key, or from a node to itself: dropped and counted.
- A `group` that names an unknown node, or a chain of groups that loops: the group is
  ignored, the node kept.
- No nodes: the run's dialog says the analysis was empty and nothing is stored.
- Limits: 200 nodes per diagram, 60 characters of label, 160 of summary, 4,000 of detail, 2
  MiB of reply. More is cut and counted, never silently.
- The summary of what was dropped is stored on the diagram and shown in the pane's header.
- **What replaces what.** Analyze replaces the top-level diagrams of its kinds; Analyze
  deeper replaces the diagram with the same parent; a flow adds one. A replaced or deleted
  diagram takes the diagrams below its nodes with it.
- Two windows analyze at once: both runs go; the merge is under the lock and the later one
  replaces the earlier. A second Analyze in one window while one runs is refused (409).
- The run is stopped, fails, or edits files: nothing is stored and the old analysis stays.
- The analyzer is not installed, or disabled since: 409 naming the agent, before a thread is
  made.
- Labels and details are an agent's text: drawn with `textContent` and the Markdown subset,
  never as HTML.
- Update with nothing outdated and nothing off the map: refused (400), no run.
- Update's reply names a node id that is not outdated: ignored, so an update cannot rewrite
  the whole diagram.

**Tests**

- `analyze_stores_checked_diagrams` (site): the right citation `Found`, the shifted one
  `Found` at its real lines, the wrong one `Unverified`, the bad edge dropped and counted.
- `an_analysis_that_edits_stores_nothing`, `a_stopped_analysis_keeps_the_old_one`,
  `an_empty_analysis_stores_nothing`.
- `analyze_deeper_replaces_only_its_branch`, `deleting_a_diagram_takes_its_children`.
- `update_replaces_only_outdated_nodes` (catches an update that rewrites healthy nodes).
- `the_analyzer_falls_back_to_the_default_agent`, `a_version_2_settings_file_still_reads`
  (catches the version refusal in `parse`), the settings byte-for-byte test.
- Unit, `merge`: a property test that every stored edge joins two stored nodes.

### 8.3 The split pane and the diagram view (`layout.json` 2)

**Changes**

- `ui/index.html`: inside `main.code-pane`, a row holding `#map` and a code column that
  holds `#code`, `#prompt-preview` and `#changes-view`. The overlays are anchored to the
  pane today (`inset: 40px 0 0`, `ui/app.css:1693`, `:2846`): they move into the code
  column, so the diagram stays visible beside the changes view.
- `src/layout.rs`: `map` width, version 2, version 1 read with the default.
- `ui/panes.js`: `fit` (`:23`) and `widest` take whether the map is open and keep
  `MAP_MIN = 320` beside `CODE_MIN`. `ui/dividers.js`: a third divider; its two-way branches
  (`:87`) become a lookup.
- `ui/vendor/dagre.min.js` with its license, a `PAGE` row, loaded by a script element when
  the map first opens. `.oxlintrc.json` and oxfmt ignore `ui/vendor/`. A new test that every
  file under `ui/vendor/` is served, since the existing asset tests do not look there.
- New `ui/diagram.js` (pure: the graph given to the layout, node search) and
  `ui/diagram-view.js` (SVG, pan, zoom, selection, the node dialog), each with a `PAGE` row.
- `--map` and its wash in both palettes; `Cmd-Shift-M` in `src/menu.rs` (View > Map).

**Edge cases**

- A window too narrow for 320 and 420: the map takes the whole pane and `Cmd-Shift-M`
  switches. Decided in `fit`, so it is tested without a window.
- Focus mode (`Cmd-\`) hides the map with the side panes.
- The send step's prompt preview and the changes view open in the code column; the map
  stays.
- The vendored script fails to load: the pane says so and offers Retry; the code pane works.
- A diagram with one node, with no edges, and with 200 nodes: each draws; the 200-node
  layout time is the S4 number, asserted in the bench mode, not in a unit test.
- Layout is computed once per diagram and kept for the session; a font-size change does not
  relayout (node sizes are fixed in px).
- Zoom is clamped to 25% to 200%; `0` fits; a pan cannot lose the diagram off screen (one
  node always stays in view).
- Keyboard: nodes are buttons in document order; arrows move to the nearest node in that
  direction; Enter deepens; `/` focuses the search.
- An analysis that arrives while the pane is open redraws and keeps the selection when its
  node still exists.
- No analysis: the empty state names the analyzer and says it reads the last commit.

**Tests**

- `ui/panes.test.js`: `fit` with the map open at four widths; the narrow fallback.
- `ui/diagram.test.js`: groups become compound nodes; search ranks label before summary.
- Rust: layout version 1 reads; the vendor-is-served test.
- e2e (an analysis written into the bridge's repository): the map opens, a click on a node
  opens its file at its lines, the changes view opens beside the map, Escape closes changes
  and not the map.

### 8.4 The link shown in the code

**Changes**

- `ui/app.js`: `markByLine`, built per open file from the analysis as `noteByLine` is
  (`indexNotes`, `:243`).
- `ui/code-view.js`, `decorateRow` (`:226`): the stripe element and the chip, built from
  state on every row build, because rows are rebuilt and not recycled.
- `ui/diagram.js`: `innermost(anchors, line)`.
- View > Map Markers.

**Edge cases**

- The stripe is a sibling in `.ln`, not a child of `.n`: a mousedown on `.n` starts a line
  selection (`:285`). Its own mousedown is skipped by the text-drag branch (`:318`) the way
  `.add-note` is, or a click on it would clear the selection.
- The chip uses the remark's guard: compare before replace (`:234`). A gutter drag
  redecorates every row on every mouse move.
- A line with a note and a node: the amber wash and remark stay, the stripe and chip are
  added; the chip comes before the remark.
- Copy (`selectedCodeText`, `:408`) leaves out the chip, as it leaves out the remark.
- Nested nodes: the smallest range wins; equal ranges: the diagram deepest under the top.
- The chip is cut at 180 px with the full label in its title; in a code column under 520 px
  chips are hidden and stripes stay.
- The composer open under a striped line: the stripe continues below it.
- Lost and unverified anchors draw nothing; outdated ones draw dashed.
- Code to diagram: the node for the selected line, else the first visible line, computed in
  the scroll's animation frame; only while the map is open; it never switches diagram.

**Tests**

- `ui/diagram.test.js`: `innermost` for nested, equal and disjoint ranges.
- e2e: a chip click selects its node; a click on a stripe keeps the line selection; a note
  written on a striped line shows both; Cmd-C over striped lines copies code only.

### 8.5 Deeper, flows, data, outdated and Update

**Changes.** Analyze deeper on a node and in the file list's menu (`ui/file-list.js:195`
passes one item today; it takes a list); the Flows list with its text box; Trace from here in
`codeActions`; the Data kind; the outdated mark and the Update button; "not on the map".

**Edge cases**

- Analyze deeper on a folder no node covers: allowed; the diagram's parent is the node whose
  anchor is the nearest ancestor path, else none.
- A flow's question is empty or over 300 characters: refused in the page and by the route.
- A flow is asked for with no Architecture yet: allowed; flows do not depend on it.
- Deleting a flow takes two clicks, as deleting a note does.
- "Not on the map" counts files added since the diagram's run; in a repository of generated
  files this can be thousands: the list shows 20 and a count, and Update sends at most 200
  paths, saying so.
- Update while an update runs: refused (409).

**Tests.** Site: `a_flow_keeps_its_step_order`, `deeper_on_a_folder_finds_its_parent`,
`update_sends_only_outdated_nodes_and_new_files` (the fake agent echoes its prompt, as every
scenario does), `update_caps_the_new_files`. e2e: the menu item on a folder starts a run.

### 8.6 The change map

**Changes.** `ui/diagram.js`: `touched(analysis, files) -> {nodes, near, unmapped}`, from a
run's parsed diff. The run dialog's header gains Change map beside Changes.

**Edge cases**

- Anchors are placed in the engineer's files; the diff is of the thread's branch. A node is
  touched when a hunk's old side overlaps its anchor's lines at the thread's fork, which
  `GET /api/analysis?at=<commit>` places with `git show`. Without this, a checkout that has
  moved since the fork marks the wrong nodes.
- A file the run added: no node covers it, so it lists under "Not on the map".
- A file the run deleted: every node citing it is touched.
- No analysis: the button is absent. An analysis with no Architecture: disabled with a hint.
- A run with no snapshot has no Changes button today; it has no Change map either.

**Tests.** `ui/diagram.test.js`: `touched` for each case, and that `near` never includes a
touched node. Site: `an_analysis_is_placed_at_a_commit`. e2e: a click on a touched node
opens the changes view at its hunk.

### 8.7 The forced update

**Changes**

- `src/repo.rs`: `changed_paths(from, to) -> Result<Vec<(Change, String)>>`
  (`git diff --name-status --no-renames`); none exists today.
- `src/analysis.rs`: `checked: CommitId`, set by the first analysis to its thread's fork.
  `check_head(repo) -> Verdict { Same, Untouched, Update, Diverged }`.
- `POST /api/analysis/check`, called by the page's `refresh` (`ui/app.js:464`), which runs
  on window focus and when a run's status changes. There is no focus hook in Rust
  (`src/shell.rs` ignores `WindowEvent::Focused`), and the page already has this one.

**Edge cases**

- `checked` is not an ancestor of `HEAD` (a branch switch, a rebase, a reset):
  `Diverged`; no run; the pane offers Update, which moves `checked`.
- `checked` no longer exists (pruned after a rebase): the same as `Diverged`.
- The move touches nothing cited and adds or deletes nothing relevant: `checked` moves, no
  run.
- Two windows check at once: the compare and the write of `checked` are one step under
  `analysis.lock`, so one run starts.
- The cap of 3 is reached, or the analyzer cannot start: no run, `checked` stays, the next
  check tries again. The pane's header says why.
- The run fails or is stopped: `checked` moved as it started, so it is not started again for
  this commit; the nodes stay outdated.
- Several commits arrive during an update: the next check compares from the new `checked`.
- An unborn or detached `HEAD`: a detached `HEAD` is compared like any other; a repository
  with no commit has no analysis.
- Uncommitted edits never force an update.
- Postil was closed while commits landed: the check runs on the first refresh after open.
- The settings' analyzer changed between runs: the update uses the current one.

**Tests** (site, real `git`)

- `a_commit_that_changes_cited_lines_starts_an_update`.
- `a_commit_that_touches_nothing_cited_moves_checked_with_no_run`.
- `a_new_file_starts_an_update`, `a_deleted_cited_file_starts_an_update`.
- `a_branch_switch_starts_no_update` (catches the token burn on every switch).
- `a_failed_update_is_not_started_twice_for_one_commit`.
- `an_update_refused_at_the_cap_starts_on_the_next_check`.
- `two_checks_at_once_start_one_run` (two threads through `Site::respond`).
- Unit: `changed_paths` with added, deleted, modified and type-changed files.

## Milestone 9: team memory

### 9.1 Finding and setting up hippius-mem (settings 4)

**Changes.** `src/memory.rs`: `find` (`agents::find_program` on the `Scanner`'s path) and
`state() -> Missing | Unconfigured | Ready`, from `doctor` as S3 describes it, run on a
worker. `SettingsView` (`site.rs:308`) gains the state. Settings: `agent_memory: bool`
(version 4). Routes: `POST /api/memory/setup`, `POST /api/memory/check`. The settings dialog's
Team memory section, with the rescan poll pattern (`ui/settings-dialog.js:320`).

**Edge cases**

- `doctor` hangs: a 10 second limit, then "did not answer".
- `quickstart` refuses because a configuration exists: its message is shown as it is.
- Set up is one click on a button that says what it writes, and the paths come from the
  command's own output, not from Postil's guess.
- The binary is removed while Postil runs: the next call fails, the state turns to Missing,
  and runs start without memory.
- The setting is on and the state is not Ready: runs start without memory, and the run's
  dialog says so once.

**Tests.** With a fake `hippius-mem` on a temporary `PATH` (a shell script: the real one
cannot be made to hang or be missing): each state; the hang; the switch refused when not
ready. With the real binary when installed, against a trial vault in a temporary home:
`doctor` reads Ready. Skipped with a message when it is not installed.

### 9.2 The memory client, the Memory tab and the run's Memory section

**Changes.** `src/memory.rs`: an MCP client over stdio (`initialize`, `tools/list`,
`tools/call`), one process per window started on first use, on a thread named `memory`, with
a bounded queue of requests. Routes return `{state: "pending"}` and nudge the page when the
answer is in: `GET /api/memory/recall?path=&node=`, `GET /api/memory?id=`,
`POST /api/memory/refresh`. The tray's third tab: `showTab` (`ui/notes-tray.js:429`) is a
boolean today and becomes a lookup.

**Edge cases**

- `tools/list` lacks a tool Postil calls: the tab says this hippius-mem does not match, with
  both versions.
- The process dies: restarted once on the next call; a second death in a minute is reported
  and not retried until Rescan.
- A call takes over 10 seconds: reported; the queue moves on.
- The queue is full (the engineer opens files quickly): the oldest pending recall for a file
  no longer open is dropped.
- Every pointer is weak: "Nothing recorded for this file". None at all: the same.
- A memory id is checked against `^mem_[0-9A-Z]+$` before it is used in a request.
- A body is a teammate's text: Markdown subset, never HTML, and cut at 64 KiB in the dialog.
- The tab is closed: no recall runs. A recall is asked on file open while the tab shows.
- A file path with spaces or non-ASCII characters in the query.
- Closing the project stops the process.

**Tests.** Against the real binary with a trial vault in a temporary home (skipped with a
message when not installed): remember three notes through the CLI, recall by path, read one,
read its history. Against the fake script: a dead process is restarted once; a slow call is
reported; a missing tool is named. Page: the tab's cards, the weak group, the pending state.
e2e: the Memory tab opens and the send bar stays hidden there.

### 9.3 Agents get it

**Changes.** `NewSessionParams.mcp_servers` (`src/acp/wire.rs:151`) becomes a list;
`Launch` and `Starting` (`connection.rs:48`, `:123`) carry it; `Runner::start`
(`run.rs:1634`) fills it when the setting is on and the state is Ready. The prompt gains its
one line. `tests/fake-agent.mjs`: `echo-session` replies with the `session/new` params it got.

**Edge cases.** The absolute path has spaces. The agent answers `session/new` with an error
(S7): one retry with no servers, and the run's dialog says memory was not given. The setting
is off: the list is empty and the prompt has no line. A reviewer and a read-only run get the
server too: recall is a read.

**Tests.** `a_run_gives_the_agent_team_memory` (site, `echo-session`),
`the_setting_off_gives_none_and_no_prompt_line`, `a_refused_server_list_is_retried_without`.

### 9.4 Remember (run log 9, `review.json` 6)

**Changes.** `Runner::draft(of)`: a read-only run in the note's thread, marked
`remember: {of}` in its header, whose reply ends with `{summary, body}`.
`POST /api/memory/draft`, `POST /api/memory/remember {note?, summary, body, scope}`.
`Note.memory: Option<String>`. The dialog, with dictation attached in 10.2.

**Edge cases**

- The thread's run is under way: Remember is disabled with "wait for the run".
- The thread ended (applied or discarded) or the note was never sent: no draft run; the
  dialog opens with the note's text.
- The draft run fails or returns no block: the same fallback, with the reason.
- The duplicate recall fails: Save stays possible and the dialog says the check did not run.
- Save fails: the dialog stays open with the error and the text. Save is disabled while it
  runs, so two clicks write once.
- The note is deleted later: the memory stays; nothing in Postil forgets a memory.
- A summary or body that is empty, or a body over 16 KiB: refused before the call.
- The dialog shows exactly the text that is sent. Nothing else leaves the machine.

**Tests.** Site with the real binary (skipped when missing): draft, save, recall finds it,
the note carries the id. With the fake script: a failed save keeps the draft and writes no
id. `a_draft_run_cannot_edit`. `a_never_sent_note_drafts_from_its_text`. The `review.json`
byte-for-byte test.

### 9.5 Review checks against memory (run log 10)

**Changes.** `Runner::review` recalls for the run's prompt and changed paths on the start
thread, before the first stage. `review::context` adds the block. `Finding` gains
`memory: Option<String>`. The finding's card and the changes view show the memory.

**Edge cases**

- hippius-mem is missing, slow or failing: the review runs without the block and says so.
- The block is capped at 12 memories, 2 KiB each, 16 KiB in all; a cut body says it was cut.
- The block is labeled as reference data written by teammates. The judging reviewer may edit
  files, so the label is in its prompt too.
- A finding names a memory id that was not in the block: the field is dropped, the finding
  kept.
- The same memory recalled for several paths: listed once.

**Tests.** `a_review_carries_recalled_memories` (the fake agent echoes its prompt),
`a_review_runs_without_memory_when_recall_fails`, `a_finding_keeps_only_a_memory_it_was_
given`, `the_memory_block_is_capped`. Page: the finding card with a memory.

## Milestone 10: voice

### 10.1 Dictation, native

**Changes.** `Cargo.toml`: `objc2-speech`, `objc2-avf-audio`, `block2`, in the macOS table
with explicit features; the `unsafe_code` comment names `src/dictation.rs`. New
`src/dictation.rs` with `#[expect(unsafe_code, reason = ...)]`, a `// SAFETY:` comment per
block and one unsafe operation per block (the lints deny otherwise). `AppEvent` variants and
`window.postil.dictation`. `scripts/bundle.sh`: the two usage descriptions.
`assets/Postil.entitlements`: `com.apple.security.device.audio-input`.

Pure logic is kept out of the framework calls so it can be tested: `Rollover` (when to start
a fresh task, how to join two tasks' text) and `level(samples) -> f32`.

**Edge cases**

- Microphone or speech permission denied, or restricted by a profile: a state the page shows
  with where to change it; no second prompt.
- No on-device model for the language: an error; the network is never used.
- The input device changes or is unplugged while recording: the tap is removed and
  installed again on the engine's configuration-change notification, or recording stops with
  a message.
- A rollover seam: the words spoken across it appear once.
- Stop while a final result is pending: the final text is delivered, then the state changes.
- The window loses focus, the project closes or the app quits while recording: recording
  stops and the engine is released.
- A second start while recording is ignored.
- Run from `cargo run` with no `Info.plist`: macOS would end the process, so dictation is
  refused with a message unless the process is inside a bundle.

**Tests.** Unit: `Rollover` (a pause, the 60 second limit, a seam), `level` for silence and
full scale. `tests/cli_errors.rs`: a bare binary refuses dictation and does not exit.
By hand, from `Postil.app`, with the list in the pull request: each permission state, a
dictation of two minutes, unplugging the microphone. The pull request says no end-to-end
test can run: CI has no microphone and cannot answer the permission prompt.

### 10.2 The dictation widget

**Changes.** New `ui/dictation.js` (a `PAGE` row), attached to the composer, the note,
path-note and run dialogs, the question form, the memory draft and the flow box.
`insertion(text, range, partial) -> {text, range}` is pure.

**Edge cases.** The engineer types or moves the caret while a partial result is shown: the
partial is committed where it is and dictation continues at the new caret. The text box is
removed while recording (a dialog closes): recording stops and the partial is dropped. An
input method composition is under way: dictation waits for it to end. Reduce Motion: the
level meter is a static mark. Two text boxes: only the focused one records.

**Tests.** `ui/dictation.test.js`: `insertion` with text before and after the caret, a
shrinking partial, a final result, a caret moved mid-partial. e2e with a stubbed
`window.postil.dictation` event source: partial text replaces itself and the surrounding
text survives.

### 10.3 Voice commands

**Changes.** New `ui/voice.js` (a `PAGE` row): `command(transcript, context) -> Command |
null`. The key handling in `ui/app.js`.

**Edge cases**

- The whole utterance must match the grammar: "next" matches, "what is next" does not.
- Case, punctuation and number words are normalized before matching.
- A command with nothing to act on ("resolve" with no note dialog) shows "nothing to
  resolve" and does nothing.
- "send" opens the send step; only a second "send" there sends. Apply, Discard, deleting and
  saving a memory have no command.
- "stop" stops the run the header shows, as Escape does, never another.
- The key: recording starts after it is held 200 ms with no other key, so typing an accented
  character with Option is not a command. Key repeat is ignored. Releasing the key, or the
  window losing focus, ends it.
- "open" and "go to" with several matches open the list, as a click would.
- The key in a text box dictates there instead (10.2).

**Tests.** `ui/voice.test.js`: one row per command, the normalization cases, three
utterances that must not match, and each command with an empty context. e2e with the stubbed
event source: "next" moves the walkthrough; "send" twice sends.

## Order and what each step unblocks

```text
7.1 -> 7.2 -> 7.3 -> 7.4
              7.3 -> 7.5
7.1 -> 7.6 (read-only threads)  -> 8.2 -> 8.5, 8.7
7.7 (independent; its e2e test needs `POST /new` to take `files`, below)
8.1 -> 8.2 -> 8.3 -> 8.4 -> 8.6
9.1 -> 9.2 -> 9.4, 9.5        9.3 needs only 9.1
10.1 -> 10.2 -> 10.3          10.3 uses 7.5 and 7.7 for two commands
```

7.6 comes before the map because analysis runs are read-only threads. 9.1 and 10.1 have no
dependency and can run beside the main line, each in a worktree of its own, and so can 7.7:
its e2e test needs the bridge to write files with definitions
(`POST /new` takes a scenario since 7.4, and `files` since 7.5).

## What this plan leaves open

The owner has not confirmed these; each is the design's assumption carried into a step.

1. An ask is a note with an intent (7.6).
2. The keys: `q`, `n`, `p`, `/`, `0`, `Cmd-Shift-M`, `Cmd-[`, `Cmd-]`, and the key held to
   talk (10.3).
3. The map's color (8.3).
4. Postil does not offer `hippius-mem init`, and setup makes a trial vault only (9.1).
5. Read-only runs in threads of their own, correction 1 above, which replaces the shared
   reading worktree the design proposed.
6. The forced update only when history moved forward, correction 5 above.

Found when the plan's review was checked against the code on 2026-11-25 (#51), each with its
options, and settled since.

7. to 11. Settled by the owner on 2026-11-26, each as (a): the page asks again on a 250 ms
    timer while a background result is pending, as the settings dialog does, with no new
    nudge (7.7, 9.2); Cmd-click in a comment does nothing, as in a string (7.7); deleting a
    live ask succeeds and the thread ends when the run does, and resolving one is refused with
    409 while it runs (7.6); the undo notice is a new run log entry, `Entry::Undone { text }`
    (7.6, run log 7); `POST /api/run/ask` takes the new note and saves it only when the run
    starts, so a 409 at the cap leaves nothing (7.6).
12. to 15. Settled by the owner on 2026-11-25, each as (a): a deleted file's `lines` stay
    required and are ignored (7.3); lines past the end are not checked in Rust (7.3); a check
    that failed and was edited after shows failed, and the card ranks failed, not seen,
    stale, unfinished, passed (7.4); any finding on a `routine` or `mechanical` hunk shows it
    as `eyes`, with no new field (7.5).
16. **Checks on follow-ups** (7.4). Settled by the owner on 2026-11-25 as (a): each turn's
    Checks under its own Steps (`followUpPart`).
