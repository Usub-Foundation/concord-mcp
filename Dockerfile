# concord-mcp hub image. Works with BuildKit and with the classic builder.
ARG RUST_IMAGE=rust:1.99-slim-trixie
ARG RUNTIME_IMAGE=debian:trixie-slim

FROM ${RUST_IMAGE} AS build
# Lower it on small machines: docker build --build-arg CARGO_BUILD_JOBS=2 .
ARG CARGO_BUILD_JOBS
WORKDIR /src
# 1. Dependencies only (cached until Cargo.toml / Cargo.lock change).
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && : > src/lib.rs \
    && cargo build --release --locked --bin concord ${CARGO_BUILD_JOBS:+--jobs $CARGO_BUILD_JOBS}
# 2. The hub itself (touch: make cargo see the real sources as newer than the stubs).
COPY migrations ./migrations
COPY ui ./ui
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked --bin concord ${CARGO_BUILD_JOBS:+--jobs $CARGO_BUILD_JOBS}

FROM ${RUNTIME_IMAGE}
# CA roots for HTTPS calls to task trackers.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin concord \
    && mkdir -p /data/invites && chown -R concord /data
COPY --from=build /src/target/release/concord /usr/local/bin/concord
USER concord
WORKDIR /data
VOLUME ["/data"]
ENV CONCORD_LISTEN=0.0.0.0:8787 \
    CONCORD_INVITES_DIR=/data/invites \
    RUST_LOG=info,sqlx=warn,rmcp=warn
EXPOSE 8787
HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=5 CMD ["concord", "healthcheck"]
ENTRYPOINT ["concord"]
CMD ["serve"]
