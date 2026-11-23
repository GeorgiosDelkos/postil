// Small DOM helpers shared by the page's modules.

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

// The status icon of a card or a dialog: its shape is CSS (`.status-icon.<kind>`).
export function statusIcon(kind) {
  const glyph = { needs: "!", addressed: "✓", resolved: "✓", silent: "–", failed: "×" }[kind] ?? "";
  const icon = el("span", `status-icon ${kind}`, glyph);
  icon.setAttribute("aria-hidden", "true");
  return icon;
}

// Puts icons.svg into the page once, so every icon refers to a symbol in this document.
// Until it arrives, or if it fails, the icons are empty boxes and nothing else changes.
export async function loadIcons() {
  const response = await fetch("icons.svg");
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
