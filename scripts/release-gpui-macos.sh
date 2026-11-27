#!/usr/bin/env bash
set -euo pipefail

# Builds, signs (Developer ID + Hardened Runtime), notarizes, staples, and packages
# the native GPUI macOS app (`Agent Superviser.app`) + bundled `llama.cpp` into a
# distributable `.dmg` and `SHA256SUMS.txt` in `dist/`.
#
# Usage:
#   ./scripts/release-gpui-macos.sh --notary-profile <keychain-profile>
#   ./scripts/release-gpui-macos.sh --apple-id you@example.com --team-id TEAMID --password-file /path/to/app-pw
#   ./scripts/release-gpui-macos.sh --notary-profile <profile> --draft-release
#
# Environment variables (overridden by flags):
#   APPLE_SIGNING_IDENTITY  Developer ID Application identity (auto-detected if unique in Keychain)
#   NOTARY_PROFILE          Keychain profile created with `xcrun notarytool store-credentials`
#   APPLE_ID                Apple Account email (if not using NOTARY_PROFILE)
#   APPLE_TEAM_ID           10-char Apple Developer Team ID (if not using NOTARY_PROFILE)
#   APPLE_PASSWORD          App-specific password (if not using NOTARY_PROFILE / --password-file)

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="aarch64-apple-darwin"
IDENTITY="${APPLE_SIGNING_IDENTITY:-}"
NOTARY_PROFILE="${NOTARY_PROFILE:-}"
APPLE_ID="${APPLE_ID:-}"
APPLE_TEAM_ID="${APPLE_TEAM_ID:-}"
APPLE_PASSWORD="${APPLE_PASSWORD:-}"
PASSWORD_FILE=""
NO_BUILD=0
NO_NOTARIZE=0
DRAFT_RELEASE=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --target)
      TARGET="${2:?missing value for --target}"
      shift 2
      ;;
    --identity)
      IDENTITY="${2:?missing value for --identity}"
      shift 2
      ;;
    --notary-profile)
      NOTARY_PROFILE="${2:?missing value for --notary-profile}"
      shift 2
      ;;
    --apple-id)
      APPLE_ID="${2:?missing value for --apple-id}"
      shift 2
      ;;
    --team-id)
      APPLE_TEAM_ID="${2:?missing value for --team-id}"
      shift 2
      ;;
    --password-file)
      PASSWORD_FILE="${2:?missing value for --password-file}"
      shift 2
      ;;
    --no-build)
      NO_BUILD=1
      shift
      ;;
    --no-notarize)
      NO_NOTARIZE=1
      shift
      ;;
    --draft-release)
      DRAFT_RELEASE=1
      shift
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

if [ -n "$PASSWORD_FILE" ]; then
  APPLE_PASSWORD="$(tr -d '\r\n' < "$PASSWORD_FILE")"
fi

VERSION="$(python3 - "$ROOT" <<'PY'
import json, os, plistlib, sys, tomllib

root = sys.argv[1]
cargo = tomllib.load(open(os.path.join(root, "Cargo.toml"), "rb"))["workspace"]["package"]["version"]
plist = plistlib.load(open(os.path.join(root, "app-gpui", "Info.plist"), "rb"))["CFBundleShortVersionString"]
tauri = json.load(open(os.path.join(root, "app", "tauri.conf.json")))["version"]
if not (cargo == plist == tauri):
    sys.exit(f"Version mismatch: Cargo.toml={cargo}, app-gpui/Info.plist={plist}, app/tauri.conf.json={tauri}")
print(cargo)
PY
)"

if [ -z "$IDENTITY" ]; then
  MATCHES="$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p')"
  COUNT="$(printf '%s\n' "$MATCHES" | sed '/^$/d' | wc -l | tr -d ' ')"
  if [ "$COUNT" -eq 1 ]; then
    IDENTITY="$(printf '%s\n' "$MATCHES" | sed '/^$/d')"
    echo "Using signing identity: $IDENTITY"
  elif [ "$COUNT" -eq 0 ]; then
    echo "No 'Developer ID Application' signing identity found in Keychain." >&2
    exit 1
  else
    echo "Multiple 'Developer ID Application' identities found; pass --identity:" >&2
    printf '%s\n' "$MATCHES" >&2
    exit 1
  fi
fi

