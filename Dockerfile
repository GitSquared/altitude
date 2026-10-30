# Build with BuildKit: only the selected target and its dependencies run.
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 libstdc++6 curl git openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home altitude
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
