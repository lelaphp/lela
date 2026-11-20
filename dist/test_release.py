"""The release script, held by its dry run and by tools that record, each
test in a repository of its own (testlib.py) whose manifest carries the
version the test names, beside the publish script the images come from."""

import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

import testlib
from testlib import IMAGE, ROOT, VARIANTS, manifest


class Repository(testlib.Repository):
    version = "1.2.0"
    files = ("dist/release.py", "dist/publish.py", "dist/Dockerfile", "scripts/dist-sums.sh")

    def release(self, *arguments, env=None):
        return self.script("release.py", *arguments, env=env)


STEPS = (
    ("check",),
    ("build", "linux-x86_64", "out", "--dry-run"),
    ("build", "linux-x86_64", "out"),
    ("publish", "out", "--dry-run"),
    ("publish", "out"),
)


class TheTag(Repository):
    def test_a_tag_the_manifest_does_not_name_is_refused_by_every_step(self):
        for tag in ("v1.2.1", "v1.2.0-rc.1", "v1.20.0"):
            for step in STEPS:
                with self.subTest(tag=tag, step=step[0]):
                    run = self.release(step[0], tag, *step[1:])
                    self.assertEqual(run.returncode, 1, run.stdout)
                    self.assertIn(
                        f"{tag} is not the workspace manifest's version, 1.2.0, from Cargo.toml's "
                        "[workspace.package]",
                        run.stderr,
                    )
                    self.assertEqual(run.stdout, "")
                    self.assertFalse(self.log.exists())

    def test_the_manifests_tag_is_taken(self):
        run = self.release("check", "v1.2.0")
        self.assertEqual(self.lines(run, "version"), ["1.2.0"])

    def test_a_run_leaves_the_tree_clean(self):
        # The publish script is imported, and its bytecode would be a change
        # in the tree that a real run refuses.
        env = {key: value for key, value in os.environ.items() if key != "PYTHONDONTWRITEBYTECODE"}
        run = subprocess.run(
            [sys.executable, str(self.root / "dist/release.py"), "check", "v1.2.0"],
            cwd=self.root, env=env, capture_output=True, text=True,
        )
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_a_ref_that_is_no_releases_tag_is_refused(self):
        for ref in ("main", "1.2.0", "v1.2", "v1.2.0+build.5"):
            with self.subTest(ref=ref):
                run = self.release("check", ref)
                self.assertEqual(run.returncode, 1, run.stdout)
                self.assertIn(f"{ref} is not a release's tag, v<version>", run.stderr)


class NoVersion(Repository):
    version = None

    def test_a_manifest_with_no_version_is_refused_with_the_line_it_lacks(self):
        for step in STEPS:
            with self.subTest(step=step[0]):
                run = self.release(step[0], "v0.1.0-alpha.1", *step[1:])
                self.assertEqual(run.returncode, 1, run.stdout)
                self.assertIn(
                    "Cargo.toml's [workspace.package] has no version line; "
                    'a release of v0.1.0-alpha.1 needs version = "0.1.0-alpha.1" there',
                    run.stderr,
                )
                self.assertFalse(self.log.exists())


PLATFORMS = ("darwin-arm64", "linux-aarch64", "linux-x86_64")


class PublishDryRun(Repository):
    def test_a_release_is_the_latest_and_moves_the_bare_variant(self):
        run = self.release("publish", "v1.2.0", "--dry-run")
        self.assertEqual(self.lines(run, "artifact"), [
            *(f"php-1.2.0-{platform}.tar.gz" for platform in PLATFORMS), "SHA256SUMS",
        ])
        self.assertEqual(self.lines(run, "release"), [
            "tag v1.2.0", "title v1.2.0", f"commit {self.commit}", "prerelease false", "latest true",
        ])
        self.assertEqual(self.lines(run, "image"), [
            f"{IMAGE}:{tag}" for variant in VARIANTS for tag in (f"1.2.0-{variant}", variant)
        ])
        self.assertIn("dry run: nothing is uploaded or created", run.stdout)
        self.assertFalse(self.log.exists())

    def test_the_hosts_name_names_the_tarballs(self):
        run = self.release("publish", "v1.2.0", "--dry-run", env={"NAME": "lela"})
        self.assertEqual(self.lines(run, "artifact")[0], "lela-1.2.0-darwin-arm64.tar.gz")


