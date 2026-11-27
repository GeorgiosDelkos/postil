// The context menu: right-click on the code opens actions for the selected lines, right-click
// on a row of the file list opens a note on its file or folder, and the send bar's agent
// picker opens one too. Each action is `{ label, run, icon?, badge?, detail?, key?,
// matches?(event), disabled?, separated? }`; `icon` is an element drawn before the label, and
// `badge` an optional element (such as inline quota meters) drawn after it.

import { el, showToast } from "./dom.js";

// `menuEl` is the menu's element. `codeEl` takes the focus back when the menu closes with the
// focus inside it, or nowhere.
export function contextMenu({ menuEl, codeEl }) {
  // The actions of the open menu, for its keyboard shortcuts.
  let menuActions = [];

  // Closes the menu and runs `action`. A failure is shown, never swallowed: an action that
  // silently did nothing is what made notes look broken once.
  async function runAction(action) {
    closeMenu();
    try {
      await action.run();
    } catch (error) {
      showToast(`${action.label} failed: ${error}`);
    }
  }

  function openMenu(x, y, actions) {
    menuActions = actions;
    // Where some items have an icon, the rest keep an empty slot of its width, so every
    // label starts at the same place.
    const iconed = actions.some((action) => action.icon);
    const items = actions.flatMap((action) => {
      const item = el("button", "menu-item");
      item.type = "button";
      item.setAttribute("role", "menuitem");
      item.disabled = Boolean(action.disabled);
      if (action.icon) item.append(action.icon);
      else if (iconed) item.append(el("span", "menu-icon-slot"));
      item.append(el("span", "menu-label", action.label));
      if (action.badge) item.append(action.badge);
      if (action.detail) item.append(el("span", "menu-detail", action.detail));
      if (action.key) item.append(el("kbd", "", action.key));
      item.addEventListener("click", () => runAction(action));
      // The highlight is the focus, and the pointer moves it, so the pointer and the arrow keys
      // never highlight two items at once.
      // A disabled item cannot take the focus, so hovering it clears the highlight instead of
      // leaving it on the item before, which Enter would then run.
      item.addEventListener("mouseenter", () => (item.disabled ? menuEl : item).focus());
      if (!action.separated) return [item];
      const separator = el("div", "menu-separator");
      separator.setAttribute("role", "separator");
      return [separator, item];
    });
    menuEl.replaceChildren(...items);
    menuEl.hidden = false;

    // Opens at the pointer, moved back inside the window when it would cross an edge.
    const { width, height } = menuEl.getBoundingClientRect();
    menuEl.style.left = `${Math.max(4, Math.min(x, innerWidth - width - 4))}px`;
    menuEl.style.top = `${Math.max(4, Math.min(y, innerHeight - height - 4))}px`;
    // Opened by the pointer, so no item is highlighted yet, as in a macOS menu. The menu itself
    // takes the focus so the arrow keys and Escape still reach it.
    menuEl.focus();
  }

  function closeMenu() {
    if (menuEl.hidden) return;
    menuEl.hidden = true;
    if (menuEl.contains(document.activeElement) || document.activeElement === document.body) {
      codeEl.focus();
    }
  }

  // While open, the menu owns the keyboard, as system menus do: no key reaches the window's
  // handlers, so `c` cannot open a composer under it and Escape cannot discard one.
  menuEl.addEventListener("keydown", (event) => {
    // A Command shortcut the menu has no item for (Cmd-Q, Cmd-H, Cmd-Shift-O) belongs to the
    // menu bar: close this menu and let the key through, so WebKit passes it on.
    const own = menuActions.some((each) => !each.disabled && each.matches?.(event));
    if (event.metaKey && !own) {
      closeMenu();
      return;
    }
    event.stopPropagation();
    // Enter and Space are the focused item's own: they click it.
    if (event.key === "Enter" || event.key === " ") return;
    event.preventDefault();

    const items = [...menuEl.querySelectorAll("button:not(:disabled)")];
    // -1 while no item is highlighted: Down then takes the first item and Up the last.
    const index = items.indexOf(document.activeElement);
    const up = index === -1 ? items.length - 1 : (index - 1 + items.length) % items.length;
    if (event.key === "ArrowDown") items[(index + 1) % items.length]?.focus();
    else if (event.key === "ArrowUp") items[up]?.focus();
    else if (event.key === "Home") items[0]?.focus();
    else if (event.key === "End") items.at(-1)?.focus();
    else if (event.key === "Escape" || event.key === "Tab") closeMenu();
    else {
      const action = menuActions.find((each) => !each.disabled && each.matches?.(event));
      if (action) runAction(action);
    }
  });

  // WebKit does not focus a button that is clicked: the press would move focus out of the menu
  // first. Keeping the focus where it is lets the click reach the item. (A `focusout` handler
  // that closed the menu here once made every item do nothing.)
  menuEl.addEventListener("mousedown", (event) => event.preventDefault());
  // Leaving the menu clears the highlight, as in a macOS menu.
  menuEl.addEventListener("mouseleave", () => menuEl.focus());

  // Any press outside the menu, a scroll of the code, or leaving the window closes it.
  window.addEventListener(
    "mousedown",
    (event) => {
      if (!menuEl.contains(event.target)) closeMenu();
    },
    true,
  );
  window.addEventListener("blur", closeMenu);

  return { open: openMenu, close: closeMenu };
}
