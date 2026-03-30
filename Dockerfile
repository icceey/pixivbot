FROM lukemathwalker/cargo-chef:latest-rust-1.91-slim-bookworm AS planner
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM lukemathwalker/cargo-chef:latest-rust-1.91-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libswresample-dev pkg-config \
    clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
ARG TARGETPLATFORM
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=registry-$TARGETPLATFORM,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json
COPY . .
ARG TARGETPLATFORM
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=registry-$TARGETPLATFORM,sharing=locked \
    cargo build --release --locked && \
    cp target/release/pixivbot /app/pixivbot

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    libavcodec59 libavformat59 libavutil57 libswscale6 libswresample4 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/pixivbot /app/pixivbot
ENTRYPOINT ["/app/pixivbot"]