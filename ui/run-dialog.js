// The run dialog: a run whole, with its plan, its notes, every step, the agent's reply, its
// follow-ups and its review (docs/design.md, "The Run tab"); or one of its steps, or one of
// its review's findings. ui/run.js has the rules. A run's follow-ups are part of its dialog,
// which ends with the box that sends the next one.

import { diffView, reviewSection } from "./dialog-parts.js";
import { actOnSecondClick, el, statusIcon } from "./dom.js";
import { shortPlace } from "./note-place.js";
import { keptQuestion } from "./question-form.js";
import { MAX_LIVE } from "./run-store.js";
import {
  elapsed,
  foldRun,
  iconKind,
  isLive,
  markdownBlocks,
  statusLine,
  statusWord,
  toolRow,
} from "./run.js";
import {
  noteStatus,
  planProgress,
  replyText,
  reviewKey,
  reviewOf,
  threadKey,
  threadOf,
} from "./status.js";

const capital = (word) => word.charAt(0).toUpperCase() + word.slice(1);

// `store` holds the runs (run-store.js); `detailEl` is the `<dialog>`. `noteOf(id)` gives a
// note by id, or null once it is deleted; `openNote(id)` opens its detail; `openFile(path)`
// shows a file in the code pane; `reviewers()` names the settings' reviewers, in order;
// `showChanges(from, to)` shows what runs `from` to `to` changed in the code pane.
export function runDialog({
  store,
  detailEl,
  noteOf,
  openNote,
  openFile,
  reviewers,
  showChanges,
  onError,
}) {
  // What the dialog shows while open: `{ kind: "run", id }` or `{ kind: "step", id, key }`;
  // and what it was drawn from, to draw it again only when that changes.
  let detail = null;
  let detailDrawn = null;
  // The open question's form, kept while it stays open: a redraw must not drop what the
  // engineer has chosen or typed.
  let asking = null;
  // The follow-up box of the thread shown, kept across redraws for the same reason:
  // `{ root, form, update(latest), hasDraft(), hold(action) }`.
  let following = null;
  // What the dialog writes in place while the stamp it was drawn from holds: the text the
  // agent streams, and the time. Each writes what is now known of its run.
  let streamed = [];
  detailEl.addEventListener("close", () => {
    detail = null;
    detailDrawn = null;
    asking = null;
    following = null;
    streamed = [];
  });
  detailEl.addEventListener("keydown", (event) => {
    if (detail?.kind !== "step" || event.metaKey || event.altKey || event.ctrlKey) return;
    if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
      event.preventDefault();
      moveStep(event.key === "ArrowLeft" ? -1 : 1);
    }
  });

  function openDetail(what) {
    detail = what;
    detailDrawn = null;
    drawDetail();
    if (!detailEl.open) {
      detailEl.showModal();
      detailEl.focus();
    }
  }

  // The run of the thread that starts at run `root` under way in this window, a stage of its
  // review included; undefined when none is. One at a time per thread (src/run.rs).
  function liveOf(root) {
    const views = store.knownViews();
    return store.liveRuns().find((run) => {
      if (run.review) return run.review.of === root;
      return (threadOf(run.id, views)[0]?.id ?? run.id) === root;
    });
  }

  // Why a follow-up or a review of the thread that starts at run `root` cannot start now, or
  // "" when it can: one of its runs is under way, or as many runs as a repository may have.
  function busyThread(root) {
    if (liveOf(root)) return "Wait for the run under way, or stop it.";
    if (store.liveRuns().length >= MAX_LIVE) {
      return "Three agents are already working: wait for one to end, or stop one.";
    }
    return "";
  }

  // A run deleted while its dialog is open closes the dialog.
  function closeIfGone(list) {
    if (detail && !list.includes(detail.id)) detailEl.close();
  }

  // Draws the dialog again when what it shows changed, keeping its scroll; otherwise writes
  // only the streamed text in place, while the run streams.
  function drawDetail() {
    if (!detail) return;
    const view = store.byId(detail.id);
    if (!view) return;
    const steps = stepsOf(view);
    let stamp;
    let draw;
    if (detail.kind === "run") {
      // A follow-up opens as its thread, from the run it started from.
      const thread = view.review ? [view] : threadOf(view.id, store.knownViews());
      const root = thread[0] ?? view;
      const notes = root.notes.map((id) => noteLine(id, root));
      const review = root.review ? null : reviewOf(root.id, store.knownViews());
      // A reviewer's run belongs to the thread of the run it reviews, shown there.
      const record = root.review ? null : store.thread(root.id);
      const shown = [notes, reviewKey(review), reviewers(), record ?? null];
      const live = root.review ? (isLive(root) ? root : undefined) : liveOf(root.id);
      const liveKey = `${live?.id}:${live?.status}`;
      stamp = [
        `run:${root.id}`,
        threadKey(thread),
        JSON.stringify(shown),
        liveKey,
        busyThread(root.id),
      ];
      draw = () => runDetail(root, stepsOf(root), notes, review, thread, record);
    } else if (detail.kind === "finding") {
      const review = reviewOf(view.id, store.knownViews());
      const finding = review?.findings.find((item) => item.id === detail.finding);
      if (!finding) return;
      stamp = [`finding:${view.id}:${finding.id}`, JSON.stringify(finding)];
      draw = () => findingDetail(view, finding);
    } else {
      const index = steps.findIndex((step) => step.key === detail.key);
      if (index < 0) return;
      const { item } = steps[index];
      // An answer as JSON, compared by its content: a question's is an object, and a fold
      // made from the whole run again (run.js, `foldRun`) builds a new one.
      const answer = JSON.stringify(item.answer);
      stamp = [
        `${view.id}:${detail.key}`,
        item.status,
        item.output,
        item.text,
        answer,
        isLive(view),
      ];
      draw = () => stepDetail(view, item, index, steps.length);
    }
    if (detailDrawn && stamp.every((part, i) => part === detailDrawn[i])) {
      for (const write of streamed) write();
      return;
    }
    detailDrawn = stamp;
    streamed = [];
    const scroll = detailEl.querySelector(".note-dialog-body")?.scrollTop ?? 0;
    const same = detailEl.dataset.shown === stamp[0];
    detailEl.dataset.shown = stamp[0];
    // The question's form is the same node across redraws; so is the control that had the
    // focus in it, which gets it back.
    const typing = [asking?.form, following?.form].some((form) =>
      form?.contains(document.activeElement),
    )
      ? document.activeElement
      : null;
    detailEl.replaceChildren(...draw());
    typing?.focus();
    const body = detailEl.querySelector(".note-dialog-body");
    if (body && same) body.scrollTop = scroll;
  }

  function moveStep(by) {
    const view = store.byId(detail.id);
    if (!view) return;
    const steps = stepsOf(view);
    const next = steps[steps.findIndex((step) => step.key === detail.key) + by];
    if (next) openDetail({ kind: "step", id: detail.id, key: next.key });
  }

  function detailHeader(icon, title, extra, actions = []) {
    const head = el("header", "note-dialog-header");
    const heading = el("h2", "note-dialog-title", title);
    heading.id = "run-detail-title";
    const close = el("button", "button", "esc");
    close.type = "button";
    close.setAttribute("aria-label", "Close");
    close.addEventListener("click", () => detailEl.close());
    const right = el("span", "note-dialog-actions");
    right.append(...actions, close);
    head.append(...(icon ? [icon] : []), heading, el("span", "note-dialog-where", extra), right);
    return head;
  }

  // A note sent in the run: its status in this run, its place and id.
  function noteLine(id, view) {
    const note = noteOf(id);
    if (!note) return { id, where: "deleted", kind: "stopped", label: "Deleted", tone: "idle" };
    return { id, where: shortPlace(note), ...noteStatus(note, view) };
  }

  // The run whole: its question if it asks one, its plan, its notes, every step, and the
  // agent's reply; then each follow-up in `thread` (the run first), and the box for the next.
  // `record` is the thread's branch and state (`store.thread`).
  function runDetail(view, steps, notes, review, thread, record) {
    const latest = thread.at(-1) ?? view;
    const body = el("div", "note-dialog-body");
    // A reviewer's run says which run it reviews, and leads back to it.
    if (view.review) {
      const { of, reviewer, reviewers: count } = view.review;
      const role =
        count > 1 && reviewer === count
          ? "judges the findings and makes the changes"
          : "proposes findings, read-only";
      const back = el("button", "button", `‹ ${of}`);
      back.type = "button";
      back.addEventListener("click", () => openDetail({ kind: "run", id: of }));
      const nav = el("div", "run-detail-nav");
      nav.append(back, el("span", "", `Reviewer ${reviewer} of ${count} of ${of}: ${role}`));
      body.append(nav);
    }
    const { plan } = foldRun(view.entries);

    // The thread's question, from whichever of its runs is under way: the latest.
    asking = keptQuestion(asking, latest, store.answerQuestion);
    if (asking) body.append(asking.form);

    if (plan?.length) {
      const progress = planProgress(view);
      const rows = el("div", "run-plan-rows");
      for (const step of plan) {
        const row = el("div", `plan-step ${step.status}`);
        const mark = el(
          "span",
          "plan-mark",
          { completed: "✓", in_progress: "●" }[step.status] ?? "○",
        );
        mark.setAttribute("aria-hidden", "true");
        row.append(mark, el("span", "", step.content));
        rows.append(row);
      }
      body.append(section(`Plan · ${progress.done} of ${progress.total}`, rows));
    }

    const noteCards = el("div", "run-cards");
    for (const card of notes) {
      const button = el("button", `run-card ${card.kind}`);
      button.type = "button";
      button.append(
        statusIcon(card.kind),
        el("span", `run-card-text run-note-status tone-${card.tone}`, card.label),
        el("span", "run-note-where", card.where),
        el("span", "note-id", card.id),
      );
      button.disabled = card.label === "Deleted";
      button.addEventListener("click", () => {
        detailEl.close();
        openNote(card.id);
      });
      noteCards.append(button);
    }
    body.append(section(notes.length === 1 ? "Note" : `Notes · ${notes.length}`, noteCards));
    const branch = threadPart(view, record, latest);
    if (branch) body.append(branch);

    if (steps.length) body.append(stepsSection(view, steps));

    const reply = replyPart(view);
    if (reply) body.append(reply);
    if (view.problem) body.append(el("div", "run-notice", view.problem));
    for (const turn of thread.slice(1)) body.append(...followUpPart(turn));
    if (!view.review) {
      body.append(reviewPart(view, review));
      body.append(followUpBox(view, latest));
    }

    const started = new Date(view.startedAt * 1000).toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
    const status = () => {
      const now = store.byId(latest.id) ?? latest;
      const when = isLive(now) ? elapsed(Date.now() / 1000 - now.startedAt) : `started ${started}`;
      return `${statusWord(now)} · ${when}`;
    };
    const title =
      latest.id === view.id
        ? `${view.id} · ${view.agent}`
        : `${view.id} to ${latest.id} · ${view.agent}`;
    const actions = [];
    // Stops the thread's run under way. A second Stop from 3 s after the first ends an agent
    // that did not stop (src/run.rs, "Stopping"); sooner, it is the same request.
    const live = view.review ? (isLive(view) ? view : undefined) : liveOf(view.id);
    if (live) {
      const stopping = live.status === "cancelling";
      const button = el("button", "button stop", stopping ? "Force stop" : "Stop");
      button.type = "button";
      button.title = `Stop ${live.id}`;
      button.addEventListener("click", () => store.stop(live.id));
      actions.push(button);
    }
    // A run logged before snapshots has no changes view.
    if (view.entries.some((entry) => entry.kind === "snapshot")) {
      const button = el("button", "button run-detail-changes", "Changes");
      button.type = "button";
      button.title =
        latest.id === view.id
          ? `What ${view.id} changed`
          : `What ${view.id} to ${latest.id} changed`;
      button.addEventListener("click", () => {
        detailEl.close();
        showChanges(view.id, latest.id);
      });
      actions.push(button);
    }
    const head = detailHeader(statusIcon(iconKind(latest)), title, status(), actions);
    const where = head.querySelector(".note-dialog-where");
    streamed.push(() => {
      where.textContent = status();
    });
    return [head, body];
  }

  // The thread's branch, and what ends the thread (docs/design.md, "Ending a thread"): Apply
  // writes its changes into the working tree, uncommitted; Discard, on a second click, drops
  // them. Either frees its worktree. Null for a run from before threads had worktrees, and
  // while the record is read.
  function threadPart(root, record, latest) {
    if (!record) return null;
    const line = el("div", "run-thread");
    line.append(el("code", "run-thread-branch", record.branch));
    if (record.state !== "open") {
      const ended = record.state === "applied" ? "Applied to your working tree" : "Discarded";
      line.append(el("span", "run-thread-state", ended));
      return section("Branch", line);
    }
    const busy = isLive(store.byId(latest.id) ?? latest);
    const apply = el("button", "button primary", "Apply");
    apply.type = "button";
    apply.title = "Write the thread's changes into your working tree, uncommitted";
    const discard = el("button", "button", "Discard");
    discard.type = "button";
    discard.title = "Drop the thread's changes and free its worktree";
    // Both wait for the answer, so a second click does not ask again.
    const end = (ending) => {
      apply.disabled = true;
      discard.disabled = true;
      store.endThread(root.id, ending).then((ended) => {
        if (ended) return;
        apply.disabled = false;
        discard.disabled = false;
      });
    };
    apply.addEventListener("click", () => end("apply"));
    actOnSecondClick(discard, {
      arm: () => {
        discard.textContent = "Click again to discard";
      },
      disarm: () => {
        discard.textContent = "Discard";
      },
      act: () => end("discard"),
    });
    apply.disabled = busy;
    discard.disabled = busy;
    const actions = el("span", "run-thread-actions");
    actions.append(discard, apply);
    line.append(actions);
    const part = section("Branch", line);
    if (record.skipped?.length) {
      const skipped = `Not given to the worktree: ${record.skipped.join("; ")}`;
      part.append(el("div", "note-dialog-said muted", skipped));
    }
    return part;
  }

  // A run's reply, or null before it writes one: its text is written in place as it streams
  // (`streamed`), and only the reply's own nodes change.
  function replyPart(turn) {
    let text = replyText(turn);
    if (!text) return null;
    const view = el("div", "run-message");
    fillMarkdown(view, text);
    streamed.push(() => {
      const now = replyText(store.byId(turn.id) ?? turn);
      if (now === text) return;
      text = now;
      fillMarkdown(view, text);
    });
    return section(isLive(turn) ? `${turn.agent} is writing` : `${turn.agent}'s reply`, view);
  }

  // One follow-up in its thread's dialog: what the engineer asked, its steps, and the reply.
  function followUpPart(turn) {
    const parts = [];
    const asked = section(`You · ${turn.id}`, el("div", "follow-up-text", turn.followUp.text));
    // The prompt holds more than the words when a fresh agent was told the thread first.
    if (turn.prompt !== turn.followUp.text) {
      asked.append(
        el(
          "div",
          "note-dialog-said muted",
          `Sent to a new ${turn.agent} with the thread so far: the earlier conversation had ended.`,
        ),
      );
    }
    parts.push(asked);
    const steps = stepsOf(turn);
    if (steps.length) parts.push(stepsSection(turn, steps));
    const reply = replyPart(turn);
    if (reply) parts.push(reply);
    if (turn.problem) parts.push(el("div", "run-notice", turn.problem));
    return parts;
  }

  // The cards of `steps`, some of `stepsOf(view)`, each opening its step.
  function stepsSection(view, steps) {
    const cards = el("div", "run-cards");
    for (const step of steps) {
      const open = () => openDetail({ kind: "step", id: view.id, key: step.key });
      cards.append(stepCard(view, step, open));
    }
    return section(`Steps · ${steps.length}`, cards);
  }

  // The box that sends a follow-up to the thread's latest run, `latest`. One node per thread,
  // kept across redraws so a draft and the focus survive the run streaming.
  function followUpBox(root, latest) {
    if (following?.root !== root.id) {
      const form = el("form", "follow-up");
      const input = el("textarea", "field follow-up-input");
      input.rows = 3;
      input.setAttribute("aria-label", "Follow-up");
      const why = el("span", "note-dialog-why");
      const button = el("button", "button primary");
      button.type = "submit";
      const actions = el("div", "follow-up-actions");
      actions.append(why, button);
      form.append(input, actions);
      let target = latest;
      let sending = false;
      // Why the draft kept the page from ending (`hold`), until the engineer types again.
      let held = "";
      const update = (next) => {
        target = next;
        const busy = busyThread(root.id);
        input.placeholder = `Ask ${next.agent} for more, or to change what it did in ${next.id}`;
        button.replaceChildren(`Send to ${next.agent}`, el("kbd", "", "⌘↵"));
        button.disabled = busy !== "" || sending;
        // The held message wins: it says why the page did not end, which the engineer asked for.
        why.textContent = held || busy;
        why.classList.toggle("failed", held !== "");
      };
      const hasDraft = () => input.value.trim() !== "";
      const hold = (action) => {
        held = `Send or clear this follow-up before ${action}.`;
        update(target);
        input.focus();
      };
      input.addEventListener("input", () => {
        if (!held) return;
        held = "";
        update(target);
      });
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        const text = input.value.trim();
        if (!text || button.disabled) return;
        sending = true;
        update(target);
        store
          .post("api/run/follow", { of: target.id, text })
          .then(
            () => {
              input.value = "";
            },
            (error) => onError(error.message),
          )
          .finally(() => {
            sending = false;
            update(target);
          });
      });
      input.addEventListener("keydown", (event) => {
        if (event.key === "Enter" && event.metaKey) {
          event.preventDefault();
          event.stopPropagation();
          form.requestSubmit();
        }
      });
      following = { root: root.id, form, update, hasDraft, hold };
    }
    following.update(latest);
    return section("Follow up", following.form);
  }

  // When the run dialog holds a follow-up being written, says so beside it, focuses it, and
  // returns true: what would end the page (`action`, "quitting") waits for the engineer.
  function holdDraft(action) {
    if (!following?.hasDraft()) return false;
    // A step, a finding or a reviewer's run leaves the box out of the dialog, though it is
    // kept: the thread it belongs to is shown again, with the box.
    if (!following.form.isConnected) openDetail({ kind: "run", id: following.root });
    following.hold(action);
    return true;
  }

  // The run's review: the Review button, a card per reviewer of the latest review, and a
  // card per finding with its verdict (docs/design.md, "Reviews").
  function reviewPart(view, review) {
    const part = reviewSection(review ? `Review · ${review.findings.length} found` : "Review", {
      reviewed: Boolean(review),
      names: reviewers(),
      busy: busyThread(view.id) !== "" || isLive(view),
      start: () => store.startReview(view.id),
      hintClass: "run-detail-text muted",
    });
    if (!review) return part;
    const cards = el("div", "run-cards");
    for (const stage of review.stages) {
      const judges = review.reviewers > 1 && stage.review.reviewer === review.reviewers;
      const button = el("button", "run-card");
      button.type = "button";
      button.append(
        statusIcon(iconKind(stage)),
        el("span", "run-id", stage.id),
        el("span", "run-card-text", `${stage.agent} · ${judges ? "judges and fixes" : "proposes"}`),
        el("span", `run-entry-status tone-${statusLine(stage).tone}`, statusWord(stage)),
        el("span", "run-chevron", "›"),
      );
      button.addEventListener("click", () => openDetail({ kind: "run", id: stage.id }));
      cards.append(button);
    }
    for (const finding of review.findings) {
      const icon =
        { accepted: "addressed", rejected: "stopped", judging: "working" }[finding.verdict.word] ??
        "open";
      const button = el("button", `run-card finding ${finding.verdict.word.replace(" ", "-")}`);
      button.type = "button";
      button.title = finding.text;
      button.append(
        statusIcon(icon),
        el("span", "note-id", finding.id),
        el("span", "run-card-text", finding.text),
        el("span", "run-entry-status", finding.verdict.word),
        el("span", "run-chevron", "›"),
      );
      button.addEventListener("click", () =>
        openDetail({ kind: "finding", id: view.id, finding: finding.id }),
      );
      cards.append(button);
    }
    part.append(cards);
    for (const problem of review.problems)
      part.append(el("div", "run-notice", `A reviewer's findings could not be read: ${problem}`));
    return part;
  }

  // A finding whole: what the reviewer found and proposed, where, and the judge's verdict.
  function findingDetail(view, finding) {
    const body = el("div", "note-dialog-body");
    const back = el("button", "button", `‹ ${view.id}`);
    back.type = "button";
    back.addEventListener("click", () => openDetail({ kind: "run", id: view.id }));
    const nav = el("div", "run-detail-nav");
    const where = [
      finding.note,
      finding.path && (finding.line ? `${finding.path}:${finding.line}` : finding.path),
    ];
    nav.append(
      back,
      el("span", "", [`found by ${finding.from}`, ...where].filter(Boolean).join(" · ")),
    );
    body.append(nav, section("Finding", el("div", "run-detail-text", finding.text)));
    if (finding.fix)
      body.append(section("Proposed fix", el("div", "run-detail-text", finding.fix)));
    const verdict =
      finding.verdict.text ||
      {
        "not judged": "A single reviewer only proposes: no one judged it.",
        waiting: "The judging reviewer has not started yet.",
        judging: "The judging reviewer is at work.",
        "no word": "The judging reviewer ended without a line about it.",
      }[finding.verdict.word] ||
      finding.verdict.word;
    body.append(section("Verdict", el("div", "run-detail-text", verdict)));
    return [
      detailHeader(null, `${finding.id} · ${finding.verdict.word}`, `review of ${view.id}`),
      body,
    ];
  }

  // A step's card: what it did and how it went, in one line. `step` is one of `stepsOf(view)`.
  function stepCard(view, { item, index }, open) {
    const card = (parts) => {
      const button = el("button", "run-card step");
      button.type = "button";
      button.append(...parts, el("span", "run-chevron", "›"));
      button.addEventListener("click", open);
      return button;
    };
    switch (item.type) {
      case "tool": {
        const row = toolRow(item, isLive(view));
        const tone = row.state === "failed" || row.state === "unfinished" ? ` ${row.state}` : "";
        const state = row.running
          ? statusIcon("working")
          : el("span", `tool-state${tone}`, row.state);
        const node = card([
          el("span", "tool-verb", row.verb),
          el("span", "run-card-text tool-target", row.target),
          state,
        ]);
        node.title = item.title;
        return node;
      }
      case "thought": {
        const words = (thought) => thought.text.trim().replace(/\s+/g, " ");
        const preview = el("span", "run-card-text run-thought-preview", words(item));
        // The thought streams: its words change in place, and the card stays the same node.
        streamed.push(() => {
          const now = foldRun((store.byId(view.id) ?? view).entries).items[index];
          if (now?.type === "thought" && preview.textContent !== words(now))
            preview.textContent = words(now);
        });
        return card([el("span", "tool-verb", "think"), preview]);
      }
      case "permission": {
        const chosen = item.options.find((option) => option.id === item.answer);
        // "allow" when it was allowed, by Postil or by hand; "ask" when rejected or closed.
        const verb = chosen?.kind?.startsWith("allow") ? "allow" : "ask";
        return card([
          el("span", "tool-verb", verb),
          el("span", "run-card-text", item.title),
          el("span", "tool-state", chosen ? chosen.name : "cancelled"),
        ]);
      }
      case "question":
        return card([
          el("span", "tool-verb", "ask"),
          el("span", "run-card-text", item.message),
          el("span", "tool-state", questionState(item)),
        ]);
      case "error":
        return el("div", "run-error", item.text);
      default:
        return el("div", "run-notice", item.text);
    }
  }

  function stepDetail(view, item, index, count) {
    const body = el("div", "note-dialog-body");
    const nav = el("div", "run-detail-nav");
    const back = el("button", "button", `‹ ${view.id}`);
    back.type = "button";
    back.setAttribute("aria-label", `Back to ${view.id}`);
    back.addEventListener("click", () => openDetail({ kind: "run", id: view.id }));
    const previous = el("button", "button", "‹");
    previous.type = "button";
    previous.setAttribute("aria-label", "The step before");
    previous.disabled = index === 0;
    previous.addEventListener("click", () => moveStep(-1));
    const following = el("button", "button", "›");
    following.type = "button";
    following.setAttribute("aria-label", "The step after");
    following.disabled = index === count - 1;
    following.addEventListener("click", () => moveStep(1));
    nav.append(
      back,
      el("span", "", `Step ${index + 1} of ${count} · ← → to move`),
      previous,
      following,
    );
    body.append(nav);

    if (item.type === "thought") {
      body.append(el("div", "run-detail-text", item.text.trim()));
      return [detailHeader(null, "Thinking", view.id), body];
    }
    if (item.type === "permission") {
      const chosen = item.options.find((option) => option.id === item.answer);
      body.append(el("div", "run-detail-text", `Answered: ${chosen ? chosen.name : "cancelled"}`));
      return [detailHeader(null, item.title, view.id), body];
    }
    if (item.type === "question") {
      body.append(el("div", "run-detail-text", item.message));
      const content = item.answer?.content;
      const answered = content ? Object.entries(content) : [];
      if (answered.length) {
        const list = el("dl", "question-answers");
        for (const [key, value] of answered) {
          list.append(
            el("dt", "", key),
            el("dd", "", Array.isArray(value) ? value.join(", ") : String(value)),
          );
        }
        body.append(section("Your answer", list));
      } else body.append(el("div", "run-detail-text muted", questionState(item)));
      return [detailHeader(null, "Question", `${view.id} · ${questionState(item)}`), body];
    }
    const row = toolRow(item, isLive(view));
    const kind = item.status === "failed" ? "failed" : row.running ? "working" : "addressed";
    const actions = [];
    const path = item.places[0]?.path ?? item.output.find((part) => part.type === "diff")?.path;
    if (path) {
      const show = el("button", "button", "Open file");
      show.type = "button";
      show.addEventListener("click", () => {
        detailEl.close();
        openFile(path);
      });
      actions.push(show);
    }
    // A command's title is the command; a file tool's only repeats the header.
    const onFile = ["read", "edit", "delete", "move"].includes(item.tool);
    if (!onFile) body.append(el("pre", "run-detail-command", item.title));
    for (const part of item.output) {
      if (part.type === "diff") body.append(section(part.path, diffView(part)));
      else body.append(el("pre", "tool-output", part.text));
    }
    if (item.output.length === 0) {
      body.append(el("div", "run-detail-text muted", row.running ? "Under way…" : "No output."));
    }
    return [
      detailHeader(statusIcon(kind), `${capital(row.verb)} ${row.target}`, row.state, actions),
      body,
    ];
  }

  return { open: openDetail, draw: drawDetail, closeIfGone, holdDraft, busyThread };
}

