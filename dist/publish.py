#!/usr/bin/env python3
"""Publish one ref's images to ghcr.io/lelaphp/lela, in two steps.

    dist/publish.py build <ref> [--platform <platform>] [--digests <file>] [--dry-run]
    dist/publish.py join <ref> [<digests>...] [--manifests <file>] [--dry-run]

<ref> is `main` or a release's tag, `v<version>`.

`build` runs on one platform, the daemon's own unless --platform names
another: it builds the three variants of dist/Dockerfile with the labels
below and loads each, runs `dist/smoke.sh --image` over each, and only once
all three have passed pushes each by digest, with BuildKit's SBOM attached.
A smoke that fails pushes nothing. Each pushed image's config must be the
config of the image smoked. It prints `digest <platform> <variant>
<digest>` per variant, the lines --digests writes too.

`join` reads those lines for both platforms and puts each variant's two
images under one manifest list, tagged with every tag of the variant:

    main                  edge-<variant>, sha-<commit>-<variant>
    v<version>            <version>-<variant>, and <variant> unless the
                          version is a pre-release or an older one than a
                          release tag here names

It prints `manifest <variant> <digest>`, the lines --manifests writes.

The labels, on each image and as annotations on each manifest list, are
the standard OCI ones spec #9 names: source, revision, version (`edge`
from main) and licenses, read from the workspace manifest, and the
description, `Lela`, which replaces the Ubuntu base's. The base's other
labels, its title and creation time among them, the image inherits. The
build's SOURCE_DATE_EPOCH is the commit's time, so the image loaded and
the one pushed from the cache have one config.

--dry-run prints the platform, the labels, the tags and the digests it
would join, and builds, smokes and pushes nothing; without --platform it
asks docker for the daemon's platform and nothing else. A real `build`
refuses a tree with changes, and a release whose tag is not HEAD; a real
`join` refuses a variant without a digest for each platform. The tags
from a release are read from this checkout, so it holds them all, and the
caller is logged in to ghcr.io with a token that may write packages.

NAME (default php) is the hosts' name (ADR-0016), CARGO_BUILD_JOBS
(default 1) reaches the build, and SMOKE (default dist/smoke.sh) is the
smoke the build step runs.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path

REPOSITORY = "lelaphp/lela"
IMAGE = "ghcr.io/" + REPOSITORY
SOURCE = "https://github.com/" + REPOSITORY
VARIANTS = ("cli", "fpm", "apache")
PLATFORMS = ("linux/amd64", "linux/arm64")
ROOT = Path(__file__).resolve().parent.parent
# Semantic Versioning 2.0's version with no build metadata, which a tag
# cannot carry: the core, and a pre-release after a hyphen.
IDENTIFIER = r"(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
VERSION = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    rf"(?:-({IDENTIFIER}(?:\.{IDENTIFIER})*))?"
)
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")


# What the caller's environment sets, or its default: NAME is the hosts'
# name (ADR-0016), CARGO_BUILD_JOBS reaches the build, and SMOKE is the
# smoke a build runs. dist/release.py reads them here too.
DEFAULTS = {"NAME": "php", "CARGO_BUILD_JOBS": "1", "SMOKE": str(ROOT / "dist/smoke.sh")}


def setting(key):
    return os.environ.get(key) or DEFAULTS[key]


class Refusal(Exception):
    pass


@dataclass(frozen=True)
class Ref:
    """`main`, or a release with its version."""

    version: str | None

    @staticmethod
    def parse(name):
        if name == "main":
            return Ref(None)
        if name.startswith("v") and VERSION.fullmatch(name[1:]):
            return Ref(name[1:])
        raise Refusal(f"{name} is neither main nor a release's tag, v<version>")

    @property
    def prerelease(self):
        return self.version is not None and "-" in self.version

    @property
    def label(self):
        return self.version or "edge"

    @property
    def name(self):
        return "v" + self.version if self.version else "main"

    def core(self):
        return tuple(int(part) for part in VERSION.fullmatch(self.version).groups()[:3])


def releases():
    """The versions of the release tags here, pre-releases left out."""
    found = [Ref.parse(tag) for tag in git("tag", "--list", "v*").split() if VERSION.fullmatch(tag[1:])]
    return [ref for ref in found if not ref.prerelease]


def tags(ref, commit, variant):
    if ref.version is None:
        return [f"edge-{variant}", f"sha-{commit.sha}-{variant}"]
    named = [f"{ref.version}-{variant}"]
    if not ref.prerelease and all(other.core() <= ref.core() for other in releases()):
        named.append(variant)
    return named


def git(*arguments):
    return subprocess.run(
        ["git", "-C", str(ROOT), *arguments], check=True, capture_output=True, text=True,
    ).stdout.strip()


@dataclass(frozen=True)
class Commit:
    sha: str
    epoch: int

    @staticmethod
    def head():
        sha, epoch = git("show", "-s", "--format=%H %ct", "HEAD").split()
        return Commit(sha, int(epoch))


def licenses():
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        return tomllib.load(manifest)["workspace"]["package"]["license"]


def labels(ref, commit):
    prefix = "org.opencontainers.image."
    return [
        (prefix + "source", SOURCE),
        (prefix + "revision", commit.sha),
        (prefix + "version", ref.label),
        (prefix + "licenses", licenses()),
        (prefix + "description", "Lela"),
    ]


def main(argv):
    parser = argparse.ArgumentParser(prog="dist/publish.py")
    steps = parser.add_subparsers(dest="step", required=True)
    build_step = steps.add_parser("build")
    build_step.add_argument("ref")
    build_step.add_argument("--platform", choices=PLATFORMS)
    build_step.add_argument("--digests", type=Path)
    build_step.add_argument("--dry-run", action="store_true")
    join_step = steps.add_parser("join")
    join_step.add_argument("ref")
    join_step.add_argument("digests", nargs="*", type=Path)
    join_step.add_argument("--manifests", type=Path)
    join_step.add_argument("--dry-run", action="store_true")
    arguments = parser.parse_args(argv)
    # Each line before the commands that follow it.
    sys.stdout.reconfigure(line_buffering=True)
    try:
        ref = Ref.parse(arguments.ref)
        commit = Commit.head()
        if arguments.step == "build":
            build(ref, commit, arguments)
        else:
            join(ref, commit, arguments)
    except Refusal as refusal:
        print(f"dist/publish.py: {refusal}", file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as failed:
        print(f"dist/publish.py: {' '.join(failed.cmd)}: exit {failed.returncode}", file=sys.stderr)
        return 1
    return 0


def docker(*arguments):
    subprocess.run(["docker", *arguments], check=True)


def metadata(path):
    with path.open() as file:
        return json.load(file)


def daemon():
    return subprocess.run(
        ["docker", "version", "--format", "{{.Server.Os}}/{{.Server.Arch}}"],
        check=True, capture_output=True, text=True,
    ).stdout.strip()


def built_from(ref, commit):
    """Refuse a tree a real run would mislabel: one with changes, or a
    release whose tag is not HEAD. dist/release.py holds its steps to it
    too."""
    if git("status", "--porcelain"):
        raise Refusal("the tree has changes; what is published is built from a commit")
    if ref.version is None:
        return
    try:
        tagged = git("rev-parse", "--verify", "--quiet", f"refs/tags/{ref.name}^{{commit}}")
    except subprocess.CalledProcessError:
        raise Refusal(f"no tag {ref.name} here; a release is built at its tag") from None
    if tagged != commit.sha:
        raise Refusal(f"{ref.name} is not HEAD: it names {tagged}, and HEAD is {commit.sha}")


def build(ref, commit, arguments):
    if not arguments.dry_run:
        built_from(ref, commit)
    platform = arguments.platform or daemon()
    if platform not in PLATFORMS:
        raise Refusal(f"the daemon's platform is {platform}, and the images are {', '.join(PLATFORMS)}")
    print(f"ref {ref.name}")
    print(f"commit {commit.sha}")
    print(f"platform {platform}")
    for key, value in labels(ref, commit):
        print(f"label {key}={value}")
    common = [
        "buildx", "build", "-f", str(ROOT / "dist/Dockerfile"), "--platform", platform,
        "--build-arg", f"NAME={setting('NAME')}",
        "--build-arg", f"CARGO_BUILD_JOBS={setting('CARGO_BUILD_JOBS')}",
        "--build-arg", f"SOURCE_DATE_EPOCH={commit.epoch}",
    ]
    for key, value in labels(ref, commit):
        common += ["--label", f"{key}={value}"]
    with tempfile.TemporaryDirectory() as scratch:
        scratch = Path(scratch)
        smoked = {}
        for variant in VARIANTS:
            local = f"lela-publish:{variant}"
            print(f"build {variant} {local}")
            print(f"smoke {variant} {local}")
            if arguments.dry_run:
                continue
            loaded = scratch / f"load-{variant}.json"
            docker(*common, "--target", variant, "--metadata-file", str(loaded), "--tag", local, "--load", str(ROOT))
            smoked[variant] = config(metadata(loaded), variant)
            if subprocess.run([setting("SMOKE"), "--image", local, variant]).returncode != 0:
                raise Refusal(f"the smoke of {variant} failed; nothing is pushed")
        pushed = []
        for variant in VARIANTS:
            print(f"push {variant} {IMAGE} by digest")
            if arguments.dry_run:
                continue
            result = scratch / f"push-{variant}.json"
            docker(
                *common, "--target", variant, "--metadata-file", str(result), "--sbom=true",
                "--output", f"type=image,name={IMAGE},push-by-digest=true,name-canonical=true,push=true",
                str(ROOT),
            )
            result = metadata(result)
            if config(result, variant) != smoked[variant]:
                raise Refusal(
                    f"{variant}'s pushed config is {config(result, variant)}, and the one smoked "
                    f"{smoked[variant]}; its digest is named nowhere"
                )
            pushed.append(f"{platform} {variant} {digest(result, variant)}")
    if arguments.dry_run:
        print("dry run: nothing is built, smoked or pushed")
        return
    for line in pushed:
        print(f"digest {line}")
    if arguments.digests:
        arguments.digests.write_text("".join(line + "\n" for line in pushed))


def config(result, variant):
    found = result.get("containerimage.config.digest", "")
    if not DIGEST.fullmatch(found):
        raise Refusal(f"the build of {variant} named no config digest")
    return found


def digest(result, variant):
    found = result.get("containerimage.digest", "")
    if not DIGEST.fullmatch(found):
        raise Refusal(f"the push of {variant} named no digest")
    return found


def read_digests(paths):
    """The build steps' `<platform> <variant> <digest>` lines: one digest
    of each variant for each platform, or a refusal."""
    found = {}
    for path in paths:
        for line in path.read_text().splitlines():
            fields = line.split()
            if not fields:
                continue
            if (
                len(fields) != 3 or fields[0] not in PLATFORMS or fields[1] not in VARIANTS
                or not DIGEST.fullmatch(fields[2])
            ):
                raise Refusal(f"{path}: {line!r} is not a digest line, <platform> <variant> <digest>")
            platform, variant, named = fields
            if (variant, platform) in found:
                raise Refusal(f"{path}: a second digest of {variant} for {platform}")
            found[variant, platform] = named
    for variant in VARIANTS:
        for platform in PLATFORMS:
            if (variant, platform) not in found:
                raise Refusal(f"no digest of {variant} for {platform}; nothing is joined")
    return found


def join(ref, commit, arguments):
    found = read_digests(arguments.digests) if arguments.digests or not arguments.dry_run else None
    print(f"ref {ref.name}")
    print(f"commit {commit.sha}")
    annotations = [f"index:{key}={value}" for key, value in labels(ref, commit)]
    for annotation in annotations:
        print(f"annotation {annotation}")
    manifests = []
    with tempfile.TemporaryDirectory() as scratch:
        for variant in VARIANTS:
            sources = [
                f"{IMAGE}@{found[variant, platform]}" if found else f"{IMAGE}@<{variant} {platform}>"
                for platform in PLATFORMS
            ]
            named = [f"{IMAGE}:{tag}" for tag in tags(ref, commit, variant)]
            for tag in named:
                print(f"tag {tag} from {' '.join(sources)}")
            if arguments.dry_run:
                continue
            result = Path(scratch) / f"{variant}.json"
            create = ["buildx", "imagetools", "create", "--metadata-file", str(result)]
            for annotation in annotations:
                create += ["--annotation", annotation]
            for tag in named:
                create += ["--tag", tag]
            docker(*create, *sources)
            manifests.append(f"{variant} {digest(metadata(result), variant)}")
    if arguments.dry_run:
        print("dry run: nothing is pushed")
        return
    for line in manifests:
        print(f"manifest {line}")
    if arguments.manifests:
        arguments.manifests.write_text("".join(line + "\n" for line in manifests))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
