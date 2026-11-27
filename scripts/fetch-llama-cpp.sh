#!/usr/bin/env bash
set -euo pipefail

# Downloads and SHA-256 verifies the pinned llama.cpp release from `app/sidecars.json`
# into `app/binaries/llama.cpp/` along with all `@rpath/*.dylib` dependencies and `LICENSE`.
# Reuses existing files if `app/binaries/.llama.cpp.sha256` already matches the pinned digest.
#
# Usage:
#   ./scripts/fetch-llama-cpp.sh
#   ./scripts/fetch-llama-cpp.sh --target aarch64-apple-darwin

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --target)
      TARGET="${2:?missing value for --target}"
      shift 2
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

if [ -z "$TARGET" ]; then
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) TARGET="aarch64-apple-darwin" ;;
    Darwin:x86_64) TARGET="x86_64-apple-darwin" ;;
    *)
      echo "Unsupported host platform for macOS llama.cpp fetch: $(uname -s)/$(uname -m)" >&2
      exit 1
      ;;
  esac
fi

python3 - "$ROOT" "$TARGET" <<'PY'
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile

root, target = sys.argv[1], sys.argv[2]
sidecars_path = os.path.join(root, "app", "sidecars.json")
with open(sidecars_path, "r", encoding="utf-8") as f:
    pins = json.load(f)

pin = pins.get("llama-server")
if not isinstance(pin, dict):
    sys.exit("app/sidecars.json is missing 'llama-server'")

build = pin.get(target)
if not isinstance(build, dict):
    sys.exit(f"app/sidecars.json has no llama-server entry for {target}")

url = build["url"]
expected_sha = build["sha256"]
member = build["member"]
folder = pin.get("folder", "llama.cpp")

binaries_dir = os.path.join(root, "app", "binaries")
dest = os.path.join(binaries_dir, folder)
marker = os.path.join(binaries_dir, f".{folder}.sha256")
os.makedirs(binaries_dir, exist_ok=True)

if (
    os.path.isfile(os.path.join(dest, "llama-server"))
    and os.path.isfile(marker)
    and open(marker, "r", encoding="utf-8").read().strip() == expected_sha
):
    print(f"Using cached {dest} ({expected_sha[:12]})")
    sys.exit(0)

with tempfile.TemporaryDirectory(prefix="superviser-llama-") as tmp:
    archive = os.path.join(tmp, url.rsplit("/", 1)[-1])
    print(f"Downloading {url}...")
    subprocess.run(
        ["curl", "--fail", "--location", "--silent", "--show-error", "--output", archive, url],
        check=True,
    )

    h = hashlib.sha256()
    with open(archive, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    actual_sha = h.hexdigest()
    if actual_sha != expected_sha:
        sys.exit(
            f"SHA-256 mismatch for {url}: expected {expected_sha}, got {actual_sha}"
        )

    unpacked = os.path.join(tmp, "unpacked")
    os.makedirs(unpacked, exist_ok=True)
    if url.endswith(".zip"):
        subprocess.run(["unzip", "-q", archive, "-d", unpacked], check=True)
    else:
        subprocess.run(["tar", "-xzf", archive, "-C", unpacked], check=True)

    program = os.path.join(unpacked, member)
    if not os.path.isfile(program):
        sys.exit(f"Expected binary not found in archive: {member}")

    source_dir = os.path.dirname(program)
    if os.path.exists(dest):
        shutil.rmtree(dest)
    os.makedirs(dest, exist_ok=True)

    queue = [os.path.basename(program)]
    copied = set()
    while queue:
        name = queue.pop()
        if name in copied:
            continue
        copied.add(name)
        src_file = os.path.join(source_dir, name)
        dst_file = os.path.join(dest, name)
        if not os.path.exists(src_file):
            sys.exit(f"Missing linked library in archive: {src_file}")
        shutil.copy2(src_file, dst_file, follow_symlinks=True)

        out = subprocess.run(
            ["otool", "-L", src_file],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        for line in out.splitlines():
            stripped = line.strip()
            if stripped.startswith("@rpath/"):
                dep = stripped[len("@rpath/"):].split(" ", 1)[0]
                if dep not in copied:
                    queue.append(dep)

    llama_dst = os.path.join(dest, os.path.basename(program))
    os.chmod(llama_dst, os.stat(llama_dst).st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    license_src = os.path.join(source_dir, "LICENSE")
    if os.path.isfile(license_src):
        shutil.copy2(license_src, os.path.join(dest, "LICENSE"), follow_symlinks=True)

    with open(marker, "w", encoding="utf-8") as f:
        f.write(expected_sha + "\n")

print(f"Bundled {len(copied)} llama.cpp binaries/libraries into {dest}")
PY
