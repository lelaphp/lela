#!/usr/bin/env python3
"""A release of Lela, built from its tag.

    dist/release.py check <tag>
    dist/release.py build <tag> <platform> <out> [--dry-run]
    dist/release.py publish <tag> [<dir>] [--dry-run]

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
runtime-packages.txt installed, and the macOS hosts once each is read to
carry the linker's ad-hoc signature.

`publish` takes the three tarballs from <dir>, and no other, writes
SHA256SUMS over them with scripts/dist-sums.sh, and creates the tag's
GitHub Release with the four attached, the sums and the image references
in its notes. It is a pre-release when the version has a pre-release
part, and the latest when its images move the bare variant tags, which is
dist/publish.py's rule, read from it. The images are that script's to
push, on its release path: this one names their tags and pushes nothing.

--dry-run prints what the step would make, the release's fields and the
image tags among it, and builds, smokes, uploads and creates nothing. A
real `build` or `publish` refuses a tree with changes and a tag that is
not HEAD; `publish` uses gh, authorised to write the repository's
releases.

NAME (default php) is the hosts' name (ADR-0016), CARGO_BUILD_JOBS
(default 1) reaches the Linux build, and SMOKE (default dist/smoke.sh) is
the smoke a macOS tarball is given.
"""

import argparse
import os
import platform as pyplatform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from pathlib import Path

# The images' script, beside this one, imported without the bytecode that
# would leave dist/__pycache__ in the tree a real run refuses to be dirty.
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import publish  # noqa: E402

ROOT = publish.ROOT
Refusal = publish.Refusal


def release(tag):
    """The release `tag` names, held to the workspace manifest's version."""
    try:
        ref = publish.Ref.parse(tag)
    except Refusal:
        ref = publish.Ref(None)
    if ref.version is None:
        raise Refusal(f"{tag} is not a release's tag, v<version>")
    with (ROOT / "Cargo.toml").open("rb") as file:
        package = tomllib.load(file).get("workspace", {}).get("package", {})
    if "version" not in package:
        raise Refusal(
            "Cargo.toml's [workspace.package] has no version line; "
            f'a release of {tag} needs version = "{ref.version}" there'
        )
    if package["version"] != ref.version:
        raise Refusal(
            f"{tag} is not the workspace manifest's version, {package['version']}, "
            "from Cargo.toml's [workspace.package]"
        )
    return ref


PLATFORMS = ("darwin-arm64", "linux-aarch64", "linux-x86_64")


def tarball(ref, platform):
    return f"{name()}-{ref.version}-{platform}.tar.gz"


def at_tag(ref, commit):
    """Refuse a tree a real run would mislabel: one with changes, or one
    whose HEAD is not the release's tag."""
    if publish.git("status", "--porcelain"):
        raise Refusal("the tree has changes; a release is built from a commit")
    try:
        tagged = publish.git("rev-parse", "--verify", "--quiet", f"refs/tags/{ref.name}^{{commit}}")
    except subprocess.CalledProcessError:
        raise Refusal(f"no tag {ref.name} here; a release is built at its tag") from None
    if tagged != commit.sha:
        raise Refusal(f"{ref.name} is not HEAD: it names {tagged}, and HEAD is {commit.sha}")


def name():
    return os.environ.get("NAME") or "php"


# Each Linux platform's tarball is dist/Dockerfile's binaries target, built
# for the architecture Docker names it by.
LINUX = {"linux-aarch64": "linux/arm64", "linux-x86_64": "linux/amd64"}


def native(platform):
    """Refuse a platform this machine does not run as its own, so that
    every tarball is built and smoked where it is meant to run."""
    if platform == "darwin-arm64":
        here = f"{pyplatform.system()} {pyplatform.machine()}"
        if here != "Darwin arm64":
            raise Refusal(f"darwin-arm64 is built on an Apple silicon Mac, and this machine is {here}")
        return
    daemon = publish.daemon()
    if daemon != LINUX[platform]:
        raise Refusal(
            f"{platform} is built on a machine of its own platform, {LINUX[platform]}, "
            f"and the daemon's is {daemon}"
        )


def stage_darwin(stage):
    """The hosts built here with cargo, and php's ini files beside them."""
    subprocess.run([
        "cargo", "build", "--release", "--locked", "-p", "php_sapi_cli", "-p", "php_sapi_cgi", "-p", "php_sapi_fpm",
        "--bin", name(), "--bin", f"{name()}-cgi", "--bin", f"{name()}-fpm",
    ], cwd=ROOT, check=True)
    built = Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target") / "release"
    for directory, host in (("bin", name()), ("bin", f"{name()}-cgi"), ("sbin", f"{name()}-fpm")):
        (stage / directory).mkdir(parents=True, exist_ok=True)
        shutil.copyfile(built / host, stage / directory / host)
        (stage / directory / host).chmod(0o755)
    subprocess.run([sys.executable, str(ROOT / "dist/inputs.py"), "ini", str(stage / "etc")], cwd=ROOT, check=True)


