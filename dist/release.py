#!/usr/bin/env python3
"""A release of Lela, built from its tag.

    dist/release.py check <tag>
    dist/release.py build <tag> <platform> <out> [--dry-run]
    dist/release.py publish <tag> [<dir>] [--dry-run]
    dist/release.py matrix

<tag> is `v<version>`, and every step first holds it to the version line
of the workspace manifest's package table, Cargo.toml's
`[workspace.package]`: a tag that names another version is refused, and
so is a manifest with no version line, saying the line it lacks. `check`
does that alone and prints the version.

`build` builds one platform's tarball into <out>,
`<name>-<version>-<platform>.tar.gz`, holding one directory of that name:
`darwin-arm64` with cargo on an Apple silicon Mac, `linux-aarch64` and
`linux-x86_64` from dist/Dockerfile's binaries target on a Docker daemon
of that architecture, and no platform on a machine of another. It then
unpacks the tarball and runs dist/smoke.sh over it on this machine: a
Linux one in the Ubuntu image the Dockerfile builds on, with its
runtime-packages.txt installed, and the macOS hosts once every Mach-O in
the tarball is read to carry an ad-hoc signature. A macOS tarball carries
the libraries its hosts load outside the system's under lib/, each name
rewritten relative to the file that carries it and each rewritten file
signed again ad hoc, with the kegs' licences and the list of them, which
dist/bundle.py lays; its dry run names the libraries the hosts built here
load, where they are built. Last, the release's install.sh installs the
tarball there into a home of its own, as a user runs it, and runs
`lela -v` through its link.

The platforms are one table, PLATFORMS, a row per platform with its name,
how its tarball is built, the `uname` words of a machine of it, the
checks of install.sh it is held to and whether install.sh installs on it,
a row that installs held to checks the script has not refused; the tarballs a release is built and
published with, platforms.json, install.sh's platforms and the release
workflow's build matrix are read from it.

`matrix` prints that build matrix, one line of JSON the workflow's jobs
read with `fromJSON`: the table's platforms in its order, and under
`families` a matrix for each family's job, `darwin` for the platforms
cargo builds on a Mac and `linux` for those dist/Dockerfile builds, a row
per platform with its name and its Docker platform. It takes no tag and
holds nothing; the workflow chooses each row's runner.

`publish` takes the table's tarballs from <dir>, and no other, stages
install.sh beside them, dist/install.sh with the release's name, version,
Ubuntu release, download URL and the table's platforms baked in, and
platforms.json, the
release's version and each platform by its name, its tarball's file name
and whether install.sh installs on it. It writes SHA256SUMS over every
file staged with scripts/dist-sums.sh, and creates the tag's GitHub
Release with them and the sums attached, the sums and the image
references in its notes. It is a
pre-release when the version has a pre-release part, and the latest when
its images move the bare variant tags, which is dist/publish.py's rule,
read from it. The images are that script's to
push, on its release path: this one names their tags and pushes nothing.

--dry-run prints what the step would make, the release's fields, the
image tags and, for `publish`, platforms.json and the platforms baked into
install.sh among it, and builds, smokes, uploads and creates nothing. A real `build` or `publish` refuses
a tree with changes and a tag that is not HEAD; `publish` uses gh,
authorised to write the repository's releases.

The hosts and the tarballs are named after dist/Dockerfile's `ARG NAME`
(ADR-0016), `lela`: a tarball holds `bin/lela`, `bin/lela-cgi`,
`sbin/lela-fpm`, `etc/php.ini-development` and `etc/php.ini-production`,
a Linux one `runtime-packages.txt` too, and a macOS one `lib/`,
`licenses/<keg>/` and `bundled-libraries.txt`. CARGO_BUILD_JOBS (default 1)
reaches the Linux build, and SMOKE (default dist/smoke.sh) is the smoke a
macOS tarball is given.
"""

import argparse
import hashlib
import json
import os
import platform as pyplatform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from dataclasses import dataclass
from pathlib import Path

