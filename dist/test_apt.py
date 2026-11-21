"""dist/apt.sh, the one way dist/Dockerfile, ci/oracle/Dockerfile and
dist/release.py's Linux smoke run apt, held offline: each step it retries,
how often and how long it waits, what it passes apt, and that every apt
step of either Dockerfile goes through it; test_release.py holds the
smoke's to it with runs_apt_bare.

The script runs with a PATH of one directory, the test's, where an
`apt-get` that exits as the test says and a `sleep` that returns at once
record each call; nothing reaches a mirror and nothing waits.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path

from test_inputs import stages

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "dist/apt.sh"
# dist/Dockerfile's context is the checkout; the oracle's is the directory
# scripts/oracle-build.py fills, where the script is apt.sh.
MOUNT = "--mount=type=bind,source=dist/apt.sh,target=/tmp/apt.sh"
ORACLE_MOUNT = "--mount=type=bind,source=apt.sh,target=/tmp/apt.sh"
OPTIONS = ["-o", "Acquire::Retries=5", "-o", "APT::Update::Error-Mode=any"]
INSTALL = ["install", "-y", "-qq", "--no-install-recommends"]
# What the stand-in prints on a failed try, as apt does on a 502 with no body.
FAILURE = "E: Failed to fetch http://ports.ubuntu.com/ubuntu-ports/dists/resolute/InRelease  502  Bad Gateway"


def runs_apt_bare(shell):
    """Whether shell text runs apt or apt-get as a command word, not a
    path such as /var/lib/apt or the script's own /dist/apt.sh."""
    return re.search(r"(?<![\w/.-])apt(-get)?(?=\s)", shell) is not None


def source_module(name, path):
    """The module at `path`, run from its source text. An import's loader
    caches bytecode in a __pycache__ beside the file unless Python runs
    with -B, and outside dist/ the checkout ignores none; compile() writes
    nothing, so the load leaves the tree as it found it however the tests
    are run."""
    module = types.ModuleType(name)
    module.__file__ = str(path)
    exec(compile(path.read_bytes(), str(path), "exec"), module.__dict__)
    return module


def unguarded(text, mount=MOUNT):
    """Each RUN of a Dockerfile that runs apt other than through
    /tmp/apt.sh, or runs /tmp/apt.sh without `mount`, the bind of the
    script there from the Dockerfile's context, as (stage, instruction)."""
    found = []
    for name, stage in stages(text).items():
        for keyword, rest in stage["instructions"]:
            if keyword != "RUN":
                continue
            if runs_apt_bare(rest) or ("/tmp/apt.sh" in rest and mount not in rest.split()):
                found.append((name, rest))
    return found


