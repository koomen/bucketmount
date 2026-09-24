#!/bin/bash
# Download the pinned rclone release into binaries/rclone-<target-triple>
# (universal-apple-darwin is lipo-ed from the two arches),
# where Tauri's externalBin expects it. Skips targets already present at the
# right version.
#
#   scripts/fetch-rclone.sh [target-triple...]   # default: the host triple
#
# Environment overrides:
#   RCLONE_VERSION   rclone release to fetch (default pinned below)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RCLONE_VERSION="${RCLONE_VERSION:-v1.75.1}"
TARGETS="${*:-$(rustc -vV | sed -n 's/^host: //p')}"

mkdir -p "$ROOT/binaries"
for t in $TARGETS; do
  if [[ "$t" == universal-apple-darwin ]]; then
    # Tauri's universal build wants one fat binary; stitch the two arches.
    "$0" aarch64-apple-darwin x86_64-apple-darwin
    lipo -create -output "$ROOT/binaries/rclone-$t" \
      "$ROOT/binaries/rclone-aarch64-apple-darwin" "$ROOT/binaries/rclone-x86_64-apple-darwin"
    echo "    $ROOT/binaries/rclone-$t created with lipo"
    continue
  fi
  case "$t" in
    aarch64-apple-darwin) r_arch=osx-arm64 ;;
    x86_64-apple-darwin)  r_arch=osx-amd64 ;;
    *) echo "unknown target $t" >&2; exit 1 ;;
  esac
  dest="$ROOT/binaries/rclone-$t"
  if [[ -x "$dest" ]] && "$dest" version 2>/dev/null | head -1 | grep -q "rclone $RCLONE_VERSION"; then
    echo "    $dest already present"
    continue
  fi
  echo "    fetching rclone $RCLONE_VERSION ($r_arch) -> $dest"
  tmp="$(mktemp -d)"
  curl -fsSL "https://downloads.rclone.org/$RCLONE_VERSION/rclone-$RCLONE_VERSION-$r_arch.zip" -o "$tmp/rclone.zip"
  unzip -q -o "$tmp/rclone.zip" -d "$tmp/x"
  cp "$(find "$tmp/x" -type f -name rclone | head -1)" "$dest"
  chmod +x "$dest"
  rm -rf "$tmp"
done
