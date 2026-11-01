#!/usr/bin/env python3
"""oracle-build.py — an oracle build from the pinned tag, as #1782 ruled.

A build is the pinned tag (`oracle.toml` `[source]`) configured with the
main oracle's own configure line, less its install paths, plus only the
flags `[variant.<name>]` names, so it differs from the oracle only where
intended (#1782, #1889). `main` names the main oracle itself, no flags
added: CI's Linux platform oracle (#1891).

Usage (`--pin FILE` before the command reads another pin than oracle.toml,
and `--tree DIR` builds from another checkout of the pinned tag than
`[source]`'s path, as the oracle runner's corpus checkout, #2298):
  scripts/oracle-build.py configure-line
      Print ci/oracle/configure.toml afresh from the pinned oracle's
      `php -n -i` "Configure Command" row, after asserting its md5.
  scripts/oracle-build.py build <platform> <variant> [--jobs N] [--fresh]
      Build for `darwin-arm64` here, or for `linux-x86_64` or
      `linux-aarch64` in Docker (ci/oracle/Dockerfile); the binary lands at
      target/oracle/<platform>/<variant>/php, and the last line is the
      platform's table for the pin. On Linux php-fpm lands beside it, and
      for `main` its `[sapi.<platform>]` table is printed before that line
      and written to sapi.toml (#3385): the php-fpm the bench's tables
      take as their baseline on dax-build (#3774). On macOS a person lays
      it at the table's path, the prefix, which is root's (`sudo mkdir -p`
      and `cp`). On Linux runtime.txt lands beside the binary too: the
      packages that ship the libraries php and php-fpm link, which a host
      running them out of the image installs (#3825).
      The darwin-arm64 table names the digest of the pin's `[closure]`, the
      Homebrew kegs the build is made against, beside its md5 (#2935).
  scripts/oracle-build.py closure
      Print that digest: SHA-256 over each keg's `name=version\n` in name
      order, as xtask's `Closure::digest` makes it and its tests hold.
  scripts/oracle-build.py test <platform> <variant> <phpt>...
      `php -n -m`, then php's own run-tests.php over the named tests of the
      build's tree, with the build as TEST_PHP_EXECUTABLE.

What makes two runs give the same md5, each neutralised here:
  - `__DATE__`/`__TIME__` (main/main.c `php_build_date`, Zend/zend_system_id.c):
    SOURCE_DATE_EPOCH is the pinned commit's committer time, which gcc and
    clang read in UTC. It is the keg's own `Build Date` too.
  - `uname -a` in configure (PHP_UNAME, PHP_BUILD_SYSTEM): PHP_UNAME is set to
    the platform's `uname -s` and `uname -m`, recorded on the configure line
    as any precious variable is.
  - The install prefix in build-defs.h (include_path, extension_dir,
    PHP_BINDIR): fixed at PREFIX/<variant> on every platform.
  - The build directory in debug info (DWARF on Linux, the debug map's object
    paths on macOS) and bison's `#line`s: the tree is laid at BUILD/<variant>
    on every platform, so the paths are the same bytes each run.
  - The debug map's object mtimes on macOS: ZERO_AR_DATE=1, which ld64 reads.
  - The environment (CFLAGS, LDFLAGS, a locale, a time zone): the build runs
    under `env -i` with only the variables named here, TZ=UTC and LC_ALL=C.
  - Libraries and tools: Linux's come from the base image pinned by digest
    and Ubuntu's archive snapshot SNAPSHOT; macOS's are the maintainer's
    Homebrew, which a person keeps at the versions `[closure]` records, and
    the table names that record's digest (#2935).
"""

import argparse
import hashlib
import os
import platform
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from typing import NoReturn

