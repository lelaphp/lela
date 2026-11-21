#!/bin/bash
# Write <dir>/SHA256SUMS over the files of <dir> it names, one line per
# file in the order named, in sha256sum's format: the digest, two spaces,
# the file's name with no directory. A release names every file it
# attaches but SHA256SUMS itself. `sha256sum -c SHA256SUMS` on Linux and
# `shasum -a 256 -c SHA256SUMS` on macOS read it back, run in <dir>. The
# list is printed as well, for a release's notes.
#
#   scripts/dist-sums.sh <dir> <file>...
set -euo pipefail

[ "$#" -ge 2 ] || { echo "usage: $0 <dir> <file>..." >&2; exit 2; }
cd "$1"
shift

for file in "$@"; do
	case $file in
		SHA256SUMS) echo "$0: SHA256SUMS is not summed in itself" >&2; exit 1 ;;
		*/*) echo "$0: $file is a path, where a file of $PWD is named" >&2; exit 1 ;;
	esac
	[ -f "$file" ] || { echo "$0: no $file in $PWD" >&2; exit 1; }
done

if command -v sha256sum >/dev/null; then
	sum=(sha256sum)
else
	sum=(shasum -a 256)
fi

trap 'rm -f SHA256SUMS.tmp' EXIT
"${sum[@]}" "$@" > SHA256SUMS.tmp
mv SHA256SUMS.tmp SHA256SUMS
cat SHA256SUMS
