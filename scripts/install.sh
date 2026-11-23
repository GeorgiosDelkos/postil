#!/usr/bin/env bash
# Builds Postil.app (scripts/bundle.sh) and installs it in /Applications, or in ~/Applications
# when /Applications is not writable, so Launchpad, Spotlight and Finder find it. A Postil.app
# already there goes to the Trash first, never deleted outright: it can be put back.
#
# Usage: scripts/install.sh [DIR], DIR being where to install instead. Quit Postil first: a
# running copy keeps running the old build until it is quit. Needs what scripts/bundle.sh
# needs, and macOS's `trash` and `ditto`.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

if [[ $# -gt 1 ]]; then
  echo "usage: scripts/install.sh [DIR]" >&2
  exit 2
fi
if [[ $# -eq 1 ]]; then
  dest="$1"
elif [[ -w /Applications ]]; then
  dest=/Applications
else
  dest="$HOME/Applications"
fi

"$root/scripts/bundle.sh"

mkdir -p "$dest"
if [[ -e "$dest/Postil.app" ]]; then
  trash "$dest/Postil.app"
fi
# ditto keeps the bundle's extended attributes and symbolic links as Finder would.
ditto "$root/target/release/Postil.app" "$dest/Postil.app"
echo "installed $dest/Postil.app"
