#!/usr/bin/env bash
set -euo pipefail

# Packages `agent-superviser-gpui` into a native macOS `.app` bundle
# (`target/[<target>/]{debug,release}/Agent Superviser.app`) with `AppIcon.icns`,
# `Info.plist`, and bundled `Contents/Resources/llama.cpp/` (`llama-server` + dylibs).
#
# Usage:
#   ./scripts/bundle-gpui-macos.sh                                    # build + bundle debug app
#   ./scripts/bundle-gpui-macos.sh --release                          # build + bundle release app
#   ./scripts/bundle-gpui-macos.sh --release --target aarch64-apple-darwin
#   ./scripts/bundle-gpui-macos.sh --no-build --no-llama              # re-bundle existing binary offline
#   ./scripts/bundle-gpui-macos.sh --release --dmg dist/App.dmg       # also create a macOS DMG

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="debug"
TARGET=""
DO_BUILD=1
BUNDLE_LLAMA=1
DMG_PATH=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --release)
      PROFILE="release"
      shift
      ;;
    --no-build)
      DO_BUILD=0
      shift
      ;;
    --no-llama)
      BUNDLE_LLAMA=0
      shift
      ;;
    --target)
      TARGET="${2:?missing value for --target}"
      shift 2
      ;;
    --dmg)
      DMG_PATH="${2:?missing value for --dmg}"
      shift 2
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

if [ "$DO_BUILD" -eq 1 ]; then
  BUILD_ARGS=(-p agent-superviser-gpui)
  if [ "$PROFILE" = "release" ]; then
    BUILD_ARGS+=(--release)
  fi
  if [ -n "$TARGET" ]; then
    BUILD_ARGS+=(--target "$TARGET")
  fi
  cargo build "${BUILD_ARGS[@]}"
fi

if [ -n "$TARGET" ]; then
  OUT_DIR="$ROOT/target/$TARGET/$PROFILE"
else
  OUT_DIR="$ROOT/target/$PROFILE"
fi

BIN="$OUT_DIR/agent-superviser-gpui"
APP="$OUT_DIR/Agent Superviser.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
ICNS="$RESOURCES/AppIcon.icns"

if [ ! -f "$BIN" ]; then
  echo "Binary not found: $BIN" >&2
  exit 1
fi

rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES"
cp "$ROOT/app-gpui/Info.plist" "$CONTENTS/Info.plist"
cp "$BIN" "$MACOS/agent-superviser-gpui"
chmod +x "$MACOS/agent-superviser-gpui"

# Generate AppIcon.icns from app/icons/*.png.
python3 - "$ROOT/app/icons" "$ICNS" <<'PY'
import os, struct, sys

icons_dir, out_path = sys.argv[1], sys.argv[2]
entries = [
    (b"ic09", os.path.join(icons_dir, "icon.png")),     # 512x512 PNG
    (b"ic07", os.path.join(icons_dir, "128x128.png")),  # 128x128 PNG
    (b"icp5", os.path.join(icons_dir, "32x32.png")),    # 32x32 PNG
]
chunks = []
for ostype, path in entries:
    if os.path.isfile(path):
        with open(path, "rb") as f:
            data = f.read()
        chunks.append(ostype + struct.pack(">I", 8 + len(data)) + data)

body = b"".join(chunks)
with open(out_path, "wb") as f:
    f.write(b"icns" + struct.pack(">I", 8 + len(body)) + body)
PY

if [ "$BUNDLE_LLAMA" -eq 1 ]; then
  if [ -n "$TARGET" ]; then
    "$ROOT/scripts/fetch-llama-cpp.sh" --target "$TARGET"
  else
    "$ROOT/scripts/fetch-llama-cpp.sh"
  fi
  cp -R "$ROOT/app/binaries/llama.cpp" "$RESOURCES/llama.cpp"
  chmod +x "$RESOURCES/llama.cpp/llama-server"
fi

touch "$APP"

if [ -n "$DMG_PATH" ]; then
  STAGE="$(mktemp -d)"
  trap 'rm -rf "$STAGE"' EXIT
  ditto "$APP" "$STAGE/Agent Superviser.app"
  ln -s /Applications "$STAGE/Applications"
  mkdir -p "$(dirname "$DMG_PATH")"
  hdiutil create \
    -volname "Agent Superviser" \
    -srcfolder "$STAGE" \
    -ov \
    -format UDZO \
    -imagekey zlib-level=9 \
    "$DMG_PATH"
fi

echo "$APP"
