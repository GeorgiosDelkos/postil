// The multi-repo Agentic Developer Hub view:
// Left column: monitored repositories + hippius-mem storage pill (Local Vault vs Remote S3)
// Center column: priority-ranked Issues & Pull Requests across selected repos
// Right column: task evaluation, hippius-mem memories, affected architecture, lane selector,
//               live tier-resolved stage roster, and launch button.
//
// Builds the 3-pane shell and search input once so typing, scrolling, and background sync polls
// never drop focus, caret position, or scroll offsets.

import { agentLogoIcon, el, postilCheckbox, uiIcon } from "./dom.js";
import {
  HUB_PANE_DEFAULTS,
  HUB_PANE_LIMITS,
  LANES,
  PRIORITY_META,
  applyStageOverride,
  dragHubPane,
  filterHubItems,
  fitHubPanes,
  formatAge,
  groupByPriority,
  itemKey,
  laneLabel,
  laneSummary,
  memoryBackendSummary,
  resetHubPane,
  stepHubPane,
} from "./hub.js";
import { BIG_STEP, STEP } from "./panes.js";
import { modelReports, modelSelect, stateMark, toggleSwitch } from "./settings-controls.js";

const POLL_SYNC_MS = 1000;
const HUB_LAYOUT_KEY = "postil.hub.layout";

function loadHubLayout() {
  try {
    const raw = localStorage.getItem(HUB_LAYOUT_KEY);
    if (!raw) return { ...HUB_PANE_DEFAULTS };
    const parsed = JSON.parse(raw);
    const repos = Number(parsed?.repos);
    const inspector = Number(parsed?.inspector);
    return {
      repos: Number.isFinite(repos) ? repos : HUB_PANE_DEFAULTS.repos,
      inspector: Number.isFinite(inspector) ? inspector : HUB_PANE_DEFAULTS.inspector,
    };
  } catch {
    return { ...HUB_PANE_DEFAULTS };
  }
}

