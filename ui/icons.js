// Which icon a row of the file tree shows: the id of a symbol in icons.svg (Material Icon
// Theme, see the comment at the top of that file). Plain functions of a name; file-list.js draws.
// Also which logo an agent's name shows, from agent-logos.svg; dom.js `agentLogoIcon` draws.

// Folders with an icon of their own, by name in lower case. Every other folder is plain.
const FOLDERS = new Map([
  ["src", "src"],
  ["source", "src"],
  ["test", "test"],
  ["tests", "test"],
  ["__tests__", "test"],
  ["spec", "test"],
  ["docs", "docs"],
  ["doc", "docs"],
  [".github", "github"],
  ["config", "config"],
  [".config", "config"],
  ["images", "images"],
  ["img", "images"],
  ["scripts", "scripts"],
  ["dist", "dist"],
  ["build", "dist"],
  ["out", "dist"],
  ["target", "target"],
  ["ui", "ui"],
  ["lib", "lib"],
  ["tools", "tools"],
  [".vscode", "vscode"],
  ["public", "public"],
]);

// Files known by their whole name, in lower case. Checked before the extension.
const FILE_NAMES = new Map([
  ["cargo.lock", "lock"],
  ["package.json", "nodejs"],
  ["package-lock.json", "lock"],
  ["license", "license"],
  ["copying", "license"],
  ["changelog.md", "changelog"],
  ["contributing.md", "contributing"],
  [".gitignore", "git"],
  [".gitattributes", "git"],
  [".gitmodules", "git"],
  ["dockerfile", "docker"],
  ["makefile", "makefile"],
  [".editorconfig", "editorconfig"],
  [".env", "tune"],
]);

// Files known by the part of the name after the last dot, in lower case.
const EXTENSIONS = new Map([
  ["rs", "rust"],
  ["ts", "typescript"],
  ["mts", "typescript"],
  ["cts", "typescript"],
  ["tsx", "react_ts"],
  ["js", "javascript"],
  ["mjs", "javascript"],
  ["cjs", "javascript"],
  ["jsx", "react"],
  ["json", "json"],
  ["jsonc", "json"],
  ["toml", "toml"],
  ["md", "markdown"],
  ["py", "python"],
  ["go", "go"],
  ["sh", "console"],
  ["bash", "console"],
  ["zsh", "console"],
  ["fish", "console"],
  ["lock", "lock"],
  ["svg", "svg"],
  ["png", "image"],
  ["jpg", "image"],
  ["jpeg", "image"],
  ["gif", "image"],
  ["webp", "image"],
  ["ico", "image"],
  ["html", "html"],
  ["htm", "html"],
  ["css", "css"],
  ["scss", "sass"],
  ["sass", "sass"],
  ["yml", "yaml"],
  ["yaml", "yaml"],
  ["xml", "xml"],
  ["woff", "font"],
  ["woff2", "font"],
  ["ttf", "font"],
  ["otf", "font"],
  ["nix", "nix"],
  ["mk", "makefile"],
]);

// Built-in agents with a logo in agent-logos.svg, by name (BUILT_IN in src/agents.rs). The
// comment at the top of that file says where each came from, and why the others have none.
const AGENT_LOGOS = new Map([["OpenCode", "agent-opencode"]]);

// The symbol id of the agent's logo, or null. The name is matched exactly: a custom agent
// cannot take a built-in name in any case (saveCustomAgent in settings.js), so it gets none.
export function agentLogo(name) {
  return AGENT_LOGOS.get(name) ?? null;
}

export function folderIcon(name, open) {
  const kind = FOLDERS.get(name.toLowerCase()) ?? "base";
  return `icon-folder-${kind}${open ? "-open" : ""}`;
}

export function fileIcon(name) {
  const lower = name.toLowerCase();
  const byName = FILE_NAMES.get(lower);
  if (byName) return `icon-${byName}`;
  // README.md, README.txt, LICENSE-MIT and the like: the name says more than the extension.
  if (lower.startsWith("readme")) return "icon-readme";
  if (lower.startsWith("license")) return "icon-license";

  // A leading dot starts a hidden file's name, not an extension: `.bashrc` has none.
  const dot = lower.lastIndexOf(".");
  const extension = dot > 0 ? lower.slice(dot + 1) : "";
  return `icon-${EXTENSIONS.get(extension) ?? "document"}`;
}
