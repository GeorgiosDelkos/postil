// Jump to definition, the parts that need no page: the name at a position in a line's text,
// the history of places jumped from and to, and the picker's count line. The index itself is
// the Rust process's (src/definitions.rs).

// What a name is made of: Unicode identifier characters, and `$`, which JavaScript allows.
const NAME_PART = /[\p{ID_Continue}$]/u;
const NAME_START = /[\p{ID_Start}_$]/u;

// The token classes (src/highlight.rs, `STYLES`) whose text names no definition to jump to:
// strings, keywords, and comments (the owner's choice, 2026-11-26).
export const NO_DEFINITION = new Set(["s", "k", "m"]);

// How many places the history keeps: the oldest goes first. As the design says
// (docs/review-assist-plan.md, 7.7).
export const MAX_PLACES = 50;

// The name in `text` at `offset`, a caret position in UTF-16 code units, which a click puts
// before or after the character it lands on: `{ start, end, name }`, or null on whitespace,
// punctuation, or a number.
export function nameAt(text, offset) {
  let start = offset;
  while (start > 0) {
    const before = charBefore(text, start);
    if (!NAME_PART.test(before)) break;
    start -= before.length;
  }
  let end = offset;
  while (end < text.length) {
    const at = String.fromCodePoint(text.codePointAt(end));
    if (!NAME_PART.test(at)) break;
    end += at.length;
  }
  if (start === end) return null;
  const name = text.slice(start, end);
  if (!NAME_START.test(String.fromCodePoint(name.codePointAt(0)))) return null;
  return { start, end, name };
}

// The character that ends just before `index`, one or two code units long.
function charBefore(text, index) {
  const low = text.charCodeAt(index - 1);
  const high = index >= 2 ? text.charCodeAt(index - 2) : 0;
  const pair = low >= 0xdc00 && low <= 0xdfff && high >= 0xd800 && high <= 0xdbff;
  return text.slice(pair ? index - 2 : index - 1, index);
}

// The places jumped from and to, for Cmd-[ and Cmd-]. A place is `{ path, line, scrollTop,
// selection }`: `line` is 1-based; `scrollTop` and `selection` are the pane's when it was
// left, or null for a place not seen yet.
export function jumpHistory(max = MAX_PLACES) {
  let places = [];
  // The place being shown; -1 before the first jump.
  let index = -1;

  return {
    // A jump from `from`, where the engineer is, to `to`. Places ahead of the current one go,
    // as in a browser. The current place is replaced by `from` when it is in the same file,
    // so going back returns where the engineer was, not where the last jump landed.
    jump(from, to) {
      places = places.slice(0, index + 1);
      if (places.at(-1)?.path === from.path) places[places.length - 1] = from;
      else places.push(from);
      places.push(to);
      places = places.slice(-max);
      index = places.length - 1;
    },

    // Steps `direction` (-1 back, 1 forward) from `current`, the place on screen, through
    // `show(place)`, which resolves to "shown", "missing" (its file is gone), or "superseded"
    // (something else took the pane meanwhile). A missing place is dropped and the next one in
    // that direction is tried; a superseded step stops and drops nothing. Resolves to whether
    // a place was shown.
    async step(direction, current, show) {
      if (index < 0) return false;
      // Away from the current place, in a file opened another way since, back goes to that
      // place first.
      const away = current?.path !== places[index].path;
      if (!away) places[index] = current;
      let next = away && direction < 0 ? index : index + direction;
      while (next >= 0 && next < places.length) {
        const shown = await show(places[next]);
        if (shown === "shown") {
          index = next;
          return true;
        }
        if (shown === "superseded") return false;
        places.splice(next, 1);
        if (next <= index) index -= 1;
        // Going forward, the next place has moved into the slot of the one dropped.
        if (direction < 0) next -= 1;
      }
      if (places.length > 0) index = Math.max(index, 0);
      return false;
    },
  };
}

// The picker's line under its title: how many definitions it lists, and that they match by
// name only (docs/review-assist.md, "Jump to definition").
export function countLine(shown, total) {
  const count = shown === total ? `${total}` : `${shown} of ${total}`;
  return `${count} definitions · by name`;
}
