"""The publish script, held by its dry run and by a docker that records,
each test in a repository of its own (testlib.py)."""

import hashlib
import json
import os
import shutil
import subprocess
import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path

from testlib import EPOCH, IMAGE, ROOT, VARIANTS, WHEN, Repository


class Readme(unittest.TestCase):
    def test_the_root_readme_gives_every_reference_a_block_of_its_own(self):
        text = (ROOT / "README.md").read_text()
        blocks = [block.split("\n", 1) for block in text.split("```")[1::2]]
        bodies = {body.strip() for _, body in blocks if body.strip()}
        for variant in VARIANTS:
            for tag in (variant, f"<version>-{variant}", f"edge-{variant}", f"sha-<commit>-{variant}"):
                with self.subTest(tag=tag):
                    self.assertIn(f"{IMAGE}:{tag}", bodies)
        self.assertIn("ENV PATH=/opt/lela/compat:$PATH", bodies)


class Bytecode(unittest.TestCase):
    def test_the_checkout_ignores_the_bytecode_an_import_leaves_in_dist(self):
        # The tests import testlib.py and release.py imports publish.py; a
        # real build refuses a tree with changes, so their bytecode is none.
        for name in ("testlib", "publish"):
            path = f"dist/__pycache__/{name}.cpython-{sys.version_info.major}{sys.version_info.minor}.pyc"
            with self.subTest(path=path):
                ignored = subprocess.run(["git", "-C", str(ROOT), "check-ignore", "-q", path])
                self.assertEqual(ignored.returncode, 0, f"{path} is not ignored")


class Publish(Repository):
    def publish(self, *arguments, env=None):
        return self.script("publish.py", *arguments, env=env)


class ThePath(Repository):
    def test_the_scripts_path_holds_the_tests_directories_alone(self):
        self.assertEqual(self.path.split(os.pathsep), [str(self.bin), str(self.scratch / "tools")])
        for tool in ("docker", "gh", "cargo"):
            with self.subTest(tool=tool):
                self.assertIsNone(shutil.which(tool, path=self.path))


