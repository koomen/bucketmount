#!/bin/bash
# Build BucketMount.app with the Tauri bundler: a universal (Apple silicon +
# Intel) bundle with rclone inside, so the app has no dependencies on the
# target Mac.
#
#   scripts/build-app.sh            # -> dist/BucketMount.app, dist/BucketMount_<version>.dmg, dist/BucketMount-<version>.zip
#
# Environment overrides:
#   ARCHS            "universal" (default) or "native" for a quick build of the current architecture
#   RCLONE_VERSION   rclone release to bundle (default pinned in scripts/fetch-rclone.sh)
#   OUT              output directory (default <build dir>/dist)
#   APPLE_SIGNING_IDENTITY, APPLE_CERTIFICATE, APPLE_CERTIFICATE_PASSWORD,
#   APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID
#                    picked up by the Tauri bundler for Developer ID signing + notarization.
#                    Without them the app is ad-hoc signed.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCHS="${ARCHS:-universal}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"

command -v cargo-tauri >/dev/null || { echo "Tauri CLI missing: cargo install tauri-cli --version '^2' --locked" >&2; exit 1; }

# Cargo needs file locks, which network mounts (NFS, SMB, rclone) do not
# provide. Probe for that and, when the checkout lives on one, build from a
# local mirror instead.
SRC="$ROOT"
can_lock() {
  /usr/bin/perl -e 'use Fcntl qw(:flock); open(F, ">", "$ARGV[0]/.locktest") or exit 1; my $ok = flock(F, LOCK_EX | LOCK_NB); close(F); unlink "$ARGV[0]/.locktest"; exit($ok ? 0 : 1)' "$1"
}
if ! can_lock "$ROOT"; then
  SRC="$HOME/.cache/bucketmount/src"
  echo "Source directory does not support file locks (network mount?); mirroring to $SRC for the build"
  mkdir -p "$SRC"
  rsync -a --delete --exclude target --exclude dist --exclude .git --exclude '._*' "$ROOT/" "$SRC/"
  export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/bucketmount/target}"
fi
TARGET_DIR="${CARGO_TARGET_DIR:-$SRC/target}"
OUT="${OUT:-$SRC/dist}"

case "$ARCHS" in
  universal) TARGETS="aarch64-apple-darwin x86_64-apple-darwin"; TAURI_TARGET="universal-apple-darwin"; BUNDLE_DIR="$TARGET_DIR/universal-apple-darwin/release/bundle" ;;
  native)    TARGETS="$(rustc -vV | sed -n 's/^host: //p')"; TAURI_TARGET=""; BUNDLE_DIR="$TARGET_DIR/release/bundle" ;;
  *) echo "ARCHS must be 'universal' or 'native'" >&2; exit 1 ;;
esac

echo "==> Fetching rclone for: $TARGETS"
"$SRC/scripts/fetch-rclone.sh" $TARGETS $TAURI_TARGET
for t in $TARGETS; do rustup target add "$t" >/dev/null 2>&1 || true; done

echo "==> Building BucketMount $VERSION ($ARCHS)"
cd "$SRC"
if [[ -n "$TAURI_TARGET" ]]; then
  cargo tauri build --target "$TAURI_TARGET" --bundles app,dmg
else
  cargo tauri build --bundles app,dmg
fi

APP="$BUNDLE_DIR/macos/BucketMount.app"
if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  echo "==> No signing identity; ad-hoc signing the bundle"
  codesign --force --deep --sign - "$APP"
fi

echo "==> Collecting output in $OUT"
mkdir -p "$OUT"
rm -rf "$OUT/BucketMount.app" "$OUT"/BucketMount*.dmg "$OUT"/BucketMount*.zip
ditto "$APP" "$OUT/BucketMount.app"
cp "$BUNDLE_DIR"/dmg/*.dmg "$OUT/" 2>/dev/null || echo "    (no dmg produced)"
ditto -c -k --keepParent "$OUT/BucketMount.app" "$OUT/BucketMount-$VERSION.zip"
ls -la "$OUT"
echo "Done: $OUT/BucketMount.app"
