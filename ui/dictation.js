// Dictation widget attached to Postil's text boxes (docs/review-assist.md, "Dictation").
//
// `insertion` and `sliceDelta` are pure so `ui/dictation.test.js` tests them without a browser.
// One module-level active controller ensures at most one text box records at a time, and
// `startDictationFor(input)` lets hold-to-talk (step 10.3) dictate into whichever box is focused.

import { el, showToast } from "./dom.js";

// Replaces `range` (`{ start, end }`) inside `text` with `partial`, preserving surrounding text
// and returning the updated string and the new active partial range.
export function insertion(text, range, partial) {
  const start = Math.max(0, Math.min(text.length, range.start));
  const end = Math.max(start, Math.min(text.length, range.end));
  const insert = partial ?? "";
  return {
    text: text.slice(0, start) + insert + text.slice(end),
    range: { start, end: start + insert.length },
  };
}

// Given a full cumulative `transcript` from a live dictation session and the `committed` prefix
// already locked in (because the engineer typed or moved the caret mid-partial), returns only
// the newly spoken suffix after `committed`.
export function sliceDelta(transcript, committed) {
  if (!committed) return transcript;
  if (transcript.startsWith(committed)) {
    return transcript.slice(committed.length).replace(/^\s+/, "");
  }
  const cWords = committed.trim().split(/\s+/).filter(Boolean);
  const tWords = transcript.trim().split(/\s+/).filter(Boolean);
  let matched = 0;
  while (
    matched < cWords.length &&
    matched < tWords.length &&
    cWords[matched].toLowerCase() === tWords[matched].toLowerCase()
  ) {
    matched += 1;
  }
  return tWords.slice(matched).join(" ");
}

// Optional callback set by `ui/app.js` returning contextual vocabulary strings (open file name,
// diagram node labels) for `SFSpeechRecognitionRequest.contextualStrings`.
let contextProvider = () => [];

export function setDictationContext(fn) {
  contextProvider = typeof fn === "function" ? fn : () => [];
}

// Registry of attached inputs -> their widget controller, so hold-to-talk can start dictation
// inside whichever text box currently has focus.
const attached = new WeakMap();

// The single widget currently recording (starting, listening, or stopping), or null.
let active = null;

// Starts dictation in `input` if a widget is attached to it. Returns true if started or already
// active on that input.
export function startDictationFor(input) {
  const widget = input ? attached.get(input) : null;
  if (!widget) return false;
  widget.start();
  return true;
}

// Stops dictation if a text box widget is currently recording. Returns true if one was active.
export function stopActiveDictation() {
  if (!active) return false;
  active.stop();
  return true;
}

// Whether a text box widget currently owns the dictation session.
export function isBoxDictating() {
  return active !== null;
}