ROOT = Path(__file__).resolve().parent.parent
CONFIGURE = ROOT / "ci/oracle/configure.toml"
DOCKERFILE = ROOT / "ci/oracle/Dockerfile"
PACKAGES = ROOT / ".github/actions/platform-libraries/packages.txt"
# The install prefix every build is configured for, one directory per
# variant; on Linux the image installs there, and a runner lays the binary
# it takes out of the image at the same path (#1948).
PREFIX = "/opt/dax-php/oracle"
# Where the tree is laid and built, the same path on every platform.
BUILD = "/tmp/dax-php-oracle"
# Ubuntu's archive as it stood at this instant (snapshot.ubuntu.com), so a
# rebuild installs the same packages the pinned build linked.
SNAPSHOT = "20261005T000000Z"
# ubuntu:26.04, CI's release (`runs-on: ubuntu-26.04`, #1891), by the digest
# of each platform's manifest under the index
# sha256:f144425ff09be612d6d9ad965196e9cdc23dae1f42110a8a11a3e9a8198759f7.
# A pin's `image` for the platform stands in for these.
BASE = {
    "linux-x86_64": ("linux/amd64", "sha256:88a381d5b5eeb2b35d3ad70925a362c37ce569daf43ede89ff818ec20e4d3794"),
    "linux-aarch64": ("linux/arm64", "sha256:3ad1c71aa6b7e477f7aa91445a5442fbb163742849393b511661a88081c3d7a2"),
}
PLATFORMS = ("darwin-arm64", *BASE)
# The configure options that say where php is installed, not what it is,
# dropped on every platform: the prefix and php.ini's directories, PEAR's
# (whose install step fetches from the network), and the Apache module's
# (apxs installs it into httpd's tree; the module is no part of `php`).
INSTALL_PATHS = (
    "--prefix=",
    "--localstatedir=",
    "--sysconfdir=",
    "--with-config-file-path=",
    "--with-config-file-scan-dir=",
    "--with-pear=",
    "--with-apxs2=",
)
# Where the keg's libraries live on macOS: a value under one of these names
# a library's location there, and on Linux the library is the image's.
MACOS_PATHS = ("/opt/homebrew/", "/Library/Developer/")
# Variables on the line that only locate macOS libraries, dropped on Linux,
# where pkg-config and the compiler's own search find the image's.
MACOS_VARIABLES = ("PKG_CONFIG_PATH=", "PKG_CONFIG_LIBDIR=", "SASL_CFLAGS=", "SASL_LIBS=")
DARWIN_PATH = "/opt/homebrew/opt/bison/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
# The SDK the line itself names (`--with-iconv=…/MacOSX15.sdk/usr`) and the
# keg's `minos` (otool -l, LC_BUILD_VERSION).
DARWIN_SDK = "/Library/Developer/CommandLineTools/SDKs/MacOSX15.sdk"
DARWIN_TARGET = "15.0"
# The line's `--with-bz2=<SDK>/usr` and its kin put `-I<SDK>/usr/include` and
# `-L<SDK>/usr/lib` ahead of the Homebrew libraries' own, so the SDK's old
# libldap and libtidy would shadow the keg's (`ld: symbol(s) not found`).
# Homebrew's compiler shim drops both, the SDK being the sysroot already;
# these shims, first on PATH under the names configure tries, drop the same
# two and nothing else, and are named on no line the binary carries.
DARWIN_COMPILERS = {
    "cc": "/usr/bin/clang", "gcc": "/usr/bin/clang", "clang": "/usr/bin/clang",
    "c++": "/usr/bin/clang++", "g++": "/usr/bin/clang++", "clang++": "/usr/bin/clang++",
}
DARWIN_SHIM = """#!/bin/sh
for arg; do
    shift
    case $arg in
    -I{sdk}/usr/include | -L{sdk}/usr/lib) ;;
    *) set -- "$@" "$arg" ;;
    esac
done
exec {compiler} "$@"
"""


def fail(message: str) -> NoReturn:
    sys.exit(f"oracle-build: {message}")


def md5(path: Path) -> str:
    return hashlib.md5(path.read_bytes()).hexdigest()


def run(argv, **kwargs) -> subprocess.CompletedProcess:
    print("+", shlex.join(str(a) for a in argv), file=sys.stderr, flush=True)
    return subprocess.run(argv, check=True, **kwargs)