class DockerfileSteps(unittest.TestCase):
    def test_every_apt_step_of_the_dockerfile_goes_through_apt_sh(self):
        text = (ROOT / "dist/Dockerfile").read_text()
        self.assertEqual(unguarded(text), [])
        # Each stage that starts from Ubuntu updates through it.
        updates = {
            name for name, stage in stages(text).items()
            for keyword, rest in stage["instructions"]
            if keyword == "RUN" and "sh /tmp/apt.sh update;" in rest
        }
        self.assertEqual(updates, {"inputs", "toolchain", "base", "apache"})

    def test_a_bare_apt_step_or_an_unmounted_script_is_found(self):
        for run in (
            "RUN apt-get update -qq; apt-get install -y curl",
            "RUN apt update",
            f"RUN {MOUNT} \\\n    sh /tmp/apt.sh update; apt-get install -y curl",
            "RUN sh /tmp/apt.sh update",
        ):
            with self.subTest(run=run):
                self.assertEqual(len(unguarded(f"FROM ubuntu AS one\n{run}\n")), 1)
        self.assertEqual(unguarded(f"FROM ubuntu AS one\nRUN {MOUNT} sh /tmp/apt.sh update\n"), [])

    def test_a_bare_step_of_the_smoke_is_found_and_one_through_the_script_is_not(self):
        for line in (
            "apt-get update -qq",
            'xargs -a "$d/runtime-packages.txt" apt-get install -y -qq --no-install-recommends >/dev/null',
            "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends curl >/dev/null",
            "apt install curl",
        ):
            with self.subTest(line=line):
                self.assertTrue(runs_apt_bare(line))
        for line in (
            "sh /dist/apt.sh update",
            'xargs -a "$d/runtime-packages.txt" sh /dist/apt.sh install >/dev/null',
            "rm -rf /var/lib/apt/lists/*",
        ):
            with self.subTest(line=line):
                self.assertFalse(runs_apt_bare(line))

    def test_every_apt_step_of_the_oracle_s_dockerfile_goes_through_apt_sh(self):
        text = (ROOT / "ci/oracle/Dockerfile").read_text()
        self.assertEqual(unguarded(text, ORACLE_MOUNT), [])
        updates = {
            name for name, stage in stages(text).items()
            for keyword, rest in stage["instructions"]
            if keyword == "RUN" and "sh /tmp/apt.sh update;" in rest
        }
        self.assertEqual(updates, {"deps"})
        # The checkout's mount names a file the oracle's context does not hold.
        mounted = text.replace(ORACLE_MOUNT, MOUNT)
        self.assertEqual([name for name, _ in unguarded(mounted, ORACLE_MOUNT)], ["deps"])

    def test_the_oracle_s_build_context_carries_apt_sh(self):
        # scripts/oracle-build.py, loaded as a module: its main() runs only as a script.
        recipe = source_module("oracle_build", ROOT / "scripts/oracle-build.py")
        self.assertEqual(recipe.CONTEXT["apt.sh"], SCRIPT)
        self.assertEqual(recipe.DOCKERFILE, ROOT / "ci/oracle/Dockerfile")


class Leftovers(unittest.TestCase):
    """The one dist test that loads a module from outside dist/, run as
    dist/README.md runs the tests, with bytecode written, in a repository
    of the test's own holding the files it reads: it leaves `git status
    --porcelain` as it found it. Running the whole suite here would run
    this test inside itself; a load added to another test is not held."""

    FILES = (
        "dist/test_apt.py", "dist/test_inputs.py", "dist/inputs.py", "oracle.toml",
        "scripts/oracle-build.py", "scripts/php_src_tree.py", ".gitignore",
    )

    def git(self, root, *arguments):
        env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
        return subprocess.run(
            ["git", "-C", str(root), "-c", "user.name=test", "-c", "user.email=test@example.invalid", *arguments],
            env=env, check=True, capture_output=True, text=True,
        ).stdout

    def test_loading_the_oracle_s_recipe_leaves_the_tree_as_it_found_it(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name).resolve()
        for path in self.FILES:
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / path, root / path)
        self.git(root, "init", "-q")
        self.git(root, "add", "-A")
        self.git(root, "commit", "-q", "-m", "the tree")
        found = self.git(root, "status", "--porcelain", "--untracked-files=all")
        # No -B, and none of the variables that stop or move the bytecode.
        env = {key: value for key, value in os.environ.items() if not key.startswith("PYTHON")}
        name = DockerfileSteps.test_the_oracle_s_build_context_carries_apt_sh.__qualname__
        run = subprocess.run(
            [sys.executable, "-m", "unittest", f"test_apt.{name}"],
            cwd=root / "dist", env=env, capture_output=True, text=True,
        )
        self.assertEqual(run.returncode, 0, run.stderr)
        # The run wrote bytecode, where the checkout ignores it.
        self.assertTrue((root / "dist/__pycache__").is_dir())
        self.assertEqual(self.git(root, "status", "--porcelain", "--untracked-files=all"), found)


