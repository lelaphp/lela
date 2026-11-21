# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
#
# The four hosts built from this checkout, and the three images that stand
# in for php's official ones, with the checkout as the build context
# (Dockerfile.dockerignore beside this file keeps target/ and what no build
# reads out of it):
#
#   docker buildx build -f dist/Dockerfile --target binaries --output <dir> .
#   docker buildx build -f dist/Dockerfile --target <variant> -t <tag> --load .
#
# The variants are `cli`, `fpm` and `apache`, each built on `base` from the
# one `build` stage, so the hosts compile once per platform however many
# variants are built after it. --platform linux/arm64 or linux/amd64 picks
# the platform. rustc runs on the builder's own architecture and
# cross-compiles, since QEMU's emulation crashes it; only the layers of
# `base` and the variants run as the target.
#
# The base is the Ubuntu release CI builds and tests the workspace on,
# whose package names packages.txt gives. ext/intl links the
# distribution's ICU, at or above the floor ext/intl's build states, as
# php built on it would.

ARG UBUNTU=ubuntu:26.04@sha256:19fcd1bf268f02b60aca3bd30bd8a6d9a11759a430e32921833e65f16d87348a
# Where php's official images keep php.ini, $PHP_INI_DIR, with conf.d in it.
ARG PHP_INI_DIR=/usr/local/etc/php
# The hosts' names, `$NAME`, `$NAME-cgi`, `$NAME-fpm` and `$NAME-dbg`, as
# ADR-0016 derives them from one constant, php_engine's PRODUCT_NAME, which
# xtask's tests hold this line to. dist/release.py and dist/smoke.sh read it here, and the
# images' own scripts and the CMD lines below spell it.
ARG NAME=lela
# The compat directory, under Lela's own prefix and on no image's PATH: the
# usual names, each a symlink to the host of Lela's name, which one
# `ENV PATH=<dir>:$PATH` line puts first (dist/image/compat.sh).
ARG COMPAT=/opt/lela/compat

# --- distribution inputs, held without another checkout -----------------
# Keep ini verification independent of the compiler and platform libraries.
# inputs.py holds the dist-local pins to oracle.toml and rust-toolchain.toml
# before either download runs; see dist/README.md for verification bounds.
FROM --platform=$BUILDPLATFORM ${UBUNTU} AS inputs
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
# Every apt step goes through dist/apt.sh, which tries each fetch again
# while a mirror answers 5xx for a moment; a RUN mounts it, so no image
# keeps it, and dist/test_apt.py holds each apt step to it.
RUN --mount=type=bind,source=dist/apt.sh,target=/tmp/apt.sh \
    sh /tmp/apt.sh update; \
    sh /tmp/apt.sh install ca-certificates curl python3; \
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
RUN --mount=type=bind,source=dist/apt.sh,target=/tmp/apt.sh \
    case "$TARGETARCH" in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; \
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
    sh /tmp/apt.sh update; \
    sed -e 's/#.*//' -e 's/[[:space:]]*$//' -e '/^$/d' -e "s/-dev\$/-dev:$TARGETARCH/" /tmp/packages.txt > /tmp/packages; \
    xargs -a /tmp/packages sh /tmp/apt.sh install ca-certificates curl build-essential "${cross[@]}"; \
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
# Every host a release ships, which dist/publish.py's HOSTS names and
# dist/test_publish.py holds this stage and the ones below to: each in
# every tarball, the CLI and CGI in every image, the FPM in `fpm` and
# `apache`, and the debugger in `cli` alone, as Mitchell ruled on #79. What
# `cli` alone holds is laid under /out/cli.
FROM toolchain AS build
ARG TARGETARCH
ARG NAME
ARG PHP_INI_DIR
# What a release names as the build's provider (php's --with-build-provider),
# none where it is empty; and the build date php_build_date renders.
ARG BUILD_PROVIDER
ARG SOURCE_DATE_EPOCH
# Thin LTO links each host in one process that a 4 GiB VM holds one of at a
# time; more jobs link several at once there and the kernel kills one.
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
# turns php_bundle's `jit` on by default, and php-cgi and phpdbg share the
# bundle's features with them on this one cargo line, as the phpt lane
# builds the four; it stays off until opcache.jit asks, as php's does, and
# tracing is experimental (ADR-0017).
RUN --mount=type=cache,id=${CACHE}-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=${CACHE}-target-${TARGETARCH},target=/src/target \
    . /usr/local/share/dax-target.env; \
    cargo build --release --locked \
        -p php_sapi_cli -p php_sapi_cgi -p php_sapi_fpm -p php_sapi_phpdbg \
        --bin "$NAME" --bin "$NAME-cgi" --bin "$NAME-fpm" --bin "$NAME-dbg"; \
    release="target/$CARGO_BUILD_TARGET/release"; \
    install -D -m 0755 -t /out/usr/local/bin "$release/$NAME" "$release/$NAME-cgi"; \
    install -D -m 0755 -t /out/usr/local/sbin "$release/$NAME-fpm"; \
    install -D -m 0755 -t /out/cli/usr/local/bin "$release/$NAME-dbg"
