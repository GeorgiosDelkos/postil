// Small DOM helpers shared by the page's modules.

import { agentLogo } from "./icons.js";

export const $ = (id) => document.getElementById(id);

export function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

const SVG = "http://www.w3.org/2000/svg";

// Crisp 16x16 inline stroke icons matching Postil's native Activity Bar visual language
// (stroke="currentColor", stroke-width="1.5", round caps/joins, fill="none").
const UI_ICON_PATHS = {
  plus: ['<path d="M8 3.25v9.5M3.25 8h9.5"/>'],
  close: ['<path d="M4.25 4.25l7.5 7.5M11.75 4.25l-7.5 7.5"/>'],
  refresh: ['<path d="M13 8a5 5 0 1 1-1.35-3.42"/>', '<path d="M13 3.25v2.75h-2.75"/>'],
  issue: [
    '<circle cx="8" cy="8" r="5.5"/>',
    '<circle cx="8" cy="8" r="1.25" fill="currentColor" stroke="none"/>',
  ],
  pr: [
    '<circle cx="4.5" cy="4" r="1.5"/>',
    '<circle cx="4.5" cy="12" r="1.5"/>',
    '<circle cx="11.5" cy="12" r="1.5"/>',
    '<path d="M4.5 5.5v5M11.5 10.5V6.5a2 2 0 0 0-2-2H8"/>',
    '<path d="M9.5 3 8 4.5 9.5 6"/>',
  ],
  branch: [
    '<circle cx="5" cy="4" r="1.5"/>',
    '<circle cx="5" cy="12" r="1.5"/>',
    '<circle cx="11.5" cy="6" r="1.5"/>',
    '<path d="M5 5.5v5M11.5 7.5a3 3 0 0 1-3 3H5"/>',
  ],
  arrowRight: ['<path d="M3.5 8h9M9 4.5 12.5 8 9 11.5"/>'],
  chevronDown: ['<path d="M4 6l4 4 4-4"/>'],
  chevronRight: ['<path d="M6 4l4 4-4 4"/>'],
  sparkles: ['<path d="M8 2.5 9.2 6.8 13.5 8 9.2 9.2 8 13.5 6.8 9.2 2.5 8 6.8 6.8Z"/>'],
  cloud: [
    '<path d="M4.75 12h7a2.5 2.5 0 0 0 .4-4.96A3.75 3.75 0 0 0 5 6.2a2.75 2.75 0 0 0-.25 5.8Z"/>',
  ],
  database: [
    '<ellipse cx="8" cy="4.25" rx="4.75" ry="1.75"/>',
    '<path d="M3.25 4.25v7.5c0 .97 2.13 1.75 4.75 1.75s4.75-.78 4.75-1.75v-7.5"/>',
    '<path d="M3.25 8c0 .97 2.13 1.75 4.75 1.75s4.75-.78 4.75-1.75"/>',
  ],
  check: ['<path d="M3.5 8.25 6.5 11.25 12.5 4.75"/>'],
};

const UI_ICON_TEMPLATE_CACHE = new Map();

export function uiIcon(name, className = "") {
  let cached = UI_ICON_TEMPLATE_CACHE.get(name);
  if (!cached) {
    const svg = document.createElementNS(SVG, "svg");
    svg.setAttribute("viewBox", "0 0 16 16");
    svg.setAttribute("aria-hidden", "true");
    const doc = new DOMParser().parseFromString(
      `<svg xmlns="${SVG}">${(UI_ICON_PATHS[name] ?? []).join("")}</svg>`,
      "image/svg+xml",
    );
    for (const child of doc.documentElement.childNodes) {
      svg.append(document.importNode(child, true));
    }
    cached = svg;
    UI_ICON_TEMPLATE_CACHE.set(name, cached);
  }
  const node = cached.cloneNode(true);
  node.setAttribute("class", className ? `ui-icon ${className}` : "ui-icon");
  return node;
}

// A styled Postil checkbox wrapping a native `<input type="checkbox">` so keyboard navigation,
// screen-reader semantics, and Graphite/Paper visuals all stay uniform across Hub, Plan Gate,
// and PR Review.
export function postilCheckbox({
  checked = false,
  disabled = false,
  ariaLabel = "",
  title = "",
  onChange,
}) {
  const input = el("input", "postil-check");
  input.type = "checkbox";
  input.checked = checked;
  input.disabled = disabled;
  if (ariaLabel) input.setAttribute("aria-label", ariaLabel);
  if (title) input.title = title;
  if (onChange) input.addEventListener("change", () => onChange(input.checked));
  return input;
}

// An icon from the sprite that `loadIcons` put in the page, by symbol id.
export function icon(id) {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS(SVG, "use");
  use.setAttribute("href", `#${id}`);
  svg.append(use);
  return svg;
}

// Makes `button` act on its second click only, as deleting what the engineer wrote does. The
// first click calls `arm()`, which shows that the next click acts; the second calls `act()`.
// The button losing the focus, or the pointer leaving `area` when one is given, calls
// `disarm()` and starts over.
export function actOnSecondClick(button, { area = null, arm, disarm, act }) {
  let armed = false;
  const reset = () => {
    armed = false;
    disarm();
  };
  button.addEventListener("click", () => {
    if (armed) {
      act();
      return;
    }
    armed = true;
    arm();
  });
  button.addEventListener("blur", reset);
  area?.addEventListener("mouseleave", reset);
}

// The glyph inside each kind of status icon that has one.
const STATUS_GLYPHS = {
  needs: "!",
  addressed: "✓",
  answered: "✓",
  resolved: "✓",
  silent: "–",
  failed: "×",
};

// The status icon of a card or a dialog: its shape is CSS (`.status-icon.<kind>`).
export function statusIcon(kind) {
  const glyph = STATUS_GLYPHS[kind] ?? "";
  const icon = el("span", `status-icon ${kind}`, glyph);
  icon.setAttribute("aria-hidden", "true");
  return icon;
}

// The logo of the agent called `name` (agentLogo in icons.js), or null for an agent without
// one, which shows its name alone.
export function agentLogoIcon(name) {
  const id = agentLogo(name);
  if (!id) return null;
  const svg = icon(id);
  svg.setAttribute("class", "agent-logo");
  return svg;
}

// Puts icons.svg (the file tree's icons) and agent-logos.svg into the page once, so every
// icon refers to a symbol in this document. Until a sprite arrives, or if it fails, its icons
// are empty boxes and nothing else changes.
export async function loadIcons() {
  await Promise.all(["icons.svg", "agent-logos.svg"].map(loadSprite));
}

async function loadSprite(file) {
  const response = await fetch(file);
  if (!response.ok) return;
  const sprite = new DOMParser().parseFromString(await response.text(), "image/svg+xml");
  const root = document.importNode(sprite.documentElement, true);
  root.setAttribute("class", "icon-sprite");
  root.setAttribute("aria-hidden", "true");
  document.body.prepend(root);
}

// A short message at the bottom of the window, for failures with no better place to show.
export function showToast(text) {
  const toast = $("toast");
  toast.textContent = text;
  toast.hidden = false;
  clearTimeout(showToast.timer);
  showToast.timer = setTimeout(() => {
    toast.hidden = true;
  }, 5000);
}