class Script(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.bin = Path(temporary.name)
        self.calls = self.bin / "calls"
        self.sleeps = self.bin / "sleeps"
        self.exits = self.bin / "exits"
        self.stub("sleep", f'echo "$1" >> {self.sleeps}')
        # Each call takes the next line of `exits` as its exit, 0 once they run out.
        self.stub("apt-get", f"""
echo "DEBIAN_FRONTEND=${{DEBIAN_FRONTEND:-}} $*" >> {self.calls}
status=0
if [ -s {self.exits} ]; then
    {{ read -r status; rest=""; while read -r line; do rest="$rest$line
"; done; }} < {self.exits}
    printf '%s' "$rest" > {self.exits}
fi
[ "$status" -eq 0 ] || echo "{FAILURE}" >&2
exit "$status"
""")

    def stub(self, name, body):
        path = self.bin / name
        path.write_text("#!/bin/sh\n" + body.lstrip("\n"))
        path.chmod(0o755)

    def run_script(self, *arguments, exits=()):
        self.exits.write_text("".join(f"{status}\n" for status in exits))
        run = subprocess.run(
            ["/bin/sh", str(SCRIPT), *arguments], env={"PATH": str(self.bin)},
            capture_output=True, text=True,
        )
        calls = self.calls.read_text().splitlines() if self.calls.exists() else []
        sleeps = self.sleeps.read_text().split() if self.sleeps.exists() else []
        return run, calls, sleeps

    def test_an_update_that_succeeds_runs_once_with_apt_s_retries_and_errors_on(self):
        run, calls, sleeps = self.run_script("update")
        self.assertEqual((run.returncode, run.stderr), (0, ""))
        self.assertEqual(calls, ["DEBIAN_FRONTEND= " + " ".join([*OPTIONS, "update", "-qq"])])
        self.assertEqual(sleeps, [])

    def test_an_update_that_fails_for_a_moment_is_tried_again_after_a_growing_wait(self):
        run, calls, sleeps = self.run_script("update", exits=(100, 100))
        self.assertEqual(run.returncode, 0)
        self.assertEqual(len(calls), 3)
        self.assertEqual(len(set(calls)), 1)
        self.assertEqual(sleeps, ["15", "30"])
        self.assertEqual(run.stderr.count(FAILURE), 2)
        self.assertIn("apt.sh: update exited 100, try 2 of 4; trying again in 30s", run.stderr)

    def test_a_mirror_that_stays_down_fails_with_apt_s_message_and_status(self):
        run, calls, sleeps = self.run_script("update", exits=(100,) * 10)
        self.assertEqual(run.returncode, 100)
        self.assertEqual(len(calls), 4)
        self.assertEqual(sleeps, ["15", "30", "45"])
        lines = run.stderr.splitlines()
        self.assertEqual(lines[-2:], [FAILURE, "apt.sh: update failed 4 times, the last with exit 100"])

    def test_install_retries_the_download_and_installs_once_without_fetching(self):
        run, calls, sleeps = self.run_script("install", "curl", "python3", exits=(100, 0, 0))
        self.assertEqual(run.returncode, 0)
        fetch = "DEBIAN_FRONTEND=noninteractive " + " ".join([*OPTIONS, *INSTALL, "--download-only", "curl", "python3"])
        place = "DEBIAN_FRONTEND=noninteractive " + " ".join([*OPTIONS, *INSTALL, "--no-download", "curl", "python3"])
        self.assertEqual(calls, [fetch, fetch, place])
        self.assertEqual(sleeps, ["15"])

    def test_an_install_that_fails_after_its_download_is_not_tried_again(self):
        run, calls, sleeps = self.run_script("install", "curl", exits=(0, 100))
        self.assertEqual(run.returncode, 100)
        self.assertEqual([call.split()[-2] for call in calls], ["--download-only", "--no-download"])
        self.assertEqual(sleeps, [])

    def test_a_step_it_does_not_know_or_a_missing_package_list_is_refused(self):
        for arguments in ((), ("upgrade",), ("install",), ("update", "extra")):
            with self.subTest(arguments=arguments):
                run, calls, _ = self.run_script(*arguments)
                self.assertEqual(run.returncode, 2)
                self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
