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

    main                  nightly-<variant>, which moves on every merge
                          to main, and sha-<commit>-<variant>
    v<version>           <version>-<variant>, and <variant> unless the
                          version is a pre-release or an older one than a
                          release tag here names; a shallow checkout, which
                          may not hold every tag, is refused

It prints `manifest <variant> <digest>`, the lines --manifests writes,
the digest the sha256 of the bytes the registry holds at every tag of
the list, which must be one image index.

A real step first prints `buildx <version>`, what `docker buildx version`
says, so a run's log names the buildx that answered: each flag and format
the steps and the images workflow pass is one buildx v0.14.0 knows, the
oldest they serve (dist/README.md).

The labels, on each image and as annotations on its manifest list, are
the standard OCI ones spec #9 names: source, revision, version (`nightly`
from main) and licenses, the union of the licences of the workspace's
crates the image's hosts link, HOSTS saying which hosts an image holds,
read from their manifests (`licenses()`); and three that replace the
Ubuntu base's: description and title, `Lela`, and created, the commit's
time in RFC 3339, UTC. The build's
SOURCE_DATE_EPOCH is that same time, so the image loaded and the one
pushed from the cache have one config.

--dry-run prints the platform, each image's labels as `label <variant>
<key>=<value>` and the join's annotations as `annotation <variant> ...`,
the tags and the digests it would join, and builds, smokes and pushes
nothing; without --platform it
asks docker for the daemon's platform and nothing else. A real `build`
refuses a tree with changes, and a release whose tag is not HEAD; a real
`join` refuses those too, and a variant without a digest for each
platform. A crate the hosts link, or a workspace manifest, with no
license line is refused by name, dry run or real, before anything runs.
Whether a release is the newest is read from this checkout's
release tags, so it holds them all: a shallow checkout, which may hold
its own tag alone, is refused, dry run or real, saying what to fetch. The
caller is logged in to ghcr.io with a token that may write packages.

CARGO_BUILD_JOBS (default 1) reaches the build, and SMOKE (default
dist/smoke.sh) is the smoke the build step runs. The hosts' name is
dist/Dockerfile's `ARG NAME` (ADR-0016), which no caller sets.
"""

import argparse
import hashlib
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
        return self.version or "nightly"

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
        return [f"nightly-{variant}", f"sha-{commit.sha}-{variant}"]
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


def read_manifest(root, path):
    """The TOML at `path`, relative to `root`, or a refusal naming it."""
    try:
        with (root / path).open("rb") as manifest:
            return tomllib.load(manifest)
    except FileNotFoundError:
        raise Refusal(f"{path} is not there") from None
    except tomllib.TOMLDecodeError as error:
        raise Refusal(f"{path} is not a manifest TOML reads: {error}") from None


def workspace_package(root=ROOT):
    """Cargo.toml's `[workspace.package]` table."""
    return read_manifest(root, "Cargo.toml").get("workspace", {}).get("package", {})


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


@dataclass(frozen=True)
class Host:
    """One host a release ships: its crate's directory and package, its
    binary's name after the hosts' name (`-cgi` for `lela-cgi`), whether
    every tarball holds it and the images that do; and the directory of a
    prefix it lies in, `bin` or `sbin`, as php's `make install` lays it,
    in a tarball and under an image's /usr/local alike."""

    crate: str
    package: str
    suffix: str
    directory: str
    tarball: bool
    images: tuple[str, ...]

    @property
    def binary(self):
        return name() + self.suffix

    @property
    def path(self):
        """Where it lies under a prefix: `bin/lela`."""
        return f"{self.directory}/{self.binary}"


# Where a release ships each host, as Mitchell ruled on #79 on 2026-11-21:
# all four in every tarball, the debugger host in the `cli` image and in no
# other. The harness's one host table says it, and `cargo xtask hosts
# --list` prints it; the jobs that publish install no Rust toolchain, so
# the scripts read this copy, and a test holds it to that command's lines,
# in its order, wherever cargo is. Every other list of hosts in dist/ is
# read from it or held to it by a test: dist/Dockerfile's cargo line and
# where each of its stages lays a host, dist/release.py's tarballs,
# dist/smoke.sh's and dist/testlib.py's.
HOSTS = (
    Host("sapi/cli", "php_sapi_cli", "", "bin", tarball=True, images=("cli", "fpm", "apache")),
    Host("sapi/cgi", "php_sapi_cgi", "-cgi", "bin", tarball=True, images=("cli", "fpm", "apache")),
    Host("sapi/fpm", "php_sapi_fpm", "-fpm", "sbin", tarball=True, images=("fpm", "apache")),
    Host("sapi/phpdbg", "php_sapi_phpdbg", "-dbg", "bin", tarball=True, images=("cli",)),
)


