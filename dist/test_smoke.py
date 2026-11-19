"""The smoke's arguments, held without Docker or a build."""

import os
import shutil
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


class SmokeArguments(unittest.TestCase):
    def smoke(self, *arguments):
        # Nothing on PATH, docker least of all: a refusal comes before any
        # command the script runs.
        return subprocess.run(
            [shutil.which("bash"), str(ROOT / "dist/smoke.sh"), *arguments],
            env={**os.environ, "PATH": ""},
            capture_output=True, text=True,
        )

    def test_a_variant_the_images_do_not_have_is_refused_before_docker_runs(self):
        for arguments in (("--image", "lela:edge", "zts"), ("--image", "lela:edge"), ("--image",)):
            with self.subTest(arguments=arguments):
                run = self.smoke(*arguments)
                self.assertEqual(run.returncode, 2, run.stderr)
                self.assertIn("--image <image> cli|fpm|apache", run.stderr)

    def test_no_directory_or_two_are_refused(self):
        for arguments in ((), ("a", "b"), ("--unknown",)):
            with self.subTest(arguments=arguments):
                run = self.smoke(*arguments)
                self.assertEqual(run.returncode, 2, run.stderr)
                self.assertIn("usage:", run.stderr)


if __name__ == "__main__":
    unittest.main()
