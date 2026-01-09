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
        # Useful tools
        curl \
        git \
        ripgrep \
        fd-find \
        jq \
    && apt-get autoremove -y \
    && apt-get clean -y \
    && rm -rf /var/lib/apt/lists/*

# Install Rust components (rust-analyzer managed by VS Code extension)
RUN rustup component add \
    rustfmt \
    clippy

# Install cargo-binstall for fast binary installation
RUN curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash

# Install useful cargo tools via binstall (pre-compiled binaries, ~10x faster)
RUN cargo binstall -y --locked \
    cargo-watch \
    cargo-edit \
    cargo-expand \
    cargo-outdated \
    sea-orm-cli

# Create non-root user
ARG USERNAME=vscode
ARG USER_UID=1000
ARG USER_GID=$USER_UID

RUN groupadd --gid $USER_GID $USERNAME \
    && useradd --uid $USER_UID --gid $USER_GID -m $USERNAME \
    && apt-get update \
    && apt-get install -y sudo \
    && echo $USERNAME ALL=\(root\) NOPASSWD:ALL > /etc/sudoers.d/$USERNAME \
    && chmod 0440 /etc/sudoers.d/$USERNAME \
    && apt-get autoremove -y \
    && apt-get clean -y \
    && rm -rf /var/lib/apt/lists/*

# Set cargo home for caching
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:${PATH}"

# Ensure cargo directories have correct permissions
RUN mkdir -p ${CARGO_HOME}/registry \
    && chmod -R 777 ${CARGO_HOME}

USER $USERNAME
WORKDIR /workspaces/pixivbot
