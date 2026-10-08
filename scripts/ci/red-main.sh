#!/usr/bin/env bash
# red-main.sh — a red main, filed once (#1156, as Mitchell adopted on #1151):
# harness.yml's last job, on a push to main alone, runs it with this run's
# verdict. The run before it is the newest finished harness run of a push to
# main other than this one, a cancelled run passed over.
#
#   red, a ticket labelled ci:red-main open   comment on it, file none
#   red, the run before red                   file one, `ready-for-agent`,
#                                             with both runs and the first
#                                             red commit of the streak
#   red, the run before green                 nothing yet; one red is a flake
#   green, the run before red, a ticket open  comment that main is green and
#                                             leave it for its builder to
#                                             close through `xtask ticket
#                                             close`; no workflow closes one
#
# The environment: RED (`true` or `false`, whether a job of this run failed),
# UNMET (`done`'s last lines, quoted in the ticket), GH_TOKEN, and the
# runner's GITHUB_REPOSITORY, GITHUB_RUN_ID, GITHUB_SHA and GITHUB_SERVER_URL.
# With DRY_RUN=1 it reads as it would and prints each write, the ticket or
# the comment, instead of making it.
set -euo pipefail

repo=${GITHUB_REPOSITORY:?} run=${GITHUB_RUN_ID:?} sha=${GITHUB_SHA:?}
red=${RED:?RED is true or false}
this="${GITHUB_SERVER_URL:-https://github.com}/$repo/actions/runs/$run"
label=ci:red-main

# A write to the tracker, made, or under DRY_RUN=1 printed, its body last.
write() {
    if [ "${DRY_RUN:-}" != 1 ]; then
        "$@"
        return
    fi
    local body="" arg
    printf 'would run:'
    while [ $# -gt 0 ]; do
        arg=$1; shift
        if [ "$arg" = --body ]; then body=$1; shift; printf ' --body <below>'; else printf ' %q' "$arg"; fi
    done
    printf '\n'
    [ -z "$body" ] || printf '%s\n' "$body" | sed 's/^/    | /'
}

# The finished runs of a push to main, newest first, this one and the
# cancelled ones left out.
runs=$(gh run list --repo "$repo" --workflow harness.yml --branch main --event push --limit 50 \
    --json databaseId,status,conclusion,headSha,url |
    jq --argjson run "$run" '[.[] | select(.databaseId != $run and .status == "completed"
        and (.conclusion == "success" or .conclusion == "failure"))]')
before=$(jq -r '.[0].conclusion // "none"' <<<"$runs")
open=$(gh issue list --repo "$repo" --label "$label" --state open --json number --jq '.[0].number // empty')
echo "this run: $([ "$red" = true ] && echo red || echo green); the run before: $before; open $label ticket: ${open:-none}"

if [ "$red" != true ]; then
    if [ -n "$open" ] && [ "$before" = failure ]; then
        write gh issue comment "$open" --repo "$repo" --body "Main is green again at $sha ($this). This ticket stays open for its builder, who closes it through \`xtask ticket close\` naming the cause; no workflow closes a ticket."
    fi
    exit 0
fi

if [ -n "$open" ]; then
    write gh issue comment "$open" --repo "$repo" --body "Still red at $sha: $this"
    exit 0
fi
if [ "$before" != failure ]; then
    echo "the first red run on main; a second in a row files the ticket"
    exit 0
fi

# The red streak's first run: the oldest of the red runs before the newest green.
first=$(jq -r '(map(.conclusion == "success") | index(true)) as $g | .[0:($g // length)] | last | "\(.headSha) \(.url)"' <<<"$runs")
# The title names the failing readings, `done`'s first unmet line and
# how many follow it, and the failing jobs and steps where done listed
# none (it did not run, or something before it failed).
unmet=$(grep -E '^- ' <<<"${UNMET:-}" || true)
failing=$(gh run view "$run" --repo "$repo" --json jobs --jq '[.jobs[] | select(.conclusion == "failure")
    | .name + ([.steps[] | select(.conclusion == "failure") | .name] | if length > 0 then " (" + join(", ") + ")" else "" end)]
    | join("; ")')
if [ -n "$unmet" ]; then
    more=$(($(wc -l <<<"$unmet") - 1))
    named="$(head -n 1 <<<"$unmet" | sed 's/^- //')$([ "$more" -eq 0 ] || echo " and $more more")"
else
    named=${failing:-a failed job}
fi
title="harness.yml: main is red since ${first:0:8}: $named"
ticket() {
    cat <<EOF
## What happened

The harness workflow failed on two pushes to main in a row (#1156).

- this run: $this, at $sha
- the run before it: $(jq -r '.[0].url' <<<"$runs"), at $(jq -r '.[0].headSha' <<<"$runs")
- the first red commit: ${first% *} (${first#* })

Failing: ${failing:-no job named its failure}

\`cargo xtask done\`'s last lines in this run:

\`\`\`
${UNMET:-(done did not run, or wrote nothing)}
\`\`\`

## Acceptance criteria

- [ ] The cause is named, and the harness workflow on main is green again
- [ ] \`cargo xtask done\` lists nothing unmet for the crates touched

## Blocked by

None
EOF
}
write gh label create "$label" --repo "$repo" --color B60205 --description "Two red harness runs on main in a row (#1156)" --force
write gh issue create --repo "$repo" --title "${title:0:240}" --label "$label" --label ready-for-agent --body "$(ticket)"