def in_tarball():
    """The hosts every tarball holds, in the table's order."""
    return [host for host in HOSTS if host.tarball]


def in_image(variant):
    """The hosts the image of `variant` holds, in the table's order."""
    if variant not in VARIANTS:
        raise Refusal(f"no image {variant}; the images are {', '.join(VARIANTS)}")
    return [host for host in HOSTS if variant in host.images]


# The licenses label (ruled on #14), one per image: the union of the
# licences of the workspace's own crates the image's hosts link, read from
# their manifests with tomllib alone: the images workflow's jobs install no
# Rust toolchain, so no cargo is known to be on the runners that publish,
# and a test holds this reading to `cargo metadata` wherever cargo is.
# Third parties' crates and the system's C libraries are the SBOM's.
#
# dist/Dockerfile's one cargo line builds every host of HOSTS, with its
# default features, for each platform of the images, so the features are
# unified across all four; an image's crates are the ones its own hosts
# reach under them. A crate counts where a normal dependency reaches it: dev- and
# build-dependencies link nothing into a host. An optional dependency
# counts where a feature the build turns on names it, as cargo's resolver
# turns it on: `dep:<name>`, `<name>/<feature>`, or the implicit feature of
# its own name; a weak `<name>?/<feature>` turns on no dependency. A target
# table counts where its `cfg` holds for linux/amd64 or linux/arm64, whose
# Rust targets' `cfg` (rustc --print cfg --target) TARGETS names in the keys
# a table here may test; a table testing any other is refused, not guessed.
LINUX = {
    "target_os": "linux", "target_family": "unix", "target_env": "gnu", "target_vendor": "unknown",
    "target_endian": "little", "target_pointer_width": "64", "unix": None,
}
TARGETS = {
    "linux/amd64": ("x86_64-unknown-linux-gnu", {**LINUX, "target_arch": "x86_64"}),
    "linux/arm64": ("aarch64-unknown-linux-gnu", {**LINUX, "target_arch": "aarch64"}),
}
CFG_KEYS = {key for _, cfg in TARGETS.values() for key in cfg} | {"windows"}
# The identifiers of the SPDX License List, version 3.29.0
# (https://spdx.org/licenses/), that a crate here names; `dtoa` is the
# list's own, David M. Gay's licence, LICENSES/dtoa.txt. A term the list
# holds and this set does not is added here from the list; one it does not
# hold is written `LicenseRef-<name>`. Any other is refused.
SPDX = frozenset({
    "Apache-2.0", "BSD-2-Clause", "BSD-2-Clause-Darwin", "BSD-3-Clause", "BSD-4-Clause-UC", "Beerware",
    "LGPL-2.1-only", "LGPL-2.1-or-later", "MIT", "NCSA", "PostgreSQL", "TCL", "Zlib", "dtoa",
})
LICENSE_REF = re.compile(r"LicenseRef-[0-9A-Za-z.-]+")


@dataclass(frozen=True)
class Crate:
    """One crate of the workspace the hosts link: its directory, relative
    to the root, its name and its `license` line."""

    path: str
    name: str
    license: str


