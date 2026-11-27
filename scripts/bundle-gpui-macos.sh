#!/usr/bin/env bash
set -euo pipefail

# Packages `agent-superviser-gpui` into a native macOS `.app` bundle
# (`target/{debug,release}/Agent Superviser.app`) so macOS Dock hover,
# Cmd+Tab, menu bar, Finder, and system notifications all show
# "Agent Superviser" and the app icon natively.
#
# Usage:
#   ./scripts/bundle-gpui-macos.sh            # build + bundle debug app
#   ./scripts/bundle-gpui-macos.sh --release  # build + bundle release app
#   ./scripts/bundle-gpui-macos.sh --no-build # bundle existing debug binary

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="debug"
DO_BUILD=1

for arg in "$@"; do
  case "$arg" in
    --release) PROFILE="release" ;;
    --no-build) DO_BUILD=0 ;;
    *) echo "Unknown option: $arg" >&2; exit 1 ;;
  esac
done

if [ "$DO_BUILD" -eq 1 ]; then
  if [ "$PROFILE" = "release" ]; then
    cargo build -p agent-superviser-gpui --release
  else
    cargo build -p agent-superviser-gpui
  fi
fi

BIN="$ROOT/target/$PROFILE/agent-superviser-gpui"
APP="$ROOT/target/$PROFILE/Agent Superviser.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
ICON_PNG="$ROOT/app/icons/icon.png"
ICNS="$RESOURCES/AppIcon.icns"

mkdir -p "$MACOS" "$RESOURCES"
cp "$ROOT/app-gpui/Info.plist" "$CONTENTS/Info.plist"
cp "$BIN" "$MACOS/agent-superviser-gpui"
chmod +x "$MACOS/agent-superviser-gpui"

# Generate AppIcon.icns from app/icons/*.png if missing or older than icon.png.
if [ ! -f "$ICNS" ] || [ "$ICON_PNG" -nt "$ICNS" ]; then
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
fi

touch "$APP"
echo "$APP"
