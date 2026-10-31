# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
#
# The CLI, CGI and FPM hosts built from this checkout, and the image that
# stands in for php's official apache image, with the checkout as the build
# context
# (Dockerfile.dockerignore beside this file keeps target/ and what no build
# reads out of it):
#
#   docker buildx build -f dist/Dockerfile --target binaries --output <dir> .
#   docker buildx build -f dist/Dockerfile --target apache -t <tag> --load .
#
# --platform linux/arm64 or linux/amd64 picks the target. rustc runs on the
# builder's own architecture and cross-compiles, since QEMU's emulation
# crashes it; only the apache stage's own layers run as the target.
#
# The base is the Ubuntu release CI builds and tests the workspace on,
# whose package names packages.txt gives: ext/intl links the ICU the
# oracle links (packages.txt), which the Debian release php's official
# images are built on does not ship.

ARG UBUNTU=ubuntu:26.04@sha256:19fcd1bf268f02b60aca3bd30bd8a6d9a11759a430e32921833e65f16d87348a
# Where php's official images keep php.ini, $PHP_INI_DIR, with conf.d in it.
ARG PHP_INI_DIR=/usr/local/etc/php
# The hosts' names, `$NAME`, `$NAME-cgi` and `$NAME-fpm`, as ADR-0016 derives
# them from one constant; php's until #3464 renames them.
ARG NAME=php

# --- distribution inputs, held without another checkout -----------------
# Keep ini verification independent of the compiler and platform libraries.
# inputs.py holds the dist-local pins to oracle.toml and rust-toolchain.toml
# before either download runs; see dist/README.md for verification bounds.
FROM --platform=$BUILDPLATFORM ${UBUNTU} AS inputs
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
RUN apt-get update -qq; \
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        ca-certificates curl python3; \
    rm -rf /var/lib/apt/lists/*
COPY dist/inputs.py dist/inputs.toml /opt/dist/
COPY oracle.toml rust-toolchain.toml /opt/
RUN python3 /opt/dist/inputs.py check

# --- the toolchain and the platform libraries CI installs -----------------
# The libraries' -dev packages are the target's, through Ubuntu's multiarch,
# and the compilers and pkg-config the builder's.
FROM inputs AS toolchain
ARG BUILDARCH
ARG TARGETARCH
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
COPY .github/actions/platform-libraries/packages.txt /tmp/packages.txt
RUN case "$TARGETARCH" in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; \
        *) echo "no rust target for $TARGETARCH" >&2; exit 1 ;; esac; \
    triplet="$arch-linux-gnu"; rust="$arch-unknown-linux-gnu"; var="${rust//-/_}"; \
    echo "export TRIPLET=$triplet CARGO_BUILD_TARGET=$rust" > /usr/local/share/dax-target.env; \
    cross=(); \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
        . /etc/os-release; \
        case "$TARGETARCH" in amd64) mirror=http://archive.ubuntu.com/ubuntu/ ;; *) mirror=http://ports.ubuntu.com/ubuntu-ports/ ;; esac; \
        dpkg --add-architecture "$TARGETARCH"; \
        sed -i "/^Types:/a Architectures: $BUILDARCH" /etc/apt/sources.list.d/ubuntu.sources; \
        printf '%s\n' '' 'Types: deb' "URIs: $mirror" \
            "Suites: $VERSION_CODENAME $VERSION_CODENAME-updates $VERSION_CODENAME-backports $VERSION_CODENAME-security" \
            'Components: main universe restricted multiverse' "Architectures: $TARGETARCH" \
            'Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg' >> /etc/apt/sources.list.d/ubuntu.sources; \
        cross=("crossbuild-essential-$TARGETARCH"); \
        echo "export CARGO_TARGET_${var^^}_LINKER=$triplet-gcc CC_$var=$triplet-gcc CXX_$var=$triplet-g++ AR_$var=$triplet-ar" \
            "PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_LIBDIR=/usr/lib/$triplet/pkgconfig:/usr/share/pkgconfig" \
            >> /usr/local/share/dax-target.env; \
    fi; \
    apt-get update -qq; \
    sed -e 's/#.*//' -e 's/[[:space:]]*$//' -e '/^$/d' -e "s/-dev\$/-dev:$TARGETARCH/" /tmp/packages.txt > /tmp/packages; \
    DEBIAN_FRONTEND=noninteractive xargs -a /tmp/packages apt-get install -y -qq --no-install-recommends \
        ca-certificates curl build-essential "${cross[@]}"; \
    rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
