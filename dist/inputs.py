"""Verify distribution inputs before downloading or installing them."""

import argparse
import hashlib
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# What php's configure writes for each placeholder of the FPM templates as
# docker-library/php configures php: the default prefix, --sysconfdir at
# /usr/local/etc and --with-fpm-user and --with-fpm-group of www-data, the
# rest derived from those by configure.ac and sapi/fpm/config.m4. config.m4
# evaluates $prefix before autoconf fills in its default, so the image's
# www.conf.default says NONE where it names php_fpm_prefix.
CONFIGURE = {
    "prefix": "/usr/local",
    "php_fpm_prefix": "NONE",
    "php_fpm_sysconfdir": "/usr/local/etc",
    "EXPANDED_LOCALSTATEDIR": "/usr/local/var",
    "EXPANDED_DATADIR": "/usr/local/share/php",
    "php_fpm_user": "www-data",
    "php_fpm_group": "www-data",
}

# Each FPM template and where `make install` puts it, under the sysconfdir.
FPM = {
    "sapi/fpm/php-fpm.conf.in": "php-fpm.conf.default",
    "sapi/fpm/www.conf.in": "php-fpm.d/www.conf.default",
}


def read_toml(path):
    return tomllib.loads(path.read_text())


def check(root):
    pins = read_toml(root / "dist/inputs.toml")
    toolchain = root / "rust-toolchain.toml"
    if hashlib.sha256(toolchain.read_bytes()).hexdigest() != pins["toolchain_sha256"]:
        raise ValueError("rust-toolchain.toml changed: refresh dist/inputs.toml after review")
    oracle = read_toml(root / "oracle.toml")
    php = pins["php"]
    if not re.fullmatch(r"[0-9a-f]{40}", php["commit"]):
        raise ValueError("dist/inputs.toml: php.commit must be a full commit")
    source = oracle["source"]
    commit = source["commit"]
    if (
        not re.fullmatch(r"[0-9a-f]{8,40}", commit)
        or not php["commit"].startswith(commit)
        or source["tag"] != php["tag"]
        or php["tag"] != "php-" + oracle["oracle"]["version"]
    ):
        raise ValueError("oracle.toml source/version changed: refresh dist/inputs.toml after review")
    return pins


def rustup(pins, arch):
    if arch not in ("amd64", "arm64"):
        raise ValueError(f"unsupported BUILDARCH: {arch}")
    pin = pins["rustup"][arch]
    url = (
        "https://static.rust-lang.org/rustup/archive/"
        + pins["rustup"]["version"] + "/" + pin["host"] + "/rustup-init"
    )
    return url, pin["sha256"]


def ini(pins):
    return [
        (
            name,
            "https://raw.githubusercontent.com/php/php-src/" + pins["php"]["commit"] + "/" + name,
            pins["php"]["ini"][name],
        )
        for name in ("php.ini-development", "php.ini-production")
    ]


def fpm(pins):
    return [
        (
            source,
            "https://raw.githubusercontent.com/php/php-src/" + pins["php"]["commit"] + "/" + source,
            pins["php"]["fpm"][source],
            installed,
        )
        for source, installed in FPM.items()
    ]


def render(template):
    """A template as configure renders it, refusing a placeholder it would not fill."""
    def value(match):
        name = match.group(1)
        if name not in CONFIGURE:
            raise ValueError(f"FPM template placeholder @{name}@ has no configured value")
        return CONFIGURE[name]

    return re.sub(r"@([A-Za-z_]+)@", value, template)


def install_fpm(pins, directory):
    for source, url, digest, installed in fpm(pins):
        destination = directory / installed
        destination.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=destination.parent) as scratch:
            template = Path(scratch) / Path(source).name
            download(url, digest, template)
            rendered = template.with_suffix("")
            rendered.write_text(render(template.read_text()))
            rendered.replace(destination)


def download(url, digest, destination):
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise ValueError(f"invalid SHA-256 pin for {url}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    # No executable or ini is placed at its final path until its bytes pass.
    with tempfile.TemporaryDirectory(dir=destination.parent) as directory:
        temporary = Path(directory) / "download"
        subprocess.run(
            [
                "curl", "--fail", "--silent", "--show-error", "--location",
                "--proto", "=https", "--proto-redir", "=https",
                "--output", str(temporary), url,
            ],
            check=True,
        )
        with temporary.open("rb") as stream:
            actual = hashlib.file_digest(stream, "sha256").hexdigest()
        if actual != digest:
            raise ValueError(f"SHA-256 mismatch for {url}: expected {digest}, got {actual}")
        temporary.replace(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check")
    installer = commands.add_parser("rustup")
    installer.add_argument("buildarch")
    installer.add_argument("destination", type=Path)
    templates = commands.add_parser("ini")
    templates.add_argument("directory", type=Path)
    pool = commands.add_parser("fpm")
    pool.add_argument("directory", type=Path)
    args = parser.parse_args()
    try:
        pins = check(args.root)
        if args.command == "rustup":
            url, digest = rustup(pins, args.buildarch)
            download(url, digest, args.destination)
        elif args.command == "ini":
            for name, url, digest in ini(pins):
                download(url, digest, args.directory / name)
        elif args.command == "fpm":
            install_fpm(pins, args.directory)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f"dist inputs: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
