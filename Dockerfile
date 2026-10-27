# syntax=docker/dockerfile:1

# The Hearthsound server image: the `hearthsound` binary with its embedded web
# client and the mandatory first-party plugins compiled into it, and the media
# tools it shells out to, running as a non-root user.
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
COPY bundled-plugins.lock web-client.lock ./

# Downloads the web client pinned in `web-client.lock` and verifies its digest
# before extracting it into `target/web-client/`, which the server crate embeds.
RUN cargo xtask fetch-web-client

# Downloads the pinned first-party plugins into `target/bundled-plugins/` and
# verifies each digest before it is written. The server crate's build script
# embeds every archive `bundled-plugins.lock` marks `mandatory = true`, so this
# has to run before `cargo build`.
RUN cargo xtask fetch-plugins

RUN cargo build --release --locked -p hearthsound


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

# The two managed trees are owned by the service account.
RUN mkdir --parents /data /storage \
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


# An opt-in variant that also carries yt-dlp, for installations whose
# acquisition plugins fetch tracks from it. Build it with
# `docker build --target runtime-ytdlp .`; the plain `docker build .` below
# still produces the runtime image without it.
#
# yt-dlp needs Deno for the JavaScript challenges of some sites and ffmpeg
# (already in the runtime stage) for extraction. Python is only its launcher, so
# it lives in a virtual environment outside the service account's home.
#
# Docker's `ENV` instruction drops environment names containing a hyphen without
# an error, so `HEARTHSOUND__TOOLS__YT-DLP__PATH` cannot be baked into the image.
# The executable is instead left at `/opt/yt-dlp/bin/yt-dlp` and at the
# `/usr/local/bin/yt-dlp` symlink; an administrator sets `tools.yt-dlp.path` to
# either path in the admin settings. Passing
# `-e HEARTHSOUND__TOOLS__YT-DLP__PATH=/opt/yt-dlp/bin/yt-dlp` (or the same entry
# under `environment:` in Compose) also works, because the runtime environment
# accepts the name.
FROM runtime AS runtime-ytdlp

ARG TARGETARCH

USER root

RUN apt-get update \
    && apt-get install --yes --no-install-recommends python3 python3-venv unzip \
    && rm -rf /var/lib/apt/lists/*

# The pinned yt-dlp release with its default dependencies, which include the EJS
# challenge solver the current site extractors use.
RUN python3 -m venv /opt/yt-dlp \
    && /opt/yt-dlp/bin/pip install --no-cache-dir "yt-dlp[default]==2026.10.21"

# Deno, the JavaScript runtime yt-dlp starts for those challenges, from the
# pinned release built for the architecture being built.
ARG DENO_VERSION=2.9.8

RUN case "$TARGETARCH" in \
        amd64) deno_arch=x86_64 ;; \
        arm64) deno_arch=aarch64 ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl --fail --silent --show-error --location --output /tmp/deno.zip \
        "https://github.com/denoland/deno/releases/download/v${DENO_VERSION}/deno-${deno_arch}-unknown-linux-gnu.zip" \
    && unzip -q /tmp/deno.zip -d /usr/local/bin \
    && rm /tmp/deno.zip \
    && chmod 0755 /usr/local/bin/deno

RUN ln --symbolic /opt/yt-dlp/bin/yt-dlp /usr/local/bin/yt-dlp

USER hearthsound


# Docker builds the last stage, so this final stage keeps `docker build .`
# identical to the runtime image above: the yt-dlp variant is only built when it
# is named with `--target runtime-ytdlp`.
FROM runtime