# Verify the BUILDARCH installer before executing it, then let rustup
# verify the release's components. Do not let rustup update itself later.
RUN python3 /opt/dist/inputs.py rustup "$BUILDARCH" /tmp/rustup-init; \
    chmod 0755 /tmp/rustup-init; \
    /tmp/rustup-init -y -q --no-modify-path --profile minimal --default-toolchain none; \
    rm /tmp/rustup-init; \
    rustup set auto-self-update disable; \
    . /usr/local/share/dax-target.env; \
    cd /opt; rustup toolchain install; rustup target add "$CARGO_BUILD_TARGET"

# --- the hosts, laid out as php's official images lay them ----------------
FROM toolchain AS build
ARG TARGETARCH
ARG NAME
ARG PHP_INI_DIR
# What a release names as the build's provider (php's --with-build-provider),
# none where it is empty; and the build date php_build_date renders.
ARG BUILD_PROVIDER
ARG SOURCE_DATE_EPOCH
# Thin LTO links each host in one process that a 4 GiB VM holds one of at a
# time; more jobs link several at once there and the kernel kills one (#3213).
ARG CARGO_BUILD_JOBS=1
# The cache mounts' ids are `$CACHE-cargo-registry` and `$CACHE-target-<arch>`.
ARG CACHE=dist
# What php's official images configure: the php.ini search ends at
# $PHP_INI_DIR, and conf.d in it is scanned (php_engine's build_defs
# layout reads these at build time).
ENV PHP_CONFIG_FILE_PATH=${PHP_INI_DIR} \
    PHP_CONFIG_FILE_SCAN_DIR=${PHP_INI_DIR}/conf.d \
    PHP_BUILD_PROVIDER=${BUILD_PROVIDER}
WORKDIR /src
COPY . .
# The compiled tier is built in from the hosts' own manifests, each of which
# turns php_bundle's `jit` on by default, and php-cgi shares the bundle's
# features with them on this one cargo line; it stays off until opcache.jit
# asks, as php's does, and tracing is experimental (ADR-0017, #3676).
RUN --mount=type=cache,id=${CACHE}-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=${CACHE}-target-${TARGETARCH},target=/src/target \
    . /usr/local/share/dax-target.env; \
    cargo build --release --locked \
        -p php_sapi_cli -p php_sapi_cgi -p php_sapi_fpm \
        --bin "$NAME" --bin "$NAME-cgi" --bin "$NAME-fpm"; \
    release="target/$CARGO_BUILD_TARGET/release"; \
    install -D -m 0755 -t /out/usr/local/bin "$release/$NAME" "$release/$NAME-cgi"; \
    install -D -m 0755 -t /out/usr/local/sbin "$release/$NAME-fpm"