class Pin:
    """What the recipe reads of a pin: the oracle, the tree and a variant."""

    def __init__(self, path: Path, variant: str, tree: Path | None = None):
        self.checkout = tree
        try:
            pin = tomllib.loads(path.read_text())
        except (OSError, tomllib.TOMLDecodeError) as e:
            fail(f"reading the pin {path}: {e}")
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", variant):
            fail(f"{variant!r}: a variant is named in lowercase letters, digits and hyphens, since it names a directory")
        try:
            self.oracle = pin["oracle"]
            self.source = pin["source"]
            self.closure = pin.get("closure", {})
            if variant == "main":
                self.flags, self.tables = [], {}
            else:
                table = pin.get("variant", {}).get(variant)
                if table is None:
                    fail(f"{path} names no [variant.{variant}]; its `flags` are what the build adds (#1782)")
                self.flags = list(table["flags"])
                self.tables = {k: v for k, v in table.items() if k in PLATFORMS}
        except KeyError as e:
            fail(f"{path} has no {e}, which the recipe reads")
        self.variant = variant

    def tree(self) -> Path:
        return self.checkout or Path(self.source["path"])

    def commit(self) -> str:
        """The pinned commit in full, held to the tag."""
        tree, tag, short = self.tree(), self.source["tag"], self.source["commit"]
        full = subprocess.run(
            ["git", "-C", tree, "rev-parse", f"{tag}^{{commit}}"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        if not full.startswith(short):
            fail(f"{tree}: {tag} is {full}, the pin says {short}")
        return full

    def epoch(self, commit: str) -> str:
        return subprocess.run(
            ["git", "-C", self.tree(), "log", "-1", "--format=%ct", commit],
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    def closure_digest(self) -> str:
        """The digest of `[closure]`, as xtask's `Closure::digest` makes it."""
        if not self.closure:
            fail("the pin records no [closure], which a darwin-arm64 build is made against and names (#2935)")
        kegs = "".join(f"{keg}={version}\n" for keg, version in sorted(self.closure.items()))
        return "sha256:" + hashlib.sha256(kegs.encode()).hexdigest()

    def base(self, plat: str) -> str:
        image = self.tables.get(plat, {}).get("image")
        return f"ubuntu@{image or BASE[plat][1]}"


def configure_line(pin: Pin) -> None:
    """Print ci/oracle/configure.toml from the pinned oracle."""
    php = Path(pin.oracle["path"])
    if md5(php) != pin.oracle["md5"]:
        fail(f"{php} is not the pinned oracle (md5 {md5(php)}, the pin says {pin.oracle['md5']})")
    info = subprocess.run([php, "-n", "-i"], check=True, capture_output=True, text=True).stdout
    row = next((line for line in info.splitlines() if line.startswith("Configure Command => ")), None)
    if row is None:
        fail(f"{php} -n -i prints no Configure Command")
    args = shlex.split(row.removeprefix("Configure Command => "))
    if args[:1] != ["./configure"]:
        fail(f"the Configure Command starts {args[:1]}, not ./configure")
    print("# The pinned oracle's configure line, one argument per line, as `php -n -i`'s")
    print("# \"Configure Command\" row prints it (#1782). Written by")
    print("# `scripts/oracle-build.py configure-line` and read by its `build`, which drops")
    print("# the install paths, adds a variant's flags, and refuses this file once the")
    print("# pin's [oracle].md5 is not the one it was read from.")
    print(f'md5 = "{pin.oracle["md5"]}"')
    print("args = [")
    for arg in args[1:]:
        print(f"    {toml_string(arg)},")
    print("]")


def toml_string(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def arguments(pin: Pin, plat: str) -> list[str]:
    """The configure line for `plat`: the oracle's, its install paths
    dropped, the fixed prefix, the variant's flags and PHP_UNAME."""
    line = tomllib.loads(CONFIGURE.read_text())
    if line["md5"] != pin.oracle["md5"]:
        fail(f"{CONFIGURE.relative_to(ROOT)} was read from {line['md5']}, the pin's oracle is {pin.oracle['md5']}; run `configure-line` again")
    options, variables = [], []
    for arg in line["args"]:
        if arg.startswith(INSTALL_PATHS):
            continue
        if not arg.startswith("--"):
            if plat != "darwin-arm64" and arg.startswith(MACOS_VARIABLES):
                continue
            variables.append(arg)
            continue
        if plat != "darwin-arm64" and any(p in arg for p in MACOS_PATHS):
            arg = linux_option(arg)
        options.append(arg)
    os_name, arch = ("Darwin", "arm64") if plat == "darwin-arm64" else ("Linux", plat.removeprefix("linux-"))
    return [f"--prefix={PREFIX}/{pin.variant}", *options, *pin.flags, *variables, f"PHP_UNAME={os_name} {arch}"]


def linux_option(arg: str) -> str:
    """`--with-x=<macOS path>` as `--with-x`, and `--with-x=kind,<macOS path>`
    as `--with-x=kind`: the library is the image's, found where Linux keeps
    it. Any other shape is refused, so a bump's new option is read, not
    guessed."""
    name, _, value = arg.partition("=")
    if value.startswith(MACOS_PATHS):
        return name
    kind, _, path = value.partition(",")
    if path.startswith(MACOS_PATHS) and "," not in path:
        return f"{name}={kind}"
    fail(f"{arg}: a macOS path in a shape the Linux build does not translate")


def out_dir(plat: str, variant: str) -> Path:
    return ROOT / "target/oracle" / plat / variant


def build(pin: Pin, plat: str, jobs: int, fresh: bool) -> None:
    commit = pin.commit()
    epoch = pin.epoch(commit)
    args = arguments(pin, plat)
    out = out_dir(plat, pin.variant)
    out.mkdir(parents=True, exist_ok=True)
    print("configure line:", shlex.join(args), file=sys.stderr)
    if plat == "darwin-arm64":
        made = f'closure = "{pin.closure_digest()}"'
        build_darwin(pin, commit, epoch, args, jobs, out)
    else:
        made = f'image = "{build_linux(pin, plat, commit, epoch, args, jobs, fresh, out)}"'
    digest = md5(out / "php")
    table = f'path = "{PREFIX}/{pin.variant}/bin/php", md5 = "{digest}", {made}'
    (out / "pin.toml").write_text(f"{plat} = {{ {table} }}\n")
    print(f"{digest}  {out / 'php'}")
    if plat != "darwin-arm64" and pin.variant == "main":
        fpm = md5(out / "php-fpm")
        sapi = f'[sapi.{plat}]\nfpm = {{ path = "{PREFIX}/main/sbin/php-fpm", md5 = "{fpm}", {made} }}\n'
        (out / "sapi.toml").write_text(sapi)
        print(f"{fpm}  {out / 'php-fpm'}")
        print(sapi, end="")
    print(f"{plat} = {{ {table} }}")


def build_darwin(pin: Pin, commit: str, epoch: str, args: list[str], jobs: int, out: Path) -> None:
    if (sys.platform, platform.machine()) != ("darwin", "arm64"):
        fail("darwin-arm64 is built on macOS arm64")
    work = Path(BUILD) / pin.variant
    lock = Path(f"{work}.lock")
    try:
        lock.mkdir(parents=True)
    except FileExistsError:
        fail(f"{lock} stands: another build of {pin.variant} is running, or one died; remove it once none is")
    try:
        shutil.rmtree(work, ignore_errors=True)
        tree = work / "php-src"
        tree.mkdir(parents=True)
        with open(work / "php-src.tar", "wb") as tar:
            run(["git", "-C", pin.tree(), "archive", "--format=tar", commit], stdout=tar)
        run(["tar", "-x", "-f", work / "php-src.tar", "-C", tree])
        shims = work / "cc"
        shims.mkdir()
        for name, compiler in DARWIN_COMPILERS.items():
            shim = shims / name
            shim.write_text(DARWIN_SHIM.format(sdk=DARWIN_SDK, compiler=compiler))
            shim.chmod(0o755)
        env = {
            "PATH": f"{shims}:{DARWIN_PATH}",
            "HOME": os.environ.get("HOME", "/"),
            "TZ": "UTC",
            "LC_ALL": "C",
            "SOURCE_DATE_EPOCH": epoch,
            "ZERO_AR_DATE": "1",
            "SDKROOT": DARWIN_SDK,
            "MACOSX_DEPLOYMENT_TARGET": DARWIN_TARGET,
        }
        steps = [["./buildconf", "--force"], ["./configure", *args], ["make", f"-j{jobs}"]]
        for step in steps:
            run(["env", "-i", *(f"{k}={v}" for k, v in env.items()), *step], cwd=tree)
        # What `make install-cli` copies to the prefix, byte for byte.
        shutil.copy2(tree / "sapi/cli/php", out / "php")
    finally:
        lock.rmdir()


def build_linux(pin: Pin, plat: str, commit: str, epoch: str, args: list[str], jobs: int, fresh: bool, out: Path) -> str:
    docker_platform, _ = BASE[plat]
    base = pin.base(plat)
    tag = f"dax-php-oracle:{pin.variant}-{plat}"
    with tempfile.TemporaryDirectory(prefix="oracle-build-") as context:
        ctx = Path(context)
        with open(ctx / "php-src.tar", "wb") as tar:
            run(["git", "-C", pin.tree(), "archive", "--format=tar", commit], stdout=tar)
        (ctx / "configure.args").write_text("".join(f"{a}\n" for a in args))
        shutil.copy(PACKAGES, ctx / "packages.txt")
        argv = [
            "docker", "build", "--platform", docker_platform, "-f", DOCKERFILE, "-t", tag,
            "--build-arg", f"BASE={base}",
            "--build-arg", f"SNAPSHOT={SNAPSHOT}",
            "--build-arg", f"SOURCE_DATE_EPOCH={epoch}",
            "--build-arg", f"VARIANT={pin.variant}",
            "--build-arg", f"JOBS={jobs}",
        ]
        if fresh:
            argv += ["--no-cache-filter", "build"]
        run([*argv, ctx])
    container = subprocess.run(
        ["docker", "create", "--platform", docker_platform, tag], check=True, capture_output=True, text=True
    ).stdout.strip()
    try:
        run(["docker", "cp", f"{container}:{PREFIX}/{pin.variant}/bin/php", out / "php"])
        run(["docker", "cp", f"{container}:{PREFIX}/{pin.variant}/sbin/php-fpm", out / "php-fpm"])
        run(["docker", "cp", f"{container}:{BUILD}/installed.txt", out / "installed.txt"])
        run(["docker", "cp", f"{container}:{BUILD}/{pin.variant}/runtime.txt", out / "runtime.txt"])
    finally:
        subprocess.run(["docker", "rm", container], capture_output=True)
    built = subprocess.run(
        ["docker", "image", "inspect", "--format", "{{.Id}}", tag], check=True, capture_output=True, text=True
    ).stdout.strip()
    print(f"image {tag} {built}, made from {base}", file=sys.stderr)
    return base.removeprefix("ubuntu@")


def test(pin: Pin, plat: str, phpts: list[str]) -> None:
    php = f"{PREFIX}/{pin.variant}/bin/php"
    tree = f"{BUILD}/{pin.variant}/php-src"
    if plat == "darwin-arm64":
        php = str(out_dir(plat, pin.variant) / "php")
        run([php, "-n", "-m"])
        env = {**os.environ, "TEST_PHP_EXECUTABLE": php, "NO_INTERACTION": "1"}
        run([php, "-n", "run-tests.php", "-q", *phpts], cwd=tree, env=env)
    else:
        docker_platform, _ = BASE[plat]
        tag = f"dax-php-oracle:{pin.variant}-{plat}"
        inner = f"{shlex.quote(php)} -n -m && cd {shlex.quote(tree)} && {shlex.quote(php)} -n run-tests.php -q {shlex.join(phpts)}"
        run(["docker", "run", "--rm", "--platform", docker_platform,
             "-e", f"TEST_PHP_EXECUTABLE={php}", "-e", "NO_INTERACTION=1", tag, "sh", "-c", inner])


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--pin", type=Path, default=ROOT / "oracle.toml")
    parser.add_argument("--tree", type=Path, help="a checkout of the pinned tag, in place of [source]'s path")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("configure-line")
    sub.add_parser("closure")
    build_command = sub.add_parser("build")
    build_command.add_argument("platform", choices=PLATFORMS)
    build_command.add_argument("variant")
    build_command.add_argument("--jobs", type=int, default=4)
    build_command.add_argument("--fresh", action="store_true", help="rebuild the Linux build stage without its cache")
    test_command = sub.add_parser("test")
    test_command.add_argument("platform", choices=PLATFORMS)
    test_command.add_argument("variant")
    test_command.add_argument("phpt", nargs="+")
    opts = parser.parse_args()
    pin = Pin(opts.pin, getattr(opts, "variant", "main"), opts.tree)
    if opts.command == "configure-line":
        configure_line(pin)
    elif opts.command == "closure":
        print(pin.closure_digest())
    elif opts.command == "build":
        build(pin, opts.platform, opts.jobs, opts.fresh)
    else:
        test(pin, opts.platform, opts.phpt)


if __name__ == "__main__":
    main()