class DryRun(Publish):
    def labels(self, version):
        return [
            f"org.opencontainers.image.source=https://github.com/lelaphp/lela",
            f"org.opencontainers.image.revision={self.commit}",
            f"org.opencontainers.image.version={version}",
            "org.opencontainers.image.licenses=MIT AND BSD-3-Clause",
            "org.opencontainers.image.description=Lela",
            "org.opencontainers.image.title=Lela",
            f"org.opencontainers.image.created={WHEN}",
        ]

    def test_a_commit_on_main_is_labelled_edge(self):
        run = self.publish("build", "main", "--platform", "linux/arm64", "--dry-run")
        self.assertEqual(self.lines(run, "label"), self.labels("edge"))
        self.assertEqual(self.lines(run, "platform"), ["linux/arm64"])
        self.assertIn("dry run: nothing is built, smoked or pushed", run.stdout)
        self.assertFalse(self.log.exists())

    def test_a_release_is_labelled_with_its_version(self):
        self.versioned("1.2.0")
        run = self.publish("build", "v1.2.0", "--platform", "linux/amd64", "--dry-run")
        self.assertEqual(self.lines(run, "label"), self.labels("1.2.0"))
        self.versioned("1.3.0-rc.1")
        run = self.publish("build", "v1.3.0-rc.1", "--platform", "linux/amd64", "--dry-run")
        self.assertEqual(self.lines(run, "label"), self.labels("1.3.0-rc.1"))

    def tags(self, ref):
        if ref != "main":
            self.versioned(ref.removeprefix("v"))
        run = self.publish("join", ref, "--dry-run")
        return [line.split(" from ")[0] for line in self.lines(run, "tag")]

    def test_a_commit_on_main_moves_edge_and_tags_its_commit(self):
        self.assertEqual(self.tags("main"), [
            f"{IMAGE}:{tag}"
            for variant in VARIANTS
            for tag in (f"edge-{variant}", f"sha-{self.commit}-{variant}")
        ])

    def test_a_release_moves_the_bare_variant(self):
        self.assertEqual(self.tags("v1.2.0"), [
            f"{IMAGE}:{tag}"
            for variant in VARIANTS
            for tag in (f"1.2.0-{variant}", variant)
        ])

    def test_a_pre_release_does_not_move_the_bare_variant(self):
        self.assertEqual(self.tags("v1.3.0-rc.1"), [
            f"{IMAGE}:1.3.0-rc.1-{variant}" for variant in VARIANTS
        ])

    def test_an_older_release_than_a_tag_here_does_not_move_the_bare_variant(self):
        self.git("tag", "v1.2.0")
        self.git("tag", "v2.0.0-rc.1")
        self.assertEqual(self.tags("v1.1.9"), [f"{IMAGE}:1.1.9-{variant}" for variant in VARIANTS])
        self.assertEqual(self.tags("v1.2.0"), [
            f"{IMAGE}:{tag}" for variant in VARIANTS for tag in (f"1.2.0-{variant}", variant)
        ])
        self.assertEqual(self.tags("v1.10.0"), [
            f"{IMAGE}:{tag}" for variant in VARIANTS for tag in (f"1.10.0-{variant}", variant)
        ])

    def test_a_release_in_a_shallow_checkout_is_refused_saying_what_to_fetch(self):
        clone = self.backport()
        run = self.publish("join", "v1.0.1", "--dry-run")
        self.assertEqual(
            [line.split(" from ")[0] for line in self.lines(run, "tag")],
            [f"{IMAGE}:1.0.1-{variant}" for variant in VARIANTS],
        )
        run = self.script("publish.py", "join", "v1.0.1", "--dry-run", root=clone)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(
            "this checkout is shallow, so it may not hold every release tag, and whether v1.0.1 is the newest "
            "release is not known here; fetch the whole history and every tag first, "
            "`git fetch --unshallow --tags`",
            run.stderr,
        )
        self.assertNotIn("tag ", run.stdout)
        # main and a pre-release ask nothing of the tags.
        self.assertEqual(self.script("publish.py", "join", "main", "--dry-run", root=clone).returncode, 0)

    def test_every_tag_names_its_variant(self):
        for ref in ("main", "v1.2.0", "v1.3.0-rc.1"):
            for tag in self.tags(ref):
                with self.subTest(tag=tag):
                    name = tag.removeprefix(IMAGE + ":")
                    self.assertTrue(
                        any(name == v or name.endswith("-" + v) for v in VARIANTS), name,
                    )

    def test_the_join_annotates_each_list_with_the_labels(self):
        run = self.publish("join", "main", "--dry-run")
        self.assertEqual(self.lines(run, "annotation"), [f"index:{l}" for l in self.labels("edge")])

    def refused(self, ref):
        """Each step on `ref`, dry and real, with what it ran."""
        for arguments in (
            ("build", ref, "--platform", "linux/amd64", "--dry-run"), ("build", ref),
            ("join", ref, "--dry-run"), ("join", ref),
        ):
            with self.subTest(arguments=arguments):
                run = self.publish(*arguments, env={"CALLS": str(self.log)})
                self.assertEqual(run.returncode, 1, run.stdout)
                self.assertEqual(run.stdout, "")
                self.assertFalse(self.log.exists())
                yield run

    def test_a_tag_the_manifest_does_not_name_is_refused_by_both_steps(self):
        self.versioned("1.2.0")
        for ref in ("v1.2.1", "v1.2.0-rc.1", "v1.20.0"):
            for run in self.refused(ref):
                self.assertIn(
                    f"{ref} is not the workspace manifest's version, 1.2.0, from Cargo.toml's "
                    "[workspace.package]",
                    run.stderr,
                )

    def test_a_manifest_with_no_version_line_is_refused_with_the_line_it_lacks(self):
        for run in self.refused("v0.1.0-alpha.1"):
            self.assertIn(
                "Cargo.toml's [workspace.package] has no version line; "
                'a release of v0.1.0-alpha.1 needs version = "0.1.0-alpha.1" there',
                run.stderr,
            )

    def test_a_manifest_toml_does_not_read_is_refused(self):
        (self.root / "Cargo.toml").write_text("changed")
        for run in self.refused("v1.2.0"):
            self.assertIn("Cargo.toml is not a manifest TOML reads", run.stderr)

    def test_a_ref_that_is_neither_main_nor_a_version_is_refused(self):
        for ref in ("develop", "1.2.0", "v1.2", "v1.2.0+build.5", "v01.2.0", "vmain"):
            for step in ("build", "join"):
                with self.subTest(ref=ref, step=step):
                    run = self.publish(step, ref, "--platform", "linux/amd64", "--dry-run") \
                        if step == "build" else self.publish(step, ref, "--dry-run")
                    self.assertEqual(run.returncode, 1, run.stdout)
                    self.assertIn("neither main nor a release's tag", run.stderr)


