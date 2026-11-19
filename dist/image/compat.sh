#!/bin/sh
# Lays down the compat directory, $COMPAT: each usual name a symlink to the
# binary the image holds under Lela's name, `$NAME`, `$NAME-cgi` and
# `$NAME-fpm` (ADR-0016), where the two names differ and the image holds
# it. While the hosts carry php's names it stays empty. Each stage that
# adds a host runs it again; dist/Dockerfile mounts it, and no image keeps
# it.
#
#   NAME=<name> COMPAT=<dir> sh compat.sh
set -eu

mkdir -p "$COMPAT"
for usual in php php-cgi php-fpm; do
	lela="$NAME${usual#php}"
	[ "$lela" != "$usual" ] || continue
	for dir in /usr/local/bin /usr/local/sbin; do
		if [ -x "$dir/$lela" ]; then
			ln -sfn "$dir/$lela" "$COMPAT/$usual"
		fi
	done
done