# The images' script, beside this one; the checkout ignores the bytecode
# the import leaves in dist/__pycache__.
sys.path.insert(0, str(Path(__file__).resolve().parent))
import bundle  # noqa: E402
import publish  # noqa: E402

ROOT = publish.ROOT
Refusal = publish.Refusal


def release(tag):
    """The release `tag` names, held to the workspace manifest's version
    by dist/publish.py's one hold."""
    ref = publish.Ref.release(tag)
    if ref is None:
        raise Refusal(f"{tag} is not a release's tag, v<version>")
    return ref


@dataclass(frozen=True)
class Platform:
    """One platform a release builds a tarball for: its name, the Docker
    platform dist/Dockerfile's binaries target is built for where it is a
    Linux one (None where cargo builds it on an Apple silicon Mac), the
    `uname -s` and `uname -m` words of a machine of it, each pair one
    string, the checks of install.sh a machine of it is held to (`ubuntu`,
    `macos`; None where the installer refuses it), and whether the
    release's install.sh installs on it."""

    name: str
    docker: str | None
    uname: tuple[str, ...]
    checks: str | None
    installs: bool

    def tarball(self, ref):
        return f"{name()}-{ref.version}-{self.name}.tar.gz"

    @property
    def family(self):
        """The release workflow's job that builds its tarball: `linux` where
        dist/Dockerfile builds it, `darwin` where cargo does on a Mac."""
        return "darwin" if self.docker is None else "linux"


# The one platform table: the tarballs a release is built and published
# with, platforms.json, install.sh's platforms and the release workflow's
# build matrix are each read from it,
# one row a line. An Intel Mac is a refusal of the installer, not a
# platform, so it has no row; a shell under Rosetta is the installer's to
# see through, and its machine is the Apple silicon one.
PLATFORMS = (
    Platform("darwin-arm64", docker=None, uname=("Darwin arm64",), checks="macos", installs=True),
    Platform("linux-aarch64", docker="linux/arm64", uname=("Linux aarch64", "Linux arm64"), checks="ubuntu", installs=True),
    Platform("linux-x86_64", docker="linux/amd64", uname=("Linux x86_64",), checks="ubuntu", installs=True),
)


# The release workflow's build jobs, one per way a tarball is built, each
# printed by `matrix` whether or not a row of the table is of it.
FAMILIES = ("darwin", "linux")


def platform_named(text):
    """The table's row of the platform `text` names."""
    for platform in PLATFORMS:
        if platform.name == text:
            return platform
    raise Refusal(f"no platform {text}; a release's are {', '.join(p.name for p in PLATFORMS)}")


def arg(key):
    """The default dist/Dockerfile gives its `ARG <key>`."""
    prefix = f"ARG {key}="
    lines = (ROOT / "dist/Dockerfile").read_text().splitlines()
    named = [line.removeprefix(prefix) for line in lines if line.startswith(prefix)]
    if len(named) != 1:
        raise Refusal(f"dist/Dockerfile names {len(named)} `{prefix}` lines, where it names one")
    return named[0]


def name():
    """The hosts' name, dist/Dockerfile's `ARG NAME` (ADR-0016), which
    xtask's tests hold to php_engine's PRODUCT_NAME."""
    return arg("NAME")


def native(platform):
    """Refuse a platform this machine does not run as its own, so that
    every tarball is built and smoked where it is meant to run."""
    if platform.docker is None:
        here = f"{pyplatform.system()} {pyplatform.machine()}"
        if here != "Darwin arm64":
            raise Refusal(f"{platform.name} is built on an Apple silicon Mac, and this machine is {here}")
        return
    publish.native(platform.docker, platform.name)


def darwin_hosts():
    """The macOS hosts by their path in the tarball's directory."""
    return (f"bin/{name()}", f"bin/{name()}-cgi", f"sbin/{name()}-fpm")


def built_darwin():
    """Where cargo's release build leaves the macOS hosts."""
    return Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target") / "release"


