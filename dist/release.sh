#!/bin/bash
# A release by hand (ruled 2026-10-31): the CLI, CGI and FPM hosts
# of each platform built from this checkout's HEAD, one tarball per
# platform in <out>, and SHA256SUMS over them, from scripts/dist-sums.sh.
#
#   dist/release.sh <version> <out> [platform...]
#
# The platforms, all three where none is named:
#
#   darwin-arm64   built here with cargo, on an Apple silicon Mac
#   linux-aarch64  dist/Dockerfile's binaries target, --platform linux/arm64
#   linux-x86_64   the same, --platform linux/amd64
#
# Each tarball is <name>-<version>-<platform>.tar.gz and holds one
# directory of that name: bin/, sbin/, etc/php.ini-* and, for Linux,
# runtime-packages.txt, the Ubuntu packages the hosts load. The tree must
# be clean, since a release is built from a commit, whose id the script
# prints. CARGO_BUILD_JOBS (default 1) reaches the Linux builds, which a
# 4 GiB Docker VM links one at a time; NAME (default php) is the
# hosts' name (ADR-0016).
set -euo pipefail

[ "$#" -ge 2 ] || { echo "usage: $0 <version> <out> [platform...]" >&2; exit 2; }
version=$1
mkdir -p "$2"
out=$(cd "$2" && pwd)
shift 2
platforms=("$@")
[ "${#platforms[@]}" -gt 0 ] || platforms=(darwin-arm64 linux-aarch64 linux-x86_64)
NAME=${NAME:-php}
jobs=${CARGO_BUILD_JOBS:-1}

cd "$(git rev-parse --show-toplevel)"
[ -z "$(git status --porcelain)" ] || { echo "$0: the tree has changes; a release is built from a commit" >&2; exit 1; }
echo "commit $(git rev-parse HEAD)"

for platform in "${platforms[@]}"; do
	base="$NAME-$version-$platform"
	stage="$out/$base"
	rm -rf "$stage" "$out/$base.tar.gz"
	case "$platform" in
	darwin-arm64)
		[ "$(uname -sm)" = "Darwin arm64" ] || { echo "$0: $platform is built on an Apple silicon Mac" >&2; exit 1; }
		cargo build --release --locked -p php_sapi_cli -p php_sapi_cgi -p php_sapi_fpm \
			--bin "$NAME" --bin "$NAME-cgi" --bin "$NAME-fpm"
		release="${CARGO_TARGET_DIR:-target}/release"
		install -d "$stage/bin" "$stage/sbin"
		install -m 0755 "$release/$NAME" "$release/$NAME-cgi" "$stage/bin/"
		install -m 0755 "$release/$NAME-fpm" "$stage/sbin/"
		python3 dist/inputs.py ini "$stage/etc"
		;;
	linux-aarch64 | linux-x86_64)
		case "$platform" in linux-aarch64) arch=arm64 ;; *) arch=amd64 ;; esac
		docker buildx build -f dist/Dockerfile --platform "linux/$arch" --target binaries \
			--build-arg CARGO_BUILD_JOBS="$jobs" --build-arg NAME="$NAME" \
			--output "type=local,dest=$stage" .
		;;
	*)
		echo "$0: no platform $platform" >&2
		exit 2
		;;
	esac
	# buildx makes its local output 0700, and tar would record the
	# builder's user, which root's extraction restores on Linux: the
	# directory is 0755 and every entry root's. No AppleDouble files or
	# extended attributes from a Mac's tar.
	chmod 0755 "$stage"
	COPYFILE_DISABLE=1 tar --no-xattrs --uid 0 --gid 0 --uname root --gname root \
		-czf "$out/$base.tar.gz" -C "$out" "$base"
done

scripts/dist-sums.sh "$out"
