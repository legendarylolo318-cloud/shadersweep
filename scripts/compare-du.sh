#!/usr/bin/env bash
# Compare shadersweep's per-cache sizes with `du -sb` (apparent bytes) on this machine.
# Usage: scripts/compare-du.sh [path-to-shadersweep-binary]
set -euo pipefail
bin=${1:-target/release/shadersweep}
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }
"$bin" list --json 2>/dev/null | jq -r '.[] | [.appid, .size_bytes, .path, .name] | @tsv' |
{
  n=0; bad=0
  printf '%-10s %14s %14s %8s  %s\n' APPID SHADERSWEEP "DU -SB" DIFF NAME
  while IFS=$'\t' read -r id ours path name; do
    du=$(du -sb -- "$path" | cut -f1)
    diff=$((ours - du))
    [ "$diff" -eq 0 ] || bad=$((bad + 1))
    n=$((n + 1))
    printf '%-10s %14s %14s %8s  %s\n' "$id" "$ours" "$du" "$diff" "$name"
  done
  echo "compared $n caches: $((n - bad)) exact, $bad differ"
  [ "$bad" -eq 0 ]
}