// Attaches a dictation button + level meter to `input` (a `<textarea>` or text `<input>`).
// Returns `{ button, start, stop, cancel }` so the caller can place `button` in its action bar.
export function attachDictation(input, { onError = showToast } = {}) {
  const button = el("button", "button dictation-btn");
  button.type = "button";
  button.setAttribute("aria-label", "Dictate");
  button.setAttribute("aria-pressed", "false");
  button.title = "Dictate (on-device)";

  const mic = buildMicIcon();
  const meter = el("span", "dictation-meter");
  meter.setAttribute("aria-hidden", "true");
  button.append(mic, meter);

  let state = "idle";
  let range = null;
  let lastTranscript = "";
  let committedPrefix = "";
  let writing = false;
  let composing = false;
  let pendingEvent = null;
  let unsubscribe = null;
  let dialogCleanup = null;
  let observer = null;

  function setVisual(nextState) {
    state = nextState;
    const recording =
      nextState === "starting" || nextState === "listening" || nextState === "stopping";
    button.setAttribute("aria-pressed", String(recording));
    button.classList.toggle("recording", recording);
    button.classList.toggle("stopping", nextState === "stopping");
    if (!recording) meter.style.removeProperty("--level");
  }

  function commitAtCurrentCaret() {
    if (!range) return;
    committedPrefix = lastTranscript;
    const pos = input.selectionStart ?? input.value.length;
    const end = input.selectionEnd ?? pos;
    range = { start: pos, end };
  }

  function applyTranscript(text, isFinal) {
    if (!input.isConnected) {
      cancel();
      return;
    }
    if (composing) {
      pendingEvent = { text, isFinal };
      return;
    }
    lastTranscript = text;
    const delta = sliceDelta(text, committedPrefix);
    if (!range) {
      const pos = input.selectionStart ?? input.value.length;
      const end = input.selectionEnd ?? pos;
      range = { start: pos, end };
    }
    const next = insertion(input.value, range, delta);
    writing = true;
    input.value = next.text;
    input.setSelectionRange(next.range.end, next.range.end);
    input.dispatchEvent(new Event("input", { bubbles: true }));
    writing = false;
    range = next.range;
    if (isFinal) {
      range = null;
      lastTranscript = "";
      committedPrefix = "";
    }
  }

  function cleanup() {
    if (unsubscribe) {
      unsubscribe();
      unsubscribe = null;
    }
    if (dialogCleanup) {
      dialogCleanup();
      dialogCleanup = null;
    }
    if (observer) {
      observer.disconnect();
      observer = null;
    }
    if (active === controller) active = null;
    range = null;
    lastTranscript = "";
    committedPrefix = "";
    pendingEvent = null;
    setVisual("idle");
  }

  function handleEvent(event) {
    if (!input.isConnected) {
      cancel();
      return;
    }
    if (event.type === "transcript") {
      applyTranscript(event.text ?? "", Boolean(event.isFinal));
    } else if (event.type === "level") {
      const clamped = Math.max(0, Math.min(1, Number(event.level) || 0));
      meter.style.setProperty("--level", String(clamped));
    } else if (event.type === "state") {
      if (event.state === "denied" || event.state === "restricted") {
        if (event.message) onError(event.message);
        cleanup();
      } else if (event.state === "idle") {
        cleanup();
      } else {
        setVisual(event.state);
      }
    } else if (event.type === "error") {
      if (event.message) onError(event.message);
      cleanup();
    }
  }

  function start() {
    const bridge = window.postil?.dictation;
    if (!bridge) {
      onError("Dictation is not available.");
      return;
    }
    if (active === controller) return;
    if (active) active.stop();
    if (!input.isConnected) return;

    active = controller;
    input.focus();
    const pos = input.selectionStart ?? input.value.length;
    const end = input.selectionEnd ?? pos;
    range = { start: pos, end };
    lastTranscript = "";
    committedPrefix = "";
    pendingEvent = null;
    setVisual("starting");

    unsubscribe = bridge.subscribe(handleEvent);

    const dialog = input.closest("dialog");
    if (dialog) {
      const onClose = () => cancel();
      dialog.addEventListener("close", onClose, { once: true });
      dialogCleanup = () => dialog.removeEventListener("close", onClose);
    }

    if (typeof MutationObserver !== "undefined" && document.body) {
      observer = new MutationObserver(() => {
        if (!input.isConnected) cancel();
      });
      observer.observe(document.body, { childList: true, subtree: true });
    }

    const context = contextProvider();
    bridge.start(context.length > 0 ? { context } : undefined);
  }

  function stop() {
    if (active !== controller) return;
    if (state === "stopping") return;
    setVisual("stopping");
    window.postil?.dictation?.stop();
  }

  function cancel() {
    if (active !== controller) return;
    // Drop any uncommitted partial when the box is removed or cancelled.
    if (range && range.end > range.start && input.isConnected) {
      const dropped = insertion(input.value, range, "");
      writing = true;
      input.value = dropped.text;
      input.setSelectionRange(dropped.range.start, dropped.range.start);
      input.dispatchEvent(new Event("input", { bubbles: true }));
      writing = false;
    }
    cleanup();
    window.postil?.dictation?.cancel();
  }

  // Engineer types while a partial is shown: commit the partial where it is and continue from
  // the new caret.
  input.addEventListener("input", () => {
    if (writing || active !== controller) return;
    commitAtCurrentCaret();
  });

  // Engineer moves the caret with keys or pointer while a partial is shown: commit where it is
  // and continue at the new caret.
  function checkCaretMoved() {
    if (writing || active !== controller || !range) return;
    const pos = input.selectionStart;
    const end = input.selectionEnd;
    if (pos !== range.end || end !== range.end) {
      commitAtCurrentCaret();
    }
  }
  input.addEventListener("keyup", checkCaretMoved);
  input.addEventListener("mouseup", checkCaretMoved);
  input.addEventListener("select", checkCaretMoved);

  // Input method composition: wait for composition to finish before inserting transcript text.
  input.addEventListener("compositionstart", () => {
    composing = true;
  });
  input.addEventListener("compositionend", () => {
    composing = false;
    if (pendingEvent && active === controller) {
      const next = pendingEvent;
      pendingEvent = null;
      applyTranscript(next.text, next.isFinal);
    }
  });

  button.addEventListener("click", () => {
    if (active === controller) stop();
    else start();
  });

  const controller = { button, start, stop, cancel };
  attached.set(input, controller);
  return controller;
}

function buildMicIcon() {
  if (typeof document.createElementNS !== "function") {
    return el("span", "dictation-icon", "Mic");
  }
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("class", "dictation-icon");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("width", "14");
  svg.setAttribute("height", "14");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "1.4");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  const capsule = document.createElementNS(ns, "rect");
  capsule.setAttribute("x", "5.5");
  capsule.setAttribute("y", "2");
  capsule.setAttribute("width", "5");
  capsule.setAttribute("height", "7.5");
  capsule.setAttribute("rx", "2.5");
  const arc = document.createElementNS(ns, "path");
  arc.setAttribute("d", "M3.5 7.5a4.5 4.5 0 0 0 9 0M8 12v2.2M5.75 14.2h4.5");
  svg.append(capsule, arc);
  return svg;
}
