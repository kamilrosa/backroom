# Backroom server image (amd64, arm64, armv7).
#
# Ready-made image:   docker pull ghcr.io/kamilrosa/backroom-server
# Build from source:  docker build -t ghcr.io/kamilrosa/backroom-server .
# For a Raspberry Pi: docker buildx build --platform linux/arm/v7 -t ghcr.io/kamilrosa/backroom-server --load .
#
# The binary is linked statically (musl), so the same file also runs outside Docker on any
# Linux of that architecture — `scripts/server-packages.sh` exports it for the releases.

FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY proto proto
COPY server server
# The server does not need the client (GUI + neural network) — drop it from
# the workspace so its dependencies are not downloaded.
# The musl target is derived from the build platform, e.g. armv7-unknown-linux-gnueabihf
# becomes armv7-unknown-linux-musleabihf.
RUN sed -i 's/^members = .*/members = ["proto", "server"]/' Cargo.toml \
 && TARGET=$(rustc -vV | sed -n 's/^host: //p' | sed 's/gnu/musl/') \
 && rustup target add "$TARGET" \
 && export "CC_$(echo "$TARGET" | tr - _)=musl-gcc" \
 && cargo build --release -p backroom-server --target "$TARGET" \
 && cp "target/$TARGET/release/backroom-server" /backroom-server \
 && strip /backroom-server \
 && mkdir /data-dir && chown 10001:10001 /data-dir

# Just the binary, for `docker buildx build --target binary --output ...`.
FROM scratch AS binary
COPY --from=build /backroom-server /

FROM scratch
COPY --from=build /backroom-server /backroom-server
# The server certificate must survive image updates — keep it in a volume.
COPY --from=build --chown=10001:10001 /data-dir /data
USER 10001:10001
VOLUME /data
ENV BACKROOM_DATA=/data \
    BACKROOM_BIND=0.0.0.0:7777
EXPOSE 7777/udp
LABEL org.opencontainers.image.source="https://github.com/kamilrosa/backroom" \
      org.opencontainers.image.description="Backroom voice chat server" \
      org.opencontainers.image.licenses="MIT"
ENTRYPOINT ["/backroom-server"]