# The Ubuntu packages the hosts' shared libraries come from, which the
# apache stage installs and the binaries list: every library they need,
# directly or through another, read from the ELF files, since ldd cannot
# load a binary of another architecture.
RUN . /usr/local/share/dax-target.env; \
    dir="/usr/lib/$TRIPLET"; \
    todo=(/out/usr/local/bin/* /out/usr/local/sbin/*); \
    declare -A seen; \
    while [ "${#todo[@]}" -gt 0 ]; do \
        file="${todo[0]}"; todo=("${todo[@]:1}"); \
        for name in $(readelf -d "$file" | sed -n 's/.*(NEEDED).*\[\(.*\)\]$/\1/p'); do \
            [ -z "${seen[$name]:-}" ] || continue; \
            seen[$name]=1; \
            [ -e "$dir/$name" ] || { echo "$file needs $name, which $dir does not hold" >&2; exit 1; }; \
            todo+=("$dir/$name"); \
            dpkg -S "$(realpath "$dir/$name")" 2>/dev/null || dpkg -S "$dir/$name"; \
        done; \
    done | cut -d: -f1 | sort -u > /out/runtime-packages.txt; \
    cat /out/runtime-packages.txt

# --- php.ini-development and php.ini-production ---------------------------
# From the full php-src commit in dist/inputs.toml, with each SHA-256 held
# before the files reach /ini. An oracle move refuses stale pins.
FROM inputs AS ini
RUN python3 /opt/dist/inputs.py ini /ini

# --- the binaries, as a tarball's contents: `--output <dir>` --------------
FROM scratch AS binaries
COPY --from=build /out/usr/local/bin/ /bin/
COPY --from=build /out/usr/local/sbin/ /sbin/
COPY --from=build /out/runtime-packages.txt /
COPY --from=ini /ini/ /etc/

# --- the drop-in for php's apache image -----------------------------------
# Apache's prefork MPM in front of php-fpm over a unix socket, where php's
# image loads mod_php: the engine has no apache2handler yet.
FROM ${UBUNTU} AS apache
ARG PHP_INI_DIR
# The version php's image sets, which a Dockerfile written for it reads in
# its own RUN steps: the pinned php's, so a plain build sets it. xtask's
# tests hold this literal to oracle.toml's version, and the build fails
# below where the built php says another (#3533).
ARG PHP_VERSION=8.5.12
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ENV PHP_INI_DIR=${PHP_INI_DIR} \
    PHP_VERSION=${PHP_VERSION} \
    APACHE_CONFDIR=/etc/apache2 \
    APACHE_ENVVARS=/etc/apache2/envvars
COPY --from=build /out/runtime-packages.txt /tmp/runtime-packages.txt
RUN apt-get update -qq; \
    DEBIAN_FRONTEND=noninteractive xargs -a /tmp/runtime-packages.txt apt-get install -y -qq --no-install-recommends \
        ca-certificates curl xz-utils apache2; \
    rm -rf /var/lib/apt/lists/* /tmp/runtime-packages.txt
# From here to the next comment, php's official apache image as written
# (docker-library/php, Dockerfile-linux.template).
RUN sed -ri 's/^export ([^=]+)=(.*)$/: ${\1:=\2}\nexport \1/' "$APACHE_ENVVARS"; \
    . "$APACHE_ENVVARS"; \
    for dir in "$APACHE_LOCK_DIR" "$APACHE_RUN_DIR" "$APACHE_LOG_DIR" "$APACHE_RUN_DIR/socks"; do \
        rm -rf "$dir"; mkdir -p "$dir"; chown "$APACHE_RUN_USER:$APACHE_RUN_GROUP" "$dir"; chmod 1777 "$dir"; \
    done; \
    rm -rf /var/www/html/*; \
    ln -sfT /dev/stderr "$APACHE_LOG_DIR/error.log"; \
    ln -sfT /dev/stdout "$APACHE_LOG_DIR/access.log"; \
    ln -sfT /dev/stdout "$APACHE_LOG_DIR/other_vhosts_access.log"; \
    chown -R --no-dereference "$APACHE_RUN_USER:$APACHE_RUN_GROUP" "$APACHE_LOG_DIR"; \
    a2dismod -q mpm_event; a2enmod -q mpm_prefork; \
    mkdir -p "$PHP_INI_DIR/conf.d"; \
    mkdir -p /var/www/html; chown www-data:www-data /var/www/html; chmod 1777 /var/www/html
# Where php's image sets `SetHandler application/x-httpd-php`, the same
# files go to php-fpm; its socket's directory is open to any user, as
# Apache's run directories are above, so the image runs as one.
COPY dist/apache/docker-php.conf /etc/apache2/conf-available/docker-php.conf
RUN a2enmod -q proxy_fcgi; a2enconf -q docker-php; \
    mkdir -p /run/php; chmod 1777 /run/php
COPY --from=build /out/usr/local/ /usr/local/
COPY --from=ini /ini/ ${PHP_INI_DIR}/
COPY dist/apache/php-fpm.conf /usr/local/etc/php-fpm.conf
COPY dist/apache/www.conf /usr/local/etc/php-fpm.d/www.conf
COPY dist/apache/bin/ /usr/local/bin/
# A Dockerfile written for php's image runs `php`, and this image's own
# scripts run `php` and `php-fpm`. Under another NAME, how the image
# offers those names is #3464's to decide (ADR-0016); until then the
# check below fails the build.
RUN cd /usr/local/bin; \
    ln -s docker-php-ext-install docker-php-ext-enable; \
    for name in pear peardev phpize php-config; do ln -s pecl "$name"; done; \
    php -v; php-fpm -t; apache2ctl -t; \
    built="$(php -r 'echo PHP_VERSION;')"; \
    [ "$built" = "$PHP_VERSION" ] || { echo "PHP_VERSION is $PHP_VERSION, and the built php says $built" >&2; exit 1; }
ENTRYPOINT ["docker-php-entrypoint"]
WORKDIR /var/www/html
STOPSIGNAL SIGWINCH
EXPOSE 80
CMD ["apache2-foreground"]
