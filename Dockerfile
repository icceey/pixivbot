# Build stage - use slim Debian for smaller base
FROM rust:1-slim-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy source code
COPY . .

# Build release binary
RUN cargo build --release --locked

# Runtime stage - use distroless for minimal size
FROM gcr.io/distroless/cc-debian12:nonroot

# Set working directory
WORKDIR /app

# Copy the binary from builder
COPY --from=builder /app/target/release/pixivbot /app/pixivbot

# Set entrypoint
ENTRYPOINT ["/app/pixivbot"]
