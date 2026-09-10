#!/usr/bin/env bash
# close-tickets.sh — the CI caller of `xtask ticket close` (#25; CONTEXT.md,
# Ticket): on a push to main, the harness workflow's tickets job hands it
# the pushed range and the run's URL, and it reads each commit's trailers,
# `Ticket: #<n>`, the ticket the commit settles, and `Opens: #<m>, #<k>` or
# `Opens: nothing`, what the work opened, and runs the instrument for each.
# The instrument decides; this script tells the ticket what it decided: a
# close is commented with the run and where its readings are, a refusal
# with the instrument's line, which names the unticked boxes, and the
# ticket stays open. A trailer is the last paragraph as `git
# interpret-trailers` reads it, so a number cited in prose closes nothing,
# and so does a commit with no trailer. `Opens:` missing is the
# instrument's refusal, commented like any other, since it names the two
# flags. The run's URL is the session the reading carries.
#
# Usage: close-tickets.sh <before> <after> <run url>. A `before` git does
# not have (a new branch's zeros) makes the range `after` alone.
# Exit: 0 when every ticket named was told; 1 when one could not be, or a
# trailer's value is not a ticket; 2 a usage error. Nothing here exits on
# what the instrument decided.
set -u
[ $# -eq 3 ] || { echo "usage: close-tickets.sh <before> <after> <run url>" >&2; exit 2; }
before=$1 after=$2 run=$3
cd "$(git rev-parse --show-toplevel)" || exit 2
if git cat-file -e "$before^{commit}" 2>/dev/null; then range="$before..$after"; else range="$after^!"; fi
status=0
say() { printf '%s\n' "$1"; [ -n "${GITHUB_STEP_SUMMARY:-}" ] && printf -- '- %s\n' "$1" >> "$GITHUB_STEP_SUMMARY"; }
tell() { gh issue comment "$1" --body "$2" >/dev/null || { say "#$1 could not be told: $2"; status=1; }; }
for sha in $(git rev-list --reverse "$range"); do
    trailers=$(git show -s --format=%B "$sha" | git interpret-trailers --parse)
    subject=$(git show -s --format=%s "$sha")
    flags=()
    for token in $(printf '%s\n' "$trailers" | sed -n 's/^Opens: *//p' | tr ',' ' '); do
        case "$token" in
            nothing) flags+=(--opens-nothing) ;;
            \#[0-9]*) flags+=(--opens "${token#\#}") ;;
            *) say "$sha ($subject): \`Opens: $token\` is not a ticket or \`nothing\`; the instrument refuses it" ;;
        esac
    done
    for value in $(printf '%s\n' "$trailers" | sed -n 's/^Ticket: *//p'); do
        case "$value" in
            \#[0-9]*) n=${value#\#} ;;
            *) say "$sha ($subject): \`Ticket: $value\` names no ticket; nothing closed"; status=1; continue ;;
        esac
        if out=$(cargo xtask ticket close "$n" ${flags[@]+"${flags[@]}"} --session-id "$run" 2>&1); then
            say "#$n closed for $sha ($subject): $out"
            tell "$n" "Closed by \`xtask ticket close\` in $run for $sha ($subject): $out. The readings the run uploaded: the \`harness\` artifact of the per-commit job, the \`tickets\` artifact of the tickets job."
        else
            say "#$n not closed for $sha ($subject): $out"
            tell "$n" "Not closed by \`xtask ticket close\` in $run for $sha ($subject): $out"
        fi
    done
done
exit "$status"
