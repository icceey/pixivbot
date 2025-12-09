FROM lukemathwalker/cargo-chef:latest-rust-1-slim-bookworm AS planner
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM lukemathwalker/cargo-chef:latest-rust-1-slim-bookworm AS builder
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

FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /app/pixivbot /app/pixivbot
ENTRYPOINT ["/app/pixivbot"]