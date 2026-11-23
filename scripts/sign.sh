#!/usr/bin/env bash
# Signs Postil.app with a Developer ID certificate and the hardened runtime, then notarizes and
# staples it, so it opens on other Macs without a Gatekeeper warning. Run it after
# scripts/bundle.sh; scripts/install.sh runs it when POSTIL_SIGN_IDENTITY is set.
#
# Usage: scripts/sign.sh [APP], APP being target/release/Postil.app unless given.
#
# It reads the credentials from the environment, so none of them is written in the repository:
#   POSTIL_SIGN_IDENTITY   the certificate's name as `security find-identity -v -p codesigning`
#                          lists it, such as "Developer ID Application: Name (TEAMID)". Required.
#   POSTIL_NOTARY_KEY      the App Store Connect API key file (AuthKey_XXXX.p8),
#   POSTIL_NOTARY_KEY_ID   its key ID, and
#   POSTIL_NOTARY_ISSUER   its issuer ID. With all three it notarizes and staples; with none it
#                          signs only and says so; with some, it stops.
set -euo pipefail

usage="usage: scripts/sign.sh [APP]"
root="$(cd "$(dirname "$0")/.." && pwd)"

if [[ $# -gt 1 ]]; then
  echo "$usage" >&2
  exit 2
fi
case "${1:-}" in
-h | --help)
  echo "$usage"
  exit 0
  ;;
-*)
  echo "$usage" >&2
  exit 2
  ;;
esac
app="${1:-$root/target/release/Postil.app}"

if [[ -z "${POSTIL_SIGN_IDENTITY:-}" ]]; then
  echo "set POSTIL_SIGN_IDENTITY to the Developer ID certificate's name" >&2
  echo "(security find-identity -v -p codesigning lists them)" >&2
  exit 2
fi
if [[ ! -d "$app" ]]; then
  echo "no app at $app: run scripts/bundle.sh first" >&2
  exit 1
fi
notary_set=0
for name in POSTIL_NOTARY_KEY POSTIL_NOTARY_KEY_ID POSTIL_NOTARY_ISSUER; do
  if [[ -n "${!name:-}" ]]; then
    notary_set=$((notary_set + 1))
  fi
done
if [[ $notary_set -ne 0 && $notary_set -ne 3 ]]; then
  echo "set all of POSTIL_NOTARY_KEY, POSTIL_NOTARY_KEY_ID and POSTIL_NOTARY_ISSUER, or none" >&2
  exit 2
fi

# The hardened runtime is what notarization requires. Postil needs no exception from it: the
# webview's JIT runs in WebKit's own processes, and the agents it starts are programs of their
# own, signed or not by their makers.
codesign --force --options runtime --timestamp --sign "$POSTIL_SIGN_IDENTITY" "$app"
codesign --verify --strict --verbose=2 "$app"
echo "signed $app"

if [[ $notary_set -eq 0 ]]; then
  echo "not notarized: POSTIL_NOTARY_KEY, POSTIL_NOTARY_KEY_ID and POSTIL_NOTARY_ISSUER are unset"
  exit 0
fi

work="$(mktemp -d)"
trap 'trash "$work"' EXIT
# notarytool takes a zip, a disk image or a package, not a bare bundle.
ditto -c -k --keepParent "$app" "$work/Postil.zip"
xcrun notarytool submit "$work/Postil.zip" \
  --key "$POSTIL_NOTARY_KEY" \
  --key-id "$POSTIL_NOTARY_KEY_ID" \
  --issuer "$POSTIL_NOTARY_ISSUER" \
  --wait
# Stapling puts the notarization ticket in the bundle, so Gatekeeper accepts it offline too.
xcrun stapler staple "$app"
spctl --assess --type execute --verbose=2 "$app"
echo "notarized $app"