def stage_darwin(stage):
    """The hosts built here with cargo, laid with what they load."""
    subprocess.run([
        "cargo", "build", "--release", "--locked", "-p", "php_sapi_cli", "-p", "php_sapi_cgi", "-p", "php_sapi_fpm",
        "--bin", name(), "--bin", f"{name()}-cgi", "--bin", f"{name()}-fpm",
    ], cwd=ROOT, check=True)
    lay_darwin(stage, built_darwin())


def lay_darwin(stage, built):
    """The hosts from `built`, php's ini files beside them, and the
    libraries they load outside the system's under lib/, with the list of
    them and their licences (dist/bundle.py)."""
    hosts = [stage / host for host in darwin_hosts()]
    for host in hosts:
        host.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(built / host.name, host)
        host.chmod(0o755)
    subprocess.run([sys.executable, str(ROOT / "dist/inputs.py"), "ini", str(stage / "etc")], cwd=ROOT, check=True)
    for line in bundle.bundle(stage, hosts):
        print(f"bundle {line}")


def dry_darwin():
    """What a darwin build would bundle: the rule, and the libraries the
    hosts built here now load, where all three are."""
    print(f"bundle under lib/: every library {', '.join(darwin_hosts())} load outside /usr/lib and /System, "
          "and every library those load")
    built = built_darwin()
    hosts = [built / Path(host).name for host in darwin_hosts()]
    if pyplatform.system() != "Darwin" or not all(host.is_file() for host in hosts):
        print(f"bundle no hosts built in {built} on this machine, so the libraries are named once they are")
        return
    print(f"bundle read from the hosts in {built}")
    for library in bundle.closure(hosts):
        print(f"bundle lib/{library.name} {library.keg} {library.version}")


def stage_linux(stage, platform):
    """dist/Dockerfile's binaries target: the hosts, the ini files and the
    Ubuntu packages the hosts load, in runtime-packages.txt."""
    subprocess.run([
        "docker", "buildx", "build", "-f", str(ROOT / "dist/Dockerfile"), "--platform", platform.docker,
        "--target", "binaries", "--build-arg", f"CARGO_BUILD_JOBS={publish.setting('CARGO_BUILD_JOBS')}",
        "--output", f"type=local,dest={stage}", str(ROOT),
    ], check=True)


def pack(stage, path):
    """The stage as one directory of its name, 0755 and every entry
    root's, which root's extraction restores on Linux; no extended
    attribute or AppleDouble file, whichever tar the machine has."""
    stage.chmod(0o755)

    def owned(member):
        member.uid = member.gid = 0
        member.uname = member.gname = "root"
        return member

    with tarfile.open(path, "w:gz") as archive:
        archive.add(stage, arcname=stage.name, filter=owned)


def smoke(ref, platform, path):
    """dist/smoke.sh over the tarball itself, unpacked on its platform: a
    Linux one in the Ubuntu image the Dockerfile builds on, with its
    runtime-packages.txt installed; the macOS hosts after their ad-hoc
    signature is read on every Mach-O in it. Then the release's
    install.sh over the tarball there, as a user runs it, which ends with
    `lela -v` run through the link it makes."""
    base = path.name.removesuffix(".tar.gz")
    if platform.docker is not None:
        ubuntu = arg("UBUNTU")
        script = "\n".join([
            "set -euo pipefail",
            f"tar -xzf /release/{path.name} -C /tmp",
            f"d=/tmp/{base}",
            # apt through dist/apt.sh, which rides out a mirror's passing 5xx
            # as the Dockerfile's steps do.
            "sh /dist/apt.sh update",
            'xargs -a "$d/runtime-packages.txt" sh /dist/apt.sh install >/dev/null',
            '/dist/smoke.sh "$d"',
            # curl only once the smoke has passed, so that no library it
            # brings stands in for one runtime-packages.txt lacks.
            "sh /dist/apt.sh install curl >/dev/null",
            f"mkdir /tmp/assets && cp /release/{path.name} /tmp/assets/",
            "(cd /tmp/assets && sha256sum -- *.tar.gz > SHA256SUMS)",
            "useradd -m installer",
            f"su installer -c 'LELA_INSTALL_FROM=file:///tmp/assets sh /installer/{INSTALLER}'",
        ])
        with tempfile.TemporaryDirectory() as installer_dir:
            (Path(installer_dir) / INSTALLER).write_text(installer(ref))
            command = [
                "docker", "run", "--rm", "--platform", platform.docker,
                "-v", f"{path.parent}:/release:ro", "-v", f"{ROOT / 'dist'}:/dist:ro",
                "-v", f"{installer_dir}:/installer:ro",
                ubuntu, "bash", "-c", script,
            ]
            if subprocess.run(command).returncode != 0:
                raise Refusal(f"the smoke of {path.name} on {platform.name} failed")
        return
    with tempfile.TemporaryDirectory() as scratch:
        with tarfile.open(path) as archive:
            archive.extractall(scratch, filter="tar")
        unpacked = Path(scratch) / base
        for macho in bundle.machos(unpacked):
            signed = subprocess.run(["codesign", "-dv", str(macho)], capture_output=True, text=True)
            if "Signature=adhoc" not in signed.stderr.splitlines():
                raise Refusal(f"{path.name}'s {macho.relative_to(unpacked)} carries no ad-hoc signature")
        if subprocess.run([publish.setting("SMOKE"), str(unpacked)]).returncode != 0:
            raise Refusal(f"the smoke of {path.name} on {platform.name} failed")
        install_darwin(ref, platform, path, Path(scratch))


