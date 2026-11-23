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
#   POSTIL_NOTARY_PROFILE  a notarytool keychain profile, made once from an App Store Connect
#                          API key with `xcrun notarytool store-credentials NAME --key
#                          AuthKey_XXXX.p8 --key-id ID --issuer ISSUER`; the key then lives in
#                          the keychain, and the .p8 file can go. Without it, it signs only.
#   POSTIL_NOTARY_KEYCHAIN the keychain that holds that profile, when it is not the default
#                          one: the release workflow keeps its own for one run.
set -euo pipefail

usage="usage: scripts/sign.sh [APP]"
root="$(cd "$(dirname "$0")/.." && pwd)"

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
fi
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

# The hardened runtime is what notarization requires. Postil needs one exception from it,
# assets/Postil.entitlements: macOS holds Postil responsible for the agents it starts, so an
# agent's `osascript` sends Apple Events as Postil, which the runtime refuses without it. The
# webview's JIT runs in WebKit's own processes and needs none.
codesign --force --options runtime --timestamp \
  --entitlements "$root/assets/Postil.entitlements" \
  --sign "$POSTIL_SIGN_IDENTITY" "$app"
codesign --verify --strict --verbose=2 "$app"
echo "signed $app"

if [[ -z "${POSTIL_NOTARY_PROFILE:-}" ]]; then
  echo "not notarized: POSTIL_NOTARY_PROFILE is unset"
  exit 0
fi

profile=(--keychain-profile "$POSTIL_NOTARY_PROFILE")
if [[ -n "${POSTIL_NOTARY_KEYCHAIN:-}" ]]; then
  profile+=(--keychain "$POSTIL_NOTARY_KEYCHAIN")
fi
work="$(mktemp -d)"
trap 'trash "$work"' EXIT
# notarytool takes a zip, a disk image or a package, not a bare bundle.
ditto -c -k --keepParent "$app" "$work/Postil.zip"
xcrun notarytool submit "$work/Postil.zip" "${profile[@]}" \
  --wait --output-format json >"$work/submission.json"
# notarytool can exit 0 with a rejected submission, so its status is read, and on a rejection
# Apple's log says why.
status="$(plutil -extract status raw -o - "$work/submission.json")"
if [[ "$status" != "Accepted" ]]; then
  id="$(plutil -extract id raw -o - "$work/submission.json")"
  echo "notarization $id: $status" >&2
  xcrun notarytool log "$id" "${profile[@]}" >&2
  exit 1
fi
# Stapling puts the notarization ticket in the bundle, so Gatekeeper accepts it offline too.
xcrun stapler staple "$app"
spctl --assess --type execute --verbose=2 "$app"
echo "notarized $app"
