#!/usr/bin/env bash
# Compare screenshot output against the local baseline.
#
#   scripts/screenshot-diff.sh          # report changed scene names (exit 1 if any)
#   scripts/screenshot-diff.sh --accept # copy out/ → baseline/ (take a new baseline)
#
# Both directories live under .ox/ (gitignored), so baselines are per-developer
# and never committed. Run `cargo run --bin screenshots --features screenshot-test` first.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/.ox/screenshots/out"
base="$root/.ox/screenshots/baseline"

if [[ "${1:-}" == "--accept" ]]; then
  [[ -d "$out" ]] || { echo "no output directory: $out" >&2; exit 2; }
  mkdir -p "$base"
  cp "$out"/*.png "$base"/
  echo "baseline updated from $out"
  exit 0
fi

[[ -d "$base" ]] || { echo "no baseline: run with --accept first" >&2; exit 2; }

changed=0
for png in "$out"/*.png; do
  [[ -e "$png" ]] || continue
  name="$(basename "$png")"
  if [[ ! -f "$base/$name" ]]; then
    echo "added: $name"
    changed=1
  elif ! cmp -s "$png" "$base/$name"; then
    echo "changed: $name"
    changed=1
  fi
done
for png in "$base"/*.png; do
  [[ -e "$png" ]] || continue
  name="$(basename "$png")"
  if [[ ! -f "$out/$name" ]]; then
    echo "removed: $name"
    changed=1
  fi
done

if [[ "$changed" -eq 0 ]]; then
  echo "no screenshot changes"
fi
exit "$changed"
