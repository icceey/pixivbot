# PixivBot Copilot Instructions

## Project Overview

A Rust-based Telegram bot for subscribing to Pixiv artists and rankings. When artists publish new works, the bot automatically downloads and pushes images to subscribers. Also supports detecting Pixiv links in messages and responding accordingly.

**Tech Stack**: Rust 2021, teloxide (Telegram), SeaORM (SQLite), reqwest, tokio, regex

**Architecture Philosophy**: The project follows a layered architecture with clear separation between Pixiv API access (`pixiv_client/`), business logic (`pixiv/`, `scheduler/`), and presentation (`bot/`). Components communicate via `Arc`-wrapped shared state.

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

### Error Handling & User-Facing Messages

**CRITICAL SECURITY PATTERN**: Never expose raw errors to users. Always separate internal logging from user-facing messages:

```rust
// ❌ WRONG - Exposes internal error details
bot.send_message(chat_id, format!("❌ 失败: {:#}", e)).await?;

// ✅ CORRECT - Log details, send friendly message
error!("Failed to process request: {:#}", e);
bot.send_message(chat_id, "❌ 操作失败").await?;
```

**Error Handling Rules**:
1. Use `anyhow::Result<T>` for all fallible functions
2. Add context with `.context()` for debugging
3. Log errors with `{:#}` to show full error chain in logs
4. Send generic, user-friendly messages to Telegram (no technical details)
5. The `pixiv_client/` module has its own `Error` type, auto-converts to anyhow

**Example Pattern**:
```rust
match pixiv.get_illust_detail(illust_id).await {
    Ok(illust) => illust,
    Err(e) => {
        error!("Failed to get illust {}: {:#}", illust_id, e);  // Detailed logging
        bot.send_message(chat_id, format!("❌ 获取作品 {} 失败", illust_id)).await?;  // Generic user message
        return Ok(());
    }
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

### Telegram Media Groups & Batch Sending

**Key Constants**: `MAX_PER_GROUP = 10` (defined in both `notifier.rs` and `scheduler/engine.rs`)

**Batch Caption Pattern**: The notifier uses a sophisticated caption system:
- First batch shows original caption
- Subsequent batches show `(continued 2/3)` format
- This pattern MUST be replicated in retry logic for consistency

```rust
// In notifier.rs - normal batch sending
if batch_idx == 0 {
    base_cap.map(|s| s.to_string())  // First batch: full caption
} else {
    Some(format!("\\(continued {}/{}\\)", batch_idx + 1, total_batches))  // Later batches
}
```

**Retry Caption Pattern** (in `scheduler/engine.rs`):
```rust
// Calculate batch numbers for consistency with normal sends
const MAX_PER_GROUP: usize = 10;
let total_batches = total_pages.div_ceil(MAX_PER_GROUP);
let current_batch = (already_sent_pages.len() / MAX_PER_GROUP) + 1;

format!("🎨 {} \\(continued {}/{}\\)\nby *{}*\n\n🔗 [来源](...){}", 
    title, current_batch, total_batches, author, id, tags)
```

**Critical**: Retry messages MUST include tags (use `tag::format_tags_escaped(illust)`) to match normal send format.

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
## Development Commands

```bash
make ci          # Run all CI checks (fmt, clippy, check, test, build) - MANDATORY before commit
make quick       # Fast checks without full build (fmt-check, clippy, check)
make fmt         # Auto-format code
make clippy      # Run linter (warnings = errors, matches CI)
make dev         # cargo run
make fix         # Auto-fix formatting and clippy issues
make watch       # Watch for changes and rebuild (requires cargo-watch)
```

**CRITICAL**: CI uses `RUSTFLAGS=-Dwarnings` - all warnings are treated as errors. Always run `make ci` before committing to catch issues locally that would fail in CI.

**Common Clippy Patterns to Watch**:
- Use `.div_ceil()` instead of `(a + b - 1) / b` for ceiling division
- Avoid manual `Vec` construction when ranges can be used
- Prefer `matches!()` macro for enum pattern matching
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

## Adding Dependencies

**Always use `cargo add`**, never manually edit Cargo.toml:
```bash
cargo add serde --features derive
cargo add tokio --features full
cargo add regex  # Already added for link parsing
```
## Code Conventions

**Logging & Output**:
- Use `tracing::{info, warn, error}` for logging (NEVER `println!`)
- Error logs: `error!("Operation failed: {:#}", e)` - shows full error chain
- Info logs: `info!("Processing {} items", count)` - structured data preferred
- User messages: Always escape with `markdown::escape()` for MarkdownV2

**Type Safety & Async**:
- All async functions return `anyhow::Result<T>` for consistency
- Use `.context("Meaningful description")` on all `?` operations
- Derive `Clone` for types shared across async contexts
- Use `Arc<T>` for immutable shared state, `Arc<RwLock<T>>` for mutable

**Performance Patterns**:
- Use `LazyLock<Regex>` for compile-once regex patterns (see `link_handler.rs`)
- Prefer `const` for compile-time constants (`MAX_PER_GROUP`, batch sizes)
- Use `.div_ceil()` instead of manual ceiling division

**Telegram Formatting**:
- Always use `ParseMode::MarkdownV2` (not Markdown or HTML)
- Escape all dynamic text: `markdown::escape(&user_input)`
- Special chars needing escape: `_*[]()~`>#+-=|{}.!`
- Format patterns: `*bold*`, `_italic_`, `` `code` ``, `[link](url)`

