#!/bin/bash
# The smoke a release's binaries pass on the platform they were built for,
# run over one unpacked tarball: `php -v`, `php -r`, a file with its
# arguments, code on stdin, `php-cgi` on a file and `php-fpm -t` over a
# one-pool configuration. Each check prints what it ran and what it gave,
# and the script exits nonzero at the first that gives another answer.
#
#   dist/smoke.sh <dir>
#
# A Linux tarball's libraries are the Ubuntu packages its
# runtime-packages.txt lists, installed first: dist/README.md runs it in
# the Ubuntu image dist/Dockerfile builds on.
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $0 <dir>" >&2; exit 2; }
dir=$(cd "$1" && pwd)
NAME=${NAME:-php}
php="$dir/bin/$NAME" cgi="$dir/bin/$NAME-cgi" fpm="$dir/sbin/$NAME-fpm"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# One check: its label, the answer wanted, and the command.
check() {
	local label=$1 want=$2; shift 2
	local got
	got=$("$@" 2>&1) || { printf '%s: exit %s\n%s\n' "$label" "$?" "$got" >&2; exit 1; }
	printf '%s: %s\n' "$label" "$got"
	[ "$got" = "$want" ] || { printf '%s: wanted %s\n' "$label" "$want" >&2; exit 1; }
}

"$php" -v
check "-r" "2" "$php" -n -r 'echo 1 + 1;'
printf '<?php echo implode(",", array_slice($argv, 1));' > "$scratch/args.php"
check "file with arguments" "a,b c" "$php" -n "$scratch/args.php" a "b c"
check "stdin" "OK" "$php" -n <<<'<?php echo strtoupper("ok");'
printf '<?php echo PHP_SAPI;' > "$scratch/sapi.php"
check "$NAME-cgi" "cgi-fcgi" "$cgi" -n -q "$scratch/sapi.php"
printf '[global]\nerror_log = %s/fpm.log\n[www]\nlisten = %s/fpm.sock\npm = static\npm.max_children = 1\n' \
	"$scratch" "$scratch" > "$scratch/fpm.conf"
# As root, php-fpm runs a pool only when -R allows it, as php's does.
root=()
[ "$(id -u)" -ne 0 ] || root=(-R)
"$fpm" -n -t -y "$scratch/fpm.conf" "${root[@]}"
echo "$NAME-fpm -t: exit 0"
