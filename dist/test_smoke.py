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
        for arguments in ((), ("a", "b"), ("--unknown",), ("--machos",), ("--machos", "a", "b")):
            with self.subTest(arguments=arguments):
                run = self.smoke(*arguments)
                self.assertEqual(run.returncode, 2, run.stderr)
                self.assertIn("usage:", run.stderr)



class Compat(unittest.TestCase):
    """The compat check of the image mode, `--compat <dir> <variant>`, run
    here over a directory of the test's own: `lela/` stands in for the
    image's /usr/local/bin, its hosts under Lela's names, and `other/` for
    a php, php-cgi and php-fpm that are not the compat directory's."""

    USUAL = ("php", "php-cgi", "php-fpm")

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.scratch = Path(temporary.name).resolve()
        self.compat = self.scratch / "compat"
        self.compat.mkdir()
        self.other = self.scratch / "other"
        self.lela = self.scratch / "lela"
        for directory, hosts in ((self.other, self.USUAL), (self.lela, ("lela", "lela-cgi", "lela-fpm"))):
            directory.mkdir()
            for host in hosts:
                program = directory / host
                program.write_text(f"#!/bin/sh\necho {directory.name} {host}\n")
                program.chmod(0o755)

    def link(self, *usual):
        for host in usual:
            (self.compat / host).symlink_to(self.lela / ("lela" + host.removeprefix("php")))

    def check(self, *path, variant="fpm"):
        return subprocess.run(
            [shutil.which("bash"), str(ROOT / "dist/smoke.sh"), "--compat", str(self.compat), variant],
            env={**os.environ, "PATH": os.pathsep.join(map(str, [*path, self.lela]))},
            capture_output=True, text=True,
        )

    def test_each_entry_is_what_its_name_finds_once_the_directory_is_first(self):
        self.link(*self.USUAL)
        run = self.check()
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn(f"compat: {self.compat}, not on PATH", run.stdout)
        self.assertIn("compat: no php, php-cgi or php-fpm on PATH", run.stdout)
        for host in self.USUAL:
            with self.subTest(host=host):
                self.assertIn(f"compat: {host} is {self.compat / host} with {self.compat} first on PATH", run.stdout)

    def test_a_directory_on_path_or_none_fails(self):
        self.link(*self.USUAL)
        run = self.check(self.compat)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} is on PATH", run.stderr)
        for host in self.USUAL:
            (self.compat / host).unlink()
        self.compat.rmdir()
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} is not a directory", run.stderr)

    def test_a_usual_name_on_path_fails(self):
        self.link(*self.USUAL)
        run = self.check(self.other)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(
            f"compat: php is {self.other / 'php'} on PATH, where no usual name is until {self.compat} is put first",
            run.stderr,
        )

    def test_an_empty_directory_fails(self):
        run = self.check(variant="cli")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} holds no php", run.stderr)

    def test_an_entry_that_is_not_executable_fails(self):
        self.link("php-cgi", "php-fpm")
        (self.compat / "php").write_text("#!/bin/sh\necho compat php\n")
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat / 'php'} does not run", run.stderr)

    def test_an_entry_its_name_does_not_find_fails(self):
        # A directory, which the shell does not take for the name it finds.
        self.link("php", "php-fpm")
        (self.compat / "php-cgi").mkdir()
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: php-cgi is nothing with {self.compat} first on PATH", run.stderr)

    def test_an_entry_that_does_not_run_fails(self):
        self.link("php-cgi", "php-fpm")
        (self.compat / "php").symlink_to(self.scratch / "gone")
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("compat:", run.stderr)

    def test_a_usual_name_the_variant_has_a_host_for_is_an_entry(self):
        self.link("php", "php-cgi")
        run = self.check(variant="cli")
        self.assertEqual(run.returncode, 0, run.stderr)
        run = self.check(variant="fpm")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} holds no php-fpm", run.stderr)


if __name__ == "__main__":
    unittest.main()
