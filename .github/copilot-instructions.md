# PixivBot Copilot Instructions

## Project Overview

A Rust-based Telegram bot for subscribing to Pixiv artists and rankings. When artists publish new works, the bot automatically downloads and pushes images to subscribers. Also supports detecting Pixiv links in messages and responding accordingly.

**Tech Stack**: Rust 2021, teloxide (Telegram), SeaORM (SQLite), reqwest, tokio, regex

## Architecture

```
src/
├── main.rs              # Entry point, initializes all components
├── config.rs            # Configuration loading (config.toml + env vars)
├── pixiv_client/        # Low-level Pixiv API (independent, no project deps)
│   ├── auth.rs          # OAuth refresh_token → access_token
│   ├── client.rs        # API calls: user_illusts, illust_ranking, user_detail, illust_detail
│   ├── models.rs        # Raw API response types
│   └── error.rs         # Custom Error type for API errors
├── pixiv/               # Business layer (wraps pixiv_client)
│   ├── client.rs        # PixivClient with login/auth management
│   ├── downloader.rs    # Image download with hash-based caching
│   └── model.rs         # Domain models (Illust, User, RankingMode)
├── bot/                 # Telegram bot layer
│   ├── mod.rs           # Dispatcher setup, handler tree building
│   ├── handler.rs       # BotHandler: commands + message handling
│   ├── commands.rs      # Command enum definitions
│   ├── link_handler.rs  # Pixiv URL parsing, @mention detection
│   └── notifier.rs      # Send text/images to Telegram
├── scheduler/           # Background task engine
│   └── engine.rs        # Polls DB, executes tasks serially with randomized intervals
├── cache/               # File-based cache system
│   └── mod.rs           # FileCacheManager with hash-bucketing and auto-cleanup
├── utils/               # Utility functions
│   ├── markdown.rs      # MarkdownV2 escaping
│   └── html.rs          # HTML formatting, tag processing
└── db/                  # Database layer
    ├── entities/        # SeaORM entities (users, chats, tasks, subscriptions)
    ├── types/           # Custom DB types (TaskType, UserRole, TagFilter, Tags)
    ├── repo.rs          # CRUD operations
    └── mod.rs           # Connection management
```

## Key Patterns

### Error Handling
Use `anyhow::Result<T>` for all fallible functions. Use `.context()` to add context to errors:
```rust
use anyhow::{Context, Result};

async fn example() -> Result<()> {
    let chat = repo.get_chat(id).await.context("Failed to get chat")?;
    Ok(())
}
```

When logging errors, use `{:#}` format specifier to show the full error chain:
```rust
error!("Failed to process: {:#}", e);
```

The `pixiv_client/` module has its own `Error` type for API-specific errors, but these are automatically converted to anyhow errors when propagated up.

### Shared State
Components use `Arc<T>` for sharing, `Arc<RwLock<T>>` when mutable access needed:
```rust
let pixiv_client = Arc::new(RwLock::new(PixivClient::new(config)?));
let repo = Arc::new(Repo::new(db));
```

### Database Migrations
Located in `migration/src/`. Run automatically on startup via `migration::Migrator::up(&db, None).await?`.

### Image Caching & Background Cleanup
The `FileCacheManager` (`cache/mod.rs`) provides:
- **Hash-bucketed storage**: Files stored at `data/cache/{hash_prefix}/{filename}` (256 buckets: `00`-`ff`)
- **Cache-before-download**: Always call `cache.get(url)` before downloading
- **Automatic cleanup**: Background task runs every 24 hours, deleting files older than `cache_retention_days`
- **Lifecycle**: Cleanup task spawned in `FileCacheManager::new()`, runs for lifetime of cache manager

```rust
// In downloader.rs
pub async fn download(&self, url: &str) -> Result<PathBuf> {
    // Check cache first
    if let Some(path) = self.cache.get(url).await {
        return Ok(path);
    }
    // Download and save to cache
    let bytes = self.http_client.get(url)
        .header("Referer", "https://app-api.pixiv.net/")
        .send().await?...;
    self.cache.save(url, &bytes).await
}
```

Use `downloader.download(url)` for single, `downloader.download_all(&urls)` for batch with partial failure tolerance.

### Telegram Media Groups
Max 10 images per media group. `Notifier::notify_with_images` auto-splits larger sets into multiple groups.

### Teloxide Dispatcher Pattern
The bot uses teloxide's `Dispatcher` with a handler tree:
```rust
// In bot/mod.rs
fn build_handler_tree() {
    let command_handler = Update::filter_message()
        .filter_command::<Command>()
        .endpoint(handle_command);

    let message_handler = Update::filter_message()
        .endpoint(handle_message);

    dptree::entry()
        .branch(command_handler)
        .branch(message_handler)
}
```

