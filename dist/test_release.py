"""The release script, held by its dry run and by tools that record, each
test in a repository of its own (testlib.py) whose manifest carries the
version the test names, beside the publish script the images come from."""

import json
import os
import shutil
import subprocess
import sys
import unittest
from pathlib import Path

import testlib
from testlib import IMAGE, ROOT, VARIANTS, manifest


class Repository(testlib.Repository):
    version = "1.2.0"
    files = (
        ".gitignore", "dist/release.py", "dist/bundle.py", "dist/publish.py", "dist/Dockerfile",
        "dist/install.sh", "scripts/dist-sums.sh",
    )

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
        # The publish script is imported, and its bytecode, which the
        # checkout's .gitignore ignores, would be a change in the tree that
        # a real run refuses.
        env = {key: value for key, value in os.environ.items() if key != "PYTHONDONTWRITEBYTECODE"}
        run = subprocess.run(
            [sys.executable, str(self.root / "dist/release.py"), "check", "v1.2.0"],
            cwd=self.root, env=env, capture_output=True, text=True,
        )
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertTrue((self.root / "dist/__pycache__").is_dir())
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
            *(f"lela-1.2.0-{platform}.tar.gz" for platform in PLATFORMS), "SHA256SUMS", "install.sh",
        ])
        self.assertEqual(self.lines(run, "release"), [
            "tag v1.2.0", "title v1.2.0", f"commit {self.commit}", "prerelease false", "latest true",
        ])
        self.assertEqual(self.lines(run, "image"), [
            f"{IMAGE}:{tag}" for variant in VARIANTS for tag in (f"1.2.0-{variant}", variant)
        ])
        self.assertIn("dry run: nothing is uploaded or created", run.stdout)
        self.assertFalse(self.log.exists())

    def test_the_dockerfiles_name_names_the_tarballs_and_a_caller_names_none(self):
        dockerfile = self.root / "dist/Dockerfile"
        dockerfile.write_text(dockerfile.read_text().replace("\nARG NAME=lela\n", "\nARG NAME=other\n"))
        run = self.release("publish", "v1.2.0", "--dry-run", env={"NAME": "php"})
        self.assertEqual(self.lines(run, "artifact")[0], "other-1.2.0-darwin-arm64.tar.gz")

    def test_an_installer_missing_a_line_the_release_bakes_is_refused(self):
        script = self.root / "dist/install.sh"
        script.write_text(script.read_text().replace("\nversion=\n", "\n"))
        run = self.release("publish", "v1.2.0", "--dry-run")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("dist/install.sh carries 0 empty `version=` lines, where it carries one", run.stderr)
        self.assertNotIn("artifact", run.stdout)

    def test_a_dockerfile_naming_no_hosts_is_refused(self):
        dockerfile = self.root / "dist/Dockerfile"
        dockerfile.write_text(dockerfile.read_text().replace("\nARG NAME=lela\n", "\n"))
        run = self.release("publish", "v1.2.0", "--dry-run")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("dist/Dockerfile names 0 `ARG NAME=` lines, where it names one", run.stderr)


class Shallow(Repository):
    def test_whether_a_release_is_the_latest_is_refused_in_a_shallow_checkout(self):
        clone = self.backport()
        run = self.release("publish", "v1.0.1", "--dry-run")
        self.assertIn("latest false", self.lines(run, "release"))
        run = self.script("release.py", "publish", "v1.0.1", "--dry-run", root=clone)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("this checkout is shallow", run.stderr)
        self.assertNotIn("release latest", run.stdout)


