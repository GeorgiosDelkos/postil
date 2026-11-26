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

// The status icon of a card or a dialog: its shape is CSS (`.status-icon.<kind>`).
export function statusIcon(kind) {
  const glyph = { needs: "!", addressed: "✓", resolved: "✓", silent: "–", failed: "×" }[kind] ?? "";
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
