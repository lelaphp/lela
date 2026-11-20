#!/bin/bash
# The smoke a release's binaries pass on the platform they were built for.
# Each check prints what it ran and what it gave, and the script exits
# nonzero at the first that gives another answer.
#
#   dist/smoke.sh <dir>
#   dist/smoke.sh --image <image> cli|fpm|apache
#
# Over one unpacked tarball: `php -v`, `php -r`, a file with its arguments,
# code on stdin, `php-cgi` on a file and `php-fpm -t` over a one-pool
# configuration. A Linux tarball's libraries are the Ubuntu packages its
# runtime-packages.txt lists, installed first: dist/README.md runs it in
# the Ubuntu image dist/Dockerfile builds on.
#
# Over an image of one variant, on the image's own platform: the same
# checks inside it over the hosts in /usr/local, `php-fpm -t` where the
# variant holds the FPM; the ini directory php's image names; the compat
# directory off PATH, and each entry of it what its name finds and runs
# once the directory is put first;
# then what a user of the variant sees: the entrypoint passing an option to
# the variant's command, `fpm` answering a FastCGI request on port 9000 and
# `apache` serving a page, each to a second container of the image.
set -euo pipefail

usage() {
	echo "usage: $0 <dir>" >&2
	echo "       $0 --image <image> cli|fpm|apache" >&2
	exit 2
}

NAME=${NAME:-php}

# One check: its label, the answer wanted, and the command.
check() {
	local label=$1 want=$2; shift 2
	local got
	got=$("$@" 2>&1) || { printf '%s: exit %s\n%s\n' "$label" "$?" "$got" >&2; exit 1; }
	printf '%s: %s\n' "$label" "$got"
	[ "$got" = "$want" ] || { printf '%s: wanted %s\n' "$label" "$want" >&2; exit 1; }
}

# The hosts under one prefix, `<dir>/bin` and `<dir>/sbin`; the FPM's check
# only where the second argument says it is there.
hosts() {
	local dir=$1 fpm=$2
	local php="$dir/bin/$NAME" cgi="$dir/bin/$NAME-cgi" scratch
	scratch=$(mktemp -d)
	# shellcheck disable=SC2064 # the directory is this call's
	trap "rm -rf '$scratch'" EXIT
	"$php" -v
	check "-r" "2" "$php" -n -r 'echo 1 + 1;'
	# shellcheck disable=SC2016 # php's $argv, not the shell's
	printf '<?php echo implode(",", array_slice($argv, 1));' > "$scratch/args.php"
	check "file with arguments" "a,b c" "$php" -n "$scratch/args.php" a "b c"
	check "stdin" "OK" "$php" -n <<<'<?php echo strtoupper("ok");'
	printf '<?php echo PHP_SAPI;' > "$scratch/sapi.php"
	check "$NAME-cgi" "cgi-fcgi" "$cgi" -n -q "$scratch/sapi.php"
	[ "$fpm" = fpm ] || return 0
	printf '[global]\nerror_log = %s/fpm.log\n[www]\nlisten = %s/fpm.sock\npm = static\npm.max_children = 1\n' \
		"$scratch" "$scratch" > "$scratch/fpm.conf"
	# As root, php-fpm runs a pool only when -R allows it, as php's does.
	# macOS's bash 3.2 calls an empty array unbound under `set -u`, and the
	# EXIT trap then exits 0, so the empty case expands to nothing by hand.
	local root=()
	[ "$(id -u)" -ne 0 ] || root=(-R)
	"$dir/sbin/$NAME-fpm" -n -t -y "$scratch/fpm.conf" ${root[@]+"${root[@]}"}
	echo "$NAME-fpm -t: exit 0"
}

# Inside an image: its hosts, its ini directory and its compat directory,
# whose path the caller passes as COMPAT.
in_image() {
	local variant=$1 fpm=fpm
	[ "$variant" != cli ] || fpm=
	hosts /usr/local "$fpm"
	check "ini directory" "$PHP_INI_DIR $PHP_INI_DIR/conf.d" \
		"$NAME" -r 'echo PHP_CONFIG_FILE_PATH, " ", PHP_CONFIG_FILE_SCAN_DIR;'
	compat "${COMPAT:?the compat directory}" "$variant"
}

