# Distribution inputs

Build with the checkout as the context, without a php-src checkout:

```sh
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target binaries --output /tmp/dist-arm64 .
docker buildx build -f dist/Dockerfile --platform linux/amd64 --target binaries --output /tmp/dist-amd64 .
```

Run those builds sequentially on small builders. The default Cargo job count is
one. The installer runs on BUILDARCH; the compiled hosts and their libraries
follow TARGETARCH. Product naming remains governed by ADR-0016.

## The images

Three targets stand in for the variants of php's official image, each
shaped as docker-library/php's templates shape it: the same
`docker-php-entrypoint`, working directory and `$PHP_INI_DIR`
(`/usr/local/etc/php`, with `conf.d` in it).

| target | holds | default command | working directory |
|---|---|---|---|
| `cli` | the CLI and CGI hosts | `php -a` | `/` |
| `fpm` | those and the FPM, listening on port 9000 | `php-fpm` | `/var/www/html` |
| `apache` | those, and Apache's prefork MPM in front of the FPM over a unix socket | `apache2-foreground` | `/var/www/html` |

```sh
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target cli -t lela:cli --load .
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target fpm -t lela:fpm --load .
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target apache -t lela:apache --load .
```

All three are built on one `base` stage from one `build` stage: the hosts
compile once per platform, and the second and third target find that
compile cached. They share the Ubuntu packages the build stage reads from
the hosts' ELF files, and the compat directory, `/opt/lela/compat`, which
no image puts on `PATH`. Once the hosts carry Lela's names (ADR-0016) it
holds `php`, `php-cgi` and `php-fpm` as symlinks to them, and one line in a
Dockerfile built on the image opts in:

```dockerfile
ENV PATH=/opt/lela/compat:$PATH
```

Until then the hosts are named `php`, `php-cgi` and `php-fpm` already, and
the directory is laid down empty.

The `fpm` image's `php-fpm.conf` and `php-fpm.d/www.conf` start from
php-src's templates at the pinned commit, rendered as the official image's
`configure` renders them (`dist/inputs.py fpm`), and its `docker.conf`,
`zz-docker.conf` and `docker-fpm.ini` are the template's.

## A release by hand

Until CD builds the release, a person builds it on an Apple silicon Mac
with Docker, from a clean checkout of the tagged commit (ruled
2026-10-31): `darwin-arm64` with cargo, and `linux-aarch64` and
`linux-x86_64` from the `binaries` target above. Each platform is one
tarball, `<name>-<version>-<platform>.tar.gz`, holding one directory of
that name, and `SHA256SUMS` is written over them by `scripts/dist-sums.sh`:

```sh
dist/release.sh 0.1.0-beta.1 /tmp/release
(cd /tmp/release && shasum -a 256 -c SHA256SUMS)
```

The script refuses a tree with changes and prints the commit it built.
Name platforms after `<out>` to build only those. Docker
Desktop's default VM (about 4 GiB) links one host at a time, so
`CARGO_BUILD_JOBS` stays at its default of one.

Smoke each tarball on its platform before publishing it: `dist/smoke.sh`
runs `-v`, `-r`, a file with arguments, code on stdin, `php-cgi` on a file
and `php-fpm -t`, and exits nonzero at the first wrong answer. On the Mac:

```sh
mkdir -p /tmp/smoke
tar -xzf /tmp/release/php-0.1.0-beta.1-darwin-arm64.tar.gz -C /tmp/smoke
dist/smoke.sh /tmp/smoke/php-0.1.0-beta.1-darwin-arm64
```

A Linux tarball runs in the Ubuntu image the Dockerfile builds on, with its
`runtime-packages.txt` installed; `linux/amd64` runs under emulation on an
arm64 Mac:

```sh
ubuntu=$(sed -n 's/^ARG UBUNTU=//p' dist/Dockerfile)
docker run --rm --platform linux/arm64 -v /tmp/release:/release:ro -v "$PWD/dist:/dist:ro" "$ubuntu" bash -c '
    tar -xzf /release/php-0.1.0-beta.1-linux-aarch64.tar.gz -C /tmp
    d=/tmp/php-0.1.0-beta.1-linux-aarch64
    apt-get update -qq && xargs -a "$d/runtime-packages.txt" apt-get install -y -qq --no-install-recommends >/dev/null
    /dist/smoke.sh "$d"'
```

