// The multi-repo Agentic Developer Hub view:
// Left column: monitored repositories + hippius-mem storage pill (Local Vault vs Remote S3)
// Center column: priority-ranked Issues & Pull Requests across selected repos
// Right column: task evaluation, hippius-mem memories, affected architecture, lane selector,
//               live tier-resolved stage roster, and launch button.

import { el } from "./dom.js";
import {
  LANES,
  filterHubItems,
  formatAge,
  groupByPriority,
  itemKey,
  laneLabel,
  laneSummary,
  memoryBackendSummary,
} from "./hub.js";

const POLL_SYNC_MS = 1000;

export function hubView({
  hubEl,
  getSettingsView,
  openSettings,
  openRepo,
  chooseFolderForHub,
  launchLane,
  onError,
}) {
  const ui = {
    shown: false,
    data: null,
    recent: [],
    kind: "all",
    priority: "all",
    query: "",
    selectedKey: null,
    chosenLane: null,
    humanGate: true,
    stagePreview: null,
    stagePreviewKey: null,
    stagePreviewError: null,
    evaluating: false,
    launching: false,
    pollTimer: null,
  };

  async function fetchJson(route, init) {
    const response = await fetch(route, init);
    const text = await response.text();
    if (!response.ok) throw new Error(text);
    return JSON.parse(text);
  }

  async function load({ sync = false } = {}) {
    try {
      const [hub, recent] = await Promise.all([
        fetchJson(sync ? "api/hub/sync" : "api/hub", sync ? { method: "POST" } : undefined),
        fetchJson("api/recent").catch(() => []),
      ]);
      ui.data = hub;
      ui.recent = recent;
      syncPoll();
      render();
    } catch (error) {
      onError(String(error.message ?? error));
    }
  }

  function syncPoll() {
    clearTimeout(ui.pollTimer);
    if (ui.shown && ui.data?.syncing) {
      ui.pollTimer = setTimeout(() => {
        void load();
      }, POLL_SYNC_MS);
    }
  }

  async function mutateRepo(payload) {
    try {
      ui.data = await fetchJson("api/hub/repos", {
        method: "POST",
        body: JSON.stringify(payload),
      });
      syncPoll();
      render();
    } catch (error) {
      onError(String(error.message ?? error));
    }
  }

  function visibleItems() {
    const all = ui.data?.items ?? [];
    return filterHubItems(all, { kind: ui.kind, priority: ui.priority, query: ui.query });
  }

  function activeItem(items) {
    if (items.length === 0) return null;
    return items.find((item) => itemKey(item) === ui.selectedKey) ?? items[0];
  }

  async function fetchStagePreview(lane, kind) {
    const key = `${lane}:${kind}`;
    if (ui.stagePreviewKey === key && (ui.stagePreview || ui.stagePreviewError)) return;
    ui.stagePreviewKey = key;
    ui.stagePreview = null;
    ui.stagePreviewError = null;
    try {
      const reply = await fetchJson(
        `api/lanes/preview?lane=${encodeURIComponent(lane)}&kind=${encodeURIComponent(kind)}`,
      );
      if (ui.stagePreviewKey !== key) return;
      ui.stagePreview = reply;
      render();
    } catch (error) {
      if (ui.stagePreviewKey !== key) return;
      ui.stagePreviewError = String(error.message ?? error);
      render();
    }
  }

  async function reEvaluate(item) {
    if (ui.evaluating) return;
    ui.evaluating = true;
    render();
    try {
      const updated = await fetchJson("api/hub/evaluate", {
        method: "POST",
        body: JSON.stringify({
          repo: item.repoPath,
          kind: item.kind,
          number: item.number,
        }),
      });
      if (ui.data?.items) {
        const idx = ui.data.items.findIndex((each) => itemKey(each) === itemKey(updated));
        if (idx >= 0) ui.data.items[idx] = updated;
      }
      ui.chosenLane = updated.recommendedLane;
    } catch (error) {
      onError(String(error.message ?? error));
    } finally {
      ui.evaluating = false;
      render();
    }
  }

  async function startSelected(item, lane) {
    if (ui.launching) return;
    ui.launching = true;
    render();
    try {
      await launchLane({
        repoPath: item.repoPath,
        lane,
        kind: item.kind,
        number: item.number,
        title: item.title,
        body: item.body,
        url: item.url,
        labels: item.labels,
        baseBranch: item.baseRef,
        headBranch: item.headRef,
        humanGate: lane === "hard" ? ui.humanGate : false,
        memories: item.memories ?? [],
      });
    } catch (error) {
      onError(String(error.message ?? error));
    } finally {
      ui.launching = false;
      render();
    }
  }

  function render() {
    if (!ui.shown) return;
    const settingsView = getSettingsView?.() ?? null;
    const items = visibleItems();
    const selected = activeItem(items);
    if (selected) {
      const key = itemKey(selected);
      if (ui.selectedKey !== key) {
        ui.selectedKey = key;
        ui.chosenLane = selected.recommendedLane;
        ui.humanGate = settingsView?.settings?.lanes?.hardHumanGate ?? true;
      }
      const lane = ui.chosenLane ?? selected.recommendedLane;
      void fetchStagePreview(lane, selected.kind);
    }

    const shell = el("div", "hub-shell");
    shell.append(renderReposPane(settingsView), renderQueuePane(items, selected), renderInspector(selected));
    hubEl.replaceChildren(shell);
  }

  function renderReposPane(settingsView) {
    const pane = el("aside", "pane hub-repos");
    const header = el("header", "pane-header");
    header.dataset.drag = "";
    const title = el("span", "hub-pane-title", "Monitored Repos");
    const addBtn = el("button", "button hub-mini-btn", "+ Monitor…");
    addBtn.type = "button";
    addBtn.addEventListener("click", () => chooseFolderForHub());
    header.append(title, addBtn);

    const list = el("div", "hub-repo-list");
    const repos = ui.data?.repos ?? [];
    if (repos.length > 0) {
      const allSelected = repos.every((r) => r.selected);
      const allRow = el("label", "hub-repo-all");
      const cb = el("input");
      cb.type = "checkbox";
      cb.checked = allSelected;
      cb.addEventListener("change", () =>
        mutateRepo({ action: "selectAll", selected: cb.checked }),
      );
      allRow.append(cb, el("span", "", `All monitored (${repos.length})`));
      list.append(allRow);

      for (const repo of repos) {
        const card = el("div", `hub-repo-card${repo.selected ? " selected" : ""}`);
        const top = el("div", "hub-repo-top");
        const toggle = el("input");
        toggle.type = "checkbox";
        toggle.checked = repo.selected;
        toggle.title = "Include in priority queue";
        toggle.addEventListener("change", () =>
          mutateRepo({ action: "select", path: repo.path, selected: toggle.checked }),
        );
        const nameBtn = el("button", "hub-repo-name", repo.name);
        nameBtn.type = "button";
        nameBtn.title = `Open ${repo.path} in code workspace`;
        nameBtn.disabled = !repo.exists;
        nameBtn.addEventListener("click", () => openRepo(repo.path));

        const removeBtn = el("button", "icon-button hub-repo-remove", "×");
        removeBtn.type = "button";
        removeBtn.title = "Stop monitoring";
        removeBtn.addEventListener("click", () =>
          mutateRepo({ action: "remove", path: repo.path }),
        );
        top.append(toggle, nameBtn, removeBtn);

        const meta = el("div", "hub-repo-meta");
        const branch = repo.branch || (repo.exists ? "detached" : "missing");
        const counts = `${repo.openIssues} iss · ${repo.openPrs} PR`;
        meta.textContent = repo.syncing ? `${branch} · syncing…` : `${branch} · ${counts}`;
        card.append(top, meta);
        if (repo.error) {
          card.append(el("div", "hub-repo-error", repo.error));
        }
        list.append(card);
      }
    } else {
      const empty = el("div", "hub-empty-block");
      empty.append(
        el("p", "hub-empty-title", "No repositories monitored yet"),
        el(
          "p",
          "hub-empty-hint",
          "Add local git repositories to monitor open GitHub issues & PRs ranked by priority.",
        ),
      );
      list.append(empty);
    }

    // Quick-add from recent projects not yet monitored
    const monitoredPaths = new Set(repos.map((r) => r.path));
    const suggestions = ui.recent.filter((p) => p.exists && !monitoredPaths.has(p.path)).slice(0, 5);
    if (suggestions.length > 0) {
      const sec = el("div", "hub-recent-add");
      sec.append(el("div", "hub-subhead", "Add from Recent"));
      for (const proj of suggestions) {
        const row = el("button", "hub-recent-btn");
        row.type = "button";
        row.append(el("span", "", `+ ${proj.name}`), el("span", "hub-recent-sub", proj.path));
        row.addEventListener("click", () => mutateRepo({ action: "add", path: proj.path }));
        sec.append(row);
      }
      list.append(sec);
    }

    // hippius-mem storage badge at footer
    const foot = el("footer", "hub-repos-foot");
    const mem = memoryBackendSummary(settingsView?.memory);
    const memBtn = el("button", `hub-mem-pill ${mem.kind}`);
    memBtn.type = "button";
    memBtn.title = "Configure hippius-mem (Local Vault or Remote S3) in Settings";
    memBtn.append(
      el("span", "hub-mem-dot"),
      el("span", "hub-mem-title", mem.title),
      el("span", "hub-mem-detail", mem.detail),
    );
    memBtn.addEventListener("click", () => openSettings());
    foot.append(memBtn);

    pane.append(header, list, foot);
    return pane;
  }

  function renderQueuePane(items, selected) {
    const pane = el("section", "pane hub-queue");
    const header = el("header", "pane-header hub-queue-header");
    header.dataset.drag = "";

    const tabs = el("div", "hub-filter-tabs");
    for (const [value, label] of [
      ["all", "All"],
      ["issue", "Issues"],
      ["pullRequest", "PRs"],
    ]) {
      const btn = el("button", `hub-filter-tab${ui.kind === value ? " active" : ""}`, label);
      btn.type = "button";
      btn.addEventListener("click", () => {
        ui.kind = value;
        render();
      });
      tabs.append(btn);
    }

    const controls = el("div", "hub-queue-controls");
    const prio = el("select", "select hub-prio-select");
    prio.setAttribute("aria-label", "Filter by priority");
    for (const [val, label] of [
      ["all", "All priorities"],
      ["P0", "P0 Critical"],
      ["P1", "P1 High"],
      ["P2", "P2 Medium"],
      ["P3", "P3 Low"],
    ]) {
      const opt = el("option", "", label);
      opt.value = val;
      opt.selected = ui.priority === val;
      prio.append(opt);
    }
    prio.addEventListener("change", () => {
      ui.priority = prio.value;
      render();
    });

    const syncBtn = el(
      "button",
      "button hub-mini-btn",
      ui.data?.syncing ? "Syncing…" : "↻ Sync",
    );
    syncBtn.type = "button";
    syncBtn.disabled = Boolean(ui.data?.syncing) || (ui.data?.repos?.length ?? 0) === 0;
    syncBtn.addEventListener("click", () => void load({ sync: true }));
    controls.append(prio, syncBtn);
    header.append(tabs, controls);

    const searchWrap = el("div", "hub-search-wrap");
    const search = el("input", "field hub-search");
    search.type = "text";
    search.placeholder = "Filter issues & PRs by repo, #number, label, or keyword…";
    search.value = ui.query;
    search.addEventListener("input", () => {
      ui.query = search.value;
      render();
    });
    searchWrap.append(search);

    const body = el("div", "hub-queue-list");
    if (ui.data && !ui.data.ghAvailable) {
      body.append(
        el(
          "div",
          "hub-banner",
          "GitHub CLI (`gh`) was not found on PATH. Install `gh` and run `gh auth login` to sync open issues and pull requests.",
        ),
      );
    }

    const groups = groupByPriority(items);
    if (groups.length === 0) {
      const msg =
        (ui.data?.repos?.length ?? 0) === 0
          ? "Monitor a repository on the left to load prioritized issues and PRs."
          : ui.data?.syncing
            ? "Fetching open issues & PRs and recalling team memory…"
            : "No open issues or pull requests match the current filters.";
      body.append(el("div", "empty", msg));
    } else {
      for (const group of groups) {
        const heading = el("div", `hub-group-head tone-${group.tone}`, group.label);
        body.append(heading);
        for (const item of group.items) {
          const isSelected = selected && itemKey(item) === itemKey(selected);
          const card = el("button", `hub-card${isSelected ? " selected" : ""}`);
          card.type = "button";
          card.addEventListener("click", () => {
            ui.selectedKey = itemKey(item);
            ui.chosenLane = item.recommendedLane;
            ui.stagePreviewKey = null;
            render();
          });

          const row1 = el("div", "hub-card-top");
          const badge = el("span", `hub-prio-badge tone-${group.tone}`, item.priority);
          const repoTag = el("span", "hub-repo-pill", item.repoName);
          const kindTag = el(
            "span",
            "hub-kind-pill",
            item.kind === "pullRequest" ? `PR #${item.number}` : `Issue #${item.number}`,
          );
          const laneChip = el(
            "span",
            `hub-lane-chip lane-${item.recommendedLane}`,
            `${laneLabel(item.recommendedLane)} Lane`,
          );
          row1.append(badge, repoTag, kindTag, laneChip);

          const titleEl = el("div", "hub-card-title", item.title);
          const reasonEl = el("div", "hub-card-reason", item.priorityReason);

          const row3 = el("div", "hub-card-foot");
          const parts = [];
          if (item.kind === "pullRequest") {
            parts.push(`+${item.additions} -${item.deletions} (${item.changedFiles}f)`);
            if (item.checksState) parts.push(`CI: ${item.checksState}`);
          }
          if (item.memories?.length) {
            parts.push(`${item.memories.length} memor${item.memories.length === 1 ? "y" : "ies"}`);
          }
          if (item.affectedComponents?.length) {
            parts.push(`Map: ${item.affectedComponents.slice(0, 2).join(", ")}`);
          }
          const age = formatAge(item.updatedAt);
          if (age) parts.push(age);
          row3.textContent = parts.join(" · ");

          card.append(row1, titleEl, reasonEl, row3);
          body.append(card);
        }
      }
    }

    pane.append(header, searchWrap, body);
    return pane;
  }

  function renderInspector(item) {
    const pane = el("aside", "pane hub-inspector");
    const header = el("header", "pane-header");
    header.dataset.drag = "";
    header.append(el("span", "hub-pane-title", "Evaluation & Lane Launch"));

    const body = el("div", "hub-inspector-body");
    if (!item) {
      body.append(el("div", "empty", "Select an issue or PR from the queue to inspect and launch."));
      pane.append(header, body);
      return pane;
    }

    const kindWord = item.kind === "pullRequest" ? "Pull Request" : "Issue";
    const metaLine = el(
      "div",
      "hub-insp-meta",
      `${item.repoName} · ${kindWord} #${item.number} · Score ${item.priorityScore}/100`,
    );
    const titleEl = el("h2", "hub-insp-title", item.title);

    const signals = el("div", "hub-insp-section");
    signals.append(
      el("div", "hub-subhead", "Priority & Evaluation"),
      el("p", "hub-insp-text", item.priorityReason),
      el("p", "hub-insp-hint", item.laneReason),
    );
    const evalBtn = el(
      "button",
      "button hub-mini-btn",
      ui.evaluating ? "Re-evaluating…" : "Re-evaluate with hippius-mem",
    );
    evalBtn.type = "button";
    evalBtn.disabled = ui.evaluating;
    evalBtn.addEventListener("click", () => void reEvaluate(item));
    signals.append(evalBtn);

    body.append(metaLine, titleEl, signals);

    // Team memory hits
    if (item.memories?.length) {
      const memSec = el("div", "hub-insp-section");
      memSec.append(el("div", "hub-subhead", `Team Memory (${item.memories.length})`));
      for (const hit of item.memories) {
        const card = el("div", "hub-mem-hit");
        card.append(el("span", "hub-mem-id", hit.id), el("span", "", hit.summary));
        memSec.append(card);
      }
      body.append(memSec);
    }

    // Affected architecture components
    if (item.affectedComponents?.length) {
      const mapSec = el("div", "hub-insp-section");
      mapSec.append(el("div", "hub-subhead", "Affected Architecture (Map)"));
      const chips = el("div", "hub-chip-row");
      for (const comp of item.affectedComponents) {
        chips.append(el("span", "hub-map-chip", comp));
      }
      mapSec.append(chips);
      body.append(mapSec);
    }

    // Description preview
    if (item.body?.trim()) {
      const desc = el("div", "hub-insp-section");
      desc.append(
        el("div", "hub-subhead", "Description"),
        el("pre", "hub-insp-desc", item.body.trim().slice(0, 1200)),
      );
      body.append(desc);
    }

    // Lane selector
    const lane = ui.chosenLane ?? item.recommendedLane;
    const laneSec = el("div", "hub-insp-section");
    laneSec.append(el("div", "hub-subhead", "Execution Lane"));
    const laneGroup = el("div", "hub-lane-picker");
    for (const candidate of LANES) {
      const active = lane === candidate;
      const rec = item.recommendedLane === candidate;
      const opt = el("button", `hub-lane-opt${active ? " active" : ""}`);
      opt.type = "button";
      opt.addEventListener("click", () => {
        ui.chosenLane = candidate;
        ui.stagePreviewKey = null;
        render();
      });
      const head = el("div", "hub-lane-opt-head");
      head.append(el("span", "", `${laneLabel(candidate)} Lane`));
      if (rec) head.append(el("span", "hub-rec-badge", "Recommended"));
      opt.append(head, el("div", "hub-lane-opt-desc", laneSummary(candidate, item.kind)));
      laneGroup.append(opt);
    }
    laneSec.append(laneGroup);

    // Stage Roster preview
    const roster = el("div", "hub-stage-roster");
    if (ui.stagePreviewError) {
      const err = el("div", "hub-roster-error", ui.stagePreviewError);
      const cfg = el("button", "button hub-mini-btn", "Configure Tiers in Settings");
      cfg.type = "button";
      cfg.addEventListener("click", () => openSettings());
      roster.append(err, cfg);
    } else if (ui.stagePreview?.stages) {
      roster.append(el("div", "hub-subhead", "Stage Roster (from Capability Tiers)"));
      for (const [idx, st] of ui.stagePreview.stages.entries()) {
        const row = el("div", "hub-stage-row");
        const who = st.model ? `${st.agent} · ${st.model}` : st.agent;
        const mode = st.readOnly ? "RO" : "RW";
        row.append(
          el("span", "hub-stage-num", `${idx + 1}.`),
          el("span", "hub-stage-name", st.label),
          el("span", "hub-stage-model", `${who} (${st.tier}, ${mode})${st.diverse ? " ✦" : ""}`),
        );
        roster.append(row);
      }
    } else {
      roster.append(el("div", "hub-insp-hint", "Resolving capability tiers…"));
    }
    laneSec.append(roster);

    if (lane === "hard" && item.kind === "issue") {
      const gateRow = el("label", "hub-gate-toggle");
      const gateCb = el("input");
      gateCb.type = "checkbox";
      gateCb.checked = ui.humanGate;
      gateCb.addEventListener("change", () => {
        ui.humanGate = gateCb.checked;
      });
      gateRow.append(gateCb, el("span", "", "Pause after Stage 2 (Plan Judge) for my approval"));
      laneSec.append(gateRow);
    }
    body.append(laneSec);

    // Footer launch bar
    const foot = el("footer", "hub-insp-foot");
    const openBtn = el("button", "button", "Inspect Repo");
    openBtn.type = "button";
    openBtn.addEventListener("click", () => openRepo(item.repoPath));

    const verb = item.kind === "pullRequest" ? "Review PR" : "Implement";
    const startBtn = el(
      "button",
      "button primary",
      ui.launching ? "Starting…" : `${verb} in ${laneLabel(lane)} Lane →`,
    );
    startBtn.type = "button";
    startBtn.disabled = ui.launching || Boolean(ui.stagePreviewError);
    startBtn.addEventListener("click", () => void startSelected(item, lane));
    foot.append(openBtn, startBtn);

    pane.append(header, body, foot);
    return pane;
  }

  return {
    show() {
      ui.shown = true;
      document.body.classList.add("hub-mode");
      hubEl.hidden = false;
      render();
      void load();
    },
    hide() {
      ui.shown = false;
      clearTimeout(ui.pollTimer);
      document.body.classList.remove("hub-mode");
      hubEl.hidden = true;
    },
    isShown: () => ui.shown,
    refresh: () => {
      if (ui.shown) void load();
    },
    async addMonitoredRepo(path) {
      await mutateRepo({ action: "add", path });
    },
    invalidateStages() {
      ui.stagePreviewKey = null;
      if (ui.shown) render();
    },
  };
}
