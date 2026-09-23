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

EXPOSE 6080 6081 19011

ENTRYPOINT ["/app/multi_buy_service"]
CMD ["-c", "/app/config/settings.toml", "server"]
