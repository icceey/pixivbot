FROM rust:1-slim-bookworm AS builder

WORKDIR /app

COPY . .

ARG TARGETPLATFORM

RUN --mount=type=cache,target=/usr/local/cargo/registry,id=registry-$TARGETPLATFORM,sharing=locked \
    --mount=type=cache,target=/app/target,id=target-$TARGETPLATFORM,sharing=locked \
    cargo build --release --locked && \
    cp target/release/pixivbot /app/pixivbot

FROM gcr.io/distroless/cc-debian12:nonroot

WORKDIR /app

COPY --from=builder /app/pixivbot /app/pixivbot

ENTRYPOINT ["/app/pixivbot"]