# The Ubuntu packages the hosts' shared libraries come from, which the base
# stage installs and the binaries list: every library they need,
# directly or through another, read from the ELF files, since ldd cannot
# load a binary of another architecture.
RUN . /usr/local/share/dax-target.env; \
    dir="/usr/lib/$TRIPLET"; \
    todo=(/out/usr/local/bin/* /out/usr/local/sbin/* /out/cli/usr/local/bin/*); \
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
COPY --from=build /out/cli/usr/local/bin/ /bin/
COPY --from=build /out/usr/local/sbin/ /sbin/
COPY --from=build /out/runtime-packages.txt /
COPY --from=ini /ini/ /etc/

# --- php-fpm.conf.default and php-fpm.d/www.conf.default -----------------
# php-src's FPM templates at the same commit, each SHA-256 held, rendered as
# configure renders them for php's official fpm image, where `make install`
# puts them under /usr/local/etc.
FROM inputs AS fpm-config
RUN python3 /opt/dist/inputs.py fpm /fpm

# --- what the three images share ------------------------------------------
# What every variant of php's official image holds, from docker-library/php
# at 3b25f4162a9bd144d3f447d03cb8fe16c0d2c9cc (Dockerfile-linux.template and
# apply-templates.sh): $PHP_INI_DIR with conf.d in it, /var/www/html open to
# any user, the docker-php-* scripts and docker-php-entrypoint as the
# entrypoint, rendered for the CLI. Here with the CLI and CGI hosts, the
# packages the libraries of every host the build lays come from, and the
# compat directory.
FROM ${UBUNTU} AS base
ARG PHP_INI_DIR
ARG NAME
ARG COMPAT
# The version php's image sets, which a Dockerfile written for it reads in
# its own RUN steps: the pinned php's, so a plain build sets it. xtask's
# tests hold this literal to oracle.toml's version, and the build fails
# below where the built php says another.
ARG PHP_VERSION=8.5.12
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ENV PHP_INI_DIR=${PHP_INI_DIR} \
    PHP_VERSION=${PHP_VERSION}
COPY --from=build /out/runtime-packages.txt /tmp/runtime-packages.txt
RUN --mount=type=bind,source=dist/apt.sh,target=/tmp/apt.sh \
    sh /tmp/apt.sh update; \
    xargs -a /tmp/runtime-packages.txt sh /tmp/apt.sh install ca-certificates curl xz-utils; \
    rm -rf /var/lib/apt/lists/* /tmp/runtime-packages.txt
# The template as written.
RUN mkdir -p "$PHP_INI_DIR/conf.d"; \
    [ ! -d /var/www/html ]; \
    mkdir -p /var/www/html; \
    chown www-data:www-data /var/www/html; \
    chmod 1777 /var/www/html
COPY --from=build /out/usr/local/bin/ /usr/local/bin/
COPY --from=ini /ini/ ${PHP_INI_DIR}/
COPY dist/image/bin/ /usr/local/bin/
# The images' own scripts run the hosts by Lela's names (ADR-0016), and a
# Dockerfile written for php's image that runs `php` finds it once the
# compat directory is first on its PATH.
RUN --mount=type=bind,source=dist/image/compat.sh,target=/tmp/compat.sh \
    sh /tmp/compat.sh; \
    cd /usr/local/bin; \
    ln -s docker-php-ext-install docker-php-ext-enable; \
    for name in pear peardev phpize php-config; do ln -s pecl "$name"; done; \
    "$NAME" -v; \
    built="$("$NAME" -r 'echo PHP_VERSION;')"; \
    [ "$built" = "$PHP_VERSION" ] || { echo "PHP_VERSION is $PHP_VERSION, and the built $NAME says $built" >&2; exit 1; }
ENTRYPOINT ["docker-php-entrypoint"]

# --- the drop-in for php's cli image ---------------------------------------
# With the debugger host, which a release carries in this image alone (#79).
FROM base AS cli
ARG NAME
ARG COMPAT
COPY --from=build /out/cli/usr/local/bin/ /usr/local/bin/
RUN --mount=type=bind,source=dist/image/compat.sh,target=/tmp/compat.sh \
    sh /tmp/compat.sh
CMD ["lela", "-a"]

# --- the drop-in for php's fpm image ---------------------------------------
FROM base AS fpm
ARG NAME
ARG COMPAT
COPY --from=build /out/usr/local/sbin/ /usr/local/sbin/
COPY --from=fpm-config /fpm/ /usr/local/etc/
RUN --mount=type=bind,source=dist/image/compat.sh,target=/tmp/compat.sh \
    sh /tmp/compat.sh; \
    sed -i -e "s! $NAME ! $NAME-fpm !g" /usr/local/bin/docker-php-entrypoint
WORKDIR /var/www/html
# From here to the end of the stage, the template as written, with
# `$NAME-fpm -t` after it.
RUN set -eux; \
    cd "${PHP_INI_DIR%/php}"; \
    cp -v php-fpm.conf.default php-fpm.conf; \
    cp -v php-fpm.d/www.conf.default php-fpm.d/www.conf; \
    grep -E '^listen = 127.0.0.1:9000' php-fpm.d/www.conf; \
    sed -ri 's/^(listen = 127.0.0.1:9000)/;\1/' php-fpm.d/www.conf; \
    grep -E '^;listen = 127.0.0.1:9000' php-fpm.d/www.conf; \
    { \
        echo '[global]'; \
        echo 'error_log = /proc/self/fd/2'; \
        echo; echo '; https://github.com/docker-library/php/pull/725#issuecomment-443540114'; echo 'log_limit = 8192'; \
        echo; \
        echo '[www]'; \
        echo '; php-fpm closes STDOUT on startup, so sending logs to /proc/self/fd/1 does not work.'; \
        echo '; https://bugs.php.net/bug.php?id=73886'; \
        echo 'access.log = /proc/self/fd/2'; \
        echo; \
        echo 'clear_env = no'; \
        echo; \
        echo '; Ensure worker stdout and stderr are sent to the main error log.'; \
        echo 'catch_workers_output = yes'; \
        echo 'decorate_workers_output = no'; \
        echo; \
        echo '; default listen address for easy override in later php-fpm.d/*.conf files'; \
        echo 'listen = 9000'; \
    } | tee php-fpm.d/docker.conf; \
    { \
        echo '[global]'; \
        echo 'daemonize = no'; \
        echo; \
        echo '; the [www] ini section below is for backwards compatibility and will be removed in 8.6+'; \
        echo '[www]'; \
    } | tee php-fpm.d/zz-docker.conf; \
    mkdir -p "$PHP_INI_DIR/conf.d"; \
    { \
        echo '; https://github.com/docker-library/php/issues/878#issuecomment-938595965'; \
        echo 'fastcgi.logging = Off'; \
    } > "$PHP_INI_DIR/conf.d/docker-fpm.ini"; \
    "$NAME-fpm" -t
STOPSIGNAL SIGQUIT
EXPOSE 9000
CMD ["lela-fpm"]

# --- the drop-in for php's apache image -----------------------------------
# Apache's prefork MPM in front of php-fpm over a unix socket, where php's
# image loads mod_php: the engine has no apache2handler yet.
FROM base AS apache
ARG NAME
ARG COMPAT
ENV APACHE_CONFDIR=/etc/apache2 \
    APACHE_ENVVARS=/etc/apache2/envvars
RUN --mount=type=bind,source=dist/apt.sh,target=/tmp/apt.sh \
    sh /tmp/apt.sh update; \
    sh /tmp/apt.sh install apache2; \
    rm -rf /var/lib/apt/lists/*
# From here to the next comment, php's official apache image as written.
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
    a2dismod -q mpm_event; a2enmod -q mpm_prefork
# Where php's image sets `SetHandler application/x-httpd-php`, the same
# files go to php-fpm; its socket's directory is open to any user, as
# Apache's run directories are above, so the image runs as one.
COPY dist/apache/docker-php.conf /etc/apache2/conf-available/docker-php.conf
RUN a2enmod -q proxy_fcgi; a2enconf -q docker-php; \
    mkdir -p /run/php; chmod 1777 /run/php
COPY --from=build /out/usr/local/sbin/ /usr/local/sbin/
COPY dist/apache/php-fpm.conf /usr/local/etc/php-fpm.conf
COPY dist/apache/www.conf /usr/local/etc/php-fpm.d/www.conf
COPY dist/apache/bin/ /usr/local/bin/
RUN --mount=type=bind,source=dist/image/compat.sh,target=/tmp/compat.sh \
    sh /tmp/compat.sh; \
    sed -i -e "s! $NAME ! apache2-foreground !g" /usr/local/bin/docker-php-entrypoint; \
    "$NAME-fpm" -t; apache2ctl -t
STOPSIGNAL SIGWINCH
WORKDIR /var/www/html
EXPOSE 80
CMD ["apache2-foreground"]