class PreReleaseDryRun(Repository):
    version = "0.1.0-alpha.1"

    def test_a_pre_release_is_marked_one_and_moves_no_bare_variant(self):
        run = self.release("publish", "v0.1.0-alpha.1", "--dry-run")
        self.assertEqual(self.lines(run, "artifact"), [
            *(f"php-0.1.0-alpha.1-{platform}.tar.gz" for platform in PLATFORMS), "SHA256SUMS",
        ])
        self.assertEqual(self.lines(run, "release"), [
            "tag v0.1.0-alpha.1", "title v0.1.0-alpha.1", f"commit {self.commit}",
            "prerelease true", "latest false",
        ])
        self.assertEqual(self.lines(run, "image"), [
            f"{IMAGE}:0.1.0-alpha.1-{variant}" for variant in VARIANTS
        ])
        self.assertFalse(self.log.exists())


class OlderReleaseDryRun(Repository):
    version = "1.1.9"

    def test_a_release_older_than_a_tag_here_is_not_the_latest(self):
        self.git("tag", "v1.2.0")
        run = self.release("publish", "v1.1.9", "--dry-run")
        self.assertIn("latest false", self.lines(run, "release"))
        self.assertEqual(self.lines(run, "image"), [f"{IMAGE}:1.1.9-{variant}" for variant in VARIANTS])


# A gh that records each call as one JSON line, with the text of the notes
# file it is handed, since the file is gone once the script ends.
GH = r'''#!/usr/bin/env python3
import json, os, sys
arguments = sys.argv[1:]
notes = arguments[arguments.index("--notes-file") + 1] if "--notes-file" in arguments else None
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(["gh", *arguments, open(notes).read() if notes else None]) + "\n")
'''


class Recorded(Repository):
    tools = {"gh": GH}

    def setUp(self):
        super().setUp()
        for name, text in self.tools.items():
            self.tool(name, text)

    def run_step(self, *arguments, **env):
        return self.release(*arguments, env={"CALLS": str(self.log), **env})

    def calls(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]


class Publish(Recorded):
    version = "0.1.0-alpha.1"

    def tarballs(self, platforms=PLATFORMS):
        out = self.scratch / "out"
        out.mkdir(exist_ok=True)
        for platform in platforms:
            (out / f"php-0.1.0-alpha.1-{platform}.tar.gz").write_bytes(platform.encode())
        return out

    def test_the_release_carries_the_tarballs_and_their_sums_and_is_marked_a_pre_release(self):
        self.git("tag", "v0.1.0-alpha.1")
        out = self.tarballs()
        run = self.run_step("publish", "v0.1.0-alpha.1", str(out))
        self.assertEqual(run.returncode, 0, run.stderr)
        sums = (out / "SHA256SUMS").read_text()
        self.assertEqual([line.split("  ")[1] for line in sums.splitlines()], [
            f"php-0.1.0-alpha.1-{platform}.tar.gz" for platform in PLATFORMS
        ])
        (call,) = self.calls()
        notes = call.pop()
        self.assertEqual(call[:3], ["gh", "release", "create"])
        notes_file = call.index("--notes-file")
        del call[notes_file:notes_file + 2]
        self.assertEqual(call[3:], [
            "v0.1.0-alpha.1", "--verify-tag", "--title", "v0.1.0-alpha.1",
            "--prerelease", "--latest=false",
            *(str(out / f"php-0.1.0-alpha.1-{platform}.tar.gz") for platform in PLATFORMS),
            str(out / "SHA256SUMS"),
        ])
        self.assertIn(self.commit, notes)
        self.assertIn(sums, notes)
        self.assertIn(f"{IMAGE}:0.1.0-alpha.1-fpm", notes)

    def test_a_missing_or_stray_tarball_creates_nothing(self):
        self.git("tag", "v0.1.0-alpha.1")
        out = self.tarballs(PLATFORMS[1:])
        run = self.run_step("publish", "v0.1.0-alpha.1", str(out))
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"no php-0.1.0-alpha.1-darwin-arm64.tar.gz in {out}", run.stderr)
        self.tarballs()
        (out / "php-0.0.9-linux-x86_64.tar.gz").write_bytes(b"")
        run = self.run_step("publish", "v0.1.0-alpha.1", str(out))
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"php-0.0.9-linux-x86_64.tar.gz in {out} is no tarball of v0.1.0-alpha.1", run.stderr)
        self.assertEqual(self.calls(), [])
        self.assertFalse((out / "SHA256SUMS").exists())

    def test_a_release_is_published_at_its_tag_alone(self):
        out = self.tarballs()
        run = self.run_step("publish", "v0.1.0-alpha.1", str(out))
        self.assertEqual(run.returncode, 1)
        self.assertIn("no tag v0.1.0-alpha.1 here", run.stderr)
        self.git("tag", "v0.1.0-alpha.1")
        self.git("commit", "-q", "--allow-empty", "-m", "after the release")
        run = self.run_step("publish", "v0.1.0-alpha.1", str(out))
        self.assertEqual(run.returncode, 1)
        self.assertIn("v0.1.0-alpha.1 is not HEAD", run.stderr)
        run = self.run_step("publish", "v0.1.0-alpha.1")
        self.assertEqual(run.returncode, 1)
        self.assertEqual(self.calls(), [])


