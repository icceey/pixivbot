FROM rust:1-slim-bookworm AS builder

WORKDIR /app

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked && \
    cp target/release/pixivbot /app/pixivbot

FROM gcr.io/distroless/cc-debian12:nonroot

WORKDIR /app

COPY --from=builder /app/pixivbot /app/pixivbot

ENTRYPOINT ["/app/pixivbot"]