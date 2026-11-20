#!/bin/sh
# Lela's installer, attached to each GitHub Release beside its tarballs
# (dist/README.md, "The installer"), and piped into sh:
#
#   curl -fsSL https://github.com/lelaphp/lela/releases/download/v<version>/install.sh | sh
#
# It installs the release it is attached to, whose version dist/release.py
# bakes in below when it stages this file: the tarball of this machine's
# platform, fetched with curl or wget and held to the release's SHA256SUMS,
# unpacked whole into $XDG_DATA_HOME/lela/<version> (~/.local/share/lela/
# <version> where XDG_DATA_HOME is unset), with ~/.local/bin/lela a link to
# its bin/lela. A copy of bin/lela alone would find no ../lib on macOS, so
# the directory stays whole and the link is what goes on PATH.
#
# Every check runs before anything is put in place: the platform, the
# system's release (macOS on Apple silicon no older than the tarball's
# floor, or Ubuntu of the release the Linux tarballs are built on, with
# the packages they load installed), the checksum, and the unpacked
# `lela -v`. A refusal says why on one line and leaves the home as it was.
# It asks for no root and writes nowhere but those two directories; it
# leaves every php, and every file it did not make, as it is, and edits no
# shell profile: it says when ~/.local/bin is not on PATH. A run over an
# installed release replaces it in place, and the link moves only once the
# new one runs through it.
#
# LELA_INSTALL_FROM, where set, is the URL of the directory the assets are
# fetched from in place of the release's; LELA_OS_RELEASE the file read in
# place of /etc/os-release. Each is named for this script alone, so no
# other program's setting reaches either, and dist/test_install.py sets
# both.
set -eu

# Baked in by dist/release.py: the hosts' name (ADR-0016), the release's
# version, the Ubuntu release its Linux tarballs are built on, and the
# URL its assets are downloaded from.
name=
version=
ubuntu=
release=

me=install.sh

say() {
	printf '%s: %s\n' "$me" "$*"
}

fail() {
	printf '%s: %s\n' "$me" "$*" >&2
	exit 1
}

# What a refusal leaves behind is removed as the script exits: the stage,
# and each directory it made on the way to it, while it is empty.
nl='
'
stage=
made=
installed=
cleanup() {
	[ -z "$stage" ] || rm -rf "$stage"
	[ -z "$installed" ] || return 0
	set -f
	IFS=$nl
	for dir in $made; do rmdir "$dir" 2>/dev/null || :; done
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

# Each directory of a path that is missing, the outermost first, made and
# remembered innermost first, for the cleanup's rmdir.
mkdirs() {
	[ ! -d "$1" ] || return 0
	mkdirs "$(dirname "$1")"
	mkdir "$1"
	made=$1$nl$made
}

# Whether version $1 is older than version $2, each dotted numbers.
older() {
	awk -v a="$1" -v b="$2" 'BEGIN {
		n = split(a, x, "."); m = split(b, y, ".")
		for (i = 1; i <= (n > m ? n : m); i++) {
			if (x[i] + 0 < y[i] + 0) exit 0
			if (x[i] + 0 > y[i] + 0) exit 1
		}
		exit 1
	}'
}

# A macOS version by its name, as Apple names each since Catalina.
macos() {
	case "$1" in
		10.15|10.15.*) set -- "$1" Catalina ;;
		11|11.*) set -- "$1" "Big Sur" ;;
		12|12.*) set -- "$1" Monterey ;;
		13|13.*) set -- "$1" Ventura ;;
		14|14.*) set -- "$1" Sonoma ;;
		15|15.*) set -- "$1" Sequoia ;;
		26|26.*) set -- "$1" Tahoe ;;
	esac
	printf 'macOS %s' "$*"
}

# One field of the os-release file, its quotes taken off.
field() {
	sed -n "s/^$1=//p" "$os_release" | sed -n '1{s/^"\(.*\)"$/\1/;s/^'"'"'\(.*\)'"'"'$/\1/;p;}'
}

