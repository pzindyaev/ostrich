#!/usr/bin/env bash
# Packages a built ostrich binary as a release archive.
#
#   scripts/package.sh <version> <rust-target> <binary> <out-dir>
#
# Produces <out-dir>/ostrich_<version>_<os>_<arch>.tar.gz holding ostrich,
# LICENSE, README.md and docs/USAGE.md, and appends its SHA-256 to
# <out-dir>/checksums.txt.
#
# Privacy: nothing in an archive identifies who built it. Entries are owned
# by root/root and stamped with the commit date rather than the builder's
# user and clock, so two people building the same commit get the same bytes.
set -euo pipefail

version=$1
target=$2
binary=$3
out=$4

case "$target" in
  x86_64-*linux*)  os=linux;  arch=amd64 ;;
  aarch64-*linux*) os=linux;  arch=arm64 ;;
  x86_64-*darwin*) os=darwin; arch=amd64 ;;
  aarch64-*darwin*) os=darwin; arch=arm64 ;;
  *) echo "package.sh: unknown target $target" >&2; exit 1 ;;
esac

name="ostrich_${version}_${os}_${arch}"
mtime=$(git log -1 --format=%cI 2>/dev/null || date -u +%Y-%m-%dT%H:%M:%SZ)

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" "$out"
install -m755 "$binary" "$stage/$name/ostrich"
install -m644 LICENSE README.md "$stage/$name/"
mkdir -p "$stage/$name/docs"
install -m644 docs/USAGE.md "$stage/$name/docs/USAGE.md"

# GNU tar takes --owner/--group/--mtime; bsdtar (macOS) takes --uid/--gid and
# needs the files touched instead.
if tar --version 2>/dev/null | grep -q GNU; then
  tar --owner=0 --group=0 --numeric-owner --mtime="$mtime" --sort=name \
      -C "$stage" -czf "$out/$name.tar.gz" "$name"
else
  find "$stage/$name" -exec touch -d "$mtime" {} + 2>/dev/null \
    || find "$stage/$name" -exec touch -t "$(date -j -f %Y-%m-%dT%H:%M:%S%z "${mtime%:*}${mtime##*:}" +%Y%m%d%H%M.%S)" {} +
  tar --uid 0 --gid 0 --uname root --gname root \
      -C "$stage" -czf "$out/$name.tar.gz" "$name"
fi

(cd "$out" && { sha256sum "$name.tar.gz" 2>/dev/null || shasum -a 256 "$name.tar.gz"; } >> checksums.txt)
echo "$out/$name.tar.gz"