# A docker that records each call as one JSON line and answers as BuildKit
# would. The image of a target and platform is one manifest, of a config
# that depends on the target alone and of one layer. A loaded build's
# metadata file names that manifest, by its descriptor and by
# `containerimage.digest`, and its config, as the CI runners' daemon's did;
# LOAD=containerd names no config, as a load into this Mac's containerd
# store under a docker-container builder does not, and LOAD=index names an
# index instead, as the docker driver's load into a containerd store does,
# its provenance beside the image. A pushed build is attested, as
# `--sbom=true` and the default provenance make it: what it pushes, into
# REGISTRY, one file per blob named by its digest, is an index of the
# platform's manifest and an attestation manifest of `unknown/unknown`, and
# its metadata names the index alone and no config. `imagetools inspect
# --raw` answers from REGISTRY, by digest or by tag, the bytes as pushed.
# `buildx version` answers BUILDX, and `imagetools create` knows the flags
# of buildx v0.14.0's, the oldest the scripts serve, and refuses any other
# as that buildx run as a docker plugin does, `--metadata-file` among them,
# which a create learns in v0.32.0: it pushes into REGISTRY an index, with
# the `index:` annotations, of one entry per source, where buildx puts the
# sources' own entries, and names it under each tag in REGISTRY/tags.
# DRIFT names a variant whose pushed layer is not the one loaded, as a
# recompression's is, its config the same; INDEX=attestation-first lists
# the attestation manifest first, INDEX=attestation-only leaves the
# platform's out, and INDEX=none pushes the platform's manifest alone, as
# no attested build does; LIST=manifest pushes each list as an image
# manifest, and LIST=split names a second index under every tag but the
# first.
DOCKER = r'''#!/usr/bin/env python3
import hashlib, json, os, sys
arguments = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(["docker", *arguments]) + "\n")
def digest(*parts):
    return "sha256:" + hashlib.sha256(" ".join(parts).encode()).hexdigest()
def value(flag):
    return arguments[arguments.index(flag) + 1]
MANIFEST_TYPE = "application/vnd.oci.image.manifest.v1+json"
INDEX_TYPE = "application/vnd.oci.image.index.v1+json"
def described(blob, **descriptor):
    data = json.dumps(blob).encode()
    named = "sha256:" + hashlib.sha256(data).hexdigest()
    return data, {"mediaType": blob["mediaType"], "digest": named, "size": len(data), **descriptor}
def pushed(blob, **descriptor):
    data, descriptor = described(blob, **descriptor)
    with open(os.path.join(os.environ["REGISTRY"], descriptor["digest"]), "wb") as file:
        file.write(data)
    return descriptor
def manifest(config, layer):
    return {
        "schemaVersion": 2, "mediaType": MANIFEST_TYPE,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config, "size": 1},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": layer, "size": 1}],
    }
if arguments[:1] == ["version"]:
    print(os.environ.get("DAEMON", "linux/amd64"))
elif arguments[:2] == ["buildx", "version"]:
    print(os.environ["BUILDX"])
elif arguments[:2] == ["buildx", "build"]:
    target, platform = value("--target"), value("--platform")
    os_, architecture = platform.split("/")
    config = digest("config", target)
    drift = os.environ.get("DRIFT") == target and "--load" not in arguments
    layer = digest("layer", target, platform, *(["recompressed"] if drift else []))
    if "--load" in arguments:
        _, descriptor = described(manifest(config, layer), platform={"architecture": architecture, "os": os_})
        result = {"containerimage.config.digest": config, "containerimage.descriptor": descriptor}
        if os.environ.get("LOAD") == "containerd":
            del result["containerimage.config.digest"]
        elif os.environ.get("LOAD") == "index":
            _, descriptor = described({"schemaVersion": 2, "mediaType": INDEX_TYPE, "manifests": [descriptor]})
            result = {"containerimage.descriptor": descriptor}
    else:
        image = pushed(manifest(config, layer), platform={"architecture": architecture, "os": os_})
        attestation = pushed(
            manifest(digest("attestation config", target), digest("in-toto", target, platform)),
            annotations={
                "vnd.docker.reference.digest": image["digest"],
                "vnd.docker.reference.type": "attestation-manifest",
            },
            platform={"architecture": "unknown", "os": "unknown"},
        )
        entries = {
            "attestation-first": [attestation, image], "attestation-only": [attestation],
        }.get(os.environ.get("INDEX"), [image, attestation])
        if os.environ.get("INDEX") == "none":
            descriptor = image
        else:
            descriptor = pushed({"schemaVersion": 2, "mediaType": INDEX_TYPE, "manifests": entries})
        result = {"containerimage.descriptor": descriptor}
    result["containerimage.digest"] = descriptor["digest"]
    with open(value("--metadata-file"), "w") as metadata:
        json.dump(result, metadata)
elif arguments[:3] == ["buildx", "imagetools", "inspect"] and arguments[4:] == ["--raw"]:
    tags = os.path.join(os.environ["REGISTRY"], "tags")
    if "@" in arguments[3]:
        name, _, named = arguments[3].partition("@")
    else:
        name, _, tag = arguments[3].rpartition(":")
        tagged = os.path.join(tags, tag)
        named = open(tagged).read() if os.path.exists(tagged) else ""
    path = os.path.join(os.environ["REGISTRY"], named)
    if name != os.environ["IMAGE"] or not named or not os.path.exists(path):
        sys.exit("ERROR: " + arguments[3] + ": not found")
    with open(path, "rb") as file:
        sys.stdout.buffer.write(file.read())
elif arguments[:3] == ["buildx", "imagetools", "create"]:
    known = {"--annotation", "--append", "--builder", "--dry-run", "--file", "-f", "--progress", "--tag", "-t"}
    for argument in arguments[3:]:
        if argument.startswith("-") and argument.split("=")[0] not in known:
            sys.stderr.write(
                "unknown flag: " + argument.split("=")[0] + "\n\n"
                "Usage:  docker buildx imagetools create [OPTIONS] [SOURCE] [SOURCE...]\n\n"
                "Run 'docker buildx imagetools create --help' for more information\n"
            )
            sys.exit(125)
    pairs = list(zip(arguments, arguments[1:]))
    sources = [a for a in arguments if a.startswith(os.environ["IMAGE"] + "@")]
    annotations = dict(v.removeprefix("index:").split("=", 1) for flag, v in pairs if flag == "--annotation")
    if os.environ.get("LIST") == "manifest":
        listed = {**manifest(digest("config", *sources), digest("layer", *sources)), "annotations": annotations}
    else:
        entries = [{"mediaType": INDEX_TYPE, "digest": s.partition("@")[2], "size": 1} for s in sources]
        listed = {"schemaVersion": 2, "mediaType": INDEX_TYPE, "manifests": entries, "annotations": annotations}
    tags = os.path.join(os.environ["REGISTRY"], "tags")
    os.makedirs(tags, exist_ok=True)
    for n, tag in enumerate(v for flag, v in pairs if flag == "--tag"):
        split = os.environ.get("LIST") == "split" and n > 0
        named = pushed({**listed, "annotations": {**annotations, "split": str(n)}} if split else listed)["digest"]
        with open(os.path.join(tags, tag.rpartition(":")[2]), "w") as file:
            file.write(named)
else:
    sys.exit("the recording docker has no answer for " + " ".join(arguments))
'''

