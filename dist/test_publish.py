"""The publish script, held by its dry run and by a docker that records.

Each test runs dist/publish.py in a repository of its own, laid down with
the workspace's Cargo.toml and one commit at a known time, so the commit
the labels and tags name is the test's and not the checkout's.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IMAGE = "ghcr.io/lelaphp/lela"
VARIANTS = ("cli", "fpm", "apache")
# The commit's time, which the `created` label and the build's
# SOURCE_DATE_EPOCH are.
WHEN = "2026-11-19T12:34:56Z"
EPOCH = "1795091696"


class Repository(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "lela"
        (self.root / "dist").mkdir(parents=True)
        shutil.copyfile(ROOT / "Cargo.toml", self.root / "Cargo.toml")
        shutil.copyfile(ROOT / "dist/publish.py", self.root / "dist/publish.py")
        self.bin = Path(self.temporary.name) / "bin"
        self.bin.mkdir()
        self.log = Path(self.temporary.name) / "calls"
        self.git("init", "-q", "-b", "main")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "the tree")
        self.commit = self.git("rev-parse", "HEAD").strip()

    def git(self, *arguments):
        env = {
            **os.environ,
            "GIT_AUTHOR_NAME": "test", "GIT_AUTHOR_EMAIL": "test@example.invalid",
            "GIT_COMMITTER_NAME": "test", "GIT_COMMITTER_EMAIL": "test@example.invalid",
            "GIT_AUTHOR_DATE": WHEN, "GIT_COMMITTER_DATE": WHEN,
            "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
        }
        return subprocess.run(
            ["git", "-C", str(self.root), *arguments],
            env=env, check=True, capture_output=True, text=True,
        ).stdout

    def publish(self, *arguments, env=None):
        # PATH holds git and the test's own bin/ alone: a dry run that ran
        # docker would find none, or the recording one the test laid.
        path = os.pathsep.join([str(self.bin), str(Path(shutil.which("git")).parent)])
        return subprocess.run(
            [sys.executable, str(self.root / "dist/publish.py"), *arguments],
            cwd=self.root, env={**os.environ, "PATH": path, **(env or {})},
            capture_output=True, text=True,
        )

    def lines(self, run, kind):
        self.assertEqual(run.returncode, 0, run.stderr)
        return [line.split(" ", 1)[1] for line in run.stdout.splitlines() if line.startswith(kind + " ")]


class DryRun(Repository):
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
        run = self.publish("build", "v1.2.0", "--platform", "linux/amd64", "--dry-run")
        self.assertEqual(self.lines(run, "label"), self.labels("1.2.0"))
        run = self.publish("build", "v1.3.0-rc.1", "--platform", "linux/amd64", "--dry-run")
        self.assertEqual(self.lines(run, "label"), self.labels("1.3.0-rc.1"))

    def tags(self, ref):
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

    def test_a_ref_that_is_neither_main_nor_a_version_is_refused(self):
        for ref in ("develop", "1.2.0", "v1.2", "v1.2.0+build.5", "v01.2.0", "vmain"):
            for step in ("build", "join"):
                with self.subTest(ref=ref, step=step):
                    run = self.publish(step, ref, "--platform", "linux/amd64", "--dry-run") \
                        if step == "build" else self.publish(step, ref, "--dry-run")
                    self.assertEqual(run.returncode, 1, run.stdout)
                    self.assertIn("neither main nor a release's tag", run.stderr)


# A docker that records each call as one JSON line and answers as BuildKit
# would: a build's metadata file carries the image's config digest, which
# depends on the target alone, and its manifest's digest, which depends on
# the output too; imagetools writes the list's digest over its sources.
# DRIFT names a variant whose pushed config is not the one loaded.
DOCKER = r'''#!/usr/bin/env python3
import hashlib, json, os, sys
arguments = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(["docker", *arguments]) + "\n")
def digest(*parts):
    return "sha256:" + hashlib.sha256(" ".join(parts).encode()).hexdigest()
def value(flag):
    return arguments[arguments.index(flag) + 1]
if arguments[:1] == ["version"]:
    print(os.environ.get("DAEMON", "linux/amd64"))
elif arguments[:2] == ["buildx", "build"]:
    target = value("--target")
    config = digest("config", target, "drift" if os.environ.get("DRIFT") == target and "--load" not in arguments else "")
    with open(value("--metadata-file"), "w") as metadata:
        json.dump({
            "containerimage.config.digest": config,
            "containerimage.digest": digest("manifest", target, value("--platform"), str("--load" in arguments)),
        }, metadata)
elif arguments[:3] == ["buildx", "imagetools", "create"]:
    sources = [a for a in arguments if a.startswith(os.environ["IMAGE"] + "@")]
    with open(value("--metadata-file"), "w") as metadata:
        json.dump({"containerimage.digest": digest("list", *sources)}, metadata)
else:
    sys.exit("the recording docker has no answer for " + " ".join(arguments))
'''

# A smoke that records its call and fails for the variant FAILS names.
SMOKE = r'''#!/usr/bin/env python3
import json, os, sys
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(["smoke", *sys.argv[1:]]) + "\n")
sys.exit(1 if sys.argv[-1] == os.environ.get("FAILS") else 0)
'''


class Recorded(Repository):
    def setUp(self):
        super().setUp()
        for name, text in (("docker", DOCKER), ("smoke", SMOKE)):
            (self.bin / name).write_text(text.replace("/usr/bin/env python3", sys.executable))
            (self.bin / name).chmod(0o755)

    def run_step(self, *arguments, **env):
        return self.publish(*arguments, env={
            "CALLS": str(self.log), "IMAGE": IMAGE, "SMOKE": str(self.bin / "smoke"), **env,
        })

    def calls(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def pushes(self):
        return [c for c in self.calls() if c[:3] == ["docker", "buildx", "build"] and "--load" not in c]

    def flag(self, call, name):
        return call[call.index(name) + 1]


class Build(Recorded):
    def test_each_variant_is_loaded_smoked_and_only_then_pushed_by_digest(self):
        digests = Path(self.temporary.name) / "digests"
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

    def test_a_failed_smoke_pushes_nothing(self):
        for variant in VARIANTS:
            with self.subTest(failing=variant):
                self.log.unlink(missing_ok=True)
                run = self.run_step("build", "main", FAILS=variant)
                self.assertEqual(run.returncode, 1, run.stdout)
                self.assertIn(f"the smoke of {variant} failed; nothing is pushed", run.stderr)
                self.assertEqual(self.pushes(), [])
                self.assertNotIn("digest ", run.stdout)

    def test_a_pushed_image_whose_config_is_not_the_smoked_one_names_no_digest(self):
        digests = Path(self.temporary.name) / "digests"
        run = self.run_step("build", "main", "--digests", str(digests), DRIFT="fpm")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("fpm's pushed config", run.stderr)
        self.assertFalse(digests.exists())

    def test_the_daemons_platform_is_built_where_none_is_named(self):
        run = self.run_step("build", "main", DAEMON="linux/arm64")
        self.assertEqual(self.lines(run, "platform"), ["linux/arm64"])
        self.assertEqual({self.flag(c, "--platform") for c in self.pushes()}, {"linux/arm64"})

    def test_a_tree_with_changes_is_refused(self):
        (self.root / "Cargo.toml").write_text("changed")
        run = self.run_step("build", "main")
        self.assertEqual(run.returncode, 1)
        self.assertIn("the tree has changes", run.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_release_is_built_at_its_tag_alone(self):
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
            path = Path(self.temporary.name) / platform.replace("/", "-")
            path.write_text("".join(
                f"{platform} {variant} sha256:{(variant + platform[-2:]).encode().hex():0>64}\n"
                for variant in VARIANTS if (platform, variant) != skip
            ))
            files.append(str(path))
        return files

    def source(self, variant, platform):
        return f"{IMAGE}@sha256:{(variant + platform[-2:]).encode().hex():0>64}"

    def test_one_list_per_variant_over_both_platforms_carries_every_tag(self):
        manifests = Path(self.temporary.name) / "manifests"
        run = self.run_step("join", "main", *self.digests(), "--manifests", str(manifests))
        self.assertEqual(run.returncode, 0, run.stderr)
        creates = [c for c in self.calls() if c[:3] == ["docker", "buildx", "imagetools"]]
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
        written = manifests.read_text().splitlines()
        self.assertEqual([line.split()[0] for line in written], list(VARIANTS))
        self.assertEqual(self.lines(run, "manifest"), written)

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

    def test_the_dry_run_names_the_digests_it_would_join(self):
        run = self.run_step("join", "v1.2.0", *self.digests(), "--dry-run")
        self.assertIn(
            f"{IMAGE}:1.2.0-cli from {self.source('cli', 'linux/amd64')} {self.source('cli', 'linux/arm64')}",
            self.lines(run, "tag"),
        )
        self.assertEqual(self.calls(), [])