The macOS binaries carry the linker's ad-hoc signature, with no Developer
ID and no notarization. Nothing here tags, pushes or publishes;
that is a person's.

## The compiled tier

The hosts carry the compiled tier: `sapi/cli`, `sapi/fpm` and `sapi/makala`
each turn `php_bundle`'s `jit` feature on by default, so the release build
needs no flag for it. `opcache.jit` defaults to `disable`, as php's does, so a
request runs on the interpreter until `opcache.jit=tracing` (with
`opcache.enable_cli=1` in the CLI) asks for the tier. Tracing is experimental:
it sits outside every supported combination (ADR-0017). A host built with
`--no-default-features` carries no tier.

## Verify and refresh

`dist/inputs.toml` holds the full php-src commit, both ini SHA-256 values, the
SHA-256 of the two FPM configuration templates the `fpm` image renders, the
versioned rustup installer SHA-256 for each build architecture, and the SHA-256
of `rust-toolchain.toml`. `dist/inputs.py` compares the source identity and
toolchain file before downloading, then verifies each download before use.
A mismatch exits nonzero with the file to refresh. Docker runs that same
verifier; a cached layer is invalidated when an input or pin changes.

Run the offline tests and the checkout's pin comparison with Python 3.11+:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s dist -p 'test_*.py' -v
python3 dist/inputs.py check
```

The tests exercise byte rejection, incomplete downloads, oracle commit/tag/version
drift, toolchain channel/component drift, both installer architectures, and the
FPM templates rendered as the official fpm image's `configure` renders them;
with the pinned php-src tree at `TEST_PHP_SRCDIR`, they also hold the real
templates to their pins. They do not build or run platform binaries.

To exercise real downloads without installing anything:

```sh
python3 dist/inputs.py ini /tmp/dist-inputs/ini
python3 dist/inputs.py fpm /tmp/dist-inputs/fpm
python3 dist/inputs.py rustup amd64 /tmp/dist-inputs/amd64/rustup-init
python3 dist/inputs.py rustup arm64 /tmp/dist-inputs/arm64/rustup-init
docker buildx build -f dist/Dockerfile --target ini .
```

When a person moves the oracle, resolve its source tag to the full commit in a
trusted php-src checkout (`git rev-parse '<tag>^{commit}'`). Confirm that commit
extends `oracle.toml`'s abbreviated commit and that the tag matches the oracle
version. Hash each ini file and each FPM template at that commit with SHA-256
and review the bytes. Update the tag, full commit, ini and FPM hashes together
in `dist/inputs.toml`.
The verifier compares all three source/version fields, so a tag-only move
also refuses stale pins. It does not contact a mutable remote tag at build time;
the reviewed tag-to-commit relationship is recorded in the pin file.

After a reviewed toolchain change, update its file SHA-256 in `dist/inputs.toml`.
Docker installs the channel and components from that exact toolchain file.
For a rustup update, use the versioned
`https://static.rust-lang.org/rustup/archive/<version>/<host>/rustup-init.sha256`
for both hosts listed in the pin file, then verify the downloaded installers
against those published hashes. Do not derive a new trusted hash solely from
the download being installed.

The Dockerfile frontend and Ubuntu base use multi-platform image index digests
in `dist/Dockerfile`. Inspect replacement indices for both linux/amd64 and
linux/arm64 before updating them. An intentional build-argument override of
UBUNTU supplies a different base and is the caller's responsibility.

## Verification bounds

The direct curl downloads (installers and ini files) must match repository-held
SHA-256 values before use. HTTPS is required across redirects. Docker verifies
the frontend and base image content through their index digests.

Apt verifies signed repository metadata and package SHA-256 values. Package
versions and repository snapshots are not frozen here. Rustup verifies component
archives against the selected release manifest's hashes; that manifest is
retrieved over HTTPS and is not independently pinned in this repository.
Cargo uses `--locked` and the lockfile's registry checksums. These checks do
not make the whole build hermetic or guarantee byte-identical binaries.

Successful input tests do not supply the release's binary smoke evidence.
Both `binaries` targets must still be built and their `bin/php -v` run with
the matching runtime libraries on the release runners. Apache validation
also requires building its target and exercising the produced image.