# What the stand-in's `buildx version` answers: v0.14.0, the oldest buildx
# the scripts serve, as that release's binary answers.
BUILDX = "github.com/docker/buildx v0.14.0 171fcbeb69d67c90ba7f44f41a9e418f6a6ec1da"

# A smoke that records its call and fails for the variant FAILS names.
SMOKE = r'''#!/usr/bin/env python3
import json, os, sys
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(["smoke", *sys.argv[1:]]) + "\n")
sys.exit(1 if sys.argv[-1] == os.environ.get("FAILS") else 0)
'''


class Recorded(Publish):
    def setUp(self):
        super().setUp()
        self.tool("docker", DOCKER)
        self.tool("smoke", SMOKE)

    def run_step(self, *arguments, **env):
        registry = self.scratch / "registry"
        registry.mkdir(exist_ok=True)
        return self.publish(*arguments, env={
            "CALLS": str(self.log), "IMAGE": IMAGE, "SMOKE": str(self.bin / "smoke"),
            "REGISTRY": str(registry), "BUILDX": BUILDX, **env,
        })

    def calls(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def pushes(self):
        return [c for c in self.calls() if c[:3] == ["docker", "buildx", "build"] and "--load" not in c]

    def flag(self, call, name):
        return call[call.index(name) + 1]

    def assert_buildx_said_first(self, run):
        """The step printed the buildx that answered, which it asked once,
        before any other buildx command."""
        self.assertEqual(self.lines(run, "buildx"), [BUILDX])
        asked = [c for c in self.calls() if c[:2] == ["docker", "buildx"]]
        self.assertEqual(asked[0], ["docker", "buildx", "version"])
        self.assertEqual(asked.count(["docker", "buildx", "version"]), 1)


class Build(Recorded):
    def test_each_variant_is_loaded_smoked_and_only_then_pushed_by_digest(self):
        digests = self.scratch / "digests"
        run = self.run_step("build", "main", "--digests", str(digests))
        self.assertEqual(run.returncode, 0, run.stderr)
        steps = [
            (c[0], self.flag(c, "--target") if c[0] == "docker" else c[-1], "--load" in c)
            for c in self.calls() if c[:3] == ["docker", "buildx", "build"] or c[0] == "smoke"
        ]
        self.assertEqual(steps, [
            ("docker", "cli", True), ("smoke", "cli", False),
            ("docker", "fpm", True), ("smoke", "fpm", False),
            ("docker", "apache", True), ("smoke", "apache", False),
            ("docker", "cli", False), ("docker", "fpm", False), ("docker", "apache", False),
        ])
        for push in self.pushes():
            with self.subTest(target=self.flag(push, "--target")):
                self.assertEqual(self.flag(push, "--platform"), "linux/amd64")
                self.assertEqual(
                    self.flag(push, "--output"),
                    f"type=image,name={IMAGE},push-by-digest=true,name-canonical=true,push=true",
                )
                self.assertIn("--sbom=true", push)
                self.assertNotIn("--tag", push)
                self.assertIn(f"SOURCE_DATE_EPOCH={EPOCH}", push)
                self.assertIn("org.opencontainers.image.description=Lela", push)
        written = digests.read_text().splitlines()
        self.assertEqual([line.split()[:2] for line in written], [["linux/amd64", v] for v in VARIANTS])
        self.assertEqual(self.lines(run, "digest"), written)
        for line in written:
            self.assertRegex(line.split()[2], r"^sha256:[0-9a-f]{64}$")
        self.assert_buildx_said_first(run)

    def test_a_dry_run_asks_no_buildx_its_version(self):
        run = self.run_step("build", "main", "--platform", "linux/amd64", "--dry-run")
        self.assertEqual(self.lines(run, "buildx"), [])
        self.assertEqual(self.calls(), [])

    def test_a_failed_smoke_pushes_nothing(self):
        for variant in VARIANTS:
            with self.subTest(failing=variant):
                self.log.unlink(missing_ok=True)
                run = self.run_step("build", "main", FAILS=variant)
                self.assertEqual(run.returncode, 1, run.stdout)
                self.assertIn(f"the smoke of {variant} failed; nothing is pushed", run.stderr)
                self.assertEqual(self.pushes(), [])
                self.assertNotIn("digest ", run.stdout)

    def test_every_build_replaces_the_bases_title_and_created(self):
        run = self.run_step("build", "main")
        self.assertEqual(run.returncode, 0, run.stderr)
        # The commit's time in RFC 3339, UTC, is the instant the build's
        # SOURCE_DATE_EPOCH names.
        created = datetime.fromtimestamp(int(EPOCH), timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        self.assertEqual(created, WHEN)
        builds = [c for c in self.calls() if c[:3] == ["docker", "buildx", "build"]]
        self.assertEqual(len(builds), 2 * len(VARIANTS))
        for call in builds:
            with self.subTest(target=self.flag(call, "--target"), load="--load" in call):
                self.assertIn(f"SOURCE_DATE_EPOCH={EPOCH}", call)
                self.assertIn("org.opencontainers.image.title=Lela", call)
                self.assertIn(f"org.opencontainers.image.created={created}", call)

    def blob(self, named):
        return json.loads((self.scratch / "registry" / named).read_text())

    def inspected(self):
        return [
            c[4] for c in self.calls()
            if c[:4] == ["docker", "buildx", "imagetools", "inspect"] and c[5:] == ["--raw"]
        ]

    def test_the_smoked_manifest_is_the_pushed_indexs_manifest_for_the_platform(self):
        run = self.run_step("build", "main")
        self.assertEqual(run.returncode, 0, run.stderr)
        indexes = [line.split()[2] for line in self.lines(run, "digest")]
        for index in indexes:
            entries = self.blob(index)["manifests"]
            self.assertEqual(entries[0]["platform"], {"architecture": "amd64", "os": "linux"})
            self.assertEqual(entries[1]["platform"], {"architecture": "unknown", "os": "unknown"})
        # Each push's index alone: the manifest is held by its digest, so
        # nothing reads the manifest itself or its config.
        self.assertEqual(self.inspected(), [f"{IMAGE}@{index}" for index in indexes])

    def test_a_load_that_names_no_config_is_smoked_and_pushed(self):
        # As a load into a containerd store names none.
        digests = self.scratch / "digests"
        run = self.run_step("build", "main", "--digests", str(digests), LOAD="containerd")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual([c[-1] for c in self.calls() if c[0] == "smoke"], list(VARIANTS))
        self.assertEqual([self.flag(c, "--target") for c in self.pushes()], list(VARIANTS))
        self.assertEqual(self.lines(run, "digest"), digests.read_text().splitlines())
        self.assertEqual(len(self.lines(run, "digest")), 3)

    def test_a_load_that_is_an_index_is_refused_before_its_smoke(self):
        # As the docker driver's load into a containerd store is.
        run = self.run_step("build", "main", LOAD="index")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(
            "the loaded build of cli is an application/vnd.oci.image.index.v1+json, and what is smoked "
            "is one image's manifest, as a docker-container builder loads it "
            "(`docker buildx create --use --driver docker-container`)",
            run.stderr,
        )
        self.assertEqual([c for c in self.calls() if c[0] == "smoke"], [])
        self.assertEqual(self.pushes(), [])
        self.assertNotIn("digest ", run.stdout)

    def test_the_attestation_manifest_is_never_taken_for_the_image(self):
        run = self.run_step("build", "main", INDEX="attestation-first")
        self.assertEqual(run.returncode, 0, run.stderr)
        indexes = [line.split()[2] for line in self.lines(run, "digest")]
        for index in indexes:
            self.assertEqual(self.blob(index)["manifests"][0]["platform"], {"architecture": "unknown", "os": "unknown"})
        self.assertEqual(self.inspected(), [f"{IMAGE}@{index}" for index in indexes])

    def test_an_index_with_no_manifest_of_the_platform_names_no_digest(self):
        digests = self.scratch / "digests"
        run = self.run_step("build", "main", "--digests", str(digests), INDEX="attestation-only")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(
            "the push of cli names 0 manifests of linux/amd64 beside its attestations, and one is its image",
            run.stderr,
        )
        self.assertNotIn("digest ", run.stdout)
        self.assertFalse(digests.exists())

    def test_a_push_that_is_no_index_names_no_digest(self):
        run = self.run_step("build", "main", INDEX="none")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(
            "the push of cli is an application/vnd.oci.image.manifest.v1+json, "
            "and an attested build pushes an image index",
            run.stderr,
        )
        self.assertNotIn("digest ", run.stdout)

    def test_a_pushed_manifest_that_is_not_the_smoked_one_names_no_digest(self):
        digests = self.scratch / "digests"
        run = self.run_step("build", "main", "--digests", str(digests), DRIFT="fpm")
        self.assertEqual(run.returncode, 1, run.stdout)
        sha = lambda text: "sha256:" + hashlib.sha256(text.encode()).hexdigest()
        # fpm's pushed manifest, whose layer was recompressed and whose
        # config is the smoked one's: a hold of the config would pass it.
        [image] = [
            path.read_text() for path in (self.scratch / "registry").iterdir()
            if sha("layer fpm linux/amd64 recompressed") in path.read_text()
        ]
        self.assertEqual(json.loads(image)["config"]["digest"], sha("config fpm"))
        smoked = image.replace(sha("layer fpm linux/amd64 recompressed"), sha("layer fpm linux/amd64"))
        self.assertIn(
            f"fpm's pushed manifest for linux/amd64 is {sha(image)}, and the one smoked {sha(smoked)}; "
            "its digest is named nowhere",
            run.stderr,
        )
        self.assertNotIn("digest ", run.stdout)
        self.assertFalse(digests.exists())
        # cli was pushed and held before fpm was refused; apache never was.
        self.assertEqual([self.flag(c, "--target") for c in self.pushes()], ["cli", "fpm"])

    def test_the_daemons_platform_is_built_where_none_is_named(self):
        run = self.run_step("build", "main", DAEMON="linux/arm64")
        self.assertEqual(self.lines(run, "platform"), ["linux/arm64"])
        self.assertEqual({self.flag(c, "--platform") for c in self.pushes()}, {"linux/arm64"})

    def test_a_platform_the_daemon_does_not_run_as_its_own_is_refused_before_a_build(self):
        run = self.run_step("build", "main", "--platform", "linux/arm64")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(
            "an image of linux/arm64 is built on a machine of its own platform, linux/arm64, "
            "and the daemon's is linux/amd64",
            run.stderr,
        )
        self.assertEqual([c[:2] for c in self.calls()], [["docker", "version"]])
        self.log.unlink()
        run = self.run_step("build", "main", "--platform", "linux/arm64", DAEMON="linux/arm64")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual({self.flag(c, "--platform") for c in self.pushes()}, {"linux/arm64"})

    def test_a_docker_not_on_path_is_said_and_no_traceback(self):
        (self.bin / "docker").unlink()
        run = self.run_step("build", "main")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertEqual(run.stderr, "dist/publish.py: docker: not found\n")

    def test_a_failed_command_is_said_with_what_it_wrote(self):
        (self.bin / "docker").write_text("#!/bin/sh\necho 'Cannot connect to the Docker daemon' >&2\nexit 1\n")
        run = self.run_step("build", "main")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("docker version --format {{.Server.Os}}/{{.Server.Arch}}: exit 1", run.stderr)
        self.assertIn("Cannot connect to the Docker daemon", run.stderr)

    def test_a_tree_with_changes_is_refused(self):
        (self.root / "Cargo.toml").write_text("changed")
        run = self.run_step("build", "main")
        self.assertEqual(run.returncode, 1)
        self.assertIn("the tree has changes", run.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_release_is_built_at_its_tag_alone(self):
        self.versioned("1.2.0")
        run = self.run_step("build", "v1.2.0")
        self.assertEqual(run.returncode, 1)
        self.assertIn("no tag v1.2.0 here", run.stderr)
        self.git("tag", "v1.2.0")
        self.git("commit", "-q", "--allow-empty", "-m", "after the release")
        run = self.run_step("build", "v1.2.0")
        self.assertEqual(run.returncode, 1)
        self.assertIn("v1.2.0 is not HEAD", run.stderr)
        self.assertEqual(self.calls(), [])
        self.git("checkout", "-q", "v1.2.0")
        self.assertEqual(self.run_step("build", "v1.2.0").returncode, 0)


class Join(Recorded):
    def digests(self, skip=None):
        """The two build steps' digest files, as their jobs hand them on."""
        files = []
        for platform in ("linux/amd64", "linux/arm64"):
            path = self.scratch / platform.replace("/", "-")
            path.write_text("".join(
                f"{platform} {variant} {self.source(variant, platform).partition('@')[2]}\n"
                for variant in VARIANTS if (platform, variant) != skip
            ))
            files.append(str(path))
        return files

    def source(self, variant, platform):
        """A digest of `variant` for `platform`, one of each pair alone."""
        return f"{IMAGE}@sha256:{(variant + platform.split('/')[1]).encode().hex():0>64}"

    def creates(self):
        return [c for c in self.calls() if c[:4] == ["docker", "buildx", "imagetools", "create"]]

    def held(self, tag):
        """The bytes the registry holds at `tag` of the image."""
        named = (self.scratch / "registry" / "tags" / tag).read_text()
        return (self.scratch / "registry" / named).read_bytes()

    def test_one_list_per_variant_over_both_platforms_carries_every_tag(self):
        manifests = self.scratch / "manifests"
        run = self.run_step("join", "main", *self.digests(), "--manifests", str(manifests))
        self.assertEqual(run.returncode, 0, run.stderr)
        creates = self.creates()
        self.assertEqual(len(creates), 3)
        for variant, create in zip(VARIANTS, creates):
            with self.subTest(variant=variant):
                tagged = [create[i + 1] for i, a in enumerate(create) if a == "--tag"]
                self.assertEqual(tagged, [
                    f"{IMAGE}:edge-{variant}", f"{IMAGE}:sha-{self.commit}-{variant}",
                ])
                self.assertEqual(create[-2:], [
                    self.source(variant, "linux/amd64"), self.source(variant, "linux/arm64"),
                ])
                self.assertIn("index:org.opencontainers.image.description=Lela", create)
                self.assertIn("index:org.opencontainers.image.title=Lela", create)
                self.assertIn(f"index:org.opencontainers.image.created={WHEN}", create)
                # What no buildx before v0.32.0 knows.
                self.assertNotIn("--metadata-file", create)
                self.assertEqual(
                    json.loads(self.held(f"edge-{variant}"))["annotations"]["org.opencontainers.image.title"], "Lela",
                )
        written = manifests.read_text().splitlines()
        # Each list's digest is the sha256 of the bytes the registry holds
        # at its tags.
        self.assertEqual(written, [
            f"{variant} sha256:{hashlib.sha256(self.held(f'edge-{variant}')).hexdigest()}" for variant in VARIANTS
        ])
        self.assertEqual(self.lines(run, "manifest"), written)
        self.assert_buildx_said_first(run)

    def test_each_list_is_read_back_from_every_tag_it_was_given(self):
        run = self.run_step("join", "main", *self.digests())
        self.assertEqual(run.returncode, 0, run.stderr)
        steps = [
            ("create", c[-2]) if c[3] == "create" else ("inspect", c[4])
            for c in self.calls() if c[:3] == ["docker", "buildx", "imagetools"]
        ]
        self.assertEqual(steps, [
            step for variant in VARIANTS for step in (
                ("create", self.source(variant, "linux/amd64")),
                ("inspect", f"{IMAGE}:edge-{variant}"), ("inspect", f"{IMAGE}:sha-{self.commit}-{variant}"),
            )
        ])
        for call in self.calls():
            if call[:4] == ["docker", "buildx", "imagetools", "inspect"]:
                self.assertEqual(call[5:], ["--raw"])

    def test_the_stand_in_refuses_the_metadata_file_as_the_runners_buildx_does(self):
        # Run 44731512379's join, word for word, and no list made.
        registry = self.scratch / "registry"
        registry.mkdir()
        result = self.scratch / "cli.json"
        refused = subprocess.run(
            [str(self.bin / "docker"), "buildx", "imagetools", "create", "--metadata-file", str(result),
             "--tag", f"{IMAGE}:edge-cli", self.source("cli", "linux/amd64"), self.source("cli", "linux/arm64")],
            env={**os.environ, "CALLS": str(self.log), "IMAGE": IMAGE, "REGISTRY": str(registry)},
            capture_output=True, text=True,
        )
        self.assertEqual(refused.returncode, 125)
        self.assertEqual(refused.stderr, (
            "unknown flag: --metadata-file\n\n"
            "Usage:  docker buildx imagetools create [OPTIONS] [SOURCE] [SOURCE...]\n\n"
            "Run 'docker buildx imagetools create --help' for more information\n"
        ))
        self.assertFalse(result.exists())
        self.assertEqual(list(registry.iterdir()), [])

    def test_a_list_that_is_no_index_is_refused(self):
        manifests = self.scratch / "manifests"
        run = self.run_step("join", "main", *self.digests(), "--manifests", str(manifests), LIST="manifest")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(
            "the list of cli is an application/vnd.oci.image.manifest.v1+json, "
            "and imagetools create pushes an image index",
            run.stderr,
        )
        self.assertNotIn("manifest ", run.stdout)
        self.assertFalse(manifests.exists())
        self.assertEqual(len(self.creates()), 1)

    def test_tags_that_name_two_lists_are_refused(self):
        manifests = self.scratch / "manifests"
        run = self.run_step("join", "main", *self.digests(), "--manifests", str(manifests), LIST="split")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("the tags of cli name 2 lists, and one list carries every tag", run.stderr)
        self.assertNotIn("manifest ", run.stdout)
        self.assertFalse(manifests.exists())
        self.assertEqual(len(self.creates()), 1)

    def test_blank_lines_between_digests_are_passed_over(self):
        files = self.digests()
        for path in files:
            Path(path).write_text("\n" + Path(path).read_text() + "\n\n")
        run = self.run_step("join", "main", *files)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(len(self.lines(run, "manifest")), 3)

    def test_a_variant_missing_a_platform_joins_nothing(self):
        run = self.run_step("join", "main", *self.digests(skip=("linux/arm64", "fpm")))
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("no digest of fpm for linux/arm64", run.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_digest_named_twice_or_malformed_joins_nothing(self):
        files = self.digests()
        run = self.run_step("join", "main", files[0], files[0], files[1])
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("a second digest of cli for linux/amd64", run.stderr)
        Path(files[1]).write_text("linux/arm64 cli sha256:abc\n")
        run = self.run_step("join", "main", *files)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("not a digest line", run.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_release_is_joined_at_its_tag_alone(self):
        self.versioned("1.2.0")
        run = self.run_step("join", "v1.2.0", *self.digests())
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("no tag v1.2.0 here; a release is built at its tag, so fetch the tags, `git fetch --tags`", run.stderr)
        self.git("tag", "v1.2.0")
        (self.root / "stray").write_text("")
        run = self.run_step("join", "v1.2.0", *self.digests())
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("the tree has changes", run.stderr)
        self.assertEqual(self.calls(), [])
        (self.root / "stray").unlink()
        run = self.run_step("join", "v1.2.0", *self.digests())
        self.assertEqual(len(self.lines(run, "manifest")), 3)

    def test_the_dry_run_names_the_digests_it_would_join(self):
        self.versioned("1.2.0")
        run = self.run_step("join", "v1.2.0", *self.digests(), "--dry-run")
        self.assertIn(
            f"{IMAGE}:1.2.0-cli from {self.source('cli', 'linux/amd64')} {self.source('cli', 'linux/arm64')}",
            self.lines(run, "tag"),
        )
        self.assertEqual(self.calls(), [])
