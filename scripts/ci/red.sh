#!/usr/bin/env bash
# red.sh — a red CI job, filed once. Two subjects are held to one rule:
#
#   red.sh main         harness.yml's last job, on a push to main alone (#1156,
#                       as Mitchell adopted on #1151): the verdict is this
#                       run's, and the one before it the newest finished
#                       harness run of a push to main other than this one.
#   red.sh nightly JOB  nightly.yml's last job, once each for `soak`,
#                       `tailcall` and `deny` (#4372), nightly and by hand
#                       with `full` (#2980):
#                       the verdict is JOB's in this run, and the one before
#                       it JOB's in the newest finished nightly.yml run,
#                       nightly or by hand, that ran it.
#
# A cancelled run, and a run whose JOB was skipped or cancelled, is passed
# over. Each subject has a label of its own, `ci:red-main` or `ci:red-JOB`:
#
#   red, a ticket with the label open   comment on it, file none
#   red, the one before red             file one, `ready-for-agent`, with
#                                       both runs and the first red commit
#                                       of the streak
#   red, the one before green           nothing yet; one red is a flake
#   green, the one before red,          comment that it is green again and
#     a ticket open                     leave it for its builder to close
#                                       through `xtask ticket close`; no
#                                       workflow closes one
#
# A ticket names the runs, the first red commit and the failing jobs and
# steps, and nothing of `done`: harness.yml runs no `done` on a push to
# main, whose change pre-push and `land` judged (Round 1, Q21;
# harness-design.md §4.5), so main's ticket has no list to quote.
#
# The environment: RED (`true` or `false`, whether the subject failed in
# this run), GH_TOKEN, and the runner's GITHUB_REPOSITORY, GITHUB_RUN_ID,
# GITHUB_SHA and GITHUB_SERVER_URL. With DRY_RUN=1 it reads as it would and
# prints each write, the label, the ticket or the comment, instead of
# making it.
set -euo pipefail

repo=${GITHUB_REPOSITORY:?} run=${GITHUB_RUN_ID:?} sha=${GITHUB_SHA:?}
red=${RED:?RED is true or false}
this="${GITHUB_SERVER_URL:-https://github.com}/$repo/actions/runs/$run"

subject=${1:-}
case $subject in
main)
    workflow=harness.yml label=ci:red-main job_name=""
    what="the harness workflow on main" runs_of="pushes to main" cause="#1156"
    heading="harness.yml: main"
    ;;
