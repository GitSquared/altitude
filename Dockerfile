# Build with BuildKit: only the selected target and its dependencies run.
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 libstdc++6 curl git openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && (useradd --uid 1000 --create-home altitude || useradd --create-home altitude)
ENV HOME=/home/altitude
WORKDIR /home/altitude
ENTRYPOINT ["altitude"]
CMD ["serve", "--listen", "0.0.0.0:25848"]

FROM runtime AS download
ARG ALTITUDE_VERSION
COPY install.sh /tmp/install.sh
RUN ALTITUDE_INSTALL_DIR=/out sh /tmp/install.sh

# Pre-release development builds the checked-out source; no Rust is needed on the host.
FROM rust:bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev cmake clang \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# The compiled graph and the crate registry are kept between builds, so a rebuild
# recompiles this crate and what changed rather than every dependency. They live in
# cache mounts, which never reach the image, so the binary is copied out to a path
# that does.
ENV CARGO_TARGET_DIR=/cargo-target
RUN --mount=type=cache,target=/cargo-target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --release --locked \
    && install -m 0755 /cargo-target/release/altitude /usr/local/bin/altitude-built

FROM runtime AS main
COPY --from=build /usr/local/bin/altitude-built /usr/local/bin/altitude
USER altitude

# The default target installs the latest stable release, or ALTITUDE_VERSION when set.
FROM runtime AS release
COPY --from=download /out/altitude /usr/local/bin/altitude
USER altitude

# A worker is a host a remote provisions for one task, and the container is the
# sandbox: its base image is a task's base image, so it starts from the same Node
# image the microVM workspace does rather than from the minimal runtime above. The
# package list is the workspace's, so a task finds the same tools on either; what a
# host must carry, and how to bake the layers a start would otherwise rebuild, is in
# `altitude serve worker --help`.
#
# ALTITUDE_WORKER_ENVIRONMENT has to name the image, and the value it names is the one
# the layers were baked under: build with the same value you passed to
# `altitude serve environment`, or the markers will not be found.
FROM node:22-bookworm-slim AS worker
ARG ALTITUDE_WORKER_ENVIRONMENT
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash build-essential ca-certificates clang cmake curl gh git gnupg nfs-common \
        jq libssl-dev openssh-client perl pkg-config python3 ripgrep \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g --no-fund --no-audit --no-progress @tobilu/qmd@2 \
    && (useradd --uid 1000 --create-home altitude || useradd --create-home altitude)
ENV ALTITUDE_WORKER_ENVIRONMENT=${ALTITUDE_WORKER_ENVIRONMENT}
ENV HOME=/home/altitude
WORKDIR /home/altitude
COPY --from=download /out/altitude /usr/local/bin/altitude
USER altitude
# The provision script supplies the arguments; run with none to read the contract.
ENTRYPOINT ["altitude"]
CMD ["serve", "worker", "--help"]
