#!/bin/bash
# The smoke a release's binaries pass on the platform they were built for.
# Each check prints what it ran and what it gave, and the script exits
# nonzero at the first that gives another answer.
#
#   dist/smoke.sh <dir>
#   dist/smoke.sh --image <image> cli|fpm|apache
#
# Over one unpacked tarball: that its bin/ and sbin/ hold the four hosts
# and no other host of Lela's name, then `lela -v`, `lela -r`, a file with
# its arguments, code on stdin, `lela-cgi` on a file, `lela-fpm -t` over a
# one-pool configuration and `lela-dbg -qrr` on a file, each host by the
# name ADR-0016 gives it, the default of the `ARG NAME` of the Dockerfile
# beside this script. A Linux tarball's libraries are the Ubuntu packages its
# runtime-packages.txt lists, installed first: dist/README.md runs it in
# the Ubuntu image dist/Dockerfile builds on. A macOS tarball's are under
# its lib/: first every Mach-O in it is held to name only the system's
# libraries and the tarball's own (`--machos`, below), and then the hosts
# run with the machine's Homebrew prefix out of their reach, by a sandbox
# that denies reading it, wherever brew and sandbox-exec are there, the
# https requests of `https` and the openssl.cnf `openssl_conf` reads among
# them.
#
# Over an image of one variant, on the image's own platform: the same
# checks inside it over the hosts in /usr/local, which are the variant's
# and no other, `lela-fpm -t` where the variant holds the FPM and
# `lela-dbg` where it holds the debugger, `cli` alone; the ini directory
# php's image names; no `php`, `php-cgi`, `php-fpm` or `phpdbg` on PATH,
# the compat directory off it, holding each usual name the variant has a
# host for, and each entry of it what its name finds and runs once the
# directory is put first;
# then what a user of the variant sees: the entrypoint passing an option to
# the variant's command, `fpm` answering a FastCGI request on port 9000 and
# `apache` serving a page, each to a second container of the image.
set -euo pipefail

usage() {
	echo "usage: $0 <dir>" >&2
	echo "       $0 --machos <dir>" >&2
	echo "       $0 --https <dir>" >&2
	echo "       $0 --openssl <dir>" >&2
	echo "       $0 --image <image> cli|fpm|apache" >&2
	exit 2
}

# The hosts' name: NAME where the caller gives it, as the image mode gives
# it to the copy of this script it runs in a container, and otherwise the
# default of the `ARG NAME` of the Dockerfile beside this script.
named() {
	[ -z "${NAME:-}" ] || return 0
	local dockerfile
	dockerfile="$(dirname "${BASH_SOURCE[0]}")/Dockerfile"
	NAME=$(sed -n 's/^ARG NAME=//p' "$dockerfile")
	[ -n "$NAME" ] || { echo "no ARG NAME= in $dockerfile" >&2; exit 1; }
}

# One check: its label, the answer wanted, and the command.
check() {
	local label=$1 want=$2; shift 2
	local got
	got=$("$@" 2>&1) || { printf '%s: exit %s\n%s\n' "$label" "$?" "$got" >&2; exit 1; }
	printf '%s: %s\n' "$label" "$got"
	[ "$got" = "$want" ] || { printf '%s: wanted %s\n' "$label" "$want" >&2; exit 1; }
}

# The hosts a tarball holds, `tarball`, or the image of a variant, by their
# paths under its prefix, in the order they are checked: dist/publish.py's
# HOSTS, where the harness's host table says each ships, as `cargo xtask
# hosts --list` prints it (#79). test_smoke.py holds each to that table.
shipped() {
	case "$1" in
		tarball) echo "bin/$NAME bin/$NAME-cgi sbin/$NAME-fpm bin/$NAME-dbg" ;;
		cli) echo "bin/$NAME bin/$NAME-cgi bin/$NAME-dbg" ;;
		fpm|apache) echo "bin/$NAME bin/$NAME-cgi sbin/$NAME-fpm" ;;
		*) echo "shipped: no tarball or variant $1" >&2; exit 2 ;;
	esac
}

