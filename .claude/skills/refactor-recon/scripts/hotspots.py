#!/usr/bin/env python3
"""
hotspots.py — git-history mining for refactor reconnaissance.

Produces three evidence tables for a repository (or subtree):

  1. HOTSPOTS    — files ranked by churn x size (comprehension-debt proxy)
  2. FIX MAGNETS — files that keep attracting fix/bug/revert commits
  3. CO-CHANGE   — file pairs that repeatedly change in the same commit;
                   pairs living in distant parts of the tree are flagged,
                   since they suggest a misdrawn module boundary

Stdlib only; requires `git` on PATH. Run from anywhere with --repo, or
from inside the repository.

Usage:
  python3 hotspots.py [--repo PATH] [--since "18 months ago"] [--top 25]
                      [--path SUBDIR ...] [--min-pair 4]
                      [--max-commit-files 25] [--json]

Notes:
  - Merge commits are skipped (they double-count their parents' work).
  - Commits touching more than --max-commit-files files are skipped for
    the co-change table only: mass renames and reformat sweeps would
    otherwise drown the real coupling signal.
  - Renames are not followed; a renamed file counts as two paths. That is
    acceptable at recon granularity.
"""

import argparse
import collections
import itertools
import json
import math
import os
import re
import subprocess
import sys

EXCLUDE_PATTERNS = [
    r"(^|/)(node_modules|vendor|third_party|thirdparty|dist|build|target|out|\.git)(/|$)",
    r"(^|/)(package-lock\.json|yarn\.lock|pnpm-lock\.yaml|Cargo\.lock|go\.sum|poetry\.lock|composer\.lock)$",
    r"\.(min\.js|min\.css|map|snap|lock|svg|png|jpe?g|gif|ico|pdf|woff2?|ttf|eot)$",
    r"(^|/)(generated|gen|__snapshots__)(/|$)",
]
EXCLUDE_RE = re.compile("|".join(EXCLUDE_PATTERNS))

FIX_RE = re.compile(r"\b(fix(es|ed)?|bug|hotfix|revert(s|ed)?|regression)\b", re.IGNORECASE)

MAX_LOC_FILE_BYTES = 5 * 1024 * 1024  # don't line-count huge files


def run_git(repo, args):
    result = subprocess.run(
        ["git", "-C", repo] + args,
        capture_output=True, text=True, errors="replace",
    )
    if result.returncode != 0:
        sys.stderr.write("git error: %s\n" % result.stderr.strip())
        sys.exit(2)
    return result.stdout


def iter_commits(repo, since, paths):
    """Yield (sha, subject, [files]) per non-merge commit in the window."""
    fmt = "%x1e%H%x1f%s"
    args = ["log", "--since=%s" % since, "--no-merges",
            "--name-only", "--format=%s" % fmt]
    if paths:
        args += ["--"] + paths
    out = run_git(repo, args)
    for record in out.split("\x1e"):
        record = record.strip("\n")
        if not record:
            continue
        header, _, body = record.partition("\n")
        sha, _, subject = header.partition("\x1f")
        files = [
            line.strip() for line in body.splitlines()
            if line.strip() and not EXCLUDE_RE.search(line.strip())
        ]
        yield sha, subject, files


def count_lines(repo, path):
    full = os.path.join(repo, path)
    try:
        if os.path.getsize(full) > MAX_LOC_FILE_BYTES:
            return None
        with open(full, "rb") as fh:
            return fh.read().count(b"\n")
    except OSError:
        return None  # deleted, renamed away, or unreadable


def top_component(path):
    return path.split("/", 1)[0]


