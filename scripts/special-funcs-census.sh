#!/usr/bin/env bash
# special-funcs-census.sh — which names of `zend_try_compile_special_func`
# the corpus reaches, reproducibly.
#
# Usage: scripts/special-funcs-census.sh <path-to-php-src-tree>
#
# The names are read from the one table the compiler reads them from,
# `crates/php_compile/src/special.rs`, in its order, so a row added there is
# a row here on the next run. For each name, the call sites `name(` in the
# `--FILE--` section of every `.phpt` (case-insensitive, a leading `\`
# allowed, a `->` or `::` before the name excluded since that is a method),
# counted over the per-commit corpus of design §6.1 (`Zend/tests`, `tests`,
# `sapi/cli`) and over the whole tree, as sites and as files. The `written`
# column is the sites whose first argument opens with a quote or a digit,
# an upper bound on the calls the constant shape can take: the regex does
# not see that `strlen('a' . $b)` does not fold.
#
# Grep-shaped Python over the text; no build is needed, so the numbers are
# reproducible on the pinned tag from a plain checkout.
set -euo pipefail
ROOT="${1:?usage: special-funcs-census.sh <php-src tree>}"
[ -d "$ROOT/Zend/tests" ] || { echo "not a php-src tree: $ROOT" >&2; exit 2; }
HERE="$(cd "$(dirname "$0")/.." && pwd)"
exec python3 - "$ROOT" "$HERE/crates/php_compile/src/special.rs" <<'PY'
import re, sys, os, glob, collections
root, table = sys.argv[1], sys.argv[2]
names = re.findall(r'^\s*\(b"([a-z_]+)", Special::', open(table).read(), re.M)
per_commit = ("Zend/tests", "tests", "sapi/cli")

def file_section(text):
    m = re.search(r"^--FILE(?:EOF)?--\n(.*?)(?=^--[A-Z_]+--\n|\Z)", text, re.S | re.M)
    return m.group(1) if m else ""

call = {n: re.compile(r"(?<![\w$>:\\])\\?" + n + r"\s*\(\s*(['\"0-9]?)", re.I) for n in names}
sites = collections.defaultdict(collections.Counter)
files = collections.defaultdict(collections.Counter)
written = collections.defaultdict(collections.Counter)
total = collections.Counter()
for path in glob.glob(os.path.join(root, "**", "*.phpt"), recursive=True):
    rel = os.path.relpath(path, root)
    scopes = ["whole"] + ["per-commit" for d in per_commit if rel.startswith(d + os.sep)][:1]
    for s in scopes:
        total[s] += 1
    try:
        body = file_section(open(path, encoding="utf-8", errors="replace").read())
    except OSError:
        continue
    for n in names:
        hits = call[n].findall(body)
        if not hits:
            continue
        for s in scopes:
            sites[n][s] += len(hits)
            files[n][s] += 1
            written[n][s] += sum(1 for h in hits if h)

print("# zend_try_compile_special_func census (scripts/special-funcs-census.sh) over", root)
print(f"# phpt files: whole {total['whole']}, per-commit corpus {total['per-commit']}")
print()
print("| name | per-commit sites | files | written | whole sites | files | written |")
print("|---|---|---|---|---|---|---|")
for n in names:
    print(f"| `{n}` | {sites[n]['per-commit']} | {files[n]['per-commit']} | {written[n]['per-commit']} "
          f"| {sites[n]['whole']} | {files[n]['whole']} | {written[n]['whole']} |")
print()
reached = [n for n in names if sites[n]["per-commit"]]
print(f"names reached by the per-commit corpus: {len(reached)} of {len(names)}; "
      f"not reached: {', '.join(n for n in names if n not in reached) or 'none'}")
PY