# The compat directory of a variant's image: a directory, off PATH, each
# entry of which is what its name finds once the directory is first on
# PATH, and runs. That makes putting it first the only step the usual
# names need. Where the hosts carry Lela's names (ADR-0016) each usual name
# the variant holds is an entry; while they carry php's, the directory is
# empty, and an empty one passes.
compat() {
	local compat=$1 variant=$2 usual entry found
	local usuals=(php php-cgi)
	[ "$variant" = cli ] || usuals+=(php-fpm)
	[ -d "$compat" ] || { echo "compat: $compat is not a directory" >&2; exit 1; }
	case ":$PATH:" in
		*":$compat:"*) echo "compat: $compat is on PATH, $PATH" >&2; exit 1 ;;
	esac
	echo "compat: $compat, not on PATH ($PATH)"
	if [ "$NAME" != php ]; then
		for usual in "${usuals[@]}"; do
			[ -e "$compat/$usual" ] || [ -L "$compat/$usual" ] ||
				{ echo "compat: $compat holds no $usual, and the hosts are named $NAME" >&2; exit 1; }
		done
	fi
	for entry in "$compat"/*; do
		# An empty directory's glob is itself.
		[ -e "$entry" ] || [ -L "$entry" ] || continue
		usual=${entry##*/}
		found=$(PATH="$compat:$PATH" command -v "$usual") || found=nothing
		[ "$found" = "$entry" ] ||
			{ echo "compat: $usual is $found with $compat first on PATH, and not $entry" >&2; exit 1; }
		PATH="$compat:$PATH" "$usual" -v > /dev/null ||
			{ echo "compat: $entry does not run" >&2; exit 1; }
		echo "compat: $usual is $entry with $compat first on PATH"
	done
}

