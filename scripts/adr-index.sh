#!/usr/bin/env bash
# adr-index.sh — the ADR index, one line per file under docs/adr/, in
# number order: `- ADR-<number> (<status>, <date>): <title>`. The number is
# the file name's, the status and date are the front matter's, the title is
# the first heading. The `adrs` region of AGENTS.md runs it (harness §5).
set -euo pipefail
cd "$(dirname "$0")/.."
for f in $(ls docs/adr/[0-9]*.md | sort); do
  number=$(basename "$f" | cut -c1-4)
  status=$(sed -n 's/^status: *//p' "$f" | head -1)
  date=$(sed -n 's/^date: *//p' "$f" | head -1)
  title=$(sed -n 's/^# //p' "$f" | head -1)
  echo "- ADR-${number} (${status}, ${date}): ${title}"
done
