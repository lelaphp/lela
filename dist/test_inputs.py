"""Small offline tests for the production distribution verifier."""

import hashlib
import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("inputs", ROOT / "dist/inputs.py")
inputs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inputs)


class DistributionInputs(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "dist").mkdir()
        for name in ("oracle.toml", "rust-toolchain.toml", "dist/inputs.toml"):
            shutil.copyfile(ROOT / name, self.root / name)
        self.oracle = inputs.read_toml(self.root / "oracle.toml")
        self.toolchain = inputs.read_toml(self.root / "rust-toolchain.toml")["toolchain"]

    def replace(self, name, old, new):
        path = self.root / name
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new))

    def cli(self, *arguments):
        # A download attempted during drift/architecture rejection would
        # fail to find curl: rejection must happen before any download.
        return subprocess.run(
            [sys.executable, str(ROOT / "dist/inputs.py"), "--root", str(self.root), *arguments],
            env={**os.environ, "PATH": ""},
            capture_output=True, text=True,
        )

    def test_checkout_alone_has_all_pins(self):
        self.assertEqual(self.cli("check").returncode, 0)
        pins = inputs.check(self.root)
        self.assertEqual(len(inputs.ini(pins)), 2)
        for _, url, digest in inputs.ini(pins):
            self.assertIn("/" + pins["php"]["commit"] + "/", url)
            self.assertRegex(digest, r"^[0-9a-f]{64}$")

    def test_external_docker_images_are_digest_pinned(self):
        dockerfile = (ROOT / "dist/Dockerfile").read_text()
        self.assertRegex(dockerfile.splitlines()[0], r"^# syntax=docker/dockerfile:[^@]+@sha256:[0-9a-f]{64}$")
        arguments = {}
        stages = {"scratch"}
        external = []
        for line in dockerfile.splitlines():
            if line.startswith("ARG ") and "=" in line:
                name, value = line[4:].split("=", 1)
                arguments["${" + name + "}"] = value
            if line.startswith("FROM "):
                words = line.split()
                image = words[2] if words[1].startswith("--platform=") else words[1]
                image = arguments.get(image, image)
                if image not in stages:
                    external.append(image)
                    self.assertRegex(image, r"@sha256:[0-9a-f]{64}$")
                stages.add(words[-1])
        self.assertTrue(external)

    def test_oracle_commit_tag_and_version_drift_stop_before_download(self):
        for old, new in (
            (f'commit = "{self.oracle["source"]["commit"]}"', 'commit = "00000000"'),
            (f'tag = "{self.oracle["source"]["tag"]}"', 'tag = "php-0.0.0"'),
            (f'version = "{self.oracle["oracle"]["version"]}"', 'version = "0.0.0"'),
        ):
            with self.subTest(field=old):
                original = (self.root / "oracle.toml").read_text()
                self.replace("oracle.toml", old, new)
                run = self.cli("ini", str(self.root / "out"))
                self.assertEqual(run.returncode, 1)
                self.assertIn("oracle.toml source/version changed", run.stderr)
                self.assertFalse((self.root / "out").exists())
                (self.root / "oracle.toml").write_text(original)

    def test_full_oracle_commit_is_accepted_but_short_or_empty_is_not(self):
        full = inputs.check(self.root)["php"]["commit"]
        current = self.oracle["source"]["commit"]
        self.replace("oracle.toml", f'commit = "{current}"', f'commit = "{full}"')
        self.assertEqual(self.cli("check").returncode, 0)
        different = full[:-1] + ("1" if full[-1] == "0" else "0")
        for commit in ("", full[:4], different):
            with self.subTest(commit=commit):
                original = (self.root / "oracle.toml").read_text()
                self.replace("oracle.toml", f'commit = "{full}"', f'commit = "{commit}"')
                self.assertNotEqual(self.cli("check").returncode, 0)
                (self.root / "oracle.toml").write_text(original)

    def test_abbreviated_distribution_commit_is_rejected(self):
        full = inputs.check(self.root)["php"]["commit"]
        self.replace("dist/inputs.toml", full, full[:8])
        run = self.cli("check")
        self.assertEqual(run.returncode, 1)
        self.assertIn("must be a full commit", run.stderr)

    def test_toolchain_channel_and_components_drift_stop_before_download(self):
        original = (self.root / "rust-toolchain.toml").read_text()
        for old, new in (
            (self.toolchain["channel"], "0.0.0"),
            (f'"{self.toolchain["components"][0]}"', '"dist-test-component"'),
        ):
            with self.subTest(field=old):
                self.replace("rust-toolchain.toml", old, new)
                run = self.cli("rustup", "arm64", str(self.root / "rustup-init"))
                self.assertEqual(run.returncode, 1)
                self.assertIn("rust-toolchain.toml changed", run.stderr)
                self.assertFalse((self.root / "rustup-init").exists())
                (self.root / "rust-toolchain.toml").write_text(original)

    def test_build_architecture_selects_published_installer(self):
        pins = inputs.check(self.root)
        for arch, host in (("amd64", "x86_64"), ("arm64", "aarch64")):
            with self.subTest(arch=arch):
                url, digest = inputs.rustup(pins, arch)
                self.assertEqual(
                    url, "https://static.rust-lang.org/rustup/archive/"
                    + pins["rustup"]["version"] + "/" + host + "-unknown-linux-gnu/rustup-init",
                )
                self.assertRegex(digest, r"^[0-9a-f]{64}$")

    def test_unsupported_build_architecture_fails_before_download(self):
        run = self.cli("rustup", "riscv64", str(self.root / "rustup-init"))
        self.assertEqual(run.returncode, 1)
        self.assertIn("unsupported BUILDARCH: riscv64", run.stderr)

    def downloader(self, payload, failure=False):
        def curl(arguments, check):
            self.assertTrue(check)
            self.assertIn("--fail", arguments)
            self.assertIn("--location", arguments)
            self.assertEqual(arguments[arguments.index("--proto") + 1], "=https")
            self.assertEqual(arguments[arguments.index("--proto-redir") + 1], "=https")
            Path(arguments[arguments.index("--output") + 1]).write_bytes(payload)
            if failure:
                raise subprocess.CalledProcessError(22, arguments)
        return patch.object(inputs.subprocess, "run", side_effect=curl)

    def test_verified_bytes_reach_destination(self):
        payload = b"verified distribution input\n"
        destination = self.root / "out" / "rustup-init"
        with self.downloader(payload):
            inputs.download("https://example.invalid/input", hashlib.sha256(payload).hexdigest(), destination)
        self.assertEqual(destination.read_bytes(), payload)
        self.assertEqual(list(destination.parent.iterdir()), [destination])

    def test_changed_byte_of_each_download_is_rejected(self):
        pins = inputs.check(self.root)
        downloads = [(name, url, digest) for name, url, digest in inputs.ini(pins)]
        downloads += [(arch, *inputs.rustup(pins, arch)) for arch in ("amd64", "arm64")]
        for name, url, _ in downloads:
            with self.subTest(input=name):
                payload = b"original bytes\n"
                destination = self.root / name
                # A previous verified file must survive an interrupted or bad refresh.
                destination.write_bytes(payload)
                digest = hashlib.sha256(payload).hexdigest()
                with self.downloader(b"Original bytes\n"):
                    with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                        inputs.download(url, digest, destination)
                self.assertEqual(destination.read_bytes(), payload)

    def test_failed_download_leaves_no_input(self):
        destination = self.root / "out" / "php.ini-production"
        with self.downloader(b"incomplete", failure=True):
            with self.assertRaises(subprocess.CalledProcessError):
                inputs.download("https://example.invalid/input", "0" * 64, destination)
        self.assertFalse(destination.exists())
        self.assertEqual(list(destination.parent.iterdir()), [])

    def test_invalid_digest_is_rejected_before_network(self):
        with patch.object(inputs.subprocess, "run") as curl:
            with self.assertRaisesRegex(ValueError, "invalid SHA-256 pin"):
                inputs.download("https://example.invalid/input", "no pin", self.root / "out")
        curl.assert_not_called()


if __name__ == "__main__":
    unittest.main()
