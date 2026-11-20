"""The smoke's arguments, held without Docker or a build."""

import os
import shutil
import subprocess
import tempfile
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



class Compat(unittest.TestCase):
    """The compat check of the image mode, `--compat <dir> <variant>`, run
    here over a directory of the test's own: `other/` stands in for the
    image's /usr/local/bin, a php, php-cgi and php-fpm on PATH that say
    they are not the compat directory's."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.scratch = Path(temporary.name).resolve()
        self.compat = self.scratch / "compat"
        self.compat.mkdir()
        self.other = self.scratch / "other"
        self.lela = self.scratch / "lela"
        for directory in (self.other, self.lela):
            directory.mkdir()
            for host in ("php", "php-cgi", "php-fpm"):
                program = directory / host
                program.write_text(f"#!/bin/sh\necho {directory.name} {host}\n")
                program.chmod(0o755)

    def check(self, *path, name="php", variant="fpm"):
        return subprocess.run(
            [shutil.which("bash"), str(ROOT / "dist/smoke.sh"), "--compat", str(self.compat), variant],
            env={**os.environ, "NAME": name, "PATH": os.pathsep.join(map(str, [*path, self.other]))},
            capture_output=True, text=True,
        )

    def test_an_empty_directory_off_path_passes_while_the_hosts_carry_phps_names(self):
        run = self.check()
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn(f"compat: {self.compat}, not on PATH", run.stdout)

    def test_a_directory_on_path_or_none_fails(self):
        run = self.check(self.compat)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} is on PATH", run.stderr)
        self.compat.rmdir()
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} is not a directory", run.stderr)

    def test_each_entry_is_what_its_name_finds_once_the_directory_is_first(self):
        for host in ("php", "php-cgi", "php-fpm"):
            (self.compat / host).symlink_to(self.lela / host)
        run = self.check(name="lela")
        self.assertEqual(run.returncode, 0, run.stderr)
        for host in ("php", "php-cgi", "php-fpm"):
            with self.subTest(host=host):
                self.assertIn(f"compat: {host} is {self.compat / host} with {self.compat} first on PATH", run.stdout)

    def test_an_entry_its_name_does_not_find_fails_though_the_name_runs(self):
        # Not executable, so the shell passes over it to other/'s php: the
        # usual name runs, and not through the compat directory.
        (self.compat / "php").write_text("#!/bin/sh\necho compat php\n")
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(
            f"compat: php is {self.other / 'php'} with {self.compat} first on PATH, and not {self.compat / 'php'}",
            run.stderr,
        )

    def test_an_entry_that_does_not_run_fails(self):
        (self.compat / "php").symlink_to(self.scratch / "gone")
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("compat:", run.stderr)

    def test_a_usual_name_missing_once_the_hosts_carry_lelas_names_fails(self):
        (self.compat / "php").symlink_to(self.lela / "php")
        (self.compat / "php-cgi").symlink_to(self.lela / "php-cgi")
        run = self.check(name="lela", variant="cli")
        self.assertEqual(run.returncode, 0, run.stderr)
        run = self.check(name="lela", variant="fpm")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} holds no php-fpm, and the hosts are named lela", run.stderr)


if __name__ == "__main__":
    unittest.main()