# The usual name of a host at a path, php's own for it: `php` for `$NAME`,
# `php-cgi` and `php-fpm` for `$NAME-cgi` and `$NAME-fpm`, `phpdbg` for
# `$NAME-dbg`.
usual() {
	local host=${1##*/}
	case "$host" in
		"$NAME-dbg") echo phpdbg ;;
		*) echo "php${host#"$NAME"}" ;;
	esac
}

# The hosts under one prefix, `<dir>/bin` and `<dir>/sbin`: those `shipped`
# names for the second argument, a tarball or a variant, and no other host
# of Lela's name, then each one's checks. Each host runs under JAIL, a
# command prefix that `reach` sets, or none.
hosts() {
	local dir=$1 wanted found path scratch
	wanted=$(shipped "$2")
	found=$(cd "$dir" && for path in bin/"$NAME" bin/"$NAME"-* sbin/"$NAME" sbin/"$NAME"-*; do
		[ ! -e "$path" ] || echo "$path"
	done | sort)
	# shellcheck disable=SC2086 # the paths, one word each
	if [ "$found" != "$(printf '%s\n' $wanted | sort)" ]; then
		found=${found//$'\n'/ }
		echo "hosts: $dir holds ${found:-none}, where the $2 holds $wanted" >&2
		exit 1
	fi
	echo "hosts: $wanted, and no other"
	scratch=$(mktemp -d)
	# shellcheck disable=SC2064 # the directory is this call's
	trap "rm -rf '$scratch'" EXIT
	for path in $wanted; do
		case "${path##*/}" in
			"$NAME") the_cli "$dir/$path" "$scratch" ;;
			"$NAME-cgi") the_cgi "$dir/$path" "$scratch" ;;
			"$NAME-fpm") the_fpm "$dir/$path" "$scratch" ;;
			"$NAME-dbg") the_dbg "$dir/$path" "$scratch" ;;
		esac
	done
}

# The CLI's checks, at a path, with a scratch directory.
the_cli() {
	local php=$1 scratch=$2
	${JAIL[@]+"${JAIL[@]}"} "$php" -v
	check "-r" "2" ${JAIL[@]+"${JAIL[@]}"} "$php" -n -r 'echo 1 + 1;'
	# shellcheck disable=SC2016 # php's $argv, not the shell's
	printf '<?php echo implode(",", array_slice($argv, 1));' > "$scratch/args.php"
	check "file with arguments" "a,b c" ${JAIL[@]+"${JAIL[@]}"} "$php" -n "$scratch/args.php" a "b c"
	check "stdin" "OK" ${JAIL[@]+"${JAIL[@]}"} "$php" -n <<<'<?php echo strtoupper("ok");'
}

# The CGI's check, at a path, with a scratch directory.
the_cgi() {
	printf '<?php echo PHP_SAPI;' > "$2/sapi.php"
	check "$NAME-cgi" "cgi-fcgi" ${JAIL[@]+"${JAIL[@]}"} "$1" -n -q "$2/sapi.php"
}

# The debugger's check, at a path, with a scratch directory: a file run
# and the debugger quit, `-qrr`, as php's phpdbg runs one.
the_dbg() {
	printf '<?php echo PHP_SAPI;' > "$2/sapi.php"
	check "$NAME-dbg" "phpdbg" ${JAIL[@]+"${JAIL[@]}"} "$1" -n -qrr "$2/sapi.php"
}

# The FPM's check, at a path, with a scratch directory: `-t` over a
# one-pool configuration.
the_fpm() {
	local scratch=$2
	printf '[global]\nerror_log = %s/fpm.log\n[www]\nlisten = %s/fpm.sock\npm = static\npm.max_children = 1\n' \
		"$scratch" "$scratch" > "$scratch/fpm.conf"
	# As root, php-fpm runs a pool only when -R allows it, as php's does.
	# macOS's bash 3.2 calls an empty array unbound under `set -u`, and the
	# EXIT trap then exits 0, so the empty case expands to nothing by hand.
	local root=()
	[ "$(id -u)" -ne 0 ] || root=(-R)
	${JAIL[@]+"${JAIL[@]}"} "$1" -n -t -y "$scratch/fpm.conf" ${root[@]+"${root[@]}"}
	echo "$NAME-fpm -t: exit 0"
}