@dataclass(frozen=True)
class Dependency:
    """One normal dependency of a crate on one platform: the crate's
    directory where it is a path dependency, else None."""

    path: str | None
    features: tuple
    default: bool
    optional: bool


class License:
    """An SPDX license expression, parsed into the terms its top-level AND
    joins: an identifier, or a choice, an OR, which stays one term. An AND
    in parentheses is flattened, which says no more than it did."""

    def __init__(self, text, where):
        self.where = where
        self.tokens = re.findall(r"\(|\)|[^\s()]+", text)
        self.at = 0
        tree = self.disjunction()
        if self.at != len(self.tokens):
            raise Refusal(f"{where}: {text!r} is not a license expression")
        self.terms = [self.render(term) for term in self.conjuncts(tree)]

    def peek(self):
        return self.tokens[self.at] if self.at < len(self.tokens) else None

    def disjunction(self):
        alternatives = [self.conjunction()]
        while self.peek() == "OR":
            self.at += 1
            alternatives.append(self.conjunction())
        return ("OR", alternatives) if len(alternatives) > 1 else alternatives[0]

    def conjunction(self):
        terms = [self.atom()]
        while self.peek() == "AND":
            self.at += 1
            terms.append(self.atom())
        return ("AND", terms) if len(terms) > 1 else terms[0]

    def atom(self):
        token = self.peek()
        self.at += 1
        if token == "(":
            tree = self.disjunction()
            if self.peek() != ")":
                raise Refusal(f"{self.where}: an unclosed parenthesis in {' '.join(self.tokens)!r}")
            self.at += 1
            return tree
        if token is None or token in ("AND", "OR", ")"):
            raise Refusal(f"{self.where}: {' '.join(self.tokens)!r} is not a license expression")
        if token not in SPDX and not LICENSE_REF.fullmatch(token):
            raise Refusal(
                f"{self.where} names {token}, which is neither an identifier of the SPDX License List "
                "publish.py names nor a LicenseRef-"
            )
        return token

    def conjuncts(self, tree):
        if isinstance(tree, tuple) and tree[0] == "AND":
            return [term for each in tree[1] for term in self.conjuncts(each)]
        return [tree]

    def render(self, tree, inner=False):
        if isinstance(tree, str):
            return tree
        operator, parts = tree
        text = f" {operator} ".join(self.render(part, True) for part in parts)
        return f"({text})" if operator == "OR" or inner else text


def union(lead, lines):
    """One SPDX expression of the terms of `lead`, the workspace's line, in
    its order, and then every other term of `lines`, each once: the
    identifiers by name, ignoring case, and then the choices, so that the
    order is not the order the lines were read in."""
    first = License(lead, "Cargo.toml's [workspace.package] license").terms
    rest = {term for line in lines for term in License(line, "a license line").terms} - set(first)
    ordered = sorted(rest, key=lambda term: (term.startswith("("), term.casefold(), term))
    return " AND ".join(dict.fromkeys(first + ordered))


def holds(target, platform):
    """Whether a `[target.<target>]` table applies on `platform`, one of
    TARGETS: a `cfg(...)` predicate, or a target's name."""
    name, cfg = TARGETS[platform]
    if not target.startswith("cfg("):
        return target == name
    tokens = re.findall(r'"[^"]*"|[A-Za-z_][A-Za-z0-9_]*|[(),=]', target)
    at = 0

    def peek():
        return tokens[at] if at < len(tokens) else None

    def take(expected=None):
        nonlocal at
        token = peek()
        if expected is not None and token != expected:
            raise Refusal(f"[target.'{target}'] is not a cfg publish.py reads")
        at += 1
        return token

    def predicate():
        name = take()
        if name in ("all", "any", "not"):
            take("(")
            found = []
            while peek() not in (")", None):
                found.append(predicate())
                if peek() == ",":
                    take(",")
            take(")")
            if name == "not":
                if len(found) != 1:
                    raise Refusal(f"[target.'{target}'] is not a cfg publish.py reads")
                return not found[0]
            return all(found) if name == "all" else any(found)
        if name not in CFG_KEYS:
            raise Refusal(
                f"[target.'{target}'] tests {name}, which publish.py does not model for the images' platforms"
            )
        if peek() == "=":
            take("=")
            value = take() or ""
            if not value.startswith('"'):
                raise Refusal(f"[target.'{target}'] is not a cfg publish.py reads")
            return name in cfg and cfg[name] == value[1:-1]
        return name in cfg and cfg[name] is None

    take("cfg")
    take("(")
    found = predicate()
    take(")")
    if at != len(tokens):
        raise Refusal(f"[target.'{target}'] is not a cfg publish.py reads")
    return found


