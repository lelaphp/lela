#!/usr/bin/env python3
"""Publish one ref's images to ghcr.io/lelaphp/lela, in two steps.

    dist/publish.py build <ref> [--platform <platform>] [--digests <file>] [--dry-run]
    dist/publish.py join <ref> [<digests>...] [--manifests <file>] [--dry-run]

<ref> is `main` or a release's tag, `v<version>`, which every step, dry
or real, first holds to the version line of the workspace manifest's
package table, as dist/release.py does: a tag that names another version
is refused, and so is a manifest with no version line.

`build` runs on one platform, the daemon's own, which --platform may name;
a real build refuses a platform the daemon does not run as its own, so
that no image smoked under emulation is pushed. It builds the three
variants of dist/Dockerfile with the labels below and loads each, runs
`dist/smoke.sh --image` over each, and only once all three have passed
pushes each by digest, with BuildKit's SBOM attached.
A smoke that fails pushes nothing. Each pushed image's manifest must be
the manifest of the image smoked, which the loaded build's metadata
names: an attested push is an index, read from the registry, and the
image's manifest is its entry for the platform, never its attestation
manifest. A load that is an index, as the docker driver's into a
containerd store is, is refused before its smoke. It prints
`digest <platform> <variant> <digest>` per variant, the lines --digests
writes too.

`join` reads those lines for both platforms and puts each variant's two
images under one manifest list, tagged with every tag of the variant:

    main                  edge-<variant>, sha-<commit>-<variant>
    v<version>            <version>-<variant>, and <variant> unless the
                          version is a pre-release or an older one than a
                          release tag here names; a shallow checkout, which
                          may not hold every tag, is refused

It prints `manifest <variant> <digest>`, the lines --manifests writes.

The labels, on each image and as annotations on each manifest list, are
the standard OCI ones spec #9 names: source, revision, version (`edge`
from main) and licenses, read from the workspace manifest; and three
that replace the Ubuntu base's: description and title, `Lela`, and
created, the commit's time in RFC 3339, UTC. The build's
SOURCE_DATE_EPOCH is that same time, so the image loaded and the one
pushed from the cache have one config.

--dry-run prints the platform, the labels, the tags and the digests it
would join, and builds, smokes and pushes nothing; without --platform it
asks docker for the daemon's platform and nothing else. A real `build`
refuses a tree with changes, and a release whose tag is not HEAD; a real
`join` refuses those too, and a variant without a digest for each
platform. Whether a release is the newest is read from this checkout's
release tags, so it holds them all: a shallow checkout, which may hold
its own tag alone, is refused, dry run or real, saying what to fetch. The
caller is logged in to ghcr.io with a token that may write packages.

CARGO_BUILD_JOBS (default 1) reaches the build, and SMOKE (default
dist/smoke.sh) is the smoke the build step runs. The hosts' name is
dist/Dockerfile's `ARG NAME` (ADR-0016), which no caller sets.
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
from datetime import datetime, timezone
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


# What the caller's environment sets, or its default: CARGO_BUILD_JOBS
# reaches the build, and SMOKE is the smoke a build runs. dist/release.py
# reads them here too.
DEFAULTS = {"CARGO_BUILD_JOBS": "1", "SMOKE": str(ROOT / "dist/smoke.sh")}


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
        ref = Ref.release(name)
        if ref is None:
            raise Refusal(f"{name} is neither main nor a release's tag, v<version>")
        return ref

    @staticmethod
    def release(name):
        """The release the tag `name` names, held to the version line of
        the workspace manifest's package table; None where `name` is no
        `v<version>`. The one hold of a tag to the manifest: both scripts
        read a release's ref through it."""
        if not (name.startswith("v") and VERSION.fullmatch(name[1:])):
            return None
        version = name[1:]
        package = workspace_package()
        if "version" not in package:
            raise Refusal(
                "Cargo.toml's [workspace.package] has no version line; "
                f'a release of {name} needs version = "{version}" there'
            )
        if package["version"] != version:
            raise Refusal(
                f"{name} is not the workspace manifest's version, {package['version']}, "
                "from Cargo.toml's [workspace.package]"
            )
        return Ref(version)

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