if [ "$NO_NOTARIZE" -eq 0 ]; then
  if [ -z "$NOTARY_PROFILE" ]; then
    if [ -z "$APPLE_ID" ] || [ -z "$APPLE_TEAM_ID" ] || [ -z "$APPLE_PASSWORD" ]; then
      echo "Notarization credentials missing. Pass --notary-profile <name>, or --apple-id / --team-id / --password-file (or set NOTARY_PROFILE / APPLE_ID / APPLE_TEAM_ID / APPLE_PASSWORD)." >&2
      exit 1
    fi
  fi
fi

notary_auth_args() {
  if [ -n "$NOTARY_PROFILE" ]; then
    printf '%s\0' --keychain-profile "$NOTARY_PROFILE"
  else
    printf '%s\0' --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID"
  fi
}

notarize() {
  local path="$1"
  local auth=()
  while IFS= read -r -d '' arg; do
    auth+=("$arg")
  done < <(notary_auth_args)

  echo "Submitting $path for Apple notarization..."
  local out status id
  out="$(xcrun notarytool submit "$path" "${auth[@]}" --wait --timeout 1h --output-format json)"
  echo "$out"
  status="$(python3 -c 'import json, sys; print(json.loads(sys.argv[1]).get("status", ""))' "$out")"
  id="$(python3 -c 'import json, sys; print(json.loads(sys.argv[1]).get("id", ""))' "$out")"
  if [ "$status" != "Accepted" ]; then
    echo "Notarization of $path failed with status: $status" >&2
    if [ -n "$id" ]; then
      xcrun notarytool log "$id" "${auth[@]}" >&2 || true
    fi
    exit 1
  fi
}

BUNDLE_ARGS=(--release --target "$TARGET")
if [ "$NO_BUILD" -eq 1 ]; then
  BUNDLE_ARGS+=(--no-build)
fi

"$ROOT/scripts/bundle-gpui-macos.sh" "${BUNDLE_ARGS[@]}"

APP="$ROOT/target/$TARGET/release/Agent Superviser.app"
LLAMA_DIR="$APP/Contents/Resources/llama.cpp"

shopt -s nullglob
echo "Signing llama.cpp libraries and binary..."
for file in "$LLAMA_DIR"/*.dylib; do
  codesign --force --options runtime --timestamp --sign "$IDENTITY" "$file"
done
codesign --force --options runtime --timestamp --sign "$IDENTITY" "$LLAMA_DIR/llama-server"

echo "Signing Agent Superviser.app..."
codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

if [ "$NO_NOTARIZE" -eq 0 ]; then
  APP_ZIP="$TMP_DIR/Agent-Superviser.zip"
  ditto -c -k --keepParent "$APP" "$APP_ZIP"
  notarize "$APP_ZIP"
  rm -f "$APP_ZIP"
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
  spctl --assess --type execute --verbose=2 "$APP"
fi

ARCH="${TARGET%%-*}"
DIST_DIR="$ROOT/dist"
mkdir -p "$DIST_DIR"
STAGE="$TMP_DIR/dmg-stage"
mkdir -p "$STAGE"
ditto "$APP" "$STAGE/Agent Superviser.app"
ln -s /Applications "$STAGE/Applications"

DMG_NAME="Agent-Superviser_${VERSION}_${ARCH}.dmg"
DMG_PATH="$DIST_DIR/$DMG_NAME"
echo "Creating $DMG_PATH..."
hdiutil create \
  -volname "Agent Superviser" \
  -srcfolder "$STAGE" \
  -ov \
  -format UDZO \
  -imagekey zlib-level=9 \
  "$DMG_PATH"

echo "Signing DMG..."
codesign --force --timestamp --sign "$IDENTITY" "$DMG_PATH"

if [ "$NO_NOTARIZE" -eq 0 ]; then
  notarize "$DMG_PATH"
  xcrun stapler staple "$DMG_PATH"
  xcrun stapler validate "$DMG_PATH"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG_PATH"
fi

(cd "$DIST_DIR" && shasum -a 256 "$DMG_NAME" > SHA256SUMS.txt)
echo "Release artifacts ready:"
ls -lh "$DMG_PATH" "$DIST_DIR/SHA256SUMS.txt"
cat "$DIST_DIR/SHA256SUMS.txt"

if [ "$DRAFT_RELEASE" -eq 1 ]; then
  TAG="v$VERSION"
  GH_ARGS=(
    "$TAG"
    "$DMG_PATH"
    "$DIST_DIR/SHA256SUMS.txt"
    --draft
    --verify-tag
    --title "Agent Superviser $VERSION"
    --generate-notes
  )
  if [[ "$TAG" == *-* ]]; then
    GH_ARGS+=(--prerelease)
  fi
  gh release create "${GH_ARGS[@]}"
fi