# Every Mach-O of an unpacked macOS tarball names, as `otool -L` reads it,
# only the system's libraries (/usr/lib, /System), a file of the tarball by
# `@loader_path`, and a library its own install name `@rpath/<its file
# name>`; carries no run path but the system's; and passes `codesign
# --verify`. So a library the hosts load from anywhere else, a keg new to
# the build among them, fails here and not on a Mac without it.
machos() {
	local root file name own target count=0 bad=""
	root=$(cd "$1" && pwd -P)
	while IFS= read -r file; do
		case "$(file -b "$file")" in Mach-O*) ;; *) continue ;; esac
		count=$((count + 1))
		own=$(otool -D "$file" | sed -n 2p)
		while IFS= read -r name; do
			case "$name" in
				/usr/lib/*|/System/*) continue ;;
				@loader_path/*)
					target="$(dirname "$file")/${name#@loader_path/}"
					if [ -f "$target" ]; then
						case "$(cd "$(dirname "$target")" && pwd -P)/" in "$root"/*) continue ;; esac
					fi ;;
				"@rpath/${file##*/}") [ "$name" != "$own" ] || continue ;;
			esac
			bad+="${file#"$root"/}: $name"$'\n'
		done < <(otool -L "$file" | sed -e 1d -e 's/^[[:space:]]*//' -e 's/ (compatibility version .*//')
		while IFS= read -r name; do
			case "$name" in /usr/lib/*|/System/*) ;; *) bad+="${file#"$root"/}: run path $name"$'\n' ;; esac
		done < <(otool -l "$file" | sed -n '/cmd LC_RPATH$/{n;n;s/^ *path \(.*\) (offset [0-9]*)$/\1/p;}')
		codesign --verify --strict "$file" 2>/dev/null || bad+="${file#"$root"/}: no valid signature"$'\n'
	done < <(find "$root" -type f | sort)
	if [ -n "$bad" ]; then
		printf 'Mach-O: what is outside the tarball and the system, or unsigned:\n%s' "$bad" >&2
		exit 1
	fi
	echo "Mach-O: $count files name only the system's libraries and the tarball's own, each signed"
}

# On a Mac with Homebrew and sandbox-exec, JAIL is a sandbox that denies
# reading Homebrew's prefix, so a host that still loads a keg's library
# from there fails to start; elsewhere it is nothing, and the line says why.
reach() {
	local prefix
	if ! prefix=$(brew --prefix 2>/dev/null); then
		echo "kegs out of reach: no brew on this machine"
	elif ! command -v sandbox-exec > /dev/null; then
		echo "kegs out of reach: no sandbox-exec to hold $prefix away, so the hosts run beside it"
	else
		JAIL=(sandbox-exec -p "(version 1)(allow default)(deny file-read* (subpath \"$prefix\"))")
		echo "kegs out of reach: the hosts run in a sandbox that denies reading $prefix"
	fi
}

# An https stream that verifies its peer, under JAIL where `reach` set it:
# to a host whose certificate the system trusts it reads, which on a Mac
# without Homebrew's OpenSSL is by macOS's /etc/ssl/cert.pem, the bundle
# ext/openssl falls back to where the bundled libcrypto's compiled-in file
# is out of reach; to one whose certificate has expired it is still
# refused, so the fallback verifies and does not skip. SSL_CERT_FILE and
# SSL_CERT_DIR are unset, since either would stand in for the fallback.
# Each request is made only where curl, outside the sandbox, reaches its
# host, and its line says so where it does not: with no network there is
# nothing to verify against, and the line is not a pass.
https() {
	local php="$1/bin/$NAME"
	fetch "$php" "https, a trusted certificate" https://www.php.net/ read
	fetch "$php" "https, an expired certificate" https://expired.badssl.com/ refused -k
}

# One request of `https`: the host, its label, the URL, the answer wanted,
# read or refused, and what curl takes to reach the host at all.
fetch() {
	local php=$1 label=$2 url=$3 want=$4; shift 4
	if ! curl -sS -o /dev/null --max-time 20 "$@" "$url" > /dev/null 2>&1; then
		echo "$label: curl reaches nothing at $url from this machine, so the stream is not tried"
		return 0
	fi
	check "$label" "$want" ${JAIL[@]+"${JAIL[@]}"} env -u SSL_CERT_FILE -u SSL_CERT_DIR "$php" -n \
		-d default_socket_timeout=20 -r "echo @file_get_contents('$url') === false ? 'refused' : 'read';"
}

# The openssl.cnf ext/openssl reads with neither OPENSSL_CONF nor SSLEAY_CONF
# set: the library's compiled-in file where it can be read, as php's, and
# else the tarball's own lib/openssl.cnf, which dist/bundle.py lays from the
# keg libcrypto came from. Held under JAIL where `reach` set it, and outside
# it too, where a Mac with the keg reads the keg's; then a key and a request
# made with no `config` option succeed under JAIL, which they do not where no
# file is read.
openssl_conf() {
	local dir php laid compiled
	dir=$(cd "$1" && pwd -P)
	php="$dir/bin/$NAME" laid="$dir/lib/openssl.cnf"
	compiled=$(env -u OPENSSL_CONF -u SSLEAY_CONF "$php" -n -r \
		'echo openssl_get_cert_locations()["default_default_cert_area"], "/openssl.cnf";')
	[ -z "${JAIL[*]+set}" ] || default_conf "openssl.cnf, the kegs within reach" "$php" "$compiled" "$laid"
	default_conf "openssl.cnf" "$php" "$compiled" "$laid" ${JAIL[@]+"${JAIL[@]}"}
	# shellcheck disable=SC2016 # php's variables, not the shell's
	check "openssl, a key and a request with no config option" "made" \
		${JAIL[@]+"${JAIL[@]}"} env -u OPENSSL_CONF -u SSLEAY_CONF "$php" -n -r \
		'$k = openssl_pkey_new(); echo $k !== false && openssl_csr_new(["commonName" => "a"], $k) !== false ? "made" : "refused";'
}

# One reading of `openssl_conf`: its label, the host, the compiled-in file
# and the tarball's, then the command prefix it runs under, or none. The file
# read is the compiled-in one where the prefix can read it, and else the
# tarball's.
default_conf() {
	local label=$1 php=$2 compiled=$3 want=$4; shift 4
	! "$@" test -r "$compiled" || want=$compiled
	# shellcheck disable=SC2016 # php's variables, not the shell's
	check "$label" "$want" "$@" env -u OPENSSL_CONF -u SSLEAY_CONF "$php" -n -r \
		'$e = new ReflectionExtension("openssl"); ob_start(); $e->info(); preg_match("/^Openssl default config => (.*)$/m", ob_get_clean(), $m); echo $m[1];'
}

# Inside an image: its hosts, its ini directory and its compat directory,
# whose path the caller passes as COMPAT.
in_image() {
	local variant=$1
	hosts /usr/local "$variant"
	check "ini directory" "$PHP_INI_DIR $PHP_INI_DIR/conf.d" \
		"$NAME" -r 'echo PHP_CONFIG_FILE_PATH, " ", PHP_CONFIG_FILE_SCAN_DIR;'
	compat "${COMPAT:?the compat directory}" "$variant"
}

# The compat directory of a variant's image: a directory, off PATH, that
# holds each usual name the variant has a host for, while no usual name is
# on PATH (ADR-0016), and each entry of which is what its name finds once
# the directory is first on PATH, and runs. That makes putting it first
# the only step the usual names need.
compat() {
	local compat=$1 variant=$2 usual entry found path every="" usuals=() scratch
	for path in $(shipped "$variant"); do usuals+=("$(usual "$path")"); done
	for path in $(shipped tarball); do every+="${every:+ }$(usual "$path")"; done
	local said=${every% *}
	said="${said// /, } or ${every##* }"
	[ -d "$compat" ] || { echo "compat: $compat is not a directory" >&2; exit 1; }
	case ":$PATH:" in
		*":$compat:"*) echo "compat: $compat is on PATH, $PATH" >&2; exit 1 ;;
	esac
	echo "compat: $compat, not on PATH ($PATH)"
	for usual in $every; do
		if found=$(command -v "$usual"); then
			echo "compat: $usual is $found on PATH, where no usual name is until $compat is put first" >&2
			exit 1
		fi
	done
	echo "compat: no $said on PATH"
	for usual in "${usuals[@]}"; do
		[ -e "$compat/$usual" ] || [ -L "$compat/$usual" ] ||
			{ echo "compat: $compat holds no $usual" >&2; exit 1; }
	done
	scratch=$(mktemp -d)
	printf '<?php echo PHP_SAPI;' > "$scratch/sapi.php"
	for entry in "$compat"/*; do
		# An empty directory's glob is itself.
		[ -e "$entry" ] || [ -L "$entry" ] || continue
		usual=${entry##*/}
		found=$(PATH="$compat:$PATH" command -v "$usual") || found=nothing
		[ "$found" = "$entry" ] ||
			{ echo "compat: $usual is $found with $compat first on PATH, and not $entry" >&2; exit 1; }
		# The debugger runs a file and quits, where the others say their
		# version.
		if [ "$usual" = phpdbg ]; then
			PATH="$compat:$PATH" "$usual" -n -qrr "$scratch/sapi.php" > /dev/null
		else
			PATH="$compat:$PATH" "$usual" -v > /dev/null
		fi || { rm -rf "$scratch"; echo "compat: $entry does not run" >&2; exit 1; }
		echo "compat: $usual is $entry with $compat first on PATH"
	done
	rm -rf "$scratch"
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
	named
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
	--compat) named; compat "$2" "$3" ;;
	# The hosts check alone over a directory standing in for a prefix, and
	# the hosts a tarball or a variant holds, which test_smoke.py holds
	# without a build or an image.
	--hosts)
		[ "$#" -eq 3 ] || usage
		named
		hosts "$(cd "$2" && pwd)" "$3" ;;
	--shipped)
		[ "$#" -eq 2 ] || usage
		named
		shipped "$2" ;;
	--fastcgi) fastcgi "$2" "$3" "$4" ;;
	--machos)
		[ "$#" -eq 2 ] || usage
		machos "$2" ;;
	# The https requests alone, under the sandbox on a Mac where `reach`
	# sets one, which test_smoke.py holds with a curl and a host of its own.
	--https)
		[ "$#" -eq 2 ] || usage
		named
		[ "$(uname -s)" != Darwin ] || reach
		https "$(cd "$2" && pwd)" ;;
	# The openssl.cnf read alone, under the sandbox on a Mac where `reach`
	# sets one, which test_smoke.py holds with a host of its own.
	--openssl)
		[ "$#" -eq 2 ] || usage
		named
		[ "$(uname -s)" != Darwin ] || reach
		openssl_conf "$2" ;;
	-*|"") usage ;;
	*)
		[ "$#" -eq 1 ] || usage
		named
		if [ "$(uname -s)" = Darwin ]; then
			machos "$1"
			reach
		fi
		hosts "$(cd "$1" && pwd)" tarball
		# A Linux tarball's smoke runs before its curl is installed, and only a
		# macOS one carries an openssl.cnf.
		if [ "$(uname -s)" = Darwin ]; then
			https "$(cd "$1" && pwd)"
			openssl_conf "$1"
		fi ;;
esac
