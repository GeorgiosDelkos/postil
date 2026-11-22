// Fuzzy file finding: which paths match what was typed, best first.
//
// A path matches when the query's characters appear in it in order, ignoring case. The file
// name counts for more than the directories: a reviewer types the name of the file they want.

// Points for each matched character, by where it sits. Arbitrary weights, tuned on the cases
// in fuzzy.test.js: a run of adjacent characters beats the same characters scattered, and the
// start of a word beats its middle.
const MATCHED = 1;
const ADJACENT = 5;
const WORD_START = 3;
// Added once when the whole query fits inside the file name.
const IN_FILE_NAME = 10;
// Taken off per character of path, so the shorter of two equal matches wins.
const PER_CHARACTER = 0.01;

const SEPARATORS = new Set(["/", "_", "-", ".", " "]);

// The paths matching `query`, best first, at most `limit` of them. Each result carries the
// indexes of the matched characters, for the page to mark. An empty query matches nothing.
export function rank(query, paths, limit) {
  const needle = query.replaceAll(" ", "").toLowerCase();
  if (needle === "") return [];

  const results = [];
  for (const path of paths) {
    const match = matchPath(needle, path);
    if (match) results.push({ path, ...match });
  }
  results.sort((a, b) => b.score - a.score || a.path.localeCompare(b.path));
  return results.slice(0, limit).map(({ path, positions }) => ({ path, positions }));
}

// Tries the file name alone first, then the whole path.
function matchPath(needle, path) {
  const lower = path.toLowerCase();
  const nameStart = path.lastIndexOf("/") + 1;

  const inName = matchFrom(needle, path, lower, nameStart);
  if (inName) return { ...inName, score: inName.score + IN_FILE_NAME };
  return nameStart > 0 ? matchFrom(needle, path, lower, 0) : null;
}

// Matches `needle` against `path` from index `from`, taking each character at its first
// occurrence after the previous one. Returns null when a character is missing.
function matchFrom(needle, path, lower, from) {
  const positions = [];
  let score = -path.length * PER_CHARACTER;
  let at = from;

  for (const character of needle) {
    const found = lower.indexOf(character, at);
    if (found === -1) return null;

    score += MATCHED;
    if (positions.length > 0 && positions.at(-1) === found - 1) score += ADJACENT;
    if (isWordStart(path, found)) score += WORD_START;
    positions.push(found);
    at = found + 1;
  }
  return { positions, score };
}

// True at the start of the path, after a separator, and at a capital after a lowercase
// letter (the `B` of `fooBar`).
function isWordStart(path, index) {
  if (index === 0) return true;
  const previous = path[index - 1];
  if (SEPARATORS.has(previous)) return true;
  const current = path[index];
  return current !== current.toLowerCase() && previous === previous.toLowerCase();
}
