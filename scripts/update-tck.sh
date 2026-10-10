#!/usr/bin/env bash
# Re-vendor the TCKs at the commits pinned in tck/VERSIONS.
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
grep -v '^#' tck/VERSIONS | while read -r name url rev paths; do
  [ -z "$name" ] && continue
  git clone --quiet "$url" "$tmp/$name"
  git -C "$tmp/$name" checkout --quiet "$rev"
  rm -rf "tck/$name"; mkdir -p "tck/$name"
  for p in ${paths}; do cp -r "$tmp/$name/$p" "tck/$name/"; done
  cp "$tmp/$name/LICENSE" "tck/$name/"
  for n in NOTICE NOTICE.md; do [ -f "$tmp/$name/$n" ] && cp "$tmp/$name/$n" "tck/$name/"; done
done
echo "Vendored. Run: TCK_UPDATE_BASELINE=1 cargo test --test tck"
