# Multi-stage build for Rust application
FROM rust:bookworm AS base

RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

FROM base AS builder

# Create app directory
WORKDIR /app

# Copy manifests
COPY rust-toolchain.toml ./rust-toolchain.toml
COPY Cargo.toml ./Cargo.toml
COPY Cargo.lock ./Cargo.lock

# Copy source code
COPY src ./src

# Build release binary
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim AS runner

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/target/release/multi_buy_service /app/multi_buy_service

# Copy default settings
COPY pkg/settings-template.toml /app/config/settings.toml

# Deny-list changes made through the admin API are persisted here. Mount a
# volume over it (or bind-mount a host path) to keep them across container
# replacement.
ENV MB__DENY_LIST_STORE=/app/data/deny-list.json
# Per-hotspot stats, saved every minute and on shutdown.
ENV MB__HOTSPOT_STORE=/app/data/hotspots.json
# Names given to HPR addresses on the dashboard.
ENV MB__HPR_LABEL_STORE=/app/data/hpr-labels.json
VOLUME /app/data

EXPOSE 6080 6081 19011

ENTRYPOINT ["/app/multi_buy_service"]
CMD ["-c", "/app/config/settings.toml", "server"]
