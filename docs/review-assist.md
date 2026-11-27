# Postil: review assist

Status: 2026-11-26. Designed. Built so far: an execute call's command and exit code (step
7.2), the author's report (7.3), read and logged, the evidence on the run (7.4), the
walkthrough with its ratings and findings in the changes view (7.5), read-only threads with
Ask (7.6), jump to definition (7.7), the analysis file with its citation checking (8.1), and
analysis runs (8.2), with no page beyond the settings' Analyzer yet, and finding and setting
up hippius-mem (9.1), which no run uses yet. This document adds to
`docs/design.md`, which stays the reference for everything built so far.
`docs/review-assist-plan.md` turns it into pull requests, with the edge cases and tests of each.
Section "Decisions" lists what the owner settled on 2026-11-25; section "Open questions" lists
what this document assumes and the owner has not confirmed.

Mockups: https://claude.ai/code/artifact/61c79ac1-1498-4f37-9ec0-f2824cf3f92f (private to the owner until shared).

## The problem in one sentence

Agents write the changes and the engineer only reviews, so Postil must shorten the time from
"a run ended" to "I know whether this change is right, and why".

Four things serve that sentence:

1. **Reviewer aids** order and rate a run's changes, show what was verified, answer questions,
   and follow a name to its definition.
2. **The map** is an agent's analysis of the repository, drawn as diagrams that start from
   the system's architecture. Every node links to lines of code, with the link visible in
   the code too.
3. **Team memory** (hippius-mem) gives agents and the engineer the team's recorded decisions,
   and checks a run against them.
4. **Voice** dictates text and gives commands, so a review needs fewer keystrokes.

Postil stays a reader with a notes tray and an agent panel. It does not become an editor.

## Not in scope

- An LSP. Jump to definition matches names with tree-sitter tags and says so.
- Static analysis as the source of diagrams. The owner chose an agent, with its citations
  checked, over a tree-sitter graph and over a hybrid.
- Mermaid, or any renderer that owns the look. Postil draws its own SVG.
- A speech model in the binary, a network speech service, or an always-open microphone.
- Editing, forgetting or redacting team memories. The terminal does that in version 1.
- An analysis shared between clones. Each clone holds its own.

## What exists already and is reused

| Need | Reused from `docs/design.md` |
|---|---|
| Link a diagram node to code that keeps moving | A note's `Target::Lines` with its snippet, and `locate` ("Data structures") |
| An agent that may read and not edit | A proposing reviewer ("Reviews") |
| A structured result at the end of a reply | The reviewers' fenced `json` block, read as the run ends |
| A second view inside the code pane | The changes view |
| Finding an installed program | The agent catalog's search of the login shell's `PATH` ("Agents") |
| JSON-RPC over a child's pipes | `src/acp/connection.rs` |
| Small card, full dialog | "Cards and dialogs" |

## Where read-only agents run

Three features start an agent that must not edit: Ask, Analyze, and drafting a memory from a
finished thread. `docs/design.md` says the engineer's working tree is never an agent's working
directory, and that rule holds here.

