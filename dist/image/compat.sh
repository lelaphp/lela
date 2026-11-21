#!/bin/sh
# Lays down the compat directory, $COMPAT: each usual name a symlink to the
# binary the image holds under Lela's name, `php` to `$NAME`, `php-cgi` to
# `$NAME-cgi`, `php-fpm` to `$NAME-fpm` and `phpdbg` to `$NAME-dbg`
# (ADR-0016), where the image holds it. Each stage that adds a host runs it
# again; dist/Dockerfile mounts it, and no image keeps it. The hosts are
# looked for in bin/ and sbin/ under PREFIX, /usr/local where it is unset,
# as the images lay them; dist/test_smoke.py points it at a prefix of its
# own.
#
#   NAME=<name> COMPAT=<dir> sh compat.sh
set -eu

mkdir -p "$COMPAT"
for usual in php php-cgi php-fpm phpdbg; do
	case "$usual" in
		phpdbg) lela="$NAME-dbg" ;;
		*) lela="$NAME${usual#php}" ;;
	esac
	for dir in "${PREFIX:-/usr/local}/bin" "${PREFIX:-/usr/local}/sbin"; do
		if [ -x "$dir/$lela" ]; then
			ln -sfn "$dir/$lela" "$COMPAT/$usual"
		fi
	done
done