nightly)
    job=${2:?nightly names a job of nightly.yml}
    workflow=nightly.yml label=ci:red-$job
    # The job's `name:`, unquoted, which is what the API reports a job by.
    job_name=$(awk -v key="  $job:" '$0 == key { inside = 1; next }
        inside && /^  [^ #]/ { exit }
        inside && sub(/^    name: /, "") { gsub(/^["'\'']|["'\'']$/, ""); print; exit }' \
        "$(dirname "$0")/../../.github/workflows/$workflow")
    [ -n "$job_name" ] || { echo "red.sh: $workflow has no job $job with a name" >&2; exit 2; }
    what="nightly.yml's \`$job\` job" runs_of="nightlies" cause="#2980"
    heading="nightly.yml: the $job nightly"
    ;;
*)
    echo "usage: red.sh main | red.sh nightly JOB" >&2
    exit 2
    ;;
esac

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

# The earlier verdicts, newest first, each `{conclusion, headSha, url}`:
# main's are the finished runs of a push to main; a nightly JOB's are its
# own in each finished nightly.yml run of main, of the schedule or by hand,
# read back to its last green. A job that ran out of its time is red: the
# API says `timed_out`, or `cancelled` in a run nobody cancelled. This run,
# and a verdict neither green nor red, are left out.
verdicts() {
    if [ "$subject" = main ]; then
        gh run list --repo "$repo" --workflow "$workflow" --branch main --event push --limit 50 \
            --json databaseId,status,conclusion,headSha,url |
            jq --argjson run "$run" '[.[] | select(.databaseId != $run and .status == "completed"
                and (.conclusion == "success" or .conclusion == "failure"))]'
        return
    fi
    # Asked by event, since a nightly.yml run follows every push to main too.
    # Bash clears -e in the command substitution this runs in, so each call
    # that fails ends it by hand rather than read as a run with no verdict.
    local nightlies event id ran head url conclusion
    nightlies=$(for event in schedule workflow_dispatch; do
        gh api "repos/$repo/actions/workflows/$workflow/runs?status=completed&branch=main&event=$event&per_page=50" \
            --jq '.workflow_runs[] | "\(.created_at) \(.id) \(.conclusion) \(.head_sha) \(.html_url)"' || exit 1
    done | sort -r | cut -d' ' -f2-) || return 1
    while read -r id ran head url; do
        [ -n "$id" ] && [ "$id" != "$run" ] || continue
        conclusion=$(gh run view "$id" --repo "$repo" --json jobs |
            jq -r --arg name "$job_name" --arg ran "$ran" '[.jobs[] | select(.name == $name) | .conclusion][0] // "none"
                | if . == "timed_out" or (. == "cancelled" and $ran != "cancelled") then "failure" else . end') || exit 1
        [ "$conclusion" = success ] || [ "$conclusion" = failure ] || continue
        jq -n --arg c "$conclusion" --arg sha "$head" --arg url "$url" '{conclusion: $c, headSha: $sha, url: $url}'
        [ "$conclusion" != success ] || break
    done <<<"$nightlies" | jq -s .
}
runs=$(verdicts)
before=$(jq -r '.[0].conclusion // "none"' <<<"$runs")
open=$(gh issue list --repo "$repo" --label "$label" --state open --json number --jq '.[0].number // empty')
echo "$what: this run $([ "$red" = true ] && echo red || echo green); the one before: $before; open $label ticket: ${open:-none}"

if [ "$red" != true ]; then
    if [ -n "$open" ] && [ "$before" = failure ]; then
        write gh issue comment "$open" --repo "$repo" --body "Green again: $what, at $sha ($this). This ticket stays open for its builder, who closes it through \`xtask ticket close\` naming the cause; no workflow closes a ticket."
    fi
    exit 0
fi

if [ -n "$open" ]; then
    write gh issue comment "$open" --repo "$repo" --body "Still red at $sha: $this"
    exit 0
fi
if [ "$before" != failure ]; then
    echo "the first red of $what; a second in a row files the ticket"
    exit 0
fi

# The red streak's first run: the oldest of the red runs before the newest green.
first=$(jq -r '(map(.conclusion == "success") | index(true)) as $g | .[0:($g // length)] | last | "\(.headSha) \(.url)"' <<<"$runs")
# The title names the failing jobs and their failing steps: main's, and a
# nightly's own job.
failing=$(gh run view "$run" --repo "$repo" --json jobs |
    jq -r --arg name "$job_name" '[.jobs[] | select(.conclusion == "failure" and ($name == "" or .name == $name))
    | .name + ([.steps[] | select(.conclusion == "failure") | .name] | if length > 0 then " (" + join(", ") + ")" else "" end)]
    | join("; ")')
title="$heading is red since ${first:0:8}: ${failing:-a failed job}"
ticket() {
    cat <<EOF
## What happened

On two $runs_of in a row, $what failed ($cause).

- this run: $this, at $sha
- the run before it: $(jq -r '.[0].url' <<<"$runs"), at $(jq -r '.[0].headSha' <<<"$runs")
- the first red commit: ${first% *} (${first#* })

Failing: ${failing:-no job named its failure}

## Acceptance criteria

- [ ] The cause is named, and $what is green again

## Blocked by

None
EOF
}
write gh label create "$label" --repo "$repo" --color B60205 --description "Red twice in a row: $what ($cause)" --force
write gh issue create --repo "$repo" --title "${title:0:240}" --label "$label" --label ready-for-agent --body "$(ticket)"
