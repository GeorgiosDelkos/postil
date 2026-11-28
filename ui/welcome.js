// Projects and the welcome screen.
//
// One window shows one project. Without one, the welcome screen offers Open Folder, the
// recent projects, and dropping a folder on the window. Opening another project replaces the
// current one in the Rust process; the page then reloads, which resets everything it drew.

import { $, el, icon } from "./dom.js";
import { folderIcon } from "./icons.js";

// `state` is the page's state: the screen reads `projectOpen` and `repo`, and sets
// `switching` while a project opens. `guardDraft(action)` returns true when a draft must be
// saved first (app.js); `leaveFocus()` ends focus mode; `beforeSwitch()` runs as a project
// starts to open; `codeEl` takes the focus when the screen hides.
export function welcomeScreen({ state, welcomeEl, codeEl, guardDraft, leaveFocus, beforeSwitch }) {
  // Checked before the dialog opens too, so a folder is never chosen only to be refused.
  function chooseFolder() {
    if (!guardDraft("opening another project")) window.ipc.postMessage("choose-folder");
  }

  // Asks the Rust process to open `path` (`api/open`), or to make it a git repository first
  // (`api/init`). On success the page reloads into the project. A draft note would be lost by
  // the reload, so the composer keeps the focus instead. A run under way in the open project
  // is stopped only with `{ stop: true }`, sent once the engineer agreed: without it, opening
  // another project during a run answers 409.
  async function requestProject(route, path, { stop = false } = {}) {
    if (guardDraft("opening another project")) return false;
    beforeSwitch();
    state.switching = true;
    let response;
    try {
      response = await fetch(route, { method: "POST", body: JSON.stringify({ path, stop }) });
    } catch (error) {
      state.switching = false;
      showWelcome();
      showWelcomeError(`cannot open ${path}: ${error}`, null);
      return false;
    }
    if (response.ok) {
      window.ipc?.postMessage("maximize");
      location.reload();
      return true;
    }
    state.switching = false;
    showWelcome();
    if (response.status === 409) {
      // WKWebView in wry shows no `confirm()` dialog, so the question is the welcome error.
      const name = path.split("/").filter(Boolean).pop() ?? path;
      showWelcomeError(`A run is in progress. Stop it and open ${name}?`, {
        label: "Stop the run and open",
        run: () => requestProject(route, path, { stop: true }),
      });
      return "conflict";
    }
    // 422: the folder is not in a repository, which `git init` can fix in place.
    const init = { label: "Make it a git repository", run: () => requestProject("api/init", path) };
    showWelcomeError(await response.text(), response.status === 422 ? init : null);
    return false;
  }

  // Shows `text` on the welcome screen, with a button for `action` (`{ label, run }`) or none.
  function showWelcomeError(text, action) {
    $("welcome-error").hidden = false;
    $("welcome-error-text").textContent = text;
    const button = $("welcome-error-action");
    button.hidden = action === null;
    if (action === null) return;
    button.textContent = action.label;
    button.onclick = action.run;
  }

  function showWelcome() {
    leaveFocus();
    document.body.classList.add("welcome-mode");
    welcomeEl.hidden = false;
    const back = $("welcome-back");
    back.hidden = !state.projectOpen;
    if (state.projectOpen) back.textContent = `Back to ${state.repo.name}`;
    loadRecent();
    $("open-folder").focus();
  }

  function hideWelcome() {
    document.body.classList.remove("welcome-mode");
    welcomeEl.hidden = true;
    $("welcome-error").hidden = true;
    codeEl.focus();
  }

  async function loadRecent() {
    const response = await fetch("api/recent");
    const projects = response.ok ? await response.json() : [];
    const recentEl = $("recent");
    if (projects.length === 0) {
      recentEl.replaceChildren(el("div", "recent-empty", "Projects you open appear here."));
      return;
    }
    recentEl.replaceChildren(
      ...projects.map((project) => {
        const row = el("button", "recent-item");
        row.type = "button";
        row.disabled = !project.exists;
        row.append(icon(folderIcon(project.name, false)), el("span", "recent-name", project.name));
        const where = project.exists ? project.path : `${project.path} (moved or deleted)`;
        row.append(el("span", "recent-path", where));
        row.addEventListener("click", () => requestProject("api/open", project.path));
        return row;
      }),
    );
  }

  $("open-folder").addEventListener("click", chooseFolder);
  $("welcome-back").addEventListener("click", hideWelcome);
  $("repo-name").addEventListener("click", showWelcome);

  return {
    show: showWelcome,
    hide: hideWelcome,
    isShown: () => !welcomeEl.hidden,
    loadRecent,
    chooseFolder,
    requestProject,
  };
}
