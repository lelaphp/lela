"""What the publish and release tests share: a repository of the test's
own, the PATH its script runs with, and the lines a step prints.

Each test runs a script copied into a repository laid down with a
workspace manifest of the test's own, the hosts' crates (HOSTS) and one
commit at a known time, so the commit, the version, the licences and the
tags the script reads are the test's and not the checkout's.

The script's PATH is two directories and no other: the test's `bin/`,
where a test lays the docker, gh, cargo or smoke that records its calls,
and `tools/`, which links the few programs the scripts run that are no
such tool, git and what scripts/dist-sums.sh and the macOS build call. No
directory of the system's is on it, since a CI image keeps gh and docker
in /usr/bin beside git: a dry run that ran either would find none.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import publish

ROOT = Path(__file__).resolve().parent.parent
IMAGE = "ghcr.io/lelaphp/lela"
VARIANTS = ("cli", "fpm", "apache")
# The commit's time, which the build's SOURCE_DATE_EPOCH is.
WHEN = "2026-11-19T12:34:56Z"
EPOCH = "1795091696"
# The programs tools/ links where the machine has them: none is docker,
# gh or cargo.
TOOLS = ("git", "cat", "mv", "rm", "sha256sum", "shasum", "cc", "codesign", "otool", "install_name_tool")


LICENSE = "MIT AND BSD-3-Clause"


def manifest(version, license=LICENSE, rest=""):
    """A workspace manifest whose package table carries `version` and
    `license`, or no such line where one is None, with `rest` after it."""
    lines = (f'version = "{version}"\n' if version else "") + (f'license = "{license}"\n' if license else "")
    return (
        '[workspace]\nmembers = ["crates/*", "sapi/*"]\n\n'
        f'[workspace.package]\nedition = "2024"\n{lines}{rest}'
    )


def package(name, license=None, body=""):
    """A crate's manifest: `license` its line, the workspace's where it is
    None and no line where it is empty, with `body` after the package table."""
    if license is None:
        line = "license.workspace = true\n"
    else:
        line = f'license = "{license}"\n' if license else ""
    return f'[package]\nname = "{name}"\nversion = "0.0.0"\nedition.workspace = true\n{line}\n{body}'


# The hosts a release ships, dist/publish.py's HOSTS, each a crate of the
# workspace's licence and no dependency.
HOSTS = {f"{host.crate}/Cargo.toml": package(host.package) for host in publish.HOSTS}


class Dockerfile:
    """dist/Dockerfile as the tests read it, every line with its
    continuations: the packages and binaries of the build stage's cargo
    line, in its order; and the hosts each stage holds, by their paths in
    it, from the `install` lines that lay each binary under /out and the
    `COPY --from=build` lines that take a directory of /out into a stage
    or a stage it is built FROM."""

    def __init__(self, text):
        name = publish.name()
        lines = text.replace("\\\n", " ").splitlines()
        self.parents, self.copies, self.laid = {}, {}, set()
        stage = None
        for line in lines:
            if found := re.match(r"FROM (?:--platform=\S+ )?(\S+) AS (\S+)$", line):
                parent, stage = found.groups()
                self.parents[stage], self.copies[stage] = parent, []
            elif found := re.match(r"COPY --from=build (\S+) (\S+)$", line):
                self.copies[stage].append(found.groups())
        [command] = [line for line in lines if "cargo build" in line]
        command = command[command.index("cargo build"):].split(";")[0]
        self.packages = re.findall(r"-p (\S+)", command)
        self.binaries = [f"{name}{suffix}" for suffix in re.findall(r'--bin "\$NAME([-a-z]*)"', command)]
        for found in re.finditer(r"install -D -m 0755 -t (\S+) ([^;]+)", text.replace("\\\n", " ")):
            directory, files = found.groups()
            for suffix in re.findall(r'"\$release/\$NAME([-a-z]*)"', files):
                self.laid.add(f"{directory}/{name}{suffix}")

    @staticmethod
    def read():
        return Dockerfile((ROOT / "dist/Dockerfile").read_text())

    def hosts(self, stage):
        """The paths of the hosts `stage` holds, sorted."""
        found = set()
        while stage in self.parents:
            for source, target in self.copies[stage]:
                found |= {target + path[len(source):] for path in self.laid if path.startswith(source)}
            stage = self.parents[stage]
        return sorted(found)


class Repository(unittest.TestCase):
    # The manifest's version, the files of the checkout copied in, and the
    # crates' manifests laid down, by path.
    version = None
    files = ("dist/publish.py",)
    crates = HOSTS

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        # Resolved, as the script's own root is: on macOS the temporary
        # directory is under a symlink, /var to /private/var.
        self.scratch = Path(self.temporary.name).resolve()
        self.root = self.scratch / "lela"
        (self.root / "dist").mkdir(parents=True)
        (self.root / "Cargo.toml").write_text(manifest(self.version))
        for path in self.files:
            (self.root / path).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / path, self.root / path)
        self.lay(self.crates)
        self.bin = self.scratch / "bin"
        self.bin.mkdir()
        tools = self.scratch / "tools"
        tools.mkdir()
        for tool in TOOLS:
            found = shutil.which(tool)
            if found:
                (tools / tool).symlink_to(found)
        self.path = os.pathsep.join([str(self.bin), str(tools)])
        self.log = self.scratch / "calls"
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

    def lay(self, crates):
        """Each manifest of `crates` written at its path, a crate's
        `src/lib.rs` beside it."""
        for path, text in crates.items():
            (self.root / path).parent.mkdir(parents=True, exist_ok=True)
            (self.root / path).write_text(text)
            if path != "Cargo.toml":
                (self.root / path).with_name("src").mkdir(exist_ok=True)
                (self.root / path).with_name("src").joinpath("lib.rs").write_text("")

    def committed(self, crates):
        """`crates` laid down over the tree, in a commit of its own that is
        HEAD."""
        self.lay(crates)
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "the crates")
        self.commit = self.git("rev-parse", "HEAD").strip()

    def versioned(self, version):
        """The manifest's version line set to `version`, in a commit of its
        own that is HEAD, as a release's bump is."""
        (self.root / "Cargo.toml").write_text(manifest(version))
        self.git("commit", "-q", "--allow-empty", "-am", f"version {version}")
        self.commit = self.git("rev-parse", "HEAD").strip()

    def script(self, name, *arguments, env=None, root=None):
        """dist/<name> run in the repository, or in the clone at `root`,
        with the PATH above."""
        root = root or self.root
        return subprocess.run(
            [sys.executable, str(root / "dist" / name), *arguments],
            cwd=root, env={**os.environ, "PATH": self.path, **(env or {})},
            capture_output=True, text=True,
        )

    def backport(self):
        """`v1.1.0` on the first commit, and `v1.0.1`, a backport, on a later
        one; and a clone of `v1.0.1` alone, one commit deep, as a CI
        checkout without `fetch-depth: 0` is. Returns the clone."""
        self.versioned("1.1.0")
        self.git("tag", "v1.1.0")
        self.versioned("1.0.1")
        self.git("tag", "v1.0.1")
        clone = self.scratch / "clone"
        subprocess.run(
            ["git", "clone", "-q", "--depth", "1", "--branch", "v1.0.1", self.root.as_uri(), str(clone)],
            check=True, capture_output=True,
        )
        return clone

    def lines(self, run, kind):
        self.assertEqual(run.returncode, 0, run.stderr)
        return [line.split(" ", 1)[1] for line in run.stdout.splitlines() if line.startswith(kind + " ")]

    def tool(self, name, text):
        """A program of the test's own in bin/, a Python script."""
        (self.bin / name).write_text(text.replace("/usr/bin/env python3", sys.executable))
        (self.bin / name).chmod(0o755)
