# PixivBot Copilot Instructions

## Project Overview

A Rust-based Telegram bot for subscribing to Pixiv artists and rankings. When artists publish new works, the bot automatically downloads and pushes images to subscribers.

**Tech Stack**: Rust 2021, teloxide (Telegram), SeaORM (SQLite), reqwest, tokio

## Architecture

```
src/
├── main.rs              # Entry point, initializes all components
├── config.rs            # Configuration loading (config.toml + env vars)
├── error.rs             # Unified error handling with AppError/AppResult
├── pixiv_client/        # Low-level Pixiv API (independent, no project deps)
│   ├── auth.rs          # OAuth refresh_token → access_token
│   ├── client.rs        # API calls: user_illusts, illust_ranking
│   └── models.rs        # Raw API response types
├── pixiv/               # Business layer (wraps pixiv_client)
│   ├── client.rs        # PixivClient with login/auth management
│   ├── downloader.rs    # Image download with hash-based caching
│   └── model.rs         # Domain models (Illust, User)
├── bot/                 # Telegram bot layer
│   ├── handler.rs       # Command dispatcher
│   ├── commands.rs      # /sub, /list, /unsub implementations
│   └── notifier.rs      # Send text/images to Telegram
├── scheduler/           # Background task engine
│   └── engine.rs        # Polls DB, executes tasks serially with rate limiting
└── db/                  # Database layer
    ├── entities/        # SeaORM entities (users, chats, tasks, subscriptions)
    └── repo.rs          # CRUD operations
```

## Key Patterns

### Error Handling
Use `AppResult<T>` and `AppError` from `src/error.rs`. Convert external errors with `From` implementations:
```rust
use crate::error::{AppError, AppResult};

fn example() -> AppResult<()> {
    // Errors auto-convert via From traits
    let db_result = repo.get_chat(id).await?;
    Ok(())
}
```

### Shared State
Components use `Arc<T>` for sharing, `Arc<RwLock<T>>` when mutable access needed:
```rust
let pixiv_client = Arc::new(RwLock::new(PixivClient::new(config)?));
let repo = Arc::new(Repo::new(db));
```

### Database Migrations
Located in `migration/src/`. Run automatically on startup via `migration::Migrator::up(&db, None).await?`.

### Image Caching
Downloaded images cached at `data/cache/{hash_prefix}/{filename}`. Downloader checks cache before downloading. Use `downloader.download(url)` for single, `downloader.download_all(&urls)` for batch.

### Telegram Media Groups
Max 10 images per media group. `Notifier::notify_with_images` auto-splits larger sets.

## Development Commands

```bash
make ci          # Run all CI checks (fmt, clippy, check, test, build)
make quick       # Fast checks without full build
make fmt         # Auto-format code
make clippy      # Run linter (warnings = errors, matches CI)
make dev         # cargo run
make fix         # Auto-fix formatting and clippy issues
```

CI uses `RUSTFLAGS=-Dwarnings` - all warnings are errors. Run `make ci` before committing.

## Adding Dependencies

**Always use `cargo add`**, never manually edit Cargo.toml:
```bash
cargo add serde --features derive
cargo add tokio --features full
```

## Code Conventions

- Use `tracing::{info, warn, error}` for logging (not `println!`)
- Mark unused but intentional public APIs with `#[allow(dead_code)]`
- Prefer `MarkdownV2` for Telegram message formatting
- Use `chrono` for datetime handling (with `serde` feature)
- Async functions should return `AppResult<T>` for consistency

## Testing

```bash
make test        # Run all tests
cargo test -p pixivbot -- --nocapture  # Run with output
```

Tests located alongside code in `#[cfg(test)]` modules.

## Before Committing

**Always run `make ci` before committing** to ensure code passes all CI checks:
```bash
make ci          # Runs: fmt-check → clippy → check → test → build
```

This matches the exact checks in `.github/workflows/ci.yml`. Fix any issues before pushing.

## Configuration

Copy `config.toml.example` → `config.toml`. Key sections:
- `[telegram]` - bot_token, owner_id
- `[pixiv]` - refresh_token
- `[database]` - SQLite URL
- `[scheduler]` - polling intervals
- `[content]` - sensitive_tags list

Environment variables override config: `APP_TELEGRAM__BOT_TOKEN`, `APP_PIXIV__REFRESH_TOKEN`