# A docker that records each call and answers as the build would: the
# daemon's platform is DAEMON's, and the binaries target's local output is
# the three hosts and runtime-packages.txt, laid 0700 as buildx lays it.
DOCKER = r'''#!/usr/bin/env python3
import json, os, sys
arguments = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(["docker", *arguments]) + "\n")
if arguments[:1] == ["version"]:
    print(os.environ.get("DAEMON", "linux/amd64"))
elif arguments[:2] == ["buildx", "build"]:
    stage = arguments[arguments.index("--output") + 1].removeprefix("type=local,dest=")
    for path in ("bin/php", "bin/php-cgi", "sbin/php-fpm", "runtime-packages.txt"):
        os.makedirs(os.path.dirname(os.path.join(stage, path)), exist_ok=True)
        open(os.path.join(stage, path), "w").write(path)
    os.chmod(stage, 0o700)
elif arguments[:1] == ["run"]:
    sys.exit(1 if os.environ.get("FAILS") else 0)
else:
    sys.exit("the recording docker has no answer for " + " ".join(arguments))
'''

UBUNTU = next(
    line.removeprefix("ARG UBUNTU=")
    for line in (ROOT / "dist/Dockerfile").read_text().splitlines() if line.startswith("ARG UBUNTU=")
)


class BuildDryRun(Repository):
    def test_the_dry_run_names_the_tarball_and_its_smoke_and_runs_nothing(self):
        for platform in PLATFORMS:
            with self.subTest(platform=platform):
                run = self.release("build", "v1.2.0", platform, "out", "--dry-run")
                self.assertEqual(self.lines(run, "platform"), [platform])
                self.assertEqual(self.lines(run, "commit"), [self.commit])
                self.assertEqual(self.lines(run, "artifact"), [f"php-1.2.0-{platform}.tar.gz"])
                self.assertEqual(self.lines(run, "smoke"), [f"php-1.2.0-{platform}.tar.gz on {platform}"])
                self.assertIn("dry run: nothing is built or smoked", run.stdout)
                self.assertFalse(self.log.exists())
                self.assertFalse((self.root / "out").exists())

    def test_a_platform_the_release_has_not_is_refused(self):
        run = self.release("build", "v1.2.0", "freebsd-x86_64", "out", "--dry-run")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("no platform freebsd-x86_64; a release's are darwin-arm64, linux-aarch64, linux-x86_64", run.stderr)


class Build(Recorded):
    tools = {"docker": DOCKER}

    def out(self):
        return self.scratch / "out"

    def test_a_linux_tarball_is_built_on_its_platform_and_smoked_from_the_tarball(self):
        self.git("tag", "v1.2.0")
        run = self.run_step("build", "v1.2.0", "linux-x86_64", str(self.out()))
        self.assertEqual(run.returncode, 0, run.stderr)
        version, build, smoke = self.calls()
        self.assertEqual(version[:2], ["docker", "version"])
        stage = self.out() / "php-1.2.0-linux-x86_64"
        self.assertEqual(build, [
            "docker", "buildx", "build", "-f", str(self.root / "dist/Dockerfile"), "--platform", "linux/amd64",
            "--target", "binaries", "--build-arg", "CARGO_BUILD_JOBS=1", "--build-arg", "NAME=php",
            "--output", f"type=local,dest={stage}", str(self.root),
        ])
        self.assertEqual(smoke[:8], [
            "docker", "run", "--rm", "--platform", "linux/amd64",
            "-v", f"{self.out()}:/release:ro", "-v",
        ])
        self.assertEqual(smoke[8:13], [f"{self.root / 'dist'}:/dist:ro", "-e", "NAME=php", UBUNTU, "bash"])
        self.assertIn("/release/php-1.2.0-linux-x86_64.tar.gz", smoke[-1])
        self.assertIn('/dist/smoke.sh "$d"', smoke[-1])
        listing = subprocess.run(
            ["tar", "-tvzf", str(self.out() / "php-1.2.0-linux-x86_64.tar.gz")],
            check=True, capture_output=True, text=True,
        ).stdout.splitlines()
        self.assertTrue(listing[0].startswith("drwxr-xr-x"), listing[0])
        for entry in listing:
            with self.subTest(entry=entry):
                self.assertRegex(entry, r"\broot\s+root\b")
        self.assertEqual(self.lines(run, "built"), [str(self.out() / "php-1.2.0-linux-x86_64.tar.gz")])

    def test_a_failed_smoke_fails_the_build(self):
        self.git("tag", "v1.2.0")
        run = self.run_step("build", "v1.2.0", "linux-aarch64", str(self.out()), DAEMON="linux/arm64", FAILS="1")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("the smoke of php-1.2.0-linux-aarch64.tar.gz on linux-aarch64 failed", run.stderr)
        self.assertNotIn("built ", run.stdout)

    def test_a_platform_is_built_on_its_own_alone(self):
        self.git("tag", "v1.2.0")
        run = self.run_step("build", "v1.2.0", "linux-x86_64", str(self.out()), DAEMON="linux/arm64")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("linux-x86_64 is built on a machine of its own platform, linux/amd64, "
                      "and the daemon's is linux/arm64", run.stderr)
        self.assertEqual([c[:2] for c in self.calls()], [["docker", "version"]])

    def test_a_release_is_built_at_its_tag_alone(self):
        run = self.run_step("build", "v1.2.0", "linux-x86_64", str(self.out()))
        self.assertEqual(run.returncode, 1)
        self.assertIn("no tag v1.2.0 here", run.stderr)
        self.git("tag", "v1.2.0")
        (self.root / "Cargo.toml").write_text(manifest("1.2.0") + "\n")
        run = self.run_step("build", "v1.2.0", "linux-x86_64", str(self.out()))
        self.assertEqual(run.returncode, 1)
        self.assertIn("the tree has changes", run.stderr)
        self.assertEqual(self.calls(), [])


