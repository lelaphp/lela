#!/usr/bin/env bash
# runner.sh — one machine laid down as a self-hosted runner of this repository,
# as Mitchell ruled on 2026-10-07 (#2218, #2253). scripts/ci/wizard.sh copies
# scripts/ci/ to the machine and runs this there; a person can run it by hand
# the same way.
#
#   runner.sh linux --url <repository URL> --token <registration token> [--name <name>]
#       On the Hetzner server, as root, Ubuntu 26.04 (the release packages.txt
#       names, #1724). The packages the jobs need beyond packages.txt, Docker
#       for ci/mysql/compose.yaml, a `runner` user in the `docker` group who
#       may run apt-get, locale-gen and localedef as root with no password,
#       rustup for that user, and the runner, labelled `dax-build`, as a
#       systemd service.
#   runner.sh mac --url <repository URL> --token <registration token> --pin <oracle.json> [--name <name>]
#       On the Mac mini, as the admin user the runner runs as; sudo asks for
#       the password, for Homebrew and the variants' directories. Homebrew,
#       pkgconf and jq, the tools an oracle variant's build needs (bison,
#       re2c, Python for scripts/oracle-build.py), rustup, the pinned php by
#       md5 (<oracle.json> is `xtask status --section oracle --format json`,
#       rendered where the repository is checked out), the directory of each
#       variant's path made the user's, so the oracle jobs lay the variant
#       there by md5 with no root (scripts/ci/oracle-variants.sh, #2298), and
#       the runner, labelled `dax-oracle`, as a launchd agent, which runs
#       while the user is logged in: the Mac logs in on boot.
#
# The toolchain's channel is rust-toolchain.toml's: rustup is laid down with
# no toolchain, and the workspace action installs the channel the checkout
# names. Every step is idempotent: what already stands is said and kept, so a
# run that stopped halfway is run again whole. A registration token lasts an
# hour and is read only where the runner is not yet configured.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
say() { printf '\033[1m%s\033[0m\n' "$*"; }
die() { printf 'runner.sh: %s\n' "$*" >&2; exit 1; }

machine=${1:-}; shift || true
url="" token="" name=$(hostname -s) pin=""
while [ $# -gt 0 ]; do
    case $1 in
        --url) url=$2; shift 2 ;;
        --token) token=$2; shift 2 ;;
        --name) name=$2; shift 2 ;;
        --pin) pin=$2; shift 2 ;;
        *) die "unknown argument $1" ;;
    esac
done
case $machine in linux | mac) ;; *) die "usage: runner.sh linux|mac --url <URL> --token <token> [--pin <oracle.json>] [--name <name>]" ;; esac
[ -n "$url" ] || die "--url names the repository, https://github.com/<owner>/<repo>"

# The runner, downloaded into <dir> as <user>, configured with <labels> and
# installed as the platform's service. A configured runner is kept as it is.
register() {
    local os=$1 arch=$2 dir=$3 user=$4 labels=$5 path=$6 version
    as_user() { if [ "$(id -un)" = "$user" ]; then "$@"; else sudo -u "$user" -H "$@"; fi; }
    if [ -f "$dir/.runner" ]; then
        say "the runner at $dir is configured; kept"
    else
        [ -n "$token" ] || die "--token is the registration token (the wizard fetches it; it lasts an hour)"
        version=$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest | jq -er '.tag_name | ltrimstr("v")')
        say "the runner $version into $dir, as $user, labelled $labels"
        as_user mkdir -p "$dir"
        curl -fsSL "https://github.com/actions/runner/releases/download/v$version/actions-runner-$os-$arch-$version.tar.gz" |
            as_user tar -xz -C "$dir"
        # config.sh writes the PATH it runs under to .path, the service's PATH.
        (cd "$dir" && as_user env PATH="$path" ./config.sh --unattended --url "$url" --token "$token" \
            --name "$name" --labels "$labels" --work _work --replace)
    fi
    if [ -f "$dir/.service" ]; then
        say "the runner's service is installed; kept"
    elif [ "$os" = linux ]; then
        (cd "$dir" && ./svc.sh install "$user")
    else
        (cd "$dir" && ./svc.sh install)
    fi
    (cd "$dir" && { ./svc.sh start || true; ./svc.sh status; })
}

linux() {
    [ "$(id -u)" = 0 ] || die "run as root on the server"
    # shellcheck source=/dev/null
    . /etc/os-release
    [ "$ID $VERSION_ID" = "ubuntu 26.04" ] || die "Ubuntu 26.04 is the release packages.txt names (#1724); this is $PRETTY_NAME"
    say "the packages the jobs need beyond packages.txt"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends build-essential ca-certificates curl git jq unzip zstd \
        locales sudo docker.io docker-compose-v2 nodejs npm gh
    systemctl enable --now docker
    id runner >/dev/null 2>&1 || useradd --create-home --shell /bin/bash runner
    usermod -aG docker runner
    say "the runner's user may run what harness.yml runs as root"
    # apt-get as root and the docker group are each root in effect, so the
    # runner is trusted as the repository's collaborators are: the list
    # names what the jobs run and is no sandbox.
    cat > /etc/sudoers.d/dax-runner <<'EOF'
# harness.yml's steps that need root (#2253): the platform libraries of
# .github/actions/platform-libraries, and the named locales.
runner ALL=(root) NOPASSWD: /usr/bin/apt-get, /usr/sbin/locale-gen, /usr/bin/localedef
EOF
    chmod 0440 /etc/sudoers.d/dax-runner
    visudo -cf /etc/sudoers.d/dax-runner
    say "rustup, with no toolchain until a job names one"
    sudo -u runner -H bash -c '[ -x ~/.cargo/bin/rustup ] || curl -sSf https://sh.rustup.rs | sh -s -- -y -q --default-toolchain none --profile minimal'
    register linux x64 /home/runner/actions-runner runner dax-build \
        /home/runner/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
}

mac() {
    [ "$(id -u)" != 0 ] || die "run as the user the runner runs as, not root"
    [ "$(uname -sm)" = "Darwin arm64" ] || die "the pin is an Apple-silicon keg; this is $(uname -sm)"
    [ -f "$pin" ] || die "--pin names the file \`xtask status --section oracle --format json\` wrote"
    if [ ! -x /opt/homebrew/bin/brew ]; then
        say "Homebrew, and the Command Line Tools with it"
        sudo -v
        NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    fi
    eval "$(/opt/homebrew/bin/brew shellenv)"
    export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_INSTALL_CLEANUP=1
    say "the tools the jobs need beyond the keg's libraries, an oracle variant's build among them"
    brew install pkgconf jq bison re2c python
    say "rustup, with no toolchain until a job names one"
    [ -x "$HOME/.cargo/bin/rustup" ] || curl -sSf https://sh.rustup.rs | sh -s -- -y -q --default-toolchain none --profile minimal
    say "the pinned php, by md5"
    "$here/pinned-php.sh" "$pin"
    say "each oracle variant's directory, the runner's to lay its build in"
    jq -r '.variants // {} | .[].path' "$pin" | while read -r path; do
        dir=$(dirname "$path")
        [ -w "$dir" ] || sudo install -d -o "$(id -un)" "$dir"
    done
    register osx arm64 "$HOME/actions-runner" "$(id -un)" dax-oracle \
        "$HOME/.cargo/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    say "the runner is a launchd agent: it runs while $(id -un) is logged in, so the Mac logs in on boot (the wizard says where)"
}

"$machine"
