#!/bin/sh
# Render the Homebrew formula for a release.
# Usage: render.sh <version> <dir-with-.sha256-files> > Formula/mctui.rb
# The directory must contain mctui-<target>.sha256 ("<hash>  <name>") for the
# four macOS/Linux targets, as published in each GitHub release.
set -eu

version=$1
shadir=$2
tmpl=$(dirname "$0")/mctui.rb.tmpl

sha() {
  f="$shadir/mctui-$1.sha256"
  [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
  h=$(cut -d' ' -f1 "$f")
  echo "$h" | grep -Eq '^[0-9a-f]{64}$' || { echo "bad sha256 in $f" >&2; exit 1; }
  echo "$h"
}

echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$' \
  || { echo "bad version: $version" >&2; exit 1; }

# Assign first: a failing $(sha ...) inside sed's arguments would not stop set -e.
a64_mac=$(sha aarch64-apple-darwin)
x64_mac=$(sha x86_64-apple-darwin)
a64_linux=$(sha aarch64-unknown-linux-musl)
x64_linux=$(sha x86_64-unknown-linux-musl)

sed \
  -e "s/@VERSION@/$version/g" \
  -e "s/@SHA256_AARCH64_APPLE_DARWIN@/$a64_mac/" \
  -e "s/@SHA256_X86_64_APPLE_DARWIN@/$x64_mac/" \
  -e "s/@SHA256_AARCH64_UNKNOWN_LINUX_MUSL@/$a64_linux/" \
  -e "s/@SHA256_X86_64_UNKNOWN_LINUX_MUSL@/$x64_linux/" \
  "$tmpl"