function saveHubLayout(layout) {
  try {
    localStorage.setItem(HUB_LAYOUT_KEY, JSON.stringify(layout));
  } catch {
    // Ignore storage quota / privacy mode errors.
  }
}

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
    baseLane: null,
    customStages: null,
    humanGate: true,
    stagePreview: null,
    stagePreviewKey: null,
    stagePreviewError: null,
    evaluating: false,
    launching: false,
    pollTimer: null,
    loadSeq: 0,
    layout: loadHubLayout(),
    dragging: null,
    dom: null,
  };

  const models = modelReports({
    onChange: () => {
      if (ui.shown) render();
    },
    onError: () => {},
  });

  async function fetchJson(route, init) {
    const response = await fetch(route, init);
    const text = await response.text();
    if (!response.ok) throw new Error(text);
    return JSON.parse(text);
  }

  async function load({ sync = false } = {}) {
    const seq = ++ui.loadSeq;
    try {
      const [hub, recent] = await Promise.all([
        fetchJson(sync ? "api/hub/sync" : "api/hub", sync ? { method: "POST" } : undefined),
        fetchJson("api/recent").catch(() => []),
      ]);
      if (seq !== ui.loadSeq) return;
      ui.data = hub;
      ui.recent = recent;
      syncPoll();
      render();
    } catch (error) {
      if (seq !== ui.loadSeq) return;
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
    const seq = ++ui.loadSeq;
    try {
      const next = await fetchJson("api/hub/repos", {
        method: "POST",
        body: JSON.stringify(payload),
      });
      if (seq !== ui.loadSeq) return;
      ui.data = next;
      syncPoll();
      render();
    } catch (error) {
      if (seq !== ui.loadSeq) return;
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

  function selectItem(item) {
    const settingsView = getSettingsView?.() ?? null;
    ui.selectedKey = itemKey(item);
    ui.chosenLane = item.recommendedLane;
    ui.baseLane = item.recommendedLane;
    ui.customStages = null;
    ui.humanGate = settingsView?.settings?.lanes?.hardHumanGate ?? true;
    ui.stagePreviewKey = null;
    ui.stagePreview = null;
    ui.stagePreviewError = null;
    render();
  }

  async function fetchStagePreview(lane, kind) {
    const previewLane = lane === "custom" ? (ui.baseLane ?? "middle") : lane;
    const key = `${previewLane}:${kind}`;
    if (ui.stagePreviewKey === key && (ui.stagePreview || ui.stagePreviewError)) return;
    ui.stagePreviewKey = key;
    ui.stagePreview = null;
    ui.stagePreviewError = null;
    try {
      const reply = await fetchJson(
        `api/lanes/preview?lane=${encodeURIComponent(previewLane)}&kind=${encodeURIComponent(kind)}`,
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

  function activeStages() {
    if (ui.chosenLane === "custom" && ui.customStages) return ui.customStages;
    return ui.stagePreview?.stages ?? null;
  }

  function editStage(index, patch) {
    const current = activeStages();
    if (!current || index < 0 || index >= current.length) return;
    if (ui.chosenLane !== "custom") {
      ui.baseLane = ui.chosenLane ?? "middle";
    }
    ui.customStages = applyStageOverride(current, index, patch);
    ui.chosenLane = "custom";
    render();
  }

  async function reEvaluate(item) {
    if (ui.evaluating) return;
    const targetKey = itemKey(item);
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
      if (ui.selectedKey === targetKey) {
        ui.chosenLane = updated.recommendedLane;
        ui.baseLane = updated.recommendedLane;
        ui.customStages = null;
        ui.stagePreviewKey = null;
      }
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
      const stages = activeStages();
      const hasPlanJudge =
        lane === "hard" || (lane === "custom" && Boolean(stages?.some((s) => s.kind === "planJudge")));
      const payload = {
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
        humanGate: hasPlanJudge ? ui.humanGate : false,
        memories: item.memories ?? [],
      };
      if (lane === "custom") {
        payload.baseLane = ui.baseLane ?? "middle";
        if (ui.customStages?.length) {
          payload.stageOverrides = ui.customStages.map((st) => ({
            agent: st.agent,
            model: st.model ?? null,
          }));
        }
      }
      await launchLane(payload);
    } catch (error) {
      onError(String(error.message ?? error));
    } finally {
      ui.launching = false;
      render();
    }
  }

  function ensureShell() {
    if (ui.dom) return ui.dom;

    const shell = el("div", "hub-shell");

    // Left pane: Monitored Repos
    const reposPane = el("aside", "pane hub-repos");
    reposPane.id = "hub-repos";
    const reposHeader = el("header", "pane-header");
    reposHeader.dataset.drag = "";
    const addBtn = el("button", "button hub-mini-btn hub-icon-btn");
    addBtn.type = "button";
    addBtn.title = "Add repository to monitoring";
    addBtn.append(uiIcon("plus"), el("span", "", "Add"));
    addBtn.addEventListener("click", () => chooseFolderForHub());
    reposHeader.append(el("span", "hub-pane-title", "Monitored Repos"), addBtn);
    const repoList = el("div", "hub-repo-list");
    const reposFoot = el("footer", "hub-repos-foot");
    reposPane.append(reposHeader, repoList, reposFoot);

    // Center pane: Priority Queue (static header + search input so typing never loses focus)
    const queuePane = el("section", "pane hub-queue");
    queuePane.id = "hub-queue";
    const queueHeader = el("header", "pane-header hub-queue-header");
    queueHeader.dataset.drag = "";

    const kindTabs = el("div", "segmented hub-kind-tabs");
    kindTabs.setAttribute("role", "radiogroup");
    kindTabs.setAttribute("aria-label", "Filter work type");
    const kindButtons = new Map();
    for (const [value, label] of [
      ["all", "All"],
      ["issue", "Issues"],
      ["pullRequest", "PRs"],
    ]) {
      const btn = el("button", "segment", label);
      btn.type = "button";
      btn.setAttribute("role", "radio");
      btn.setAttribute("aria-checked", String(ui.kind === value));
      btn.addEventListener("click", () => {
        ui.kind = value;
        render();
      });
      kindButtons.set(value, btn);
      kindTabs.append(btn);
    }

    const controls = el("div", "hub-queue-controls");
    const prioSelect = el("select", "select hub-prio-select");
    prioSelect.setAttribute("aria-label", "Filter by priority");
    for (const [val, label] of [
      ["all", "All priorities"],
      ["P0", "P0 Critical"],
      ["P1", "P1 High"],
      ["P2", "P2 Medium"],
      ["P3", "P3 Low"],
    ]) {
      const opt = el("option", "", label);
      opt.value = val;
      prioSelect.append(opt);
    }
    prioSelect.addEventListener("change", () => {
      ui.priority = prioSelect.value;
      render();
    });

    const syncBtn = el("button", "button hub-mini-btn hub-icon-btn");
    syncBtn.type = "button";
    const syncIcon = uiIcon("refresh");
    const syncLabel = el("span", "", "Sync");
    syncBtn.append(syncIcon, syncLabel);
    syncBtn.addEventListener("click", () => void load({ sync: true }));
    controls.append(prioSelect, syncBtn);
    queueHeader.append(kindTabs, controls);

    const searchWrap = el("div", "hub-search-wrap");
    const searchInput = el("input", "field hub-search");
    searchInput.type = "text";
    searchInput.setAttribute("aria-label", "Filter issues and pull requests");
    searchInput.placeholder = "Filter issues & PRs by repo, #number, label, or keyword…";
    searchInput.autocomplete = "off";
    searchInput.spellcheck = false;
    searchInput.addEventListener("input", () => {
      ui.query = searchInput.value;
      render();
    });
    searchWrap.append(searchInput);

    const queueList = el("div", "hub-queue-list");
    queuePane.append(queueHeader, searchWrap, queueList);

    // Right pane: Evaluation & Lane Launch
    const inspPane = el("aside", "pane hub-inspector");
    inspPane.id = "hub-inspector";
    const inspHeader = el("header", "pane-header");
    inspHeader.dataset.drag = "";
    const inspLaneChip = el("span", "hub-lane-chip");
    inspLaneChip.hidden = true;
    inspHeader.append(el("span", "hub-pane-title", "Evaluation & Lane Launch"), inspLaneChip);
    const inspBody = el("div", "hub-inspector-body");
    const inspFoot = el("footer", "hub-insp-foot");
    inspPane.append(inspHeader, inspBody, inspFoot);

    function makeDivider(key, label, controlsId) {
      const div = el("div", "hub-divider");
      div.id = `divider-hub-${key}`;
      div.setAttribute("role", "separator");
      div.setAttribute("aria-orientation", "vertical");
      div.setAttribute("aria-label", label);
      div.setAttribute("aria-controls", controlsId);
      const [min, max] = HUB_PANE_LIMITS[key];
      div.setAttribute("aria-valuemin", String(min));
      div.setAttribute("aria-valuemax", String(max));
      div.tabIndex = 0;
      div.title = "Drag to resize · double-click to reset";

      let dragRaf = null;
      let dragClientX = 0;
      const applyDrag = () => {
        const rect = shell.getBoundingClientRect();
        if (rect.width <= 0) return;
        ui.layout = dragHubPane(ui.layout, key, dragClientX - rect.left, rect.width);
        drawHubLayout();
      };
      const stop = () => {
        if (ui.dragging !== key) return;
        if (dragRaf !== null) {
          cancelAnimationFrame(dragRaf);
          dragRaf = null;
          applyDrag();
        }
        ui.dragging = null;
        div.classList.remove("active");
        document.body.classList.remove("resizing");
        saveHubLayout(ui.layout);
      };
      div.addEventListener("pointerdown", (event) => {
        if (event.button !== 0) return;
        event.preventDefault();
        div.setPointerCapture(event.pointerId);
        ui.dragging = key;
        div.classList.add("active");
        document.body.classList.add("resizing");
      });
      div.addEventListener("pointermove", (event) => {
        if (ui.dragging !== key) return;
        if ((event.buttons & 1) === 0) return stop();
        dragClientX = event.clientX;
        if (dragRaf !== null) return;
        dragRaf = requestAnimationFrame(() => {
          dragRaf = null;
          if (ui.dragging !== key) return;
          applyDrag();
        });
      });
      div.addEventListener("lostpointercapture", stop);
      div.addEventListener("dblclick", () => {
        ui.layout = resetHubPane(ui.layout, key);
        drawHubLayout();
        saveHubLayout(ui.layout);
      });
      div.addEventListener("keydown", (event) => {
        if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
        event.preventDefault();
        event.stopPropagation();
        const rect = shell.getBoundingClientRect();
        const size = event.shiftKey ? BIG_STEP : STEP;
        const delta = event.key === "ArrowRight" ? size : -size;
        ui.layout = stepHubPane(ui.layout, key, delta, rect.width);
        drawHubLayout();
        saveHubLayout(ui.layout);
      });
      return div;
    }

    const reposDivider = makeDivider("repos", "Monitored repositories width", "hub-repos");
    const inspDivider = makeDivider("inspector", "Evaluation panel width", "hub-inspector");

    shell.append(reposPane, reposDivider, queuePane, inspDivider, inspPane);
    hubEl.replaceChildren(shell);

    ui.dom = {
      shell,
      reposDivider,
      inspDivider,
      repoList,
      reposFoot,
      kindButtons,
      prioSelect,
      syncBtn,
      syncIcon,
      syncLabel,
      searchInput,
      queueList,
      inspLaneChip,
      inspBody,
      inspFoot,
    };
    drawHubLayout();
    return ui.dom;
  }

  function drawHubLayout() {
    if (!ui.dom) return;
    const { shell, reposDivider, inspDivider } = ui.dom;
    const shellWidth = shell.getBoundingClientRect().width;
    const drawn = fitHubPanes(ui.layout, shellWidth);
    shell.style.setProperty("--hub-repos-width", `${drawn.repos}px`);
    shell.style.setProperty("--hub-inspector-width", `${drawn.inspector}px`);
    reposDivider.setAttribute("aria-valuenow", String(drawn.repos));
    inspDivider.setAttribute("aria-valuenow", String(drawn.inspector));
  }

  let resizeRaf = null;
  window.addEventListener("resize", () => {
    if (!ui.shown || !ui.dom || resizeRaf !== null) return;
    resizeRaf = requestAnimationFrame(() => {
      resizeRaf = null;
      if (ui.shown) drawHubLayout();
    });
  });

  function render() {
    if (!ui.shown) return;
    const dom = ensureShell();
    const settingsView = getSettingsView?.() ?? null;
    if (settingsView?.settings) {
      models.request(settingsView.settings);
    }
    const items = visibleItems();
    const selected = activeItem(items);
    if (selected) {
      const key = itemKey(selected);
      if (ui.selectedKey !== key) {
        ui.selectedKey = key;
        ui.chosenLane = selected.recommendedLane;
        ui.baseLane = selected.recommendedLane;
        ui.customStages = null;
        ui.humanGate = settingsView?.settings?.lanes?.hardHumanGate ?? true;
      }
      const lane = ui.chosenLane ?? selected.recommendedLane;
      void fetchStagePreview(lane, selected.kind);
    }

    // Preserve scroll positions across redraws.
    const repoScroll = dom.repoList.scrollTop;
    const queueScroll = dom.queueList.scrollTop;
    const inspScroll = dom.inspBody.scrollTop;

    renderReposContent(dom, settingsView);
    renderQueueContent(dom, items, selected);
    renderInspectorContent(dom, selected, settingsView);

    dom.repoList.scrollTop = repoScroll;
    dom.queueList.scrollTop = queueScroll;
    dom.inspBody.scrollTop = inspScroll;
  }

  function renderReposContent(dom, settingsView) {
    const frag = document.createDocumentFragment();
    const repos = ui.data?.repos ?? [];
    if (repos.length > 0) {
      const allSelected = repos.every((r) => r.selected);
      const allRow = el("label", "hub-repo-all");
      const cb = postilCheckbox({
        checked: allSelected,
        ariaLabel: "Toggle all monitored repositories",
        onChange: (checked) => mutateRepo({ action: "selectAll", selected: checked }),
      });
      allRow.append(cb, el("span", "", `All monitored (${repos.length})`));
      frag.append(allRow);

      for (const repo of repos) {
        const card = el("div", `hub-repo-card${repo.selected ? " selected" : ""}`);
        const top = el("div", "hub-repo-top");
        const toggle = postilCheckbox({
          checked: repo.selected,
          ariaLabel: `Include ${repo.name} in priority queue`,
          title: "Include in priority queue",
          onChange: (checked) =>
            mutateRepo({ action: "select", path: repo.path, selected: checked }),
        });
        const nameBtn = el("button", "hub-repo-name", repo.name);
        nameBtn.type = "button";
        nameBtn.title = `Open ${repo.path} in code workspace`;
        nameBtn.disabled = !repo.exists;
        nameBtn.addEventListener("click", () => openRepo(repo.path));

        const removeBtn = el("button", "icon-button hub-repo-remove");
        removeBtn.type = "button";
        removeBtn.append(uiIcon("close"));
        removeBtn.setAttribute("aria-label", `Stop monitoring ${repo.name}`);
        removeBtn.title = "Stop monitoring";
        removeBtn.addEventListener("click", () =>
          mutateRepo({ action: "remove", path: repo.path }),
        );
        top.append(toggle, nameBtn, removeBtn);

        const meta = el("div", "hub-repo-meta");
        const branch = repo.branch || (repo.exists ? "detached" : "missing");
        const counts = `${repo.openIssues} iss · ${repo.openPrs} PR`;
        meta.append(
          uiIcon("branch", "hub-meta-icon"),
          el("span", "", repo.syncing ? `${branch} · syncing…` : `${branch} · ${counts}`),
        );
        card.append(top, meta);
        if (repo.error) {
          card.append(el("div", "hub-repo-error", repo.error));
        }
        frag.append(card);
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
      frag.append(empty);
    }

    // Quick-add from recent projects not yet monitored
    const monitoredPaths = new Set(repos.map((r) => r.path));
    const suggestions = ui.recent
      .filter((p) => p.exists && !monitoredPaths.has(p.path))
      .slice(0, 5);
    if (suggestions.length > 0) {
      const sec = el("div", "hub-recent-add");
      sec.append(el("div", "hub-subhead", "Add from Recent"));
      for (const proj of suggestions) {
        const row = el("button", "hub-recent-btn");
        row.type = "button";
        const head = el("span", "hub-recent-head");
        head.append(uiIcon("plus", "hub-meta-icon"), el("span", "", proj.name));
        row.append(head, el("span", "hub-recent-sub", proj.path));
        row.addEventListener("click", () => mutateRepo({ action: "add", path: proj.path }));
        sec.append(row);
      }
      frag.append(sec);
    }

    dom.repoList.replaceChildren(frag);

    // hippius-mem storage badge at footer using Postil's native `stateMark` + storage icon
    const mem = memoryBackendSummary(settingsView?.memory);
    const memBtn = el("button", `hub-mem-pill ${mem.kind}`);
    memBtn.type = "button";
    memBtn.title = "Configure hippius-mem (Local Vault or Remote S3) in Settings";
    const storageIcon =
      mem.storage === "s3" ? uiIcon("cloud", "hub-meta-icon") : uiIcon("database", "hub-meta-icon");
    memBtn.append(
      stateMark({ kind: mem.kind, label: `${mem.title}: ${mem.detail}` }),
      storageIcon,
      el("span", "hub-mem-title", mem.title),
      el("span", "hub-mem-detail", mem.detail),
    );
    memBtn.addEventListener("click", () => openSettings());
    dom.reposFoot.replaceChildren(memBtn);
  }

  function renderQueueContent(dom, items, selected) {
    for (const [value, btn] of dom.kindButtons.entries()) {
      btn.setAttribute("aria-checked", String(ui.kind === value));
    }
    dom.prioSelect.value = ui.priority;
    const syncing = Boolean(ui.data?.syncing);
    dom.syncBtn.disabled = syncing || (ui.data?.repos?.length ?? 0) === 0;
    dom.syncIcon.classList.toggle("spinning", syncing);
    dom.syncLabel.textContent = syncing ? "Syncing…" : "Sync";

    const frag = document.createDocumentFragment();
    if (ui.data && !ui.data.ghAvailable) {
      frag.append(
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
          : syncing
            ? "Fetching open issues & PRs and recalling team memory…"
            : "No open issues or pull requests match the current filters.";
      frag.append(el("div", "empty", msg));
    } else {
      for (const group of groups) {
        frag.append(el("div", `hub-group-head tone-${group.tone}`, group.label));
        for (const item of group.items) {
          const isSelected = selected && itemKey(item) === itemKey(selected);
          const card = el("button", `hub-card${isSelected ? " selected" : ""}`);
          card.type = "button";
          card.addEventListener("click", () => selectItem(item));

          const row1 = el("div", "hub-card-top");
          const badge = el("span", `hub-prio-badge tone-${group.tone}`, item.priority);
          const repoTag = el("span", "hub-repo-pill", item.repoName);
          const kindTag = el("span", "hub-kind-pill");
          kindTag.append(
            uiIcon(item.kind === "pullRequest" ? "pr" : "issue", "hub-meta-icon"),
            el("span", "", item.kind === "pullRequest" ? `#${item.number}` : `#${item.number}`),
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
          frag.append(card);
        }
      }
    }

    dom.queueList.replaceChildren(frag);
  }

  function renderInspectorContent(dom, item, settingsView) {
    if (!item) {
      dom.inspLaneChip.hidden = true;
      dom.inspBody.replaceChildren(
        el("div", "empty", "Select an issue or PR from the queue to inspect and launch."),
      );
      dom.inspFoot.hidden = true;
      return;
    }
    dom.inspFoot.hidden = false;

    const lane = ui.chosenLane ?? item.recommendedLane;
    const prioMeta = PRIORITY_META[item.priority] ?? PRIORITY_META.P2;
    const tone = prioMeta.tone;

    dom.inspLaneChip.hidden = false;
    dom.inspLaneChip.className = `hub-lane-chip lane-${lane}`;
    dom.inspLaneChip.textContent = `${laneLabel(lane)} Lane`;

    const frag = document.createDocumentFragment();
    const metaLine = el("div", "hub-insp-meta");
    const prioBadge = el("span", `hub-prio-badge tone-${tone}`, item.priority);
    const repoTag = el("span", "hub-repo-pill", item.repoName);
    const kindTag = el("span", "hub-kind-pill");
    kindTag.append(
      uiIcon(item.kind === "pullRequest" ? "pr" : "issue", "hub-meta-icon"),
      el("span", "", `#${item.number}`),
    );
    const scoreTag = el("span", `hub-score-pill tone-${tone}`, `Score ${item.priorityScore}/100`);
    metaLine.append(prioBadge, repoTag, kindTag, scoreTag);
    const titleEl = el("h2", "hub-insp-title", item.title);

    const signals = el("div", "hub-insp-section");
    const evalCard = el("div", `hub-eval-card tone-${tone}`);
    evalCard.append(
      el("div", "hub-subhead", "Priority & Evaluation"),
      el("p", "hub-insp-text", item.priorityReason),
      el("p", "hub-insp-hint", item.laneReason),
    );
    const evalBtn = el("button", "button hub-mini-btn hub-icon-btn");
    evalBtn.type = "button";
    evalBtn.disabled = ui.evaluating;
    evalBtn.append(
      uiIcon("refresh", ui.evaluating ? "spinning" : ""),
      el("span", "", ui.evaluating ? "Re-evaluating…" : "Re-evaluate with hippius-mem"),
    );
    evalBtn.addEventListener("click", () => void reEvaluate(item));
    evalCard.append(evalBtn);
    signals.append(evalCard);

    frag.append(metaLine, titleEl, signals);

    // Team memory hits
    if (item.memories?.length) {
      const memSec = el("div", "hub-insp-section");
      memSec.append(el("div", "hub-subhead", `Team Memory (${item.memories.length})`));
      for (const hit of item.memories) {
        const card = el("div", "hub-mem-hit");
        card.append(el("span", "hub-mem-id", hit.id), el("span", "", hit.summary));
        memSec.append(card);
      }
      frag.append(memSec);
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
      frag.append(mapSec);
    }

    // Description preview
    if (item.body?.trim()) {
      const desc = el("div", "hub-insp-section");
      desc.append(
        el("div", "hub-subhead", "Description"),
        el("pre", "hub-insp-desc", item.body.trim().slice(0, 4000)),
      );
      frag.append(desc);
    }

    // Lane selector
    const laneSec = el("div", "hub-insp-section");
    laneSec.append(el("div", "hub-subhead", "Execution Lane"));
    const laneGroup = el("div", "hub-lane-picker");
    for (const candidate of LANES) {
      const active = lane === candidate;
      const rec = item.recommendedLane === candidate;
      const opt = el("button", `hub-lane-opt lane-${candidate}${active ? " active" : ""}`);
      opt.type = "button";
      opt.addEventListener("click", () => {
        if (candidate === "custom") {
          if (!ui.customStages && ui.stagePreview?.stages) {
            ui.customStages = ui.stagePreview.stages.map((st) => ({ ...st }));
          }
          if (ui.chosenLane !== "custom") {
            ui.baseLane = ui.chosenLane ?? item.recommendedLane;
          }
          ui.chosenLane = "custom";
          render();
          return;
        }
        ui.chosenLane = candidate;
        ui.baseLane = candidate;
        ui.customStages = null;
        ui.stagePreviewKey = null;
        render();
      });
      const head = el("div", "hub-lane-opt-head");
      const nameWrap = el("span", "hub-lane-opt-name");
      nameWrap.append(el("span", `hub-lane-dot lane-${candidate}`), el("span", "", `${laneLabel(candidate)} Lane`));
      head.append(nameWrap);
      if (rec) head.append(el("span", "hub-rec-badge", "Recommended"));
      opt.append(head, el("div", "hub-lane-opt-desc", laneSummary(candidate, item.kind)));
      laneGroup.append(opt);
    }
    laneSec.append(laneGroup);

    // Stage Roster preview with inline Agent & Model selectors (editing auto-switches to Custom Lane)
    const roster = el("div", `hub-stage-roster lane-${lane}`);
    const stages = activeStages();
    const enabledAgents = settingsView?.settings?.enabledAgents ?? [];
    if (ui.stagePreviewError && !ui.customStages) {
      const err = el("div", "hub-roster-error", ui.stagePreviewError);
      const cfg = el("button", "button hub-mini-btn", "Configure Tiers in Settings");
      cfg.type = "button";
      cfg.addEventListener("click", () => openSettings());
      roster.append(err, cfg);
    } else if (stages) {
      const rosterHead = el("div", "hub-roster-head");
      rosterHead.append(
        el(
          "div",
          "hub-subhead",
          lane === "custom" ? "Stage Roster (Customized)" : "Stage Roster (from Capability Tiers)",
        ),
      );
      if (lane === "custom") {
        const resetBtn = el("button", "button hub-mini-btn hub-roster-reset", "Reset");
        resetBtn.type = "button";
        resetBtn.title = `Reset to ${laneLabel(ui.baseLane ?? item.recommendedLane)} Lane defaults`;
        resetBtn.addEventListener("click", () => {
          ui.chosenLane = ui.baseLane ?? item.recommendedLane;
          ui.customStages = null;
          render();
        });
        rosterHead.append(resetBtn);
      }
      roster.append(rosterHead);

      for (const [idx, st] of stages.entries()) {
        const row = el("div", "hub-stage-row");
        const topLine = el("div", "hub-stage-top");
        const tags = el("span", "hub-stage-tags");
        if (st.tier) {
          tags.append(el("span", `hub-tier-pill tier-${st.tier}`, st.tier));
        }
        tags.append(
          el(
            "span",
            `hub-mode-pill mode-${st.readOnly ? "ro" : "rw"}`,
            st.readOnly ? "RO" : "RW",
          ),
        );
        if (st.diverse) {
          const divIcon = uiIcon("sparkles", "hub-diverse-icon");
          divIcon.setAttribute("title", "Cross-model diverse pairing");
          tags.append(divIcon);
        }
        topLine.append(el("span", "hub-stage-name", st.label), tags);

        const controlsRow = el("div", "hub-stage-controls");
        const logo = agentLogoIcon(st.agent);
        if (logo) controlsRow.append(logo);

        const agentChoices = enabledAgents.includes(st.agent)
          ? enabledAgents
          : [st.agent, ...enabledAgents];
        const agentSel = el("select", "select hub-stage-select hub-stage-agent");
        agentSel.setAttribute("aria-label", `Agent for ${st.label}`);
        for (const name of agentChoices) {
          const opt = el("option", "", name);
          opt.value = name;
          opt.selected = name === st.agent;
          agentSel.append(opt);
        }
        agentSel.addEventListener("change", () => {
          editStage(idx, { agent: agentSel.value, model: null });
        });

        const modelSel = modelSelect(
          `hub-stage-${idx}-model`,
          { name: st.agent, model: st.model ?? null },
          models.models[st.agent],
          (chosen) => editStage(idx, { model: chosen }),
        );
        modelSel.classList.add("hub-stage-select", "hub-stage-model-select");
        modelSel.setAttribute("aria-label", `Model for ${st.label}`);

        controlsRow.append(agentSel, modelSel);

        row.append(
          el("span", `hub-stage-num lane-${lane}`, `${idx + 1}`),
          topLine,
          controlsRow,
        );
        roster.append(row);
      }
    } else {
      roster.append(el("div", "hub-insp-hint", "Resolving capability tiers…"));
    }
    laneSec.append(roster);

    const hasPlanJudge =
      lane === "hard" || (lane === "custom" && Boolean(stages?.some((s) => s.kind === "planJudge")));
    if (hasPlanJudge && item.kind === "issue") {
      const gateRow = el("div", "hub-gate-toggle");
      const gateSwitch = toggleSwitch(
        "hub-hard-human-gate",
        ui.humanGate,
        () => {
          ui.humanGate = !ui.humanGate;
          render();
        },
        { label: "Pause after Stage 2 (Plan Judge) for my approval" },
      );
      gateRow.append(
        el("span", "hub-gate-label", "Pause after Stage 2 (Plan Judge) for approval"),
        gateSwitch,
      );
      laneSec.append(gateRow);
    }
    frag.append(laneSec);
    dom.inspBody.replaceChildren(frag);

    // Footer launch bar
    const openBtn = el("button", "button", "Inspect Repo");
    openBtn.type = "button";
    openBtn.addEventListener("click", () => openRepo(item.repoPath));

    const verb = item.kind === "pullRequest" ? "Review PR" : "Implement";
    const startBtn = el("button", "button primary hub-icon-btn");
    startBtn.type = "button";
    startBtn.disabled = ui.launching || (Boolean(ui.stagePreviewError) && !ui.customStages);
    startBtn.append(
      el("span", "", ui.launching ? "Starting…" : `${verb} in ${laneLabel(lane)} Lane`),
      uiIcon("arrowRight"),
    );
    startBtn.addEventListener("click", () => void startSelected(item, lane));
    dom.inspFoot.replaceChildren(openBtn, startBtn);
  }

  return {
    show() {
      ui.shown = true;
      document.body.classList.add("hub-mode");
      hubEl.hidden = false;
      render();
      drawHubLayout();
      void load();
    },
    hide() {
      ui.shown = false;
      clearTimeout(ui.pollTimer);
      models.reset();
      document.body.classList.remove("hub-mode");
      hubEl.hidden = true;
    },
    isShown: () => ui.shown,
    refresh: () => {
      if (ui.shown) void load();
    },
    renderMemory: () => {
      if (ui.shown && ui.dom) renderReposContent(ui.dom, getSettingsView?.() ?? null);
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
