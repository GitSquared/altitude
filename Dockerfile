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
RUN cargo build --release --locked

FROM runtime AS main
COPY --from=build /src/target/release/altitude /usr/local/bin/altitude
USER altitude

# The default target installs the latest stable release, or ALTITUDE_VERSION when set.
FROM runtime AS release
COPY --from=download /out/altitude /usr/local/bin/altitude
USER altitude