def stage_linux(stage, platform):
    """dist/Dockerfile's binaries target: the hosts, the ini files and the
    Ubuntu packages the hosts load, in runtime-packages.txt."""
    subprocess.run([
        "docker", "buildx", "build", "-f", str(ROOT / "dist/Dockerfile"), "--platform", LINUX[platform],
        "--target", "binaries", "--build-arg", f"CARGO_BUILD_JOBS={os.environ.get('CARGO_BUILD_JOBS') or '1'}",
        "--build-arg", f"NAME={name()}", "--output", f"type=local,dest={stage}", str(ROOT),
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
    signature is read."""
    base = path.name.removesuffix(".tar.gz")
    if platform in LINUX:
        ubuntu = next(
            line.removeprefix("ARG UBUNTU=")
            for line in (ROOT / "dist/Dockerfile").read_text().splitlines() if line.startswith("ARG UBUNTU=")
        )
        script = "\n".join([
            "set -euo pipefail",
            f"tar -xzf /release/{path.name} -C /tmp",
            f"d=/tmp/{base}",
            "apt-get update -qq",
            'xargs -a "$d/runtime-packages.txt" apt-get install -y -qq --no-install-recommends >/dev/null',
            '/dist/smoke.sh "$d"',
        ])
        command = [
            "docker", "run", "--rm", "--platform", LINUX[platform],
            "-v", f"{path.parent}:/release:ro", "-v", f"{ROOT / 'dist'}:/dist:ro", "-e", f"NAME={name()}",
            ubuntu, "bash", "-c", script,
        ]
        if subprocess.run(command).returncode != 0:
            raise Refusal(f"the smoke of {path.name} on {platform} failed")
        return
    with tempfile.TemporaryDirectory() as scratch:
        with tarfile.open(path) as archive:
            archive.extractall(scratch, filter="tar")
        unpacked = Path(scratch) / base
        for host in (f"bin/{name()}", f"bin/{name()}-cgi", f"sbin/{name()}-fpm"):
            signed = subprocess.run(["codesign", "-dv", str(unpacked / host)], capture_output=True, text=True)
            if "Signature=adhoc" not in signed.stderr.splitlines():
                raise Refusal(f"{path.name}'s {host} carries no ad-hoc signature")
        if subprocess.run([os.environ.get("SMOKE") or str(ROOT / "dist/smoke.sh"), str(unpacked)]).returncode != 0:
            raise Refusal(f"the smoke of {path.name} on {platform} failed")


def build_tarball(ref, commit, arguments):
    """One platform's tarball, built on that platform and smoked there."""
    platform = arguments.platform
    if platform not in PLATFORMS:
        raise Refusal(f"no platform {platform}; a release's are {', '.join(PLATFORMS)}")
    if not arguments.dry_run:
        at_tag(ref, commit)
        native(platform)
    artifact = tarball(ref, platform)
    print(f"platform {platform}")
    print(f"commit {commit.sha}")
    print(f"artifact {artifact}")
    print(f"smoke {artifact} on {platform}")
    if arguments.dry_run:
        print("dry run: nothing is built or smoked")
        return
    arguments.out.mkdir(parents=True, exist_ok=True)
    out = arguments.out.resolve()
    path = out / artifact
    stage = out / artifact.removesuffix(".tar.gz")
    shutil.rmtree(stage, ignore_errors=True)
    path.unlink(missing_ok=True)
    if platform in LINUX:
        stage_linux(stage, platform)
    else:
        stage_darwin(stage)
    pack(stage, path)
    smoke(ref, platform, path)
    print(f"built {path}")


def tarballs(ref, directory):
    """The three tarballs of the release in `directory`, and no other."""
    if directory is None:
        raise Refusal("publish names the directory the tarballs are in")
    wanted = [directory / tarball(ref, platform) for platform in PLATFORMS]
    for path in wanted:
        if not path.is_file():
            raise Refusal(f"no {path.name} in {directory}")
    for path in sorted(directory.glob("*.tar.gz")):
        if path not in wanted:
            raise Refusal(f"{path.name} in {directory} is no tarball of {ref.name}")
    return wanted


def publish_release(ref, commit, arguments):
    """The GitHub Release of the tag, with the tarballs and their sums."""
    if not arguments.dry_run:
        at_tag(ref, commit)
        paths = tarballs(ref, arguments.dir)
    artifacts = [tarball(ref, platform) for platform in PLATFORMS]
    for artifact in [*artifacts, "SHA256SUMS"]:
        print(f"artifact {artifact}")
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
    subprocess.run([str(ROOT / "scripts/dist-sums.sh"), str(arguments.dir)], check=True, capture_output=True)
    sums = arguments.dir / "SHA256SUMS"
    notes = "".join([
        f"The tarballs of {ref.name}, built from {commit.sha}, and their SHA-256 sums:\n\n",
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
            f"--latest={str(latest).lower()}", *map(str, paths), str(sums),
        ], check=True)
    print(f"released {ref.name}")


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
    arguments = parser.parse_args(argv)
    sys.stdout.reconfigure(line_buffering=True)
    try:
        ref = release(arguments.tag)
        commit = publish.Commit.head()
        print(f"version {ref.version}")
        if arguments.step == "build":
            build_tarball(ref, commit, arguments)
        elif arguments.step == "publish":
            publish_release(ref, commit, arguments)
    except Refusal as refusal:
        print(f"dist/release.py: {refusal}", file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as failed:
        print(f"dist/release.py: {' '.join(map(str, failed.cmd))}: exit {failed.returncode}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
