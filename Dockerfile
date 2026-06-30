# Multi-stage build for the chip server.
FROM rust:1.96-bookworm AS builder

# protoc is needed to compile the gRPC definitions.
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
# sqlx::migrate! embeds the migrations into the binary at build time, so the
# runtime image does not need the migrations/ directory.
RUN cargo build --release -p chip-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/chip-server /usr/local/bin/chip-server

ENV CHIP_BIND=0.0.0.0:8080 \
    CHIP_OBJECT_STORE=local:///data/repos
EXPOSE 8080 2222
VOLUME ["/data"]

# Dokploy/compose can override env (DATABASE_URL, CHIP_SECRET, CHIP_BASE_URL, ...).
CMD ["chip-server"]