// The run's steps in order, each with its index among the run's items and its key: every
// item but the agent's messages and, while the run is live, an open request, which shows
// above the runs. Once the run is over a request left open is a step too, shown as closed.
function stepsOf(view) {
  const live = isLive(view);
  return foldRun(view.entries)
    .items.map((item, index) => ({ item, index, key: itemKey(item, index) }))
    .filter(
      ({ item }) =>
        item.type !== "message" &&
        !(
          live &&
          (item.type === "permission" || item.type === "question") &&
          item.answer === undefined
        ),
    );
}

function itemKey(item, index) {
  if (item.type === "tool") return `tool:${item.id}`;
  if (item.type === "permission") return `permission:${item.request}`;
  return `${item.type}:${index}`;
}

function section(label, ...content) {
  const part = el("section", "note-dialog-section");
  part.append(el("div", "note-dialog-label", label), ...content);
  return part;
}

// How a closed question ended: answered, skipped, or closed with no answer.
function questionState(item) {
  if (item.answer === null) return "closed";
  return item.answer?.content ? "answered" : "skipped";
}

// Fills `view` with the agent's Markdown as elements, built node by node: its text is never
// parsed as HTML.
function fillMarkdown(view, text) {
  view.replaceChildren();
  for (const block of markdownBlocks(text.trim())) {
    if (block.type === "code") view.append(el("pre", "run-code", block.text));
    else if (block.type === "list") {
      const list = el(block.ordered ? "ol" : "ul");
      for (const item of block.items) list.append(inlineView("li", item));
      view.append(list);
    } else view.append(inlineView(block.type === "heading" ? "h4" : "p", block.inline));
  }
}

function inlineView(tag, parts) {
  const node = el(tag);
  for (const part of parts) {
    const wrap = { code: "code", strong: "strong", em: "em" }[part.type];
    node.append(wrap ? el(wrap, "", part.text) : part.text);
  }
  return node;
}
