# Development container for PixivBot
FROM rust:1.91-bookworm

# Install development dependencies
RUN apt-get update && export DEBIAN_FRONTEND=noninteractive \
    && apt-get -y install --no-install-recommends \
        # Build essentials
        build-essential \
        pkg-config \
        # SQLite support
        sqlite3 \
        libsqlite3-dev \
        # SSL/TLS support
        libssl-dev \
        ca-certificates \
        # Useful tools (curl is kept because it's needed for cargo-binstall in the next step)
        curl \
        ripgrep \
        fd-find \
        jq \
    && apt-get autoremove -y \
    && apt-get clean -y \
    && rm -rf /var/lib/apt/lists/*

# Install Rust components
RUN rustup component add \
    rustfmt \
    clippy

# Install cargo-binstall for fast binary installation
# Pin to a specific version to avoid supply-chain attacks
ARG CARGO_BINSTALL_VERSION=v1.10.16
RUN curl -L --proto '=https' --tlsv1.2 -sSf \
    "https://github.com/cargo-bins/cargo-binstall/releases/download/${CARGO_BINSTALL_VERSION}/cargo-binstall-x86_64-unknown-linux-musl.tgz" \
    -o /tmp/cargo-binstall.tgz \
    && tar -xzf /tmp/cargo-binstall.tgz -C /usr/local/cargo/bin \
    && rm /tmp/cargo-binstall.tgz

# Install useful cargo tools via binstall (pre-compiled binaries, ~10x faster)
RUN cargo binstall -y --locked \
    cargo-watch \
    cargo-edit \
    cargo-expand \
    cargo-outdated \
    sea-orm-cli

# Set cargo home for caching
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:${PATH}"

# Ensure cargo directories have correct permissions globally
RUN mkdir -p ${CARGO_HOME}/registry \
    && chmod -R 777 ${CARGO_HOME}