# One FastCGI request, as a web server makes it, for the script at a path:
# a responder's GET with no body. Prints what the FPM wrote to stdout, the
# headers and the body, once it has ended the request.
fastcgi() {
	local host=$1 port=$2 script=$3 tries params="" hex bytes
	local LC_ALL=C
	for tries in $(seq 50); do
		(exec 3<>"/dev/tcp/$host/$port") 2>/dev/null && break
		[ "$tries" -lt 50 ] || { echo "fastcgi: nothing answers on $host:$port" >&2; exit 1; }
		sleep 0.2
	done
	exec 3<>"/dev/tcp/$host/$port"
	# A record of a type with its content, which holds no NUL.
	record() {
		printf -v hex '\\x01\\x%02x\\x00\\x01\\x%02x\\x%02x\\x00\\x00' "$1" $((${#2} >> 8)) $((${#2} & 255))
		# shellcheck disable=SC2059 # the header's bytes, as escapes
		printf "$hex" >&3
		printf '%s' "$2" >&3
	}
	# One name-value pair, each shorter than 128 bytes.
	pair() {
		printf -v hex '\\x%02x\\x%02x' "${#1}" "${#2}"
		# shellcheck disable=SC2059
		printf -v bytes "$hex"
		params+="$bytes$1$2"
	}
	pair GATEWAY_INTERFACE CGI/1.1
	pair REQUEST_METHOD GET
	pair SCRIPT_FILENAME "$script"
	pair SCRIPT_NAME "/${script##*/}"
	pair REQUEST_URI "/${script##*/}"
	pair SERVER_NAME "$host"
	pair SERVER_PORT "$port"
	pair SERVER_PROTOCOL HTTP/1.1
	# FCGI_BEGIN_REQUEST, as the responder, without FCGI_KEEP_CONN: the FPM
	# closes the connection once it has ended the request.
	printf '\x01\x01\x00\x01\x00\x08\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00' >&3
	record 4 "$params"
	record 4 ""
	record 5 ""
	# shellcheck disable=SC2207 # od's words are the bytes
	bytes=($(timeout 10 cat <&3 | od -An -v -tu1))
	exec 3<&-
	local at=0 type length out='' ended='' c
	while [ $((at + 8)) -le "${#bytes[@]}" ]; do
		type=${bytes[at + 1]} length=$((bytes[at + 4] * 256 + bytes[at + 5]))
		if [ "$type" -eq 6 ]; then
			for ((c = at + 8; c < at + 8 + length; c++)); do
				printf -v hex '\\x%02x' "${bytes[c]}"
				# shellcheck disable=SC2059
				printf -v hex "$hex"
				out+=$hex
			done
		elif [ "$type" -eq 3 ]; then
			ended=1
		fi
		at=$((at + 8 + length + bytes[at + 6]))
	done
	[ -n "$ended" ] || { printf 'fastcgi: the FPM did not end the request; it wrote\n%s\n' "$out" >&2; exit 1; }
	printf '%s' "$out"
}

# The caller of an image: the checks above inside it, then the variant's own.
image() {
	local image=$1 variant=$2 here platform daemon compat net server body
	here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
	platform=$(docker image inspect --format '{{.Os}}/{{.Architecture}}{{with .Variant}}/{{.}}{{end}}' "$image")
	daemon=$(docker version --format '{{.Server.Os}}/{{.Server.Arch}}')
	compat=$(sed -n 's/^ARG COMPAT=//p' "$here/Dockerfile")
	if [ "$platform" = "$daemon" ]; then
		echo "platform: $platform, the daemon's own"
	else
		echo "platform: $platform, emulated on the daemon's $daemon"
	fi
	local run=(docker run --rm --platform "$platform")
	"${run[@]}" -e NAME="$NAME" -e COMPAT="$compat" "$image" \
		bash -c "$(cat "${BASH_SOURCE[0]}")" smoke.sh --in-image "$variant"
	case "$variant" in
		cli)
			check "entrypoint: -r is $NAME's" "cli" "${run[@]}" "$image" -r 'echo PHP_SAPI;'
			return 0 ;;
		fpm)
			"${run[@]}" "$image" -t
			echo "entrypoint: -t is $NAME-fpm's: exit 0" ;;
	esac
	net="smoke-$$-$RANDOM" server="$net-$variant"
	docker network create "$net" > /dev/null
	# shellcheck disable=SC2064 # the names are this run's
	trap "docker rm -f '$server' > /dev/null 2>&1 || :; docker network rm '$net' > /dev/null 2>&1 || :" EXIT
	docker run -d --platform "$platform" --network "$net" --name "$server" "$image" > /dev/null
	# shellcheck disable=SC2016 # php's $_SERVER, not the shell's
	docker exec -i "$server" sh -c 'cat > /var/www/html/index.php' \
		<<<'<?php echo "served:", PHP_SAPI, ":", $_SERVER["REQUEST_METHOD"];'
	if [ "$variant" = fpm ]; then
		body=$("${run[@]}" --network "$net" "$image" bash -c "$(cat "${BASH_SOURCE[0]}")" smoke.sh \
			--fastcgi "$server" 9000 /var/www/html/index.php) ||
			{ docker logs "$server" >&2; exit 1; }
		body=${body#*$'\r\n\r\n'}
		check "FastCGI on $server:9000" "served:fpm-fcgi:GET" printf '%s' "$body"
	else
		body=$("${run[@]}" --network "$net" "$image" \
			curl -fsS --retry 30 --retry-connrefused --retry-delay 1 "http://$server/") ||
			{ docker logs "$server" >&2; exit 1; }
		check "HTTP on $server:80" "served:fpm-fcgi:GET" printf '%s' "$body"
	fi
}

case "${1:-}" in
	--image)
		[ "$#" -eq 3 ] || usage
		case "$3" in cli|fpm|apache) ;; *) usage ;; esac
		image "$2" "$3" ;;
	# What the image mode runs inside a container of the image, and its
	# compat check alone, which test_smoke.py holds without an image.
	--in-image) in_image "$2" ;;
	--compat) compat "$2" "$3" ;;
	--fastcgi) fastcgi "$2" "$3" "$4" ;;
	-*|"") usage ;;
	*)
		[ "$#" -eq 1 ] || usage
		hosts "$(cd "$1" && pwd)" fpm ;;
esac