# The PATH the macOS install runs on: the system's directories alone, as a
# Mac without Homebrew has them.
SYSTEM_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"


def install_darwin(ref, platform, path, scratch):
    """The release's install.sh over the tarball, from a directory of it
    and its sum, into a home of its own."""
    assets, home = scratch / "assets", scratch / "home"
    assets.mkdir()
    home.mkdir()
    (assets / path.name).symlink_to(path)
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    (assets / "SHA256SUMS").write_text(f"{digest}  {path.name}\n")
    script = scratch / INSTALLER
    script.write_text(installer(ref))
    env = {"HOME": str(home), "PATH": SYSTEM_PATH, "LELA_INSTALL_FROM": assets.as_uri()}
    if subprocess.run(["/bin/sh", str(script)], env=env, cwd=scratch).returncode != 0:
        raise Refusal(f"the install of {path.name} by {INSTALLER} on {platform.name} failed")


def build_tarball(ref, commit, arguments):
    """One platform's tarball, built on that platform and smoked there."""
    platform = platform_named(arguments.platform)
    # The installer the tarball's smoke runs, baked in the dry run too, so
    # that a table the installer cannot follow is refused before a build.
    installer(ref)
    if not arguments.dry_run:
        publish.built_from(ref, commit)
        native(platform)
    artifact = platform.tarball(ref)
    print(f"platform {platform.name}")
    print(f"commit {commit.sha}")
    print(f"artifact {artifact}")
    print(f"smoke {artifact} on {platform.name}")
    if arguments.dry_run:
        if platform.docker is None:
            dry_darwin()
        print("dry run: nothing is built or smoked")
        return
    arguments.out.mkdir(parents=True, exist_ok=True)
    out = arguments.out.resolve()
    path = out / artifact
    stage = out / artifact.removesuffix(".tar.gz")
    shutil.rmtree(stage, ignore_errors=True)
    path.unlink(missing_ok=True)
    if platform.docker is not None:
        stage_linux(stage, platform)
    else:
        stage_darwin(stage)
    pack(stage, path)
    smoke(ref, platform, path)
    print(f"built {path}")


INSTALLER = "install.sh"
PLATFORMS_FILE = "platforms.json"
SUMS = "SHA256SUMS"


def ubuntu():
    """The Ubuntu release dist/Dockerfile's `ARG UBUNTU` names, the one the
    Linux tarballs are built and smoked on."""
    image = arg("UBUNTU")
    found = re.fullmatch(r"ubuntu:(\d+\.\d+)(?:@sha256:[0-9a-f]{64})?", image)
    if found is None:
        raise Refusal(f"dist/Dockerfile's `ARG UBUNTU={image}` names no Ubuntu release, ubuntu:<year>.<month>")
    return found.group(1)


