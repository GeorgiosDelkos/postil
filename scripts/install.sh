#!/usr/bin/env bash
# Builds Postil.app (scripts/bundle.sh) and installs it in /Applications, or in ~/Applications
# when /Applications is not writable, so Launchpad, Spotlight and Finder find it. A Postil.app
# already there goes to the Trash, never deleted outright: it can be put back.
#
# Usage: scripts/install.sh [DIR], DIR being where to install instead. Quit Postil first: a
# running copy keeps running the old build until it is quit. Needs what scripts/bundle.sh
# needs, and macOS's `trash` and `ditto`. With POSTIL_SIGN_IDENTITY set, it signs (and with
# POSTIL_NOTARY_PROFILE, notarizes) the bundle through scripts/sign.sh before installing it.
set -euo pipefail

usage="usage: scripts/install.sh [DIR]"
root="$(cd "$(dirname "$0")/.." && pwd)"

# The arguments are checked before the build, which takes a minute: a mistake fails at once.
if [[ $# -gt 1 ]]; then
  echo "$usage" >&2
  exit 2
fi
if [[ $# -eq 1 ]]; then
  case "$1" in
  -h | --help)
    echo "$usage"
    exit 0
    ;;
  "" | -*)
    echo "$usage" >&2
    exit 2
    ;;
  esac
  dest="$1"
elif [[ -w /Applications ]]; then
  dest=/Applications
else
  dest="$HOME/Applications"
fi

# A notary profile without a certificate would be ignored, and an unsigned app installed.
if [[ -n "${POSTIL_NOTARY_PROFILE:-}" && -z "${POSTIL_SIGN_IDENTITY:-}" ]]; then
  echo "POSTIL_NOTARY_PROFILE is set but POSTIL_SIGN_IDENTITY is not: set both, or neither" >&2
  exit 2
fi

"$root/scripts/bundle.sh"
if [[ -n "${POSTIL_SIGN_IDENTITY:-}" ]]; then
  "$root/scripts/sign.sh"
fi

mkdir -p "$dest"
# Copied in beside the old one first, so a copy that fails leaves the installed app as it was.
# ditto keeps the bundle's extended attributes and symbolic links as Finder would.
staged="$dest/.Postil.app.installing"
if [[ -e "$staged" ]]; then
  trash "$staged"
fi
ditto "$root/target/release/Postil.app" "$staged"
if [[ -e "$dest/Postil.app" ]]; then
  trash "$dest/Postil.app"
fi
mv "$staged" "$dest/Postil.app"
echo "installed $dest/Postil.app"
