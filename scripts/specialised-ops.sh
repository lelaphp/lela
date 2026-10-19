#!/usr/bin/env bash
# specialised-ops.sh — the opcodes ops.toml marks `specialise`, in opcode
# order, as one line: `NAME`, `NAME`, ... The `specialised` region of
# third-engine-design.md §5.8 runs it (#3001).
set -euo pipefail
cd "$(dirname "$0")/.."
awk '/^name = /{n=$3; gsub(/"/, "", n)}
     /^specialise = true$/{printf "%s`%s`", sep, n; sep=", "}
     END{print ""}' ops.toml