# The whole run is one function, called on the script's last line, so
# that a download cut short runs nothing.
main() {
	[ -n "$name" ] && [ -n "$version" ] && [ -n "$ubuntu" ] && [ -n "$release" ] ||
		fail "this is the installer's source, which names no release; run the install.sh attached to one"

	# --- what is checked before anything is written ---------------------------

	if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ]; then
		fail "run this without sudo, as the user who will run $name: it installs into that user's home alone"
	fi
	case "${HOME:-}" in
		/*) [ -d "$HOME" ] || fail "HOME, $HOME, is not a directory" ;;
		*) fail "HOME is not set to an absolute path, so there is no home to install into" ;;
	esac
	data=${XDG_DATA_HOME:-}
	# The XDG Base Directory specification ignores a relative path here.
	case "$data" in /*) ;; *) data=$HOME/.local/share ;; esac
	root=$data/$name
	bin=$HOME/.local/bin
	link=$bin/$name

	system=$(uname -s)
	machine=$(uname -m)
	case "$system $machine" in
		"Darwin arm64") platform=darwin-arm64 ;;
		"Darwin x86_64")
			# A shell under Rosetta says x86_64 on Apple silicon too.
			[ "$(sysctl -n hw.optional.arm64 2>/dev/null || :)" = 1 ] ||
				fail "this is an Intel Mac, and $name $version's macOS tarball is built for Apple silicon alone"
			platform=darwin-arm64 ;;
		"Linux x86_64") platform=linux-x86_64 ;;
		"Linux aarch64" | "Linux arm64") platform=linux-aarch64 ;;
		*) fail "$name $version has no tarball for $system $machine; its platforms are darwin-arm64, linux-aarch64 and linux-x86_64" ;;
	esac

	if [ "$system" = Linux ]; then
		os_release=${LELA_OS_RELEASE:-/etc/os-release}
		[ -n "${LELA_OS_RELEASE:-}" ] || [ -r "$os_release" ] || os_release=/usr/lib/os-release
		[ -r "$os_release" ] ||
			fail "no /etc/os-release says which Linux this is, and $name's Linux tarballs are built for Ubuntu $ubuntu alone"
		id=$(field ID)
		id_version=$(field VERSION_ID)
		if [ "$id" != ubuntu ] || [ "$id_version" != "$ubuntu" ]; then
			pretty=$(field PRETTY_NAME)
			fail "this is ${pretty:-${id:-an unnamed Linux} $id_version} ($os_release), and $name's Linux tarballs are built for Ubuntu $ubuntu alone"
		fi
		command -v dpkg-query >/dev/null 2>&1 ||
			fail "no dpkg-query on PATH to say which of the packages $name loads are installed"
	fi

	if command -v curl >/dev/null 2>&1; then
		fetch() { curl -fsSL --proto-redir =https -o "$2" "$1" </dev/null; }
	elif command -v wget >/dev/null 2>&1; then
		fetch() { wget -q -O "$2" "$1" </dev/null; }
	else
		fail "neither curl nor wget is on PATH, so the release cannot be downloaded; install one, then run this again"
	fi
	if command -v sha256sum >/dev/null 2>&1; then
		digest() { sha256sum "$1"; }
	elif command -v shasum >/dev/null 2>&1; then
		digest() { shasum -a 256 "$1"; }
	else
		fail "neither sha256sum nor shasum is on PATH, so the tarball cannot be held to SHA256SUMS"
	fi

	# The link is this installer's, to <root>/<version>/bin/<name>, or nothing:
	# a file or a link of another's at that name is left as it is.
	previous=
	if [ -L "$link" ]; then
		previous=$(readlink "$link")
		case "$previous" in
			"$root"/*/bin/"$name")
				case "${previous#"$root"/}" in */*/*/*) previous= ;; esac ;;
			*) previous= ;;
		esac
		[ -n "$previous" ] ||
			fail "$link is a link of another's, to $(readlink "$link"); move it away, then run this again"
	elif [ -e "$link" ]; then
		fail "$link is already there, and is not this installer's link; move it away, then run this again"
	fi
	[ ! -e "$bin" ] || [ -d "$bin" ] || fail "$bin is there and is not a directory"
	[ ! -e "$root" ] || [ -d "$root" ] || fail "$root is there and is not a directory"

	# --- the tarball, fetched into a stage beside where it is installed ------

	tarball=$name-$version-$platform.tar.gz
	base=${tarball%.tar.gz}
	from=${LELA_INSTALL_FROM:-$release}
	from=${from%/}

	mkdirs "$root"
	stage=$(mktemp -d "$root/.install.XXXXXX")
	say "downloading $tarball and SHA256SUMS from $from"
	fetch "$from/SHA256SUMS" "$stage/SHA256SUMS" || fail "could not download $from/SHA256SUMS"
	fetch "$from/$tarball" "$stage/$tarball" || fail "could not download $from/$tarball"

	# sha256sum's lines are `<digest>  <name>`, or `<digest> *<name>` in its
	# binary mode.
	wanted=$(awk -v n="$tarball" '$2 == n || $2 == "*" n { print $1 }' "$stage/SHA256SUMS")
	case "$wanted" in
		"") fail "the release's SHA256SUMS names no $tarball, so it is not installed" ;;
		*[!0-9a-f]*) fail "the release's SHA256SUMS names $tarball more than once or not by a SHA-256 digest, so it is not installed" ;;
	esac
	[ "${#wanted}" -eq 64 ] ||
		fail "the release's SHA256SUMS names $tarball by no SHA-256 digest, so it is not installed"
	got=$(digest "$stage/$tarball" | awk '{ print $1 }')
	[ "$got" = "$wanted" ] ||
		fail "$tarball's SHA-256 is $got, and the release's SHA256SUMS says $wanted, so it is not installed"
	say "$tarball matches the release's SHA256SUMS"

	mkdir "$stage/unpacked"
	tar -xzf "$stage/$tarball" -C "$stage/unpacked" || fail "$tarball does not unpack"
	unpacked=$stage/unpacked/$base
	[ -x "$unpacked/bin/$name" ] || fail "$tarball holds no $base/bin/$name"

	if [ "$system" = Darwin ]; then
		floor=$(sed -n '1s/^macos //p' "$unpacked/bundled-libraries.txt" 2>/dev/null || :)
		[ -n "$floor" ] || fail "$tarball's bundled-libraries.txt names no oldest macOS on its first line"
		here=$(sw_vers -productVersion </dev/null)
		! older "$here" "$floor" ||
			fail "this Mac runs $(macos "$here"), and $name $version runs on $(macos "$floor") or later"
	else
		[ -f "$unpacked/runtime-packages.txt" ] || fail "$tarball holds no $base/runtime-packages.txt"
		missing=
		while IFS= read -r package || [ -n "$package" ]; do
			[ -n "$package" ] || continue
			# shellcheck disable=SC2016 # dpkg-query's field, not the shell's
			dpkg-query -W -f='${Status}\n' "$package" 2>/dev/null </dev/null | grep -qx 'install ok installed' ||
				missing="$missing $package"
		done < "$unpacked/runtime-packages.txt"
		if [ -n "$missing" ]; then
			say "$name $version loads libraries from these Ubuntu packages, which are not installed:$missing" >&2
			say "install them, then run this again:" >&2
			printf '\n    sudo apt-get install%s\n\n' "$missing" >&2
			fail "$name $version is not installed"
		fi
	fi

	banner=$("$unpacked/bin/$name" -v 2>&1 </dev/null) || fail "the unpacked $name -v fails, so it is not installed: $banner"

	# --- put in place --------------------------------------------------------

	dest=$root/$version
	mkdirs "$bin"
	if [ -e "$dest" ]; then
		mv "$dest" "$stage/replaced"
	fi
	mv "$unpacked" "$dest"
	ln -s "$dest/bin/$name" "$stage/link"
	mv -f "$stage/link" "$link"
	if ! banner=$("$link" -v 2>&1 </dev/null); then
		# Undone: the link back where it pointed, the release it replaced back.
		if [ -n "$previous" ]; then
			ln -s "$previous" "$stage/link"
			mv -f "$stage/link" "$link"
		else
			rm -f "$link"
		fi
		rm -rf "$dest"
		[ ! -e "$stage/replaced" ] || mv "$stage/replaced" "$dest"
		fail "$link -v fails, so $name $version is not installed: $banner"
	fi
	installed=1
	# The release the link named before, which this one replaces.
	case "$previous" in
		"$dest/bin/$name" | "") ;;
		*) rm -rf "${previous%/bin/"$name"}" ;;
	esac

	say "installed $name $version in $dest, with $link a link to its bin/$name"
	printf '%s\n' "$banner"
	case ":${PATH:-}:" in
		*":$bin:"*)
			found=$(command -v "$name" 2>/dev/null || :)
			[ -z "$found" ] || [ "$found" = "$link" ] || say "$found comes before $link on PATH, so \`$name\` runs that one"
			;;
		*)
			say "$bin is not on PATH: add it in your shell's profile to run \`$name\` by name, as in"
			# shellcheck disable=SC2016 # the line to write, as written
			printf '\n    export PATH="$HOME/.local/bin:$PATH"\n\n'
			;;
	esac
}

main "$@"
