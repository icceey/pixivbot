# Contributing to PixivBot

Thank you for your interest in contributing to PixivBot! This document provides guidelines and instructions for setting up your development environment and submitting contributions.

## Development Environment Setup

### Prerequisites

- **Rust**: Ensure you have the latest stable version of Rust installed.
- **SQLite**: The project uses SQLite database.
- **Make**: The project uses a `Makefile` for common tasks.

### Getting Started

1. **Clone the repository**:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   ```

2. **Configure for Development**:
   Copy `config.toml.example` to `config.toml` and fill in the necessary tokens.

   ```bash
   cp config.toml.example config.toml
   ```

3. **Install Dependencies & Check**:
   Use the provided Makefile to ensure your environment is ready.

   ```bash
   make dev  # Runs the bot in development mode
   ```

## Workflow & Standards

### Code Style

- **Formatting**: We follow standard Rust formatting conventions. Use `rustfmt` to format your code.

  ```bash
  make fmt    # Formats the code
  ```

- **Linting**: We use `clippy` for linting. Ensure your code passes clippy checks (warnings are treated as errors in CI).

  ```bash
  make clippy # Runs clippy
  ```

### Development Commands

The project includes a `Makefile` to simplify development tasks:

- `make ci`: Run full CI suite (Format Check, Clippy, Cargo Check, Tests, Build). **Run this before pushing!**
- `make quick`: Run fast checks (Format Check, Clippy, Cargo Check).
- `make test`: Run unit tests.
- `make fix`: Automatically fix formatting and some clippy issues.
- `make watch`: continuously watch for changes and re-run the bot (requires `cargo-watch`).

### Branching & Commits

- Create a new branch for each feature or bug fix: `git checkout -b feature/my-new-feature`.
- Write clear and concise commit messages.

### Project Structure

- `src/bot/`: Telegram bot logic, command handling, and dispatching.
- `src/pixiv/`: High-level Pixiv business logic (downloading, caching).
- `src/pixiv_client/`: Low-level Pixiv API client implementation.
- `src/db/`: Database entities (SeaORM) and repository layer.
- `src/scheduler/`: Task scheduling engine.

## Database Migrations

We use `sea-orm-migration` for database schema management.

- Migrations are located in `migration/`.
- They run automatically on application startup.
- If you change the database schema, please create a new migration.

## Testing

- Add tests for new functionality, especially for logic that parses input or handles data transformation.
- Run tests using `make test`.

## Pull Request Process

1. Ensure your code compiles and passes all checks (`make ci`).
2. Update documentation (README.md) if you are changing command usage or configuration.
3. Open a Pull Request against the `master` branch.
4. Describe your changes detailedly in the PR description.

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
