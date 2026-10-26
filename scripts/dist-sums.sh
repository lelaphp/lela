#!/bin/bash
# Write <dir>/SHA256SUMS over the release tarballs in <dir>, one line per
# <dir>/*.tar.gz sorted by name, in sha256sum's format: the digest, two
# spaces, the file's name with no directory. `sha256sum -c SHA256SUMS` on
# Linux and `shasum -a 256 -c SHA256SUMS` on macOS read it back, run in
# <dir>. The list is printed as well, for a release's notes.
#
#   scripts/dist-sums.sh <dir>
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $0 <dir>" >&2; exit 2; }
cd "$1"
# The glob below sorts byte by byte, whatever the caller's locale.
export LC_ALL=C
shopt -s nullglob

if command -v sha256sum >/dev/null; then
	sum=(sha256sum)
else
	sum=(shasum -a 256)
fi

tarballs=(*.tar.gz)
[ "${#tarballs[@]}" -gt 0 ] || { echo "$0: no *.tar.gz in $1" >&2; exit 1; }

trap 'rm -f SHA256SUMS.tmp' EXIT
"${sum[@]}" "${tarballs[@]}" > SHA256SUMS.tmp
mv SHA256SUMS.tmp SHA256SUMS
cat SHA256SUMS