def installer_checks():
    """The checks dist/install.sh has, each the name of a `before_<checks>`
    and an `after_<checks>` function of it."""
    text = (ROOT / "dist" / INSTALLER).read_text()
    before, after = ({found for found in re.findall(rf"^{phase}_([a-z]+)\(\) {{$", text, re.M)}
                     for phase in ("before", "after"))
    return sorted(before & after)


def installer_platforms():
    """install.sh's platforms, rendered from the table: a line per `uname`
    pair of each row, `<platform> installs|refuses <checks> <system>
    <machine>`, the checks `none` where it refuses. A row that installs
    held to checks the script has not is refused, so that the table never
    says the installer installs where it would refuse."""
    checks = installer_checks()
    rows = []
    for platform in PLATFORMS:
        if platform.installs and platform.checks is None:
            raise Refusal(f"{platform.name} installs held to no checks of the installer's; "
                          f"its checks are {', '.join(checks)}")
        if platform.installs and platform.checks not in checks:
            raise Refusal(f"{platform.name} installs held to the installer's {platform.checks} checks, and "
                          f"dist/{INSTALLER} has none of that name; its checks are {', '.join(checks)}")
        held = platform.checks if platform.installs else "none"
        for words in platform.uname:
            if not re.fullmatch(r"[A-Za-z0-9_.-]+ [A-Za-z0-9_.-]+", words):
                raise Refusal(f"{platform.name}'s uname words {words!r} are not `<system> <machine>`")
            rows.append(f"{platform.name} {'installs' if platform.installs else 'refuses'} {held} {words}")
    return rows


def installer(ref):
    """dist/install.sh with the release baked into its five empty lines:
    the hosts' name, the version, the Ubuntu release of the Linux
    tarballs, the URL of the release's assets, which the script fetches
    its tarball and SHA256SUMS from, and the table's platforms."""
    text = (ROOT / "dist" / INSTALLER).read_text()
    values = {
        "name": name(),
        "version": ref.version,
        "ubuntu": ubuntu(),
        "release": f"{publish.SOURCE}/releases/download/{ref.name}",
        "platforms": "\n".join(installer_platforms()),
    }
    for key, value in values.items():
        count = text.count(f"\n{key}=\n")
        if count != 1:
            raise Refusal(f"dist/{INSTALLER} carries {count} empty `{key}=` lines, where it carries one")
        text = text.replace(f"\n{key}=\n", f"\n{key}='{value}'\n")
    return text


def platforms_file(ref):
    """platforms.json, what a reader of the release takes its platforms
    from: the version, and each row of the table by its name, its
    tarball's file name and whether install.sh installs on it."""
    platforms = [
        {"name": platform.name, "tarball": platform.tarball(ref), "installs": platform.installs}
        for platform in PLATFORMS
    ]
    return json.dumps({"version": ref.version, "platforms": platforms}, indent=2) + "\n"


def tarballs(ref, directory):
    """Refuse a `directory` that lacks a tarball of the table's, or holds
    one of another release."""
    if directory is None:
        raise Refusal("publish names the directory the tarballs are in")
    wanted = [directory / platform.tarball(ref) for platform in PLATFORMS]
    for path in wanted:
        if not path.is_file():
            raise Refusal(f"no {path.name} in {directory}")
    for path in sorted(directory.glob("*.tar.gz")):
        if path not in wanted:
            raise Refusal(f"{path.name} in {directory} is no tarball of {ref.name}")