def dependencies(path, manifest, workspace, platform):
    """The normal dependencies of the crate at `path` on `platform`, by the
    name its manifest gives each."""
    tables = [manifest.get("dependencies", {})]
    try:
        tables += [
            table.get("dependencies", {}) for target, table in manifest.get("target", {}).items()
            if holds(target, platform)
        ]
    except Refusal as refusal:
        raise Refusal(f"{path}/Cargo.toml: {refusal}") from None
    found = {}
    for table in tables:
        for name, spec in table.items():
            spec = {"version": spec} if isinstance(spec, str) else spec
            features = tuple(spec.get("features", ()))
            base, default = path, spec.get("default-features", True)
            if spec.get("workspace"):
                inherited = workspace.get("dependencies", {}).get(name)
                if inherited is None:
                    raise Refusal(f"{path}/Cargo.toml: {name} is no dependency of Cargo.toml's [workspace.dependencies]")
                inherited = {"version": inherited} if isinstance(inherited, str) else inherited
                features = tuple(inherited.get("features", ())) + features
                base, default = ".", inherited.get("default-features", True)
                spec = {**inherited, "optional": spec.get("optional", False)}
            target = os.path.normpath(os.path.join(base, spec["path"])) if "path" in spec else None
            found.setdefault(name, []).append(
                Dependency(target, features, default, spec.get("optional", False))
            )
    return found


def linked(variant, root=ROOT):
    """The crates of the workspace the hosts of the image of `variant` link
    on any platform of the images, read from their manifests."""
    top = read_manifest(root, "Cargo.toml")
    workspace = top.get("workspace", {})
    members = {}
    for pattern in workspace.get("members", ()):
        for directory in sorted(root.glob(pattern)):
            if (directory / "Cargo.toml").is_file():
                path = directory.relative_to(root).as_posix()
                members[read_manifest(root, f"{path}/Cargo.toml").get("package", {}).get("name")] = path
    for host in HOSTS:
        if host.package not in members:
            raise Refusal(f"no crate of the workspace is {host.package}, a host dist/Dockerfile builds")
    built = [members[host.package] for host in HOSTS]
    held = [members[host.package] for host in in_image(variant)]
    found = {}
    for platform in TARGETS:
        for crate in resolved(root, workspace, built, held, platform):
            found[crate.path] = crate
    return sorted(found.values(), key=lambda crate: crate.path)