def releases(ref):
    """The versions of the release tags here, pre-releases left out, which
    say whether `ref` is the newest release. A shallow checkout may hold
    its own tag alone, which would make every backport the newest, so it
    is refused, saying what to fetch."""
    if git("rev-parse", "--is-shallow-repository") == "true":
        raise Refusal(
            f"this checkout is shallow, so it may not hold every release tag, and whether {ref.name} is "
            "the newest release is not known here; fetch the whole history and every tag first, "
            "`git fetch --unshallow --tags` (actions/checkout's `fetch-depth: 0`)"
        )
    found = [Ref(tag[1:]) for tag in git("tag", "--list", "v*").split() if VERSION.fullmatch(tag[1:])]
    return [ref for ref in found if not ref.prerelease]


def tags(ref, commit, variant):
    if ref.version is None:
        return [f"edge-{variant}", f"sha-{commit.sha}-{variant}"]
    named = [f"{ref.version}-{variant}"]
    if not ref.prerelease and all(other.core() <= ref.core() for other in releases(ref)):
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

    @property
    def created(self):
        """The commit's time in RFC 3339, UTC: the instant the build's
        SOURCE_DATE_EPOCH names."""
        return datetime.fromtimestamp(self.epoch, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def workspace_package():
    """Cargo.toml's `[workspace.package]` table."""
    try:
        with (ROOT / "Cargo.toml").open("rb") as manifest:
            return tomllib.load(manifest).get("workspace", {}).get("package", {})
    except tomllib.TOMLDecodeError as error:
        raise Refusal(f"Cargo.toml is not a manifest TOML reads: {error}") from None


def licenses():
    return workspace_package()["license"]


def labels(ref, commit):
    prefix = "org.opencontainers.image."
    return [
        (prefix + "source", SOURCE),
        (prefix + "revision", commit.sha),
        (prefix + "version", ref.label),
        (prefix + "licenses", licenses()),
        (prefix + "description", "Lela"),
        (prefix + "title", "Lela"),
        (prefix + "created", commit.created),
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

    def step():
        ref = Ref.parse(arguments.ref)
        commit = Commit.head()
        if arguments.step == "build":
            build(ref, commit, arguments)
        else:
            join(ref, commit, arguments)

    return refusing("dist/publish.py", step)


def refusing(prog, step):
    """Run `step`, saying a refusal, a command that failed with what it
    wrote to stderr, or a program not on PATH in one message on stderr,
    with exit 1; dist/release.py's steps run under it too."""
    try:
        step()
    except Refusal as refusal:
        print(f"{prog}: {refusal}", file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as failed:
        said = failed.stderr.decode() if isinstance(failed.stderr, bytes) else failed.stderr or ""
        said = f"\n{said.strip()}" if said.strip() else ""
        print(f"{prog}: {' '.join(map(str, failed.cmd))}: exit {failed.returncode}{said}", file=sys.stderr)
        return 1
    except FileNotFoundError as missing:
        print(f"{prog}: {missing.filename}: not found", file=sys.stderr)
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


def native(platform, made):
    """Refuse a platform the daemon does not run as its own, so that what
    is made for it is built and smoked where it is meant to run, and no
    emulation's answers are pushed or released. dist/release.py holds its
    Linux tarballs to it too."""
    here = daemon()
    if here != platform:
        raise Refusal(f"{made} is built on a machine of its own platform, {platform}, and the daemon's is {here}")


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
        raise Refusal(
            f"no tag {ref.name} here; a release is built at its tag, so fetch the tags, `git fetch --tags`"
        ) from None
    if tagged != commit.sha:
        raise Refusal(f"{ref.name} is not HEAD: it names {tagged}, and HEAD is {commit.sha}")


def build(ref, commit, arguments):
    if not arguments.dry_run:
        built_from(ref, commit)
    platform = arguments.platform or daemon()
    if platform not in PLATFORMS:
        raise Refusal(f"the daemon's platform is {platform}, and the images are {', '.join(PLATFORMS)}")
    if not arguments.dry_run:
        native(platform, f"an image of {platform}")
    print(f"ref {ref.name}")
    print(f"commit {commit.sha}")
    print(f"platform {platform}")
    for key, value in labels(ref, commit):
        print(f"label {key}={value}")
    common = [
        "buildx", "build", "-f", str(ROOT / "dist/Dockerfile"), "--platform", platform,
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
            smoked[variant] = loaded_manifest(metadata(loaded), variant)
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
            named = digest(metadata(result), f"the push of {variant}")
            found = pushed_manifest(named, platform, variant)
            if found != smoked[variant]:
                raise Refusal(
                    f"{variant}'s pushed manifest for {platform} is {found}, and the one smoked "
                    f"{smoked[variant]}; its digest is named nowhere"
                )
            pushed.append(f"{platform} {variant} {named}")
    if arguments.dry_run:
        print("dry run: nothing is built, smoked or pushed")
        return
    for line in pushed:
        print(f"digest {line}")
    if arguments.digests:
        arguments.digests.write_text("".join(line + "\n" for line in pushed))


MANIFESTS = ("application/vnd.oci.image.manifest.v1+json", "application/vnd.docker.distribution.manifest.v2+json")
INDEXES = ("application/vnd.oci.image.index.v1+json", "application/vnd.docker.distribution.manifest.list.v2+json")
ATTESTATION = "attestation-manifest"


def loaded_manifest(result, variant):
    """The digest of the manifest of the image the build of `variant`
    loaded, the image smoked: its metadata's descriptor, whose digest is
    the `containerimage.digest` beside it. The image is held by its
    manifest, which covers the config, the layers as compressed and the
    annotations, and which every load names: a load into a containerd
    store may name no config.
    The docker driver's load into a containerd store is an index, the
    build's provenance beside the image, and is refused: a docker-container
    builder loads the image's manifest alone."""
    found = digest(result, f"the loaded build of {variant}")
    kind = result["containerimage.descriptor"].get("mediaType")
    if kind not in MANIFESTS:
        raise Refusal(
            f"the loaded build of {variant} is an {kind}, and what is smoked is one image's manifest, "
            "as a docker-container builder loads it (`docker buildx create --use --driver docker-container`)"
        )
    return found


def inspect(reference):
    """The manifest or index the registry holds at `reference`, as pushed."""
    raw = subprocess.run(
        ["docker", "buildx", "imagetools", "inspect", reference, "--raw"],
        check=True, capture_output=True, text=True,
    ).stdout
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        raise Refusal(f"{reference} is not a manifest JSON reads") from None


def pushed_manifest(named, platform, variant):
    """The digest of the manifest of the image of `platform` that the push
    of `variant` put at `named`, read from the index the registry holds.
    An attested build pushes an index of the platform's manifest and an
    attestation manifest of `unknown/unknown`, so the image's is the one
    entry of the platform that is no attestation."""
    index = inspect(f"{IMAGE}@{named}")
    if index.get("mediaType") not in INDEXES:
        raise Refusal(
            f"the push of {variant} is an {index.get('mediaType')}, and an attested build pushes an image index"
        )
    system, architecture = platform.split("/")
    images = [
        entry for entry in index.get("manifests", [])
        if (entry.get("annotations") or {}).get("vnd.docker.reference.type") != ATTESTATION
        and (entry.get("platform") or {}).get("os") == system
        and (entry.get("platform") or {}).get("architecture") == architecture
    ]
    if len(images) != 1:
        raise Refusal(
            f"the push of {variant} names {len(images)} manifests of {platform} beside its attestations, "
            "and one is its image"
        )
    found = images[0].get("digest") or ""
    if not DIGEST.fullmatch(found):
        raise Refusal(f"the push of {variant} names its manifest of {platform} with no digest")
    return found


def digest(result, made):
    """The digest of what `made`, a push or a list, put in the registry,
    read from its metadata file's `containerimage.descriptor`: the one key
    both `buildx build` and `imagetools create` write, the second writing
    no `containerimage.digest`."""
    found = (result.get("containerimage.descriptor") or {}).get("digest", "")
    if not DIGEST.fullmatch(found):
        raise Refusal(f"{made} named no digest: its metadata's containerimage.descriptor has none")
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
    if not arguments.dry_run:
        built_from(ref, commit)
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
            manifests.append(f"{variant} {digest(metadata(result), f'the list of {variant}')}")
    if arguments.dry_run:
        print("dry run: nothing is pushed")
        return
    for line in manifests:
        print(f"manifest {line}")
    if arguments.manifests:
        arguments.manifests.write_text("".join(line + "\n" for line in manifests))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
