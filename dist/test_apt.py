"""dist/apt.sh, the one way dist/Dockerfile runs apt, held offline: each
step it retries, how often and how long it waits, what it passes apt, and
that every apt step of the Dockerfile goes through it.

The script runs with a PATH of one directory, the test's, where an
`apt-get` that exits as the test says and a `sleep` that returns at once
record each call; nothing reaches a mirror and nothing waits.
"""

import re
import subprocess
import tempfile
import unittest
from pathlib import Path

from test_inputs import stages

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "dist/apt.sh"
MOUNT = "--mount=type=bind,source=dist/apt.sh,target=/tmp/apt.sh"
OPTIONS = ["-o", "Acquire::Retries=5", "-o", "APT::Update::Error-Mode=any"]
INSTALL = ["install", "-y", "-qq", "--no-install-recommends"]
# What the stand-in prints on a failed try, as apt does on a 502 with no body.
FAILURE = "E: Failed to fetch http://ports.ubuntu.com/ubuntu-ports/dists/resolute/InRelease  502  Bad Gateway"


def unguarded(text):
    """Each RUN of a Dockerfile that runs apt other than through
    /tmp/apt.sh, or runs /tmp/apt.sh without mounting dist/apt.sh there,
    as (stage, instruction)."""
    found = []
    for name, stage in stages(text).items():
        for keyword, rest in stage["instructions"]:
            if keyword != "RUN":
                continue
            # apt or apt-get as a command word, not a path such as /var/lib/apt.
            bare = re.search(r"(?<![\w/.-])apt(-get)?(?=\s)", rest)
            if bare or ("/tmp/apt.sh" in rest and MOUNT not in rest.split()):
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