def resolved(root, workspace, hosts, held, platform):
    """The crates the hosts of `held` link on `platform`, where one cargo
    line builds every host of `hosts` with its default features, the
    features unified across them; every crate that line builds is held to
    its license line."""
    manifests, features, deps, enabled, weak = {}, {}, {}, {}, {}
    work = [(host, "default") for host in hosts]

    def use(path, name):
        """The dependency `name` of `path` turned on, and what it turns on."""
        enabled[path].add(name)
        for dependency in deps[path].get(name, ()):
            reach(dependency)
        for feature in weak[path].pop(name, ()):
            for dependency in deps[path].get(name, ()):
                if dependency.path:
                    work.append((dependency.path, feature))

    def reach(dependency):
        if dependency.path is None:
            return
        work.append((dependency.path, None))
        work.extend((dependency.path, feature) for feature in dependency.features)
        if dependency.default:
            work.append((dependency.path, "default"))

    while work:
        path, feature = work.pop()
        if path not in manifests:
            manifest = read_manifest(root, f"{path}/Cargo.toml")
            manifests[path], features[path], enabled[path], weak[path] = manifest, set(), set(), {}
            deps[path] = dependencies(path, manifest, workspace, platform)
            for name, each in deps[path].items():
                for dependency in each:
                    if not dependency.optional:
                        enabled[path].add(name)
                        reach(dependency)
        if feature is None or feature in features[path]:
            continue
        features[path].add(feature)
        table = manifests[path].get("features", {})
        explicit = {entry[4:] for entries in table.values() for entry in entries if entry.startswith("dep:")}
        if feature in table:
            entries = table[feature]
        elif feature in deps[path] and feature not in explicit and any(d.optional for d in deps[path][feature]):
            entries = [f"dep:{feature}"]
        elif feature == "default":
            entries = []
        else:
            raise Refusal(f"{path}/Cargo.toml: no feature {feature} of the crate, which a dependent turns on")
        for entry in entries:
            if entry.startswith("dep:"):
                use(path, entry[4:])
            elif "?/" in entry:
                name, wanted = entry.split("?/", 1)
                if name in enabled[path]:
                    work.extend((d.path, wanted) for d in deps[path].get(name, ()) if d.path)
                else:
                    weak[path].setdefault(name, set()).add(wanted)
            elif "/" in entry:
                name, wanted = entry.split("/", 1)
                if name not in deps[path]:
                    raise Refusal(f"{path}/Cargo.toml: the feature {feature} names {name}, no dependency of the crate")
                use(path, name)
                work.extend((d.path, wanted) for d in deps[path][name] if d.path)
            else:
                work.append((path, entry))
    reached, todo = set(), list(held)
    while todo:
        path = todo.pop()
        if path in reached:
            continue
        reached.add(path)
        todo.extend(d.path for name in enabled[path] for d in deps[path].get(name, ()) if d.path)
    crates = []
    for path, manifest in manifests.items():
        package = manifest.get("package", {})
        name = package.get("name", path)
        line = package.get("license")
        if isinstance(line, dict) and line.get("workspace"):
            line = workspace.get("package", {}).get("license")
            if line is None:
                raise Refusal(f"{path}/Cargo.toml: {name} takes the workspace's license line, and it has none")
        if not isinstance(line, str):
            raise Refusal(
                f"{path}/Cargo.toml: {name}, which the hosts link, has no license line, and the images' "
                "licenses label is the union of every such crate's"
            )
        License(line, f"{path}/Cargo.toml: the license line of {name}")
        if path in reached:
            crates.append(Crate(path, name, line))
    return crates


def licenses(variant, root=ROOT):
    """The licenses label of the image of `variant`: the workspace's own
    line, and then the union of the lines of the crates its hosts link
    (see HOSTS)."""
    lead = workspace_package(root).get("license")
    if not isinstance(lead, str):
        raise Refusal("Cargo.toml's [workspace.package] has no license line, which the images' licenses label leads with")
    return union(lead, [crate.license for crate in linked(variant, root)])