def publish_release(ref, commit, arguments):
    """The GitHub Release of the tag: the tarballs, install.sh,
    platforms.json, and SHA256SUMS over the three."""
    if not arguments.dry_run:
        publish.built_from(ref, commit)
        tarballs(ref, arguments.dir)
    # Baked in the dry run too, so that a script missing a line is refused
    # before a tag's run reaches its release.
    script = installer(ref)
    platforms = platforms_file(ref)
    # SHA256SUMS last, as it is written: over every other file attached.
    summed = [*(platform.tarball(ref) for platform in PLATFORMS), INSTALLER, PLATFORMS_FILE]
    for artifact in [*summed, SUMS]:
        print(f"artifact {artifact}")
    for line in platforms.splitlines():
        print(f"{PLATFORMS_FILE} {line}")
    for line in installer_platforms():
        print(f"{INSTALLER} {line}")
    # The release is the latest where its images move the bare variant
    # tags: the publish script's rule, read from it and kept nowhere else.
    latest = all(variant in publish.tags(ref, commit, variant) for variant in publish.VARIANTS)
    print(f"release tag {ref.name}")
    print(f"release title {ref.name}")
    print(f"release commit {commit.sha}")
    print(f"release prerelease {str(ref.prerelease).lower()}")
    print(f"release latest {str(latest).lower()}")
    images = [
        f"{publish.IMAGE}:{tag}" for variant in publish.VARIANTS for tag in publish.tags(ref, commit, variant)
    ]
    for image in images:
        print(f"image {image}")
    if arguments.dry_run:
        print("dry run: nothing is uploaded or created")
        return
    staged = arguments.dir / INSTALLER
    staged.write_text(script)
    staged.chmod(0o755)
    (arguments.dir / PLATFORMS_FILE).write_text(platforms)
    # The sums, over every file staged, go to the file and its notes; what
    # the script says of a failure reaches the caller.
    subprocess.run(
        [str(ROOT / "scripts/dist-sums.sh"), str(arguments.dir), *summed], check=True, stdout=subprocess.DEVNULL,
    )
    sums = arguments.dir / SUMS
    notes = "".join([
        f"The files of {ref.name}, built from {commit.sha}, and their SHA-256 sums:\n\n",
        "```\n", sums.read_text(), "```\n\n",
        "Its images:\n\n",
        *(f"- `{image}`\n" for image in images),
    ])
    with tempfile.NamedTemporaryFile("w", suffix=".md") as file:
        file.write(notes)
        file.flush()
        subprocess.run([
            "gh", "release", "create", ref.name, "--verify-tag", "--title", ref.name,
            "--notes-file", file.name, *(["--prerelease"] if ref.prerelease else []),
            f"--latest={str(latest).lower()}", *(str(arguments.dir / artifact) for artifact in summed), str(sums),
        ], check=True)
    print(f"released {ref.name}")


def matrix():
    """The release workflow's build matrix, one line of JSON its jobs read
    with `fromJSON`: the table's platforms in its order, which name the
    tarballs the release job lays down, and a matrix for each family's job,
    a row per platform of the family with its name and Docker platform,
    which the workflow chooses the row's runner by. Every family is
    printed, one with no row too, so that a job always finds its own."""
    families = {
        family: {"include": [
            {"platform": platform.name, "docker": platform.docker}
            for platform in PLATFORMS if platform.family == family
        ]}
        for family in FAMILIES
    }
    return json.dumps({"platforms": [platform.name for platform in PLATFORMS], "families": families})


def main(argv):
    parser = argparse.ArgumentParser(prog="dist/release.py")
    steps = parser.add_subparsers(dest="step", required=True)
    check_step = steps.add_parser("check")
    check_step.add_argument("tag")
    build_step = steps.add_parser("build")
    build_step.add_argument("tag")
    build_step.add_argument("platform")
    build_step.add_argument("out", type=Path)
    build_step.add_argument("--dry-run", action="store_true")
    publish_step = steps.add_parser("publish")
    publish_step.add_argument("tag")
    publish_step.add_argument("dir", nargs="?", type=Path)
    publish_step.add_argument("--dry-run", action="store_true")
    steps.add_parser("matrix")
    arguments = parser.parse_args(argv)
    sys.stdout.reconfigure(line_buffering=True)
    if arguments.step == "matrix":
        print(matrix())
        return 0

    def step():
        ref = release(arguments.tag)
        commit = publish.Commit.head()
        print(f"version {ref.version}")
        if arguments.step == "build":
            build_tarball(ref, commit, arguments)
        elif arguments.step == "publish":
            publish_release(ref, commit, arguments)

    return publish.refusing("dist/release.py", step)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
