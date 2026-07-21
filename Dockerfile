# Lightning FM — Headless Artist Node
# Multi-stage build for minimal production image

FROM rust:1.86-slim AS builder
WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

# Cache dependencies — build with empty main first
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main(){}" > src/main.rs && \
    cargo build --release 2>/dev/null || true && \
    rm -rf src

# Build actual binary
COPY src/ ./src/
RUN cargo build --release

# Production image
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libssl3 wget && \
    rm -rf /var/lib/apt/lists/*

# Non-root user with data directory
RUN useradd -m -s /bin/false artist && mkdir -p /data && chown artist:artist /data

COPY --from=builder /app/target/release/lfm-artist-node /usr/local/bin/

USER artist

# LDK data directory
VOLUME /data

# Health check endpoint
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --retries=3 --start-period=60s \
    CMD wget -qO- http://localhost:8080/health || exit 1

CMD ["lfm-artist-node"]