def labels(ref, commit, variant):
    """The labels of the image of `variant`, which the join annotates its
    list with too."""
    prefix = "org.opencontainers.image."
    return [
        (prefix + "source", SOURCE),
        (prefix + "revision", commit.sha),
        (prefix + "version", ref.label),
        (prefix + "licenses", licenses(variant)),
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
        # Read before anything is run, so a tree the labels refuse builds,
        # smokes and pushes nothing.
        labelled = {variant: labels(ref, commit, variant) for variant in VARIANTS}
        if arguments.step == "build":
            build(ref, commit, labelled, arguments)
        else:
            join(ref, commit, labelled, arguments)

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


def answering():
    """Print the buildx that answers this step, `buildx <what docker buildx
    version says>`, so a run's log names it: the runners' buildx is not
    the one a laptop has, and every flag and format the steps pass is held to the
    oldest they serve, v0.14.0 (dist/README.md)."""
    said = subprocess.run(
        ["docker", "buildx", "version"], check=True, capture_output=True, text=True,
    ).stdout.strip()
    print(f"buildx {said}")


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


def build(ref, commit, labelled, arguments):
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
    for variant in VARIANTS:
        for key, value in labelled[variant]:
            print(f"label {variant} {key}={value}")
    if not arguments.dry_run:
        answering()
    shared = [
        "buildx", "build", "-f", str(ROOT / "dist/Dockerfile"), "--platform", platform,
        "--build-arg", f"CARGO_BUILD_JOBS={setting('CARGO_BUILD_JOBS')}",
        "--build-arg", f"SOURCE_DATE_EPOCH={commit.epoch}",
    ]
    common = {
        variant: shared + [flag for key, value in labelled[variant] for flag in ("--label", f"{key}={value}")]
        for variant in VARIANTS
    }
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
            docker(*common[variant], "--target", variant, "--metadata-file", str(loaded), "--tag", local, "--load",
                   str(ROOT))
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
                *common[variant], "--target", variant, "--metadata-file", str(result), "--sbom=true",
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


def fetched(reference):
    """The bytes the registry holds at `reference`, a manifest or an index,
    as pushed: `imagetools inspect --raw` writes them unchanged, so their
    sha256 is the digest the registry names them by."""
    return subprocess.run(
        ["docker", "buildx", "imagetools", "inspect", reference, "--raw"], check=True, capture_output=True,
    ).stdout


def parsed(reference, body):
    try:
        found = json.loads(body)
    except ValueError:
        found = None
    if not isinstance(found, dict):
        raise Refusal(f"{reference} is not a manifest JSON reads")
    return found


def inspect(reference):
    """The manifest or index the registry holds at `reference`, as pushed."""
    return parsed(reference, fetched(reference))


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


def listed(variant, named):
    """The digest of the manifest list `imagetools create` put under the
    tags `named`, read back from the registry: the sha256 of the bytes it
    holds at every tag, which are one image index. No buildx before
    v0.32.0 writes what a create pushed anywhere a script reads, and the
    runners' is older, so the registry's own bytes name the list."""
    held = {fetched(tag) for tag in named}
    if len(held) != 1:
        raise Refusal(f"the tags of {variant} name {len(held)} lists, and one list carries every tag")
    [body] = held
    kind = parsed(named[0], body).get("mediaType")
    if kind not in INDEXES:
        raise Refusal(f"the list of {variant} is an {kind}, and imagetools create pushes an image index")
    return "sha256:" + hashlib.sha256(body).hexdigest()


def digest(result, made):
    """The digest of what `made`, a build, loaded or pushed, read from its
    metadata file's `containerimage.descriptor`, which every build names."""
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


def join(ref, commit, labelled, arguments):
    if not arguments.dry_run:
        built_from(ref, commit)
    found = read_digests(arguments.digests) if arguments.digests or not arguments.dry_run else None
    print(f"ref {ref.name}")
    print(f"commit {commit.sha}")
    annotations = {
        variant: [f"index:{key}={value}" for key, value in labelled[variant]] for variant in VARIANTS
    }
    for variant in VARIANTS:
        for annotation in annotations[variant]:
            print(f"annotation {variant} {annotation}")
    if not arguments.dry_run:
        answering()
    manifests = []
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
        create = ["buildx", "imagetools", "create"]
        for annotation in annotations[variant]:
            create += ["--annotation", annotation]
        for tag in named:
            create += ["--tag", tag]
        docker(*create, *sources)
        manifests.append(f"{variant} {listed(variant, named)}")
    if arguments.dry_run:
        print("dry run: nothing is pushed")
        return
    for line in manifests:
        print(f"manifest {line}")
    if arguments.manifests:
        arguments.manifests.write_text("".join(line + "\n" for line in manifests))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