# A cargo that records its call and links each host the build names as a
# program of nothing, which the macOS linker signs ad hoc as it signs the
# real hosts; UNSIGNED takes that signature off again.
CARGO = r'''#!/usr/bin/env python3
import json, os, subprocess, sys
arguments = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(["cargo", *arguments]) + "\n")
release = os.path.join(os.environ["CARGO_TARGET_DIR"], "release")
os.makedirs(release, exist_ok=True)
for i, a in enumerate(arguments):
    if a == "--bin":
        host = os.path.join(release, arguments[i + 1])
        subprocess.run(["cc", "-x", "c", "-", "-o", host], input=b"int main(void){return 0;}\n", check=True)
        if os.environ.get("UNSIGNED"):
            subprocess.run(["codesign", "--remove-signature", host], check=True)
'''

# The ini step, standing in for dist/inputs.py, which downloads php's.
INPUTS = r'''import os, sys
os.makedirs(sys.argv[2], exist_ok=True)
for name in ("php.ini-development", "php.ini-production"):
    open(os.path.join(sys.argv[2], name), "w").write(name)
'''

# A smoke that records the directory it is given and what is in it.
SMOKE = r'''#!/usr/bin/env python3
import json, os, sys
d = sys.argv[1]
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(["smoke", sorted(os.path.relpath(os.path.join(r, f), d) for r, _, fs in os.walk(d) for f in fs)]) + "\n")
'''


@unittest.skipUnless(os.uname().sysname == "Darwin" and os.uname().machine == "arm64", "an Apple silicon Mac's")
class BuildDarwin(Recorded):
    tools = {"cargo": CARGO, "smoke": SMOKE}

    def setUp(self):
        super().setUp()
        (self.root / "dist/inputs.py").write_text(INPUTS)
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "the ini step")
        self.git("tag", "v1.2.0")
        self.out = self.scratch / "out"
        self.env = {
            "CARGO_TARGET_DIR": str(self.scratch / "target"),
            "SMOKE": str(self.bin / "smoke"),
        }

    def test_the_hosts_keep_their_ad_hoc_signature_and_are_smoked_from_the_tarball(self):
        run = self.run_step("build", "v1.2.0", "darwin-arm64", str(self.out), **self.env)
        self.assertEqual(run.returncode, 0, run.stderr)
        cargo, smoke = self.calls()
        self.assertEqual(cargo, [
            "cargo", "build", "--release", "--locked", "-p", "php_sapi_cli", "-p", "php_sapi_cgi",
            "-p", "php_sapi_fpm", "--bin", "php", "--bin", "php-cgi", "--bin", "php-fpm",
        ])
        self.assertEqual(smoke, ["smoke", [
            "bin/php", "bin/php-cgi", "etc/php.ini-development", "etc/php.ini-production", "sbin/php-fpm",
        ]])
        self.assertEqual(self.lines(run, "built"), [str(self.out / "php-1.2.0-darwin-arm64.tar.gz")])

    def test_a_host_without_its_signature_is_refused_before_the_smoke(self):
        run = self.run_step("build", "v1.2.0", "darwin-arm64", str(self.out), UNSIGNED="1", **self.env)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("php-1.2.0-darwin-arm64.tar.gz's bin/php carries no ad-hoc signature", run.stderr)
        self.assertEqual([c[0] for c in self.calls()], ["cargo"])


if __name__ == "__main__":
    unittest.main()