def analyze(repo, since, paths, max_commit_files, min_pair):
    churn = collections.Counter()      # file -> commits touching it
    fixes = collections.Counter()      # file -> fix-flavored commits
    reverts = 0
    pair_counts = collections.Counter()
    n_commits = 0

    for _sha, subject, files in iter_commits(repo, since, paths):
        n_commits += 1
        is_fix = bool(FIX_RE.search(subject))
        if re.search(r"\brevert", subject, re.IGNORECASE):
            reverts += 1
        for f in files:
            churn[f] += 1
            if is_fix:
                fixes[f] += 1
        if 2 <= len(files) <= max_commit_files:
            for a, b in itertools.combinations(sorted(set(files)), 2):
                pair_counts[(a, b)] += 1

    if n_commits == 0:
        sys.stderr.write(
            "No commits found in window (--since %r). Widen the window.\n" % since)
        sys.exit(1)

    # Hotspots: churn x log(size). Files gone from the working tree are
    # skipped — you can't refactor what no longer exists.
    hotspots = []
    for f, c in churn.items():
        loc = count_lines(repo, f)
        if loc is None:
            continue
        hotspots.append({
            "file": f, "commits": c, "loc": loc,
            "score": round(c * math.log2(loc + 2), 1),
        })
    hotspots.sort(key=lambda r: -r["score"])

    magnets = []
    for f, n_fix in fixes.items():
        if count_lines(repo, f) is None:
            continue
        magnets.append({
            "file": f, "fix_commits": n_fix, "commits": churn[f],
            "fix_ratio": round(n_fix / churn[f], 2),
        })
    # Absolute fix count first; ratio breaks ties toward concentrated pain.
    magnets.sort(key=lambda r: (-r["fix_commits"], -r["fix_ratio"]))

    pairs = []
    for (a, b), n in pair_counts.items():
        if n < min_pair:
            continue
        distant = top_component(a) != top_component(b)
        pairs.append({
            "a": a, "b": b, "together": n,
            "confidence": round(n / min(churn[a], churn[b]), 2),
            "distant": distant,
        })
    # Distant pairs are the boundary findings; surface them first.
    pairs.sort(key=lambda r: (-r["distant"], -r["together"]))

    return {
        "window": since,
        "commits_scanned": n_commits,
        "revert_commits": reverts,
        "hotspots": hotspots,
        "fix_magnets": magnets,
        "co_change": pairs,
    }


def print_table(rows, columns, title, top):
    print("\n== %s ==" % title)
    if not rows:
        print("  (nothing above thresholds)")
        return
    rows = rows[:top]
    widths = {
        col: max(len(col), max(len(str(r[col])) for r in rows))
        for col in columns
    }
    header = "  ".join(col.ljust(widths[col]) for col in columns)
    print("  " + header)
    print("  " + "-" * len(header))
    for r in rows:
        print("  " + "  ".join(str(r[col]).ljust(widths[col]) for col in columns))


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", default=".", help="repository root (default: cwd)")
    ap.add_argument("--since", default="18 months ago",
                    help='history window, git syntax (default: "18 months ago")')
    ap.add_argument("--top", type=int, default=25, help="rows per table")
    ap.add_argument("--path", action="append", default=[],
                    help="restrict to subtree(s); repeatable")
    ap.add_argument("--min-pair", type=int, default=4,
                    help="min co-occurrences for a co-change pair (default 4)")
    ap.add_argument("--max-commit-files", type=int, default=25,
                    help="skip commits larger than this for co-change (default 25)")
    ap.add_argument("--json", action="store_true", help="emit full JSON instead of tables")
    args = ap.parse_args()

    data = analyze(args.repo, args.since, args.path,
                   args.max_commit_files, args.min_pair)

    if args.json:
        json.dump(data, sys.stdout, indent=2)
        print()
        return

    print("Scanned %d non-merge commits since %r (%d revert-flavored)."
          % (data["commits_scanned"], data["window"], data["revert_commits"]))

    print_table(data["hotspots"], ["score", "commits", "loc", "file"],
                "HOTSPOTS (churn x size — where comprehension debt concentrates)",
                args.top)
    print_table(data["fix_magnets"], ["fix_commits", "commits", "fix_ratio", "file"],
                "FIX MAGNETS (read their fix commits: a shared theme names a model mismatch)",
                args.top)
    for r in data["co_change"]:
        r["pair"] = ("%s  <->  %s" % (r["a"], r["b"])) + ("   [DISTANT]" if r["distant"] else "")
    print_table(data["co_change"], ["together", "confidence", "pair"],
                "CO-CHANGE (DISTANT pairs suggest a misdrawn boundary)",
                args.top)


if __name__ == "__main__":
    main()
