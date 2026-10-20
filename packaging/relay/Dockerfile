# syntax=docker/dockerfile:1
# tq-relay with the web app bundled (served from /usr/share/tq/web).
# Build from the repository root:
#   docker build -f packaging/relay/Dockerfile --build-arg TQ_GIT_SHA=$(git rev-parse --short=10 HEAD) -t tq-relay .
# Run:
#   docker run -p 8080:8080 -p 4443:4443/udp -e TQ_SHARED_SECRET=… tq-relay --hostnames relay.example.com

FROM oven/bun:1 AS web
WORKDIR /src/web
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile
COPY web/ ./
ARG TQ_GIT_SHA=unknown
RUN TQ_GIT_SHA=$TQ_GIT_SHA ./build.sh

FROM rust:1-bookworm AS relay
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY build build
COPY crates crates
ARG TQ_GIT_SHA=unknown
ENV TQ_GIT_SHA=$TQ_GIT_SHA
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p tq-relay && cp target/release/tq-relay /usr/local/bin/

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=relay /usr/local/bin/tq-relay /usr/local/bin/tq-relay
COPY --from=web /src/web/dist /usr/share/tq/web
ARG VERSION=dev
LABEL org.opencontainers.image.title="tq-relay" \
      org.opencontainers.image.description="tq relay: WebTransport fan-out for robot video and teleop, with the web app" \
      org.opencontainers.image.source="https://github.com/haffi96/tq" \
      org.opencontainers.image.version="$VERSION"
# IPv4 by default: containers often have IPv6 disabled, and [::] would not
# bind. Flags override these.
ENV TQ_QUIC=0.0.0.0:4443 \
    TQ_HTTP=0.0.0.0:8080 \
    TQ_WEB_DIR=/usr/share/tq/web
USER 65534:65534
EXPOSE 8080/tcp 4443/udp
ENTRYPOINT ["tq-relay"]
