# syntax=docker/dockerfile:1

# The Hearthsound server image: the `hearthsound` binary, its bundled
# first-party plugins, and the media tools it shells out to, running as a
# non-root user.
#
# Bootstrap keys are not baked in. Point the image at the container paths with
# environment variables (`HEARTHSOUND__PATHS__DATA=/data`,
# `HEARTHSOUND__PATHS__STORAGE=/storage`) or with a mounted config file; the
# declared volumes below are where those keys should point. `dev/compose.yml`
# shows the development form of both.

# The builder matches the toolchain pinned in `rust-toolchain.toml`. Queries are
# checked against the committed sqlx offline data, so no database is needed.
FROM rust:1.99-trixie AS builder

ENV SQLX_OFFLINE=true

WORKDIR /build

# Only the sources and the files the build reads. `.dockerignore` keeps the
# build context to the same set. `wit/` is not optional: the plugin host's
# generated bindings read the WebAssembly interface package at compile time.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo .cargo
COPY crates crates
COPY xtask xtask
COPY wit wit
COPY .sqlx .sqlx
COPY bundled-plugins.lock ./

RUN cargo build --release --locked -p hearthsound

# Downloads the pinned first-party plugins and verifies each digest before it is
# written; the runtime stage copies the result.
RUN cargo xtask fetch-plugins --out /out/plugins


# ffmpeg carries ffprobe, which the server shells out to for probing, tagging
# and transcoding; curl serves the health check; ca-certificates are needed for
# HTTPS to plugin repositories and backends.
FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# The service account, uid/gid 1000, matching the released packages.
RUN groupadd --gid 1000 hearthsound \
    && useradd --uid 1000 --gid 1000 --home-dir /home/hearthsound --create-home --shell /usr/sbin/nologin hearthsound

COPY --from=builder /build/target/release/hearthsound /usr/local/bin/hearthsound
COPY --from=builder /out/plugins/ /usr/share/hearthsound/plugins/

# The fetched archives are readable by the service account but not writable, and
# the two managed trees are owned by it.
RUN chmod 0644 /usr/share/hearthsound/plugins/*.hsplugin \
    && mkdir --parents /data /storage \
    && chown hearthsound:hearthsound /data /storage

USER hearthsound
WORKDIR /home/hearthsound

# `/data` holds the database, installed plugins, the secret key, artwork and
# backups; `/storage` holds the managed library, cache and staging trees.
VOLUME ["/data", "/storage"]
EXPOSE 4327

HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl --fail --silent --show-error http://127.0.0.1:4327/healthz || exit 1

# `serve` runs the server; `migrate`, `admin` and `plugins` are one-shot
# commands an operator runs through the same entrypoint.
ENTRYPOINT ["hearthsound"]
CMD ["serve"]