### Pixiv Link Detection
The bot detects Pixiv links in messages via `link_handler.rs`:
- **Artwork links** (`https://pixiv.net/artworks/xxx`): Push images immediately
- **User links** (`https://pixiv.net/users/xxx`): Subscribe to the author
- In groups, the bot only responds when **@mentioned**
- Uses `LazyLock<Regex>` for efficient pattern matching

```rust
// Parse links from message text
let links = parse_pixiv_links(text);  // Returns Vec<PixivLink>

// Check if bot is @mentioned (for groups)
let mentioned = is_bot_mentioned(text, entities, bot_username);
```

### Sensitive Tags Configuration
Sensitive tags are loaded from `config.content.sensitive_tags` and passed through the component chain:
```rust
// main.rs → bot::run → BotHandler
let sensitive_tags = config.content.sensitive_tags.clone();
bot::run(..., sensitive_tags).await;
```

Use `self.sensitive_tags` in BotHandler, NOT hardcoded constants.

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
cargo add regex  # Already added for link parsing
```

## Code Conventions

- Use `tracing::{info, warn, error}` for logging (not `println!`)
- Mark unused but intentional public APIs with `#[allow(dead_code)]`
- Prefer `MarkdownV2` for Telegram message formatting
- Use `chrono` for datetime handling (with `serde` feature)
- Async functions should return `anyhow::Result<T>` for consistency
- Use `.context()` to add meaningful error context
- Use `{:#}` format specifier when logging errors to show full error chain
- Use `LazyLock<Regex>` for compile-once regex patterns
- Derive `Clone` for types that need to be shared across async contexts

## Bot Command Filtering

Commands use `filter_command::<Command>()` in the Dispatcher. For group @mention requirement on commands, use `filter_mention_command::<Command>()`:
```rust
// Commands work in any chat
Update::filter_message().filter_command::<Command>()

// Commands require @mention in groups
Update::filter_message().filter_mention_command::<Command>()
```

## User Roles

Three roles defined in `db/entities/role.rs`:
- `Owner`: Full access, set via `config.telegram.owner_id`
- `Admin`: Can enable/disable chats, set by owner via `/setadmin`
- `User`: Standard user, basic commands only

Role checks: `user_role.is_owner()`, `user_role.is_admin()`

## Testing

```bash
make test        # Run all tests
cargo test -p pixivbot -- --nocapture  # Run with output
```

Tests located alongside code in `#[cfg(test)]` modules. See `link_handler.rs` for regex test examples.

## Before Committing

**Always run `make ci` before committing** to ensure code passes all CI checks:
```bash
make ci          # Runs: fmt-check → clippy → check → test → build
```

This matches the exact checks in `.github/workflows/ci.yml`. Fix any issues before pushing.

## Configuration

Copy `config.toml.example` → `config.toml`. Key sections:
- `[telegram]` - bot_token, owner_id, bot_mode (public/private)
- `[pixiv]` - refresh_token
- `[database]` - SQLite URL (default: `sqlite:data/pixivbot.db?mode=rwc`)
- `[logging]` - level (info/debug/warn), dir (default: `data/logs`)
- `[scheduler]` - tick_interval_sec (default: 30), min/max_task_interval_sec (2-3 hours), cache_retention_days (default: 7), cache_dir (default: `data/cache`)
- `[content]` - sensitive_tags list (e.g., ["R-18", "R-18G", "NSFW"])

Environment variables override config with prefix `PIX` and double underscores: `PIX__TELEGRAM__BOT_TOKEN`, `PIX__PIXIV__REFRESH_TOKEN`

## Bot Modes

- `private`: New chats disabled by default, must be enabled by admin
- `public`: New chats enabled by default

## Feature Implementation Checklist

When adding new features:
1. **Understand before coding** - Read and fully understand existing logic before making changes
2. Design a reasonable solution, then implement
3. Read config from `config.rs` if needed
4. Pass config values through component chain (don't use globals)
5. Respect chat enabled/disabled status
6. In groups, check for @mention when appropriate
7. Use `MarkdownV2` for formatted messages (escape with `utils::markdown::escape`)
8. Add tests in `#[cfg(test)]` modules
9. **Review your changes** - Carefully review all modifications before committing
10. Run `make ci` before committing

## Crate Usage Guidelines

**When unsure about crate APIs, ALWAYS verify first:**

```bash
# Generate and open local documentation
cargo doc --open

# Generate docs for a specific crate
cargo doc -p teloxide --open
```

Or check [docs.rs](https://docs.rs) for published crate documentation.

**Never guess API usage** - incorrect assumptions about crate behavior can lead to subtle bugs. Common crates to verify:
- `teloxide` - Telegram bot framework, check dispatcher patterns and filter methods
- `sea-orm` - Database ORM, verify entity relationships and query builders
- `tokio` - Async runtime, understand spawn/select/channel patterns
- `regex` - Pattern matching, test patterns before using