- **A read-only run gets a thread of its own,** marked read-only in `threads.json`, with a
  worktree at `HEAD` like any thread. One shared reading worktree was the first design; it
  was dropped on 2026-11-25 after reading the code, because the run machinery is keyed by
  thread everywhere (the agent slots, the per-thread lock that counts live runs, the check
  for a reviewer's edits, eviction). A read-only thread reuses all of it; a shared worktree
  needs a second variant of each. The cost accepted: one `git worktree add` per ask.
- **A read-only thread clones no build folders,** cannot be applied or opened as a pull
  request, and is freed when its work is done: an analysis's as its run ends, an ask's when
  its note is resolved or deleted.
- **Read-only as reviewers are.** Postil rejects every tool call but reads, searches, fetches
  and thinking. An agent set to edit without asking is caught as its turn ends, by the edits
  its run reports; the worktree is reset and the run says so, as a proposing reviewer's does
  today. An analysis that edited stores nothing. An ask that edited shows its answer under
  that notice.
- **They count** toward the cap of 3 live runs and the 15 worktrees, and are evicted when
  idle like any thread.
- **The engineer's uncommitted edits are not seen**, as in a thread. Ask and Analyze say so
  when the checkout has any. Anchors are then placed on the engineer's files by their snippet,
  as notes are.

## Milestone 7: reviewer aids

### The author's report

The agent that made the changes ends its turn with a report, in the same conversation. The
owner chose the author over a fresh agent per run (a second turn and a wait on every run) and
over the reviewer alone (nothing guides the first read).

The prompt gains one paragraph that asks for a last code block fenced as `json`, given in
words and not as an example, for the reason "Reviews" gives:

```json
{
  "steps": [
    {"path": "src/acp/connection.rs", "lines": [37, 52], "why": "recv() no longer panics", "risk": "eyes"}
  ],
  "checks": [
    {"command": "cargo test acp", "result": "passed"}
  ]
}
```

- `steps` is the reading order: cause before effect, the core change before its call sites.
  `lines` are on the new side of the diff. `risk` is `eyes` (needs the engineer's judgment),
  `routine`, or `mechanical` (a rename, formatting, generated code).
- Postil reads the block as the run ends and logs it as one `report` entry. The run log is
  version 6 for it (version 5 keeps an execute call's command, for the evidence below).
- An agent that writes no report, or one Postil cannot parse, leaves the run without one. The
  changes view then works as it does today, and the run's dialog says the report is missing.

```rust
struct Report {
    steps: Vec<Step>,
    checks: Vec<Check>,
}

struct Step {
    path: RepoPath,
    lines: LineRange,   // New side of the diff.
    why: String,        // One line.
    risk: Risk,
}

enum Risk {
    Eyes,
    Routine,
    Mechanical,
}

struct Check {
    command: String,    // As the agent says it ran it.
    result: Claimed,    // Passed or Failed: the agent's claim, never Postil's finding.
}
```

### Guided walkthrough and risk

Built in step 7.5 (`ui/changes-view.js`, `walkthrough` in `ui/patch.js`). The changes view has
an order: Walkthrough (the default when the run's report names a step) or Files (the diff's
order). A hunk two steps overlap belongs to the first, and the later one says "shown in step
N"; a step that names no changed line is listed last.

- **A step covers the hunks its lines overlap.** `n` and `p` move between steps; the step's
  `why` shows above its hunks.
- **Hunks no step names are never hidden.** They list after the last step under "Not in the
  walkthrough", rated `eyes`: what the author left out is what most needs reading.
- **Risk filters, it does not remove.** A header control shows all, or `eyes` first.
  `mechanical` hunks start folded to one line each with their counts, and open on a click.
- **The reviewer checks the ratings.** A proposing reviewer's prompt includes the report and
  asks for a finding on any hunk rated too low. A hunk with such a finding shows as `eyes`,
  with the finding beside it.
- **One run at a time.** A walkthrough belongs to one run. The view of a whole thread (several
  runs) keeps the Files order in version 1.

### Evidence on the run

"Did it run the tests" is answered from two sources that Postil compares: what the agent
claims (`checks`) and what the run log holds (the tool calls of kind execute, each with its
command and how it ended). Run log version 5 keeps an execute call's command and exit code
(step 7.2). A non-zero exit code is a failure whatever the call's status says: Grok ends such
a command as `completed` ("Open questions", 5). With no exit code, the states read
`completed` and `failed` as the agent reports them.

| State | Meaning |
|---|---|
| Seen, passed | A logged command matches the claim, completed, and no edit finished after it. |
| Seen, failed | A logged command matches the claim and failed. |
| Stale | It passed, then an edit finished later in the same run. |
| Claimed, not seen | The report names a command no logged tool call matches. |
| None | The run ran no command. |

- The run's card shows one icon for the worst state; the run's dialog lists each check with
  its state and opens the step that ran it.
- Postil does not guess which commands are tests. It lists what the author says it checked,
  and under it every other command the run ran.
- A claim matches a logged command when the texts are equal after trimming and collapsing
  spaces. Anything looser would show a claim as seen that was not.

### Ask without changing

The engineer selects lines, a file, a folder or a diagram node and asks a question. Nothing is
edited.

- **An ask is a note with another intent.** `Note` gains `intent: Change | Ask`
  (`review.json` version 5). It keeps its place in the tray, its anchor, its dialog, its
  follow-ups and its status, for the reason `docs/design.md` gives for file notes: a note
  keeps every view working on it.
- **Keys:** `q` on a selection, or Ask in the right-click menu, opens the composer in ask
  mode; `Cmd-Enter` sends at once to the default agent. An ask is never part of a send of
  notes.
- **The run** is read-only in a thread of its own, with a fresh agent kept for the ask's
  follow-ups. The answer is the agent's reply, shown in the note's dialog, and the tray card
  shows Answered.
- **From an answer:** Make it a note (a `Change` note on the same lines, with the engineer's
  text), or Remember (milestone 9).

Built in step 7.6, as the owner settled it on 2026-11-26: `POST /api/run/ask` takes the new
note and saves it only once its run has a place, so an ask refused at the cap of 3 live runs
leaves nothing; Delete of a live ask succeeds, and its thread goes when its run ends; Resolve
of a live ask is refused (409), and of an idle one ends its thread and frees its worktree; an
undo notice is logged (`undone`, run log version 7), so it survives a restart.

### Jump to definition

`Cmd`-click on a name, or Go to Definition in the right-click menu, goes to where the name is
defined. A key such as `g d` was dropped: the code view selects whole lines and has no caret
to stand on a name.

- **Source:** `tree-sitter-tags` 0.27.2 (the same release line as the highlighter) with the
  `tags.scm` each grammar crate ships. Rust, JavaScript, TypeScript, Python and Go ship one;
  JSON, TOML and Bash do not and have no definitions.
- **The index** maps a name to its definitions, a definition being a path, a line and a kind.
  It is looked up by name on a click and rebuilt per file. A worker thread, `index-definitions`,
  builds it after the project opens and again for the files whose size or modification time
  changed when the page refreshes: on window focus and when a run's status changes, through
  `POST /api/definitions/refresh`. A lookup during a refresh answers from the index as it
  was; before the first build ends it says `{"state": "building"}`, and the page asks again
  every 250 ms. It lives in memory only. Built (`src/definitions.rs`, step 7.7). Measured
  2026-11-26 on a release build: this repository (145 files) builds in 0.19 to 0.33 s and
  refreshes in 10 to 17 ms; a synthetic one of 50,000 small Rust and JavaScript files builds
  in 1.4 to 6.3 s and refreshes in 150 to 250 ms, of which listing the files with git takes
  40 to 60 ms. The ignored test `fifty_thousand_files_build_and_refresh` measures it again:
  `cargo test --release --lib fifty_thousand -- --ignored --nocapture`.
- **One definition** jumps. **Several** open a list of one-line cards ranked: same file, same
  folder, then the rest by path, at most 50 with the count. **None** shows a toast, and so
  does the clicked name being its own only definition.
- **It matches names, not types.** Two functions named `new` both list. The list's header
  says "by name".
- **What a click finds**: the name under the pointer, Unicode identifier characters and `$`.
  A click on whitespace, punctuation, a number, a string, a keyword or a comment does nothing.
  A file whose language has no tags query says "No definitions for this file type." Files over
  2 MB or not UTF-8 are not indexed.
- `Cmd-[` and `Cmd-]` move back and forward through the places jumped from and to, at most
  50; going back restores the scroll and the selection, and a place whose file is gone is
  skipped. The history lives in the page, so opening another project, which reloads it, clears
  it. The changes view and the prompt preview do not jump.

## Milestone 8: the map

### What the engineer sees

The middle pane splits: the diagram on the left, the code on the right, a divider between
them. The owner chose the split over a diagram that replaces the code and over a tray tab,
because both directions of the link stay visible.

```text
+-------+---------------------------------+--------+
| files | Map > run        | code         | tray   |
|       |   [site]         |   37 fn send |        |
|       |     |            |   38   let   |        |
|       |   [run]*         |   39   ...   |        |
|       |     |            |              |        |
|       |   [acp]          |              |        |
+-------+---------------------------------+--------+
```

- **Opening it:** the Map button beside the project name, View > Map, or `Cmd-Shift-M`. With
  no analysis the pane shows what Analyze will do (the agent, read-only, from `HEAD`) and one
  Analyze button.
- **Sizes:** the diagram keeps at least 320 px and the code its 420 px. In a window too
  narrow for both, the diagram takes the whole pane and `Cmd-Shift-M` switches between the
  two. The width is kept in `layout.json`.
- **The header** holds a breadcrumb (Architecture, then each component deepened into), three
  kinds (Architecture, Flows, Data), a node search on `/`, and Update when nodes are outdated.
- **A node** is a small card: its label, one line of summary, and a mark when it is outdated
  or unverified. Everything else is in its dialog, by the rule of "Cards and dialogs": the
  detail, every anchor, the edges in and out, the notes on its lines, Ask, and Add note.
- **Diagram to code:** a click selects the node, opens its first anchor in the code pane and
  tints its lines. Enter opens the node's deeper diagram when it has one, else offers Analyze
  deeper.
- **Code to diagram:** the node that covers the code pane's selected line, or its first
  visible line with no selection, is ringed in the diagram, which pans to it only when it is
  off screen.
- **Moving:** scroll pans, pinch zooms, `0` fits. Panning to a node takes 160 ms, ease-out,
  and nothing moves under Reduce Motion. An idle diagram draws no frames.

### The link shown in the code

The owner asked for the link to be visible in the code, as notes are.

```text
      36
  |   37  fn send(&self, notes: &[Note])        [run: send]
  |   38      let prompt = build(notes);
  |   39      self.agent.prompt(prompt)
  |   40  }
```

- **A stripe** at the gutter's left edge spans the node's lines, and **a chip** at the end of
  the first line names the node. A click on either opens the map at that node.
- **A color of its own,** `--map`, a muted violet in both palettes. Amber stays reserved for
  notes, steel for focus. This is a second rare accent, which "Look and feel" does not allow
  yet; the owner approved the stripe and chip on 2026-11-25 and that section is amended.
- **One stripe per line.** Lines covered by nested nodes show the innermost node of the
  Architecture kind. When other diagrams cover the line too, the chip ends with a count that opens a menu
  of them.
- **A flow's steps** show in the gutter as their numbers only while that flow is open.
- **Outdated nodes** draw a dashed stripe. Lost anchors draw nothing in the code, as lost
  notes do.
- View > Map Markers turns stripes and chips off. They are on once a repository has an
  analysis.

### The four kinds

| Kind | Holds | Made by |
|---|---|---|
| Architecture | The system as it runs: its components and how they talk. | Analyze, Analyze deeper |
| Data | The main types, what they hold, how they relate. | Analyze |
| Flow | One behavior as ordered steps through the code. | Asked for by name |
| Change map | The Architecture with what a run touched marked. | Computed, never stored |

- **Architecture is the system, not the folder tree.** The owner set this on 2026-11-25. Its
  top level shows what exists when the program runs: components (a process, a thread, a
  service, a page), stores (a file, a database, a queue) and external systems (a subprocess,
  a third-party API). A group is a boundary such as a process or a machine. An edge is a way
  two of them talk, labeled with the mechanism ("JSON-RPC over stdio", "fetch on postil://",
  "appends"). For Postil that is the diagram in `docs/design.md`, "Architecture": the page,
  the Rust process with its worker and agent reader threads, the agent subprocess, and
  `review.json`.
- **Modules appear only below a component.** Analyze deeper on a component gives its
  internals: the modules that make it, then their symbols. A module graph of the whole
  repository is never the top level.
- **Every node still cites code.** A component cites where it starts or is defined; a store
  cites the code that reads and writes it; an external system cites the code that calls it.
  An edge cites the call, the send or the write that makes it.
- **Analyze** on the project asks for the Architecture's top level and the Data diagram of
  the whole repository. The prompt asks for 7 to 25 nodes per diagram, a range chosen for
  legibility at the pane's width; Postil draws what it gets.
- **Analyze deeper** on a node, or on a folder or file in the file list's right-click menu,
  asks for one more diagram under that node.
- **A flow** is asked for in the Flows list ("Trace: how a note is sent"), or from a
  selection or node with Trace from here. The order of its nodes is the order of its steps.
- **The change map** opens from a run's dialog, beside Changes. A node is touched when a hunk
  of the run's diff overlaps one of its anchors, placed in the run's snapshot before the run.
  Touched nodes fill with the added color, nodes one edge from a touched node get an outline,
  and changed files that no node covers list under "Not on the map". A click on a touched
  node opens the changes view at its hunks.

### Data structures

Built in step 8.1 (`src/analysis.rs`, `GET /api/analysis`): the types below, the file at
version 1 (version 2 from step 8.2, which adds each diagram's `dropped`) without `checked`,
which comes with the code that reads it in step 8.7 (version 3),
checking a citation, and placing every verified reference on each read. As built, an unverified
reference has no `target`, only its path and, when its file could not be read, a `reason`:
its lines may be zero, reversed or past the end of a file that may not exist.

Looked up: every anchor of the open file, on each file open. Iterated: one diagram's nodes
and edges, on each draw. Changed: whole diagrams, by an analysis run. A repository holds tens
of diagrams of tens of nodes, so every operation is a scan, as for notes.

```rust
/// `<git dir>/postil/analysis.json`, version 2.
struct Analysis {
    checked: CommitId,        // The commit the Architecture was last compared with.
    diagrams: Vec<Diagram>,
}

struct Diagram {
    id: DiagramId,            // "D1": stable, shown in the UI and sent to the agent.
    kind: DiagramKind,        // Architecture, Data or Flow.
    title: String,
    parent: Option<NodeId>,   // The node this diagram deepens. None at the top level.
    run: RunId,               // The analysis run that made it.
    nodes: Vec<Node>,         // In a Flow, in step order.
    edges: Vec<Edge>,
}

struct Node {
    id: NodeId,               // "D1.3": the diagram's id, then a number.
    kind: NodeKind,           // Decides the node's shape.
    label: String,
    summary: String,          // One line, on the card.
    detail: String,           // Markdown subset, in the dialog.
    group: Option<NodeId>,    // The boundary drawn around this one.
    anchors: Vec<CodeRef>,
}

enum NodeKind {
    Component,                // Something that runs, or a module or symbol below one.
    Store,                    // Something that holds data.
    External,                 // Something the repository talks to and does not contain.
    Boundary,                 // A group: a process, a machine, a network.
}

struct Edge {
    from: NodeId,
    to: NodeId,
    label: String,
    anchor: Option<CodeRef>,  // The call, import or field that justifies the edge.
}

/// A place in the code, with whether Postil found it.
struct CodeRef {
    path: RepoPath,
    target: Target,           // The notes' enum: lines with their snippet, or a path.
    state: RefState,
}

enum RefState {
    Found,
    Outdated,                 // The snippet is gone from the file.
    Unverified,               // The agent's citation never matched the file.
}
```

- **Each diagram owns its nodes and edges.** A shared pool of nodes with diagrams as views
  was weighed: it lets one node appear in two diagrams, but an Analyze deeper or a deleted
  flow would then edit shared records. Self-contained diagrams are replaced and deleted whole.
- **`CodeRef` reuses `Target`**, so placing an anchor is `locate`, the function that places
  notes, with the same rule: stay, move to the only other copy, or lose the place.
- **Positions are not stored.** The layout is computed from the graph when a diagram first
  draws and kept for the session. The same graph gives the same layout.
- **The file** is written atomically under a lock, refuses unknown fields, and is never
  written over when it cannot be read, as `review.json` is. An agent or a script can read it.
- **Private to the clone.** The owner chose the git directory over a tracked file (it would
  change the working tree and conflict between teammates) and over hippius-mem (a memory
  store is not made for graphs).

### An analysis run

Built in step 8.2 (`src/analysis/prompt.rs`, `src/analysis/merge.rs`, `Runner::analyze`,
`POST /api/analysis`, `POST /api/analysis/update`, `DELETE /api/analysis?diagram=D3`), with
`analysis.json` at version 2: each diagram keeps what its runs left out (`dropped`), so the
pane's header can say it.

- **The agent** is the settings' Analyzer, a new choice beside the default agent and the
  reviewers, which starts as the default agent (`settings.json` version 3).
- **The run** is read-only in a thread of its own, which ends with it: its worktree and branch
  are freed, and its record kept, with the commit the run started from, which Update compares
  the files with. Its header is marked `analysis: {kind, of, nodes}` (run log version 8);
  `nodes` is what an update may change. It counts toward the cap of 3 live runs and is not
  listed in the Run tab, which lists runs of notes; the diagram pane's header shows its status
  and Stop, and opens its dialog. One analysis runs at a time in a window; two windows may
  each run one, and the later merge wins.
- **The merge** happens as the turn ends well, before the run's end is logged: the citations
  are checked in the worktree first, then `analysis.lock` is held only to read the file as
  stored, merge and save. A run that is stopped, fails, or edits files stores nothing, and one
  whose reply gives nothing says why in its dialog (the `noAnalysis` entry). Ids are Postil's:
  the agent names nodes with keys of its own.
- **A diagram's parent** is a node of another diagram, and parents never loop: `analysis.json`
  refuses a file that breaks this. A replaced or deleted diagram, or a node Update deletes,
  takes the diagrams below it with it; a node Update replaces in place keeps its id, and the
  diagrams below it stay.
- **The reply** ends with one `json` block: diagrams of nodes and edges, where a citation is
  `{path, lines, first}` and `first` is the text of the first cited line.
- **Checking a citation.** Postil reads the file in the run's worktree. The citation holds
  when the line at `lines` equals `first`, both trimmed. Otherwise Postil looks for the only
  line in the file equal to `first` and shifts the range to it. Otherwise the reference is
  `Unverified`. Postil then stores the cited lines as the snippet itself, so the agent never
  sends file contents back.
- **What unverified means on screen.** A node with no verified anchor is dashed, says "not
  found in code" and has no stripe. The pane's header counts them ("42 nodes, 3 unverified").
  Nothing is dropped silently.
- The analysis is not checked for truth beyond its citations. An edge that cites a real call
  is drawn as the agent described it.

### Outdated nodes and Update

- **Outdated** is worked out on every read, as a note's anchor is: the snippet is no longer
  in the file. A node with an outdated anchor is marked in the diagram and dashed in the code.
- **Not on the map** is every file in the file list that no anchor covers and that was added
  since the diagram's run.
- **Update** sends the Analyzer only the outdated nodes, each with its old snippet, and the
  files not on the map. The reply replaces those nodes by id, adds nodes and edges, and may
  delete a node whose code is gone. The owner chose this over a full analysis as the only
  fix.
- An update can shift the layout, because positions follow the graph. Accepted.

### Forced update when the architecture changes

The owner set this on 2026-11-25: a merge that changes the system's architecture updates the
diagram without being asked. It replaces "update on request" for the Architecture kind only.
Flow and Data diagrams, and uncommitted edits, still mark nodes outdated and wait for Update.

- **The trigger is `HEAD` moving forward,** not a pull request. `Analysis` holds `checked`,
  the commit the Architecture was last compared with. When the window regains focus, and
  when a run's status changes, the page asks Postil to compare `checked` with `HEAD`; there
  is no focus hook in the Rust process, and the page already refreshes at both moments. The
  update runs only when `checked` is an ancestor of `HEAD`: a commit, a merge or a pull.
  That covers a pull request merged from
  Postil, one merged on GitHub and pulled, a teammate's commits, and Apply once committed,
  with one rule and no call to `gh`. Chosen over watching only the pull requests Postil
  opened, which would miss every other way a change lands.
- **A change counts when it touches the diagram.** No agent judges. After the move, any of
  these is true: an anchor of an Architecture node or edge is `Outdated`; the diff from
  `checked` to `HEAD` adds a file no node covers; it deletes a file a node cites. The downside
  accepted: a refactor inside a component's cited lines starts an update that finds nothing
  to change. An agent judging every merge was rejected (a turn per merge, and a wrong "no"
  leaves the diagram stale with nothing on screen), and so was the author declaring it (it
  covers only Postil's own runs).
- **The update starts by itself:** the Update run above, for the Architecture's outdated
  nodes and the files not on the map, with no click. The diagram pane's header says
  "Updating after" and the commit's short id, with Stop. It spends tokens unasked; the owner
  accepted that for this case.
- **`checked` moves to `HEAD` as the run starts,** written before the agent is. A run that
  fails or is stopped is therefore not started again for the same commit: its nodes stay
  outdated and Update is there to press. A change that touches nothing moves `checked` with
  no run.
- **At the cap of 3 live runs** the update does not start and `checked` does not move, so the
  next focus or thread end tries again.
- **Another history starts no update.** A branch switch, a rebase or a reset makes `checked`
  no ancestor of `HEAD`. Comparing the two would start a run on every switch between
  branches, so the diagram instead says the checkout moved to another history and offers
  Update, and `checked` moves when it is pressed. Decided 2026-11-25 while planning.
- A repository with no analysis has nothing to update, and nothing runs.

### Drawing and layout

- **Postil draws the SVG**: cards as rounded rectangles in the palette's variables, edges as
  paths with their labels, groups as outlined regions. Hover, focus, the ring and the touched
  fill are CSS classes. `ui/diagram-view.js` draws, `ui/diagram.js` holds the rules that need
  no document (which node covers a line, which nodes a diff touches) with tests beside it.
- **Layout comes from one vendored file**, the page's first third-party JavaScript:
  `@dagrejs/dagre` (MIT), kept as `ui/vendor/dagre.min.js` with its license, the way
  `ui/icons.svg` is kept at a pinned commit. Version 3.1.4 was published on 2026-11-24; the
  pin is a release at least two weeks old on the day it is vendored. Its size in the binary
  is measured then and recorded in `docs/design.md`, "Dependencies".
- **It loads when the map first opens**, so launch pays nothing for it.
- **Rejected:** `elkjs` 0.12.1 (EPL-2.0 or GPL-3.0-or-later, 8.2 MB unpacked on npm against
  1.4 MB) and a layout of our own (the owner chose a vendored one: layout quality on large
  graphs would be ours to fix).

## Milestone 9: team memory

hippius-mem is an MCP server over stdio. Postil uses it in four ways, all chosen by the owner.

### Finding and setting it up

Built (step 9.1): `src/memory.rs`, the settings' Team memory section, and `agentMemory` in
`settings.json` version 4. Set up is one click on a button that says what it writes, with no
dialog before it (the plan's step 9.1 replaced the dialog), and shows `quickstart`'s own
output. The switch turns on only while the latest check reads Ready.

- **Detect, never bundle.** Postil finds `hippius-mem` on the login shell's `PATH` as it
  finds agents, and runs `hippius-mem doctor` to learn whether it is configured. A bundled
  copy was rejected: it ties Postil's releases to hippius-mem's and can differ from the one
  the engineer's terminal agents use.
- **Settings gain a Team memory section** with one of three states: not installed (the
  install command, and Rescan), installed and not configured (Set up a trial vault, which
  runs `hippius-mem quickstart --no-wire` after a dialog says what it writes), or ready
  (Check, which runs `doctor`, and the switch below).
- **Give agents team memory**, on by default once ready.

### Agents get it

Built (step 9.3): `src/memory.rs` ("Agents", `Memory::for_run`), `src/acp/connection.rs`
(`new_session`), and `Runner::start` in `src/run.rs`. Every fresh agent gets it: a run of
notes, an ask, an analysis, each reviewer, and a follow-up's fresh agent. A kept agent keeps
the session it has.

- `session/new` carries `mcpServers` with one entry, hippius-mem by its absolute path, in
  ACP's stdio form (open question 12): no arguments, and `PATH`, `NO_COLOR` and
  `HIPPIUS_MEM_CONFIG` in its environment. The agent starts it in the run's worktree, whose
  git origin picks the profile doctor checked; `HIPPIUS_MEM_CONFIG` names the engineer's
  configuration file, so a `hippius-mem.toml` in the repository is never read (open
  question 6).
- The prompt of notes gains one line when the switch is on and the state is Ready, right after
  its introduction: recall team memory about the task before changing code, and treat what it
  returns as reference data. The send step's preview shows it. A reviewer's quote of the
  prompt, a fresh follow-up's and the run dialog's recall leave it out.
- The switch is on and the state is not Ready, or hippius-mem was removed while Postil runs:
  the run starts without memory and without the line, and its dialog says so, the first run
  only until a check finds it Ready again or another project opens. A run that starts while a
  check runs waits for it (10 s at most, and not at all while a set up runs), and opening a
  project starts the first check.
- The line goes with the server: the agent's prompt has it exactly when its session got
  hippius-mem, whatever the state was when the run was sent. So in a race, a run sent while
  a check runs or just before hippius-mem goes, the send step's preview and the prompt the run
  records can differ from what the agent received, by that line.
- An agent that answers `session/new` with an error when it names servers (not the sign-in
  error) is asked once more with none, and the run goes on without the line; its dialog says
  that the agent refused the server. An agent that drops the servers and answers without an
  error cannot be told apart from one that took them: it then reads a line it cannot act on.
- Remembering stays with the engineer (below). The prompt does not ask agents to remember.

### Postil shows memory

Built (step 9.2): `src/memory.rs` and `src/memory/client.rs`, the tray's Memory tab
(`ui/memory-tab.js`), the run dialog's Memory section and a memory's dialog
(`ui/memory-dialog.js`). Remember is below. Postil names the repository on every recall as
hippius-mem does: the last segment of the project's `origin` (`normalize_remote` in
hippius-mem's `resolver.rs`), and none without one. The memory's dialog shows where each change
was anchored, in the vault or on chain; it does not check the Merkle proof itself. The tab
recalls for the file's path; the map pane passes the selected node's label as `node` once it
is built (step 8.3).

Postil is an MCP client itself: `src/memory.rs` starts one `hippius-mem` process per window
on first use and calls `recall`, `get`, `history`, `refresh` and `remember` over its pipes,
with hand-written serde types as for ACP. The calls run on a thread of their own, never on
the one thread that answers the page: a slow recall would otherwise freeze every view. A
route answers "pending" and the page asks again every 250 ms until the result is in, as the
agent scan's page does (the plan's open item 7, settled by the owner on 2026-11-26).

- **A Memory tab in the tray** (Notes, Run, Memory) lists the memories recalled for the open
  file: one card each with the summary, the author and the age. The query is the file's path
  and the selected node's label when there is one. It runs when a file opens, not on scroll,
  and is kept per path for the session; Refresh pulls the team's latest and asks again.
- **A run's dialog** has a Memory section: the recall for the run's prompt and changed paths.
- **Weak pointers** list dimmed under "Possibly related". When every pointer is weak, the tab
  says nothing is recorded for this file.
- **A card opens the memory's dialog:** its body (`get`), its author and its history
  (`history`), with whether its anchor verified.
- Memory text is a teammate's writing. Postil draws it as text and never acts on it.

### Postil writes memory

Built (step 9.4) for a note and an answered ask, from the note's dialog: `Runner::draft` in
`src/run.rs`, `POST /api/memory/draft` and `POST /api/memory/remember` in `src/site.rs`,
`Memory::remember` in `src/memory.rs`, `Note.memory` in `src/notes.rs` (`review.json` version
6), the `remember` header of run log version 9, and the Remember dialog (`rememberDialog` in
`ui/memory-dialog.js`). Remember from a finding or a run is not built: the owner left those
out of 9.4.

- The draft is a read-only run in the note's thread, after its latest turn, whatever the
  thread's mode: it commits what was left in the worktree first, takes no snapshot, its asks
  to edit are refused, and edits it makes anyway are undone, which fails the draft. It goes
  to the thread's kept agent when that agent ran the latest turn, else to a fresh agent told
  the thread so far, which gets no team memory: a draft run never writes memory. A kept agent
  whose session got hippius-mem's server is never used for a draft (decided 2026-11-27): a
  memory it wrote would be an external write the engineer did not save. A fresh agent with no
  server drafts instead, and the kept agent, with its conversation, is let go. It is never the
  Run tab's latest run, and the thread's next follow-up goes to a fresh agent.
- A note never sent, or whose thread was applied or discarded, gets no draft run: the dialog
  opens with the note's text. A draft that fails, or whose reply ends without a
  `{"summary", "body"}` block, falls back the same way and says why.
- While a run of the thread is under way, Remember is disabled ("Wait for the run").
- Save sends exactly the summary, body, type and scope shown, through the memory thread, with
  the repository named explicitly (the whole team's when the repository has no origin). A
  blank summary or body, or one over its limit (1 KiB, 16 KiB), is refused before the call.
  Save is disabled while it runs. A near-duplicate refused by hippius-mem offers Save anyway,
  which passes `force`; any other failure keeps the dialog open with the text.
- The duplicate check is a recall of the summary; one that cannot run says so and leaves Save
  as it was.

Remember is offered in the dialog of a note, of a finding, of a run, and of an answered ask.

1. **The agent drafts.** The thread's agent gets one more turn in its conversation (or a
   fresh agent with the thread's history, as follow-ups do) and writes a self-contained fact
   with a keyword-rich summary. The owner chose a draft over the note's raw text: a note
   written for one agent in one moment ("this panics") does not stand on its own.
2. **Postil looks for duplicates:** a `recall` with the draft's summary, shown above the
   draft as "Already recorded?".
3. **The engineer edits and saves.** Only the Save click calls `remember`, from Postil. An
   agent Postil runs is never asked to write team memory.
4. **The note keeps the memory's id** (`review.json` version 6; `intent` is version 5) and shows a
   Remembered chip that opens the memory.

### Review checks against memory

The owner chose a reviewer stage that judges over listing raw recall hits beside the changes
(recall is fuzzy, and the engineer would judge every hit).

- When the engineer presses Review, Postil recalls for the run's prompt and for each changed
  path, drops weak pointers and duplicates, and keeps at most 12 (recall's default count).
- The proposing reviewers' prompt carries those memories whole, in a block labeled as
  reference data written by teammates, with the ask: report a change that contradicts a
  recorded decision or repeats a recorded gotcha as a finding that names the memory.
- A finding gains `memory: Option<MemoryId>` (run log version 10). Its card shows the
  memory's summary and opens it, and the changes view flags the finding's line, since a
  finding already carries its path and line.
- With hippius-mem missing or failing, the review runs without the block and its dialog says
  so.

## Milestone 10: voice

### Dictation

Designed in another session; this section records its decisions. Apple's Speech and AVFAudio
frameworks (`SFSpeechRecognizer`, `AVAudioEngine`), called from Rust through objc2, with
on-device recognition required: no model in the binary, no daemon, no network call.

- **`src/dictation.rs`** taps the input node, feeds a recognition request with partial
  results, computes a mic level (20 to 30 Hz, only while recording, so an idle window still
  draws no frames), and pushes state, level, transcript and error events through the event
  loop to the page. It starts a fresh recognition task on a pause and near Apple's limit of
  about 60 seconds per request.
- **Typed bindings.** `objc2-speech` 0.3.2 and `objc2-avf-audio` 0.3.2, default features off
  and only the classes used, plus `block2` 0.6.2, which is in the lockfile already. Chosen by
  the owner on 2026-11-25 over raw `msg_send!`: two more crates buy calls the compiler checks.
- **A second module with `unsafe`.** The audio tap and the completion blocks need it. The
  owner allowed `src/dictation.rs` on 2026-11-25, as `src/clone.rs` was allowed on
  2026-11-24; the crate still denies `unsafe` elsewhere, and `Cargo.toml`'s comment on the
  lint names both modules when this is built.
- **`ui/dictation.js`** is one widget attached to every text box (the composer, the note,
  path-note and run dialogs, the question form, the memory draft, the flow box). It tracks
  the range it is inserting, so partial results change in place and the text around the
  caret is kept.
- **Code-aware.** `contextualStrings` gets the open file's name, the names of the symbols in
  view (from the jump index) and the labels of the open diagram's nodes.
- **The bundle** gains `NSMicrophoneUsageDescription` and
  `NSSpeechRecognitionUsageDescription` (`scripts/bundle.sh`) and the audio-input entitlement
  (`assets/Postil.entitlements`). macOS ends a bare binary that asks for the microphone
  without an `Info.plist`, so dictation is tested from `Postil.app`, not `cargo run`.

### Voice commands

The owner chose hold-to-talk over a listening mode: nothing is heard by accident and the
microphone is off otherwise.

- **One key, held.** In a text box it dictates into the box. Anywhere else what is said is a
  command.
- **A fixed grammar, matched in the page** (`ui/voice.js`, a plain function from a transcript
  to a command or nothing, tested with `node --test`). No model interprets commands.

| Said | Does |
|---|---|
| "next", "previous" | The next or previous walkthrough step, or hunk. |
| "note", then words | Opens the composer on the selection or the current hunk, with the words. |
| "ask", then words | The same, in ask mode. |
| "open", then a file name | Opens the best fuzzy match in the file list. |
| "go to", then a name | Jump to definition for that name. |
| "show map", "show code", "show changes" | Switches the middle pane. |
| "resolve" | Resolves the note whose dialog is open. |
| "send" | Opens the send step. Said again there, it sends. |
| "stop" | Stops the run the header shows. |

- **What was heard is always shown** in a toast: the command it matched, or "not a command".
  Unmatched speech does nothing.
- **No voice for what cannot be undone:** Apply, Discard, deleting a note, saving a memory
  and opening a pull request need a click or a key.
- "note" and "ask" open the composer with the text; `Cmd-Enter` or a spoken "save" inside it
  commits.

## Decisions

Settled by the owner on 2026-11-25:

1. **Analyzer.** An ACP agent, read-only, whose every citation Postil checks. Rejected:
   tree-sitter only, and a hybrid.
2. **Rendering.** Postil's own SVG with a vendored layout file. Rejected: a layout of our
   own, and Mermaid.
3. **Diagram kinds.** Architecture, Flow, Data, and the change map of a run. The
   Architecture is the running system (components, stores, external systems, how they talk);
   modules show only below a component.
4. **Placement.** The middle pane splits into diagram and code.
5. **In the code.** A gutter stripe and a chip, in a color of their own.
6. **Storage.** `<git dir>/postil/analysis.json`, private to the clone.
7. **Staleness.** Mark outdated; update on request. The exception: new commits on the
   checkout that touch the Architecture diagram start its update without a click.
8. **Scope.** The whole repository first, then deeper on request, and flows by name.
9. **Analyzer agent.** A setting of its own, starting as the default agent.
10. **Memory.** Agents get hippius-mem; Postil shows memory; Postil writes memory from an
    agent's draft the engineer approves; a reviewer stage checks a run against memory.
11. **Memory setup.** Detect the installed binary and offer its setup steps. Not bundled.
12. **Reviewer aids.** Walkthrough, risk ratings, evidence, ask, jump to definition. Viewed
    marks were offered and not chosen.
13. **Walkthrough author.** The agent that made the changes; a reviewer can dispute a rating.
14. **Dictation.** Typed bindings, with `unsafe` allowed in `src/dictation.rs`.
15. **Voice commands.** Hold a key to talk.
16. **Order.** Reviewer aids, the map, memory, voice: milestones 7 to 10.

## Open questions

Assumed in this document and not confirmed. Each changes an interface, a data model or what
Postil writes outside itself.

1. **Read-only threads.** Read-only agents run in a thread of their own at `HEAD`, not in
   the engineer's working tree, so they do not see uncommitted edits.
2. **An ask is a note: settled by the owner on 2026-11-26** (built in step 7.6), with
   `intent: Ask`, not a record of its own.
3. **Keys.** `q` to ask, `Cmd`-click to jump, `Cmd-Shift-M` for the map, `n` and `p` for
   steps. The key held to talk is not chosen: Right Option is the proposal, since Fn
   belongs to macOS dictation.
4. **The map's color.** A muted violet; the mockups show it for approval.
5. **Exit status: answered by spike S1, 2026-11-25.** Claude (`claude-agent-acp` 0.94.0)
   and Grok (1.0.92) ran `echo hello`, `sh -c 'echo failing >&2; exit 3'`,
   `ls no-such-directory` and a Python test through Postil's own connection, in a scratch
   repository. Neither asked permission for these commands. The tool call messages, verbatim
   apart from `_meta` and long paths:

   ```text
   Claude, the call:
   {"sessionUpdate":"tool_call","toolCallId":"toolu_01EvP1...","name":"Bash","rawInput":{},
    "status":"pending","title":"Terminal","kind":"execute","content":[]}
   then updates with no kind:
   {"sessionUpdate":"tool_call_update","toolCallId":"toolu_01EvP1...",
    "rawInput":{"command":"sh -c 'echo failing >&2; exit 3'"},
    "title":"sh -c 'echo failing >&2; exit 3'"}
   {"sessionUpdate":"tool_call_update","toolCallId":"toolu_01EvP1...","status":"failed",
    "rawOutput":"Exit code 3\nfailing","content":[{"type":"content","content":{"type":"text",
    "text":"```\nExit code 3\nfailing\n```"}}]}
   a command that succeeds:
   {"sessionUpdate":"tool_call_update","toolCallId":"toolu_01KZZC...","status":"completed",
    "rawOutput":"hello","content":[...]}

   Grok, the call, with no kind and no status:
   {"sessionUpdate":"tool_call","toolCallId":"call-68c5...-1","title":"run_terminal_command",
    "rawInput":{"command":"sh -c 'echo failing >&2; exit 3'",
    "description":"Fail on purpose with exit 3"}}
   then:
   {"sessionUpdate":"tool_call_update","toolCallId":"call-68c5...-1","kind":"execute",
    "title":"Execute `sh -c 'echo failing >&2; exit 3'`","content":[...],"locations":[],
    "rawInput":{"variant":"Bash","command":"sh -c 'echo failing >&2; exit 3'",
    "description":"Fail on purpose with exit 3","is_background":false}}
   {"sessionUpdate":"tool_call_update","toolCallId":"call-68c5...-1","status":"in_progress",
    "rawOutput":{"type":"Bash","output":[],"output_for_prompt":"","exit_code":0,...}}
   {"sessionUpdate":"tool_call_update","toolCallId":"call-68c5...-1","status":"completed",
    "rawOutput":{"type":"Bash","output":[102,97,...],"output_for_prompt":"exit: 3\nfailing\n",
    "exit_code":3,"command":"sh -c 'echo failing >&2; exit 3'","truncated":false,
    "signal":null,"timed_out":false,...}}
   ```

   - **Claude reports a non-zero exit as `failed`; Grok as `completed`,** with the code only
     in `rawOutput.exit_code`, and `exit_code` 0 on updates while the command runs.
   - **The kind is not on every message.** Claude names `execute` only on the call, whose
     `rawInput` is empty; Grok names it only on its first update.
   - `rawInput.command` was a string for both. No array was seen.
   - The plan's fallback ("ran" instead of "passed", from `status` alone) would show Grok's
     failures as passed. The owner chose instead to keep the exit code with the command in
     run log version 5 (step 7.2, #54), and evidence reads a non-zero exit as failed whatever
     the status says.

6. **hippius-mem's repository scope in a worktree.** Agents run in `~/.postil/worktrees/...`;
   whether the server binds the right repository from there is not checked yet. `doctor`
   picks its profile by the git origin of its working directory, so step 9.1 runs it in the
   open project's root (a worktree of it shares its origin), in the temporary directory with
   no project open, and checks again when another project opens. With no project open, only
   a catch-all or trial profile reads Ready. hippius-mem also reads a `./hippius-mem.toml`
   before the global configuration when `HIPPIUS_MEM_CONFIG` is unset
   (`Config::from_env_and_file`, `hippius-mem/src/config.rs` at db9261b), and a reviewed
   repository could ship one that names its own endpoint, which would receive the engineer's
   notes and prompts and answer recalls with notes of its choosing. **Decided by the main
   session on 2026-11-27 in step 9.3, for the owner to confirm: not acceptable.** Every
   hippius-mem Postil starts (doctor, Set up, Postil's own server, each agent's server) gets
   `HIPPIUS_MEM_CONFIG`: the value in Postil's own environment when the engineer set one,
   else `${XDG_CONFIG_HOME:-$HOME/.config}/hippius-mem/hippius-mem.toml`, the file
   `quickstart` writes. With it set, hippius-mem reads that file only, and a missing one still
   fails with "failed to load configuration", which offers Set up. The working directory still
   picks the profile by its git origin. This changes steps 9.1's and 9.2's spawns. The downside
   accepted: an engineer who relies on a project's own `hippius-mem.toml` must start Postil with
   `HIPPIUS_MEM_CONFIG` naming it, which hippius-mem's own `install` already pins for its MCP
   server.
7. **`hippius-mem init`.** It writes `CLAUDE.md`, `AGENTS.md` and hooks into the repository.
   This document does not offer it from Postil. Step 9.1 is built that way, not confirmed.
8. **The trial vault.** Set up runs `quickstart`, which makes a local vault. Moving to a
   shared bucket (`upgrade`) stays in the terminal, since it prompts for a secret. Step 9.1 is
   built that way: Set up makes a trial vault only.
9. **The language of dictation.** The system's language is assumed. When on-device
   recognition is not available for it, dictation reports that and does not use the network.
10. **Remember without the thread's agent.** A note that was never sent has no agent to draft
    from; the dialog then opens with the note's text for the engineer to rewrite.
11. **The report block: answered by spike S2, 2026-11-25.** Each agent got a prompt built as
    `notes::prompt` builds one (one note: fix `add` in a scratch repository and run its test),
    with this paragraph after "list each note id", the wording step 7.3 ships, 10 runs each:

    > Then end your reply with a report for the engineer who reviews your changes: one JSON
    > object in a code block fenced as json, the last thing in your reply. Its field steps is
    > the order in which to read your changes, cause before effect and the core change before
    > its callers. Each step is an object with path (the file, relative to the repository
    > root), lines (the first and last line on the new side of the diff, as an array of two
    > numbers), why (one line) and risk (eyes when the change needs the engineer's judgment,
    > routine, or mechanical for a rename, formatting or generated code). Its field checks
    > lists each command you ran to verify the work, each an object with command (exactly as
    > you ran it) and result (passed or failed). Use an empty array when there is nothing to
    > list.

    - Claude: 10 of 10 replies ended with a block that parses with the shape above.
    - Grok: 10 of 10 replies held one. In 3, the owner's Grok stop hook made Grok work on in
      the same turn after the block (lint and type checks), so the block was not last and
      missed those checks. They are in the run log, and the evidence lists them among the
      run's other commands (step 7.4).
    - Both are above the plan's 8 of 10, so no second turn is needed.
    - Both agents ran with the owner's own configuration, as Postil runs them. In 2 of
      Claude's runs that configuration had the agent write a note to hippius-mem.
12. **MCP servers in `session/new`: answered by spike S7, 2026-11-27,** from the version 1
    schema at the commit `src/acp/wire.rs` pins (01a54c84):
    - `mcpServers` is required, a list of `McpServer`, which is one of three forms. HTTP and
      SSE carry `"type": "http"` or `"sse"`, and an agent offers them only when its
      `mcpCapabilities` says so. The stdio form carries no `type`, and "all Agents MUST
      support" it: `McpServerStdio` requires `name`, `command` ("absolute path to the MCP
      server executable"), `args` (strings) and `env`, a list of `{name, value}`.
    - It has no working directory: the agent starts the server in the session's `cwd`, the
      run's worktree.
    - The schema names no error for `session/new`, so an agent may answer it with any
      JSON-RPC error, a refused server among them. Postil asks once more with no servers, and
      the run says that memory was not given (section "Agents get it"). An agent that accepts
      the session and drops the server unseen cannot be detected.

## Testing

End to end first, as `docs/design.md` sets out.

- **The fake agent** gains scenarios: a report, a report that claims an unseen check, an
  analysis with one citation that matches, one that moved and one that is wrong, an answer to
  an ask, an agent that edits during a read-only run.
- **Analysis** runs through `Site::respond` against real `git` in a temporary directory:
  analyze, edit a cited file, read the outdated node, update. The forced update gets its
  own cases: a commit that changes cited lines starts one, a commit that touches nothing
  cited moves `checked` with no run, a failed update is not started twice for one commit,
  and an update refused at the cap starts on the next check.
- **Anchors** reuse the notes' property tests; citation checking gets its own (a shifted
  range, a repeated first line, a line past the end of the file).
- **Memory** runs against the real `hippius-mem` binary with a trial vault in a temporary
  home directory when it is installed, and is skipped with a message when it is not. A test
  that only replays a fake MCP server would test our reading of the protocol, not the server.
- **The page's rules** (`ui/diagram.js`, `ui/voice.js`, the walkthrough's ordering, the
  evidence states) get tests beside them under `node --test`.
- **The diagram's clicks** (node to code, chip to node, a click during a streaming analysis)
  run in Playwright's WebKit with the others.
- **Dictation** cannot run in CI: it needs a microphone, a permission prompt and the bundle.
  The rollover and insertion-range rules are separated from the framework calls and tested;
  the rest is checked by hand from `Postil.app`, and the pull request says so.

## Sources

- hippius-mem: https://github.com/thenervelab/hippius-mem/blob/db9261b1/AGENTS.md
- dagre: https://www.npmjs.com/package/@dagrejs/dagre/v/3.1.4
- elkjs: https://www.npmjs.com/package/elkjs/v/0.12.1
- tree-sitter-tags: https://crates.io/crates/tree-sitter-tags/0.27.2
- objc2-speech: https://crates.io/crates/objc2-speech/0.3.2
- objc2-avf-audio: https://crates.io/crates/objc2-avf-audio/0.3.2
