#!/usr/bin/env bash
# manifest-rules.sh — the `[[rule]]` rows of seams.toml whose ids are
# named, verbatim and in the order named, a blank line between two: each
# row from its `[[rule]]` line to the blank line that ends it, its
# comments included. An id seams.toml carries no row for fails the
# render, naming the id, and prints nothing, so a rule deleted from the
# manifest cannot drop out of an excerpt unseen. harness-design.md §4.1's
# excerpt runs it.
set -euo pipefail
cd "$(dirname "$0")/.."
out=""
missing=()
for id in "$@"; do
  row=$(awk -v want="id = \"$id\"" '
    /^\[\[rule\]\][[:space:]]*$/ { row = $0; open = 1; mine = 0; next }
    open && /^[[:space:]]*$/ { if (mine) { print row; found = 1; exit } open = 0; next }
    open { row = row "\n" $0; if ($0 == want || index($0, want " ") == 1) mine = 1 }
    END { if (!found && open && mine) print row }
  ' seams.toml)
  if [ -z "$row" ]; then
    missing+=("\"$id\"")
  elif [ -z "$out" ]; then
    out="$row"
  else
    out="$out"$'\n\n'"$row"
  fi
done
if [ ${#missing[@]} -gt 0 ]; then
  echo "manifest-rules.sh: seams.toml carries no [[rule]] with id ${missing[*]}" >&2
  exit 1
fi
printf '%s\n' "$out"