**Intentional Patterns**:
- Mark unused public APIs with `#[allow(dead_code)]` (not `#[allow(unused)]`)
- Use `#[allow(clippy::too_many_arguments)]` only when unavoidable (e.g., `SchedulerEngine::new`)

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
1. **Understand before coding** - Read existing logic in related files completely before changes
2. **Design then implement** - Sketch solution architecture, identify affected components
3. **Configuration** - Load from `config.rs`, pass through component chain (NO globals)
4. **Authorization** - Respect chat enabled/disabled status and user roles
5. **Group behavior** - Check for @mention in groups when appropriate
6. **Error handling** - Log details with `error!()`, send generic messages to users
7. **User messages** - Use `MarkdownV2`, escape all dynamic content
8. **Consistency** - Match existing patterns (e.g., batch caption format in retries)
9. **Testing** - Add tests in `#[cfg(test)]` modules (see `link_handler.rs`)
10. **Pre-commit** - Run `make ci` to catch all issues locally

**Common Integration Points**:
- Scheduler ↔ Notifier: Use `BatchSendResult` to track partial sends
- Handler ↔ Repo: Always use `.context()` for database operations
- Notifier ↔ Downloader: Check cache first with `cache.get()`, then download
- Bot ↔ Pixiv: Wrap client in `Arc<RwLock<>>` for safe concurrent access

## Scheduler Architecture & Retry Logic

**Orchestrator-Dispatcher-Worker Pattern** (see `scheduler/engine.rs`):

```
execute_author_task (Orchestrator)
  ├─ Fetches illusts from Pixiv once
  ├─ Iterates all subscriptions for task
  └─ For each subscription:
      └─ process_single_author_sub (Dispatcher)
          ├─ Checks for pending retry → handle_existing_pending (Worker)
          └─ Processes new illusts → handle_new_illusts (Worker)
```

**Retry State Machine**:
- `PushResult::Success` → Update `latest_illust_id`, clear `pending_illust`
- `PushResult::Partial` → Store `PendingIllust{sent_pages, retry_count++}`
- `PushResult::Failure` → Increment `retry_count`, retry next tick OR abandon if `max_retry_count` reached

**Key Constants**:
- `MAX_PER_GROUP = 10` - Pages per batch (MUST match `notifier.rs`)
- `max_retry_count` - Configurable retry limit (default: 3)
- Polling intervals: Randomized between `min_task_interval_sec` and `max_task_interval_sec`

**Critical Pattern**: When resuming partial sends, calculate batch numbers to match normal flow:
```rust
let total_batches = total_pages.div_ceil(MAX_PER_GROUP);
let current_batch = (already_sent_pages.len() / MAX_PER_GROUP) + 1;
```

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

## Testing Patterns

Tests are co-located with code in `#[cfg(test)]` modules. Run with:
```bash
make test                              # All tests
cargo test -p pixivbot -- --nocapture  # With output
cargo test link_handler                # Specific module
```

**Test Examples**:
- `link_handler.rs`: Regex pattern tests for Pixiv URL parsing
- `cache/mod.rs`: Hash bucketing and path resolution tests
- `db/types/tag.rs`: TagFilter parsing and serialization tests

**Test Structure**:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pixiv_link() {
        let text = "Check out https://pixiv.net/artworks/12345";
        let links = parse_pixiv_links(text);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0], PixivLink::Illust(12345));
    }
}
```