class PreReleaseDryRun(Repository):
    version = "0.1.0-alpha.1"

    def test_a_pre_release_is_marked_one_and_moves_no_bare_variant(self):
        run = self.release("publish", "v0.1.0-alpha.1", "--dry-run")
        self.assertEqual(self.lines(run, "artifact"), [
            *(f"lela-0.1.0-alpha.1-{platform}.tar.gz" for platform in PLATFORMS), "SHA256SUMS", "install.sh",
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
            (out / f"lela-0.1.0-alpha.1-{platform}.tar.gz").write_bytes(platform.encode())
        return out

    def test_the_release_carries_the_tarballs_and_their_sums_and_is_marked_a_pre_release(self):
        self.git("tag", "v0.1.0-alpha.1")
        out = self.tarballs()
        run = self.run_step("publish", "v0.1.0-alpha.1", str(out))
        self.assertEqual(run.returncode, 0, run.stderr)
        sums = (out / "SHA256SUMS").read_text()
        self.assertEqual([line.split("  ")[1] for line in sums.splitlines()], [
            f"lela-0.1.0-alpha.1-{platform}.tar.gz" for platform in PLATFORMS
        ])
        (call,) = self.calls()
        notes = call.pop()
        self.assertEqual(call[:3], ["gh", "release", "create"])
        notes_file = call.index("--notes-file")
        del call[notes_file:notes_file + 2]
        self.assertEqual(call[3:], [
            "v0.1.0-alpha.1", "--verify-tag", "--title", "v0.1.0-alpha.1",
            "--prerelease", "--latest=false",
            *(str(out / f"lela-0.1.0-alpha.1-{platform}.tar.gz") for platform in PLATFORMS),
            str(out / "SHA256SUMS"), str(out / "install.sh"),
        ])
        # The installer attached is the release's: its version, its URL,
        # and the Ubuntu release of the Dockerfile's base, baked in.
        script = (out / "install.sh").read_text()
        self.assertIn("\nname='lela'\nversion='0.1.0-alpha.1'\nubuntu='26.04'\n"
                      "release='https://github.com/lelaphp/lela/releases/download/v0.1.0-alpha.1'\n", script)
        self.assertTrue(os.access(out / "install.sh", os.X_OK))
        self.assertIn(self.commit, notes)
        self.assertIn(sums, notes)
        self.assertIn(f"{IMAGE}:0.1.0-alpha.1-fpm", notes)

    def test_a_missing_or_stray_tarball_creates_nothing(self):
        self.git("tag", "v0.1.0-alpha.1")
        out = self.tarballs(PLATFORMS[1:])
        run = self.run_step("publish", "v0.1.0-alpha.1", str(out))
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"no lela-0.1.0-alpha.1-darwin-arm64.tar.gz in {out}", run.stderr)
        self.tarballs()
        (out / "lela-0.0.9-linux-x86_64.tar.gz").write_bytes(b"")
        run = self.run_step("publish", "v0.1.0-alpha.1", str(out))
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"lela-0.0.9-linux-x86_64.tar.gz in {out} is no tarball of v0.1.0-alpha.1", run.stderr)
        self.assertEqual(self.calls(), [])
        self.assertFalse((out / "SHA256SUMS").exists())

    def test_what_the_sums_script_says_of_a_failure_reaches_the_caller(self):
        (self.root / "scripts/dist-sums.sh").write_text("#!/bin/sh\necho 'the sums: disk full' >&2\nexit 3\n")
        self.git("commit", "-q", "-am", "a sums script that fails")
        self.git("tag", "v0.1.0-alpha.1")
        run = self.run_step("publish", "v0.1.0-alpha.1", str(self.tarballs()))
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("the sums: disk full", run.stderr)
        self.assertIn("scripts/dist-sums.sh", run.stderr)
        self.assertIn("exit 3", run.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_gh_not_on_path_is_said_and_no_traceback(self):
        (self.bin / "gh").unlink()
        self.git("tag", "v0.1.0-alpha.1")
        run = self.run_step("publish", "v0.1.0-alpha.1", str(self.tarballs()))
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertEqual(run.stderr, "dist/release.py: gh: not found\n")

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
    for path in ("bin/lela", "bin/lela-cgi", "sbin/lela-fpm", "runtime-packages.txt"):
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
                self.assertEqual(self.lines(run, "artifact"), [f"lela-1.2.0-{platform}.tar.gz"])
                self.assertEqual(self.lines(run, "smoke"), [f"lela-1.2.0-{platform}.tar.gz on {platform}"])
                self.assertIn("dry run: nothing is built or smoked", run.stdout)
                self.assertFalse(self.log.exists())
                self.assertFalse((self.root / "out").exists())
                if platform == "darwin-arm64":
                    self.assertEqual(self.lines(run, "bundle"), [
                        "under lib/: every library bin/lela, bin/lela-cgi, sbin/lela-fpm load outside /usr/lib "
                        "and /System, and every library those load",
                        f"no hosts built in {self.root / 'target/release'} on this machine, "
                        "so the libraries are named once they are",
                    ])
                else:
                    self.assertEqual(self.lines(run, "bundle"), [])

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
        stage = self.out() / "lela-1.2.0-linux-x86_64"
        self.assertEqual(build, [
            "docker", "buildx", "build", "-f", str(self.root / "dist/Dockerfile"), "--platform", "linux/amd64",
            "--target", "binaries", "--build-arg", "CARGO_BUILD_JOBS=1",
            "--output", f"type=local,dest={stage}", str(self.root),
        ])
        self.assertEqual(smoke[:8], [
            "docker", "run", "--rm", "--platform", "linux/amd64",
            "-v", f"{self.out()}:/release:ro", "-v",
        ])
        self.assertEqual(smoke[8:10], [f"{self.root / 'dist'}:/dist:ro", "-v"])
        self.assertTrue(smoke[10].endswith(":/installer:ro"), smoke[10])
        self.assertEqual(smoke[11:13], [UBUNTU, "bash"])
        self.assertIn("/release/lela-1.2.0-linux-x86_64.tar.gz", smoke[-1])
        # The smoke, then the installer as a user runs it, curl coming only
        # after the smoke.
        lines = smoke[-1].splitlines()
        self.assertLess(lines.index('/dist/smoke.sh "$d"'), lines.index(
            "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends curl >/dev/null"))
        self.assertEqual(lines[-1], "su installer -c 'LELA_INSTALL_FROM=file:///tmp/assets sh /installer/install.sh'")
        listing = subprocess.run(
            ["tar", "-tvzf", str(self.out() / "lela-1.2.0-linux-x86_64.tar.gz")],
            check=True, capture_output=True, text=True,
        ).stdout.splitlines()
        self.assertTrue(listing[0].startswith("drwxr-xr-x"), listing[0])
        for entry in listing:
            with self.subTest(entry=entry):
                self.assertRegex(entry, r"\broot\s+root\b")
        self.assertEqual(self.lines(run, "built"), [str(self.out() / "lela-1.2.0-linux-x86_64.tar.gz")])

    def test_a_failed_smoke_fails_the_build(self):
        self.git("tag", "v1.2.0")
        run = self.run_step("build", "v1.2.0", "linux-aarch64", str(self.out()), DAEMON="linux/arm64", FAILS="1")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("the smoke of lela-1.2.0-linux-aarch64.tar.gz on linux-aarch64 failed", run.stderr)
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
# real hosts; UNSIGNED takes that signature off again. Where KEGS names a
# directory, it lays a Homebrew prefix there of one keg, zz 1.0_1, whose
# library each host calls by the keg's opt path, as the real hosts call
# the kegs they link.
CARGO = r'''#!/usr/bin/env python3
import json, os, subprocess, sys
arguments = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps(["cargo", *arguments]) + "\n")
release = os.path.join(os.environ["CARGO_TARGET_DIR"], "release")
os.makedirs(release, exist_ok=True)
source, link = b"int main(void){return 0;}\n", []
if os.environ.get("KEGS"):
    kegs = os.environ["KEGS"]
    lib = os.path.join(kegs, "Cellar/zz/1.0_1/lib")
    os.makedirs(lib)
    open(os.path.join(kegs, "Cellar/zz/1.0_1/COPYING"), "w").write("zz's licence\n")
    os.makedirs(os.path.join(kegs, "opt"))
    os.symlink("../Cellar/zz/1.0_1", os.path.join(kegs, "opt/zz"))
    subprocess.run(["cc", "-x", "c", "-", "-dynamiclib", "-o", os.path.join(lib, "libzz.1.dylib"),
                    "-install_name", os.path.join(kegs, "opt/zz/lib/libzz.1.dylib")],
                   input=b"int zz(void){return 0;}\n", check=True)
    source, link = b"int zz(void); int main(void){return zz();}\n", ["-x", "none", os.path.join(kegs, "opt/zz/lib/libzz.1.dylib")]
for i, a in enumerate(arguments):
    if a == "--bin":
        host = os.path.join(release, arguments[i + 1])
        subprocess.run(["cc", "-x", "c", "-", *link, "-o", host], input=source, check=True)
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
            "-p", "php_sapi_fpm", "--bin", "lela", "--bin", "lela-cgi", "--bin", "lela-fpm",
        ])
        self.assertEqual(smoke, ["smoke", [
            "bin/lela", "bin/lela-cgi", "bundled-libraries.txt", "etc/php.ini-development",
            "etc/php.ini-production", "sbin/lela-fpm",
        ]])
        self.assertRegex("\n".join(self.lines(run, "bundle")), r"^macos \d+\.\d+$")
        # The release's installer over the tarball, into a home of its own.
        self.assertRegex(run.stdout, r"\ninstall.sh: installed lela 1.2.0 in /\S+/home/.local/share/lela/1.2.0, ")
        self.assertEqual(self.lines(run, "built"), [str(self.out / "lela-1.2.0-darwin-arm64.tar.gz")])

    def test_the_libraries_the_hosts_load_are_bundled_and_run_with_their_keg_gone(self):
        kegs = self.scratch / "homebrew"
        run = self.run_step("build", "v1.2.0", "darwin-arm64", str(self.out), KEGS=str(kegs), **self.env)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.lines(run, "bundle")[1:], ["lib/libzz.1.dylib zz 1.0_1"])
        _, smoke = self.calls()
        self.assertEqual(smoke, ["smoke", [
            "bin/lela", "bin/lela-cgi", "bundled-libraries.txt", "etc/php.ini-development",
            "etc/php.ini-production", "lib/libzz.1.dylib", "licenses/zz/COPYING", "sbin/lela-fpm",
        ]])
        unpacked = self.scratch / "unpacked"
        unpacked.mkdir()
        subprocess.run(["tar", "-xzf", str(self.out / "lela-1.2.0-darwin-arm64.tar.gz"), "-C", str(unpacked)], check=True)
        tree = unpacked / "lela-1.2.0-darwin-arm64"
        names = subprocess.run(["otool", "-L", str(tree / "sbin/lela-fpm")], check=True, capture_output=True, text=True)
        self.assertIn("\t@loader_path/../lib/libzz.1.dylib (compatibility version", names.stdout)
        self.assertNotIn(str(kegs), names.stdout)
        shutil.rmtree(kegs)
        for host in ("bin/lela", "bin/lela-cgi", "sbin/lela-fpm"):
            with self.subTest(host=host):
                self.assertEqual(subprocess.run([str(tree / host)]).returncode, 0)

    def test_the_dry_run_names_what_the_hosts_built_here_load(self):
        cargo = subprocess.run(
            [str(self.bin / "cargo"), "build", "--bin", "lela", "--bin", "lela-cgi", "--bin", "lela-fpm"],
            env={**os.environ, "CALLS": str(self.log), "KEGS": str(self.scratch / "homebrew"), **self.env},
        )
        self.assertEqual(cargo.returncode, 0)
        run = self.run_step("build", "v1.2.0", "darwin-arm64", str(self.out), "--dry-run", **self.env)
        self.assertEqual(self.lines(run, "bundle")[1:], [
            f"read from the hosts in {self.scratch / 'target/release'}", "lib/libzz.1.dylib zz 1.0_1",
        ])
        self.assertEqual([c[0] for c in self.calls()], ["cargo"])
        self.assertFalse(self.out.exists())

    def test_a_host_without_its_signature_is_refused_before_the_smoke(self):
        run = self.run_step("build", "v1.2.0", "darwin-arm64", str(self.out), UNSIGNED="1", **self.env)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("lela-1.2.0-darwin-arm64.tar.gz's bin/lela carries no ad-hoc signature", run.stderr)
        self.assertEqual([c[0] for c in self.calls()], ["cargo"])


if __name__ == "__main__":
    unittest.main()
