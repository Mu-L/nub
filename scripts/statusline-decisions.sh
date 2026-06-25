#!/usr/bin/env bash
# Statusline: glanceable count of decisions awaiting the maintainer.
# Reads .fray/decisions.md (agent-maintained), counts "- " lines, prints one terse line.
# Pure file read, no network — must stay <50ms. stdin JSON is ignored.
set -euo pipefail

# Resolve repo root relative to this script so the statusline works from any cwd.
dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
file="$dir/.fray/decisions.md"

# Drain stdin (Claude pipes session JSON in) without blocking.
cat >/dev/null 2>&1 || true

if [ ! -s "$file" ]; then
  printf '✓ no pending decisions'
  exit 0
fi

# Count list items (grep -c exits non-zero on zero matches; default to 0).
n=$(grep -c '^- ' "$file" 2>/dev/null) || n=0
[ -n "$n" ] || n=0

if [ "$n" -eq 0 ]; then
  printf '✓ no pending decisions'
  exit 0
fi

# First three slugs (the [bracketed] tag, else the bare line) for context.
slugs=$(grep '^- ' "$file" \
  | sed -E -e 's/^- \[([^]]+)\].*/\1/' -e 's/^- //' \
  | head -3 | awk 'NR>1{printf ", "} {printf "%s", $0} END{print ""}')
printf '⚖ %s decision(s) pending [%s …] — see .fray/decisions.md' "$n" "$slugs"
