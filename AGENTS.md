# PixivBot Copilot Instructions

## ⚠️ HIGHEST PRIORITY: CI Verification

**MANDATORY**: Before completing ANY code changes, you MUST run `make ci` to verify all checks pass:

```bash
make ci  # Runs: fmt, clippy, check, test, build
```

- CI uses `RUSTFLAGS=-Dwarnings` - ALL warnings are treated as errors
- Never mark a task as complete without successful `make ci` execution
- If `make ci` fails, fix all issues before proceeding

This requirement is NON-NEGOTIABLE and applies to every code modification.

---

## Project Overview

A Rust-based Telegram bot for subscribing to Pixiv artists and rankings. When artists publish new works, the bot automatically downloads and pushes images to subscribers.

**Tech Stack**: Rust 2021, teloxide (Telegram), SeaORM (SQLite), reqwest, tokio

**Architecture Philosophy**: Layered architecture with clear separation between Pixiv API access (`pixiv_client/`), business logic (`pixiv/`, `scheduler/`), and presentation (`bot/`). Components communicate via `Arc`-wrapped shared state.

## Architecture

```
src/
├── main.rs              # Entry point, spawns all engines + bot
├── config.rs            # Configuration (config.toml + env vars)
├── bot/
│   ├── mod.rs           # Dispatcher setup, handler tree
│   ├── handler.rs       # BotHandler struct, command dispatch
│   ├── handlers/        # Command handlers by category
│   │   ├── admin.rs     # Owner/Admin commands
│   │   ├── subscription.rs  # Subscribe/unsubscribe logic
│   │   ├── settings.rs  # Chat settings (tags, filters)
│   │   ├── info.rs      # Help, status commands
│   │   └── download.rs  # /download command
│   ├── middleware.rs    # UserChatContext injection, access control
│   ├── commands.rs      # Command enum definitions
│   ├── link_handler.rs  # Pixiv URL regex parsing
│   └── notifier.rs      # ThrottledBot wrapper, image batching
├── scheduler/           # Background task engines (run independently)
│   ├── author_engine.rs    # Author subscription polling
│   ├── ranking_engine.rs   # Daily ranking push
│   ├── name_update_engine.rs # Author name sync
│   └── helpers.rs       # Shared push logic, PushResult enum
├── pixiv_client/        # Low-level Pixiv API (independent crate)
├── pixiv/               # Business layer wrapping pixiv_client
│   ├── client.rs        # PixivClient with auth management
│   └── downloader.rs    # Image download with caching
├── cache/               # FileCacheManager with hash-bucketing
├── db/                  # SeaORM entities, Repo CRUD
└── utils/               # Markdown escaping, tag formatting
```

## Key Patterns

### Error Handling & User-Facing Messages

**CRITICAL SECURITY PATTERN**: Never expose raw errors to users. Always separate internal logging from user-facing messages:

```rust
// ❌ WRONG - Exposes internal error details to user
bot.send_message(chat_id, format!("❌ 失败: {:#}", e)).await?;

// ✅ CORRECT - Log details internally, send friendly message
error!("Failed to process request: {:#}", e);
bot.send_message(chat_id, "❌ 操作失败").await?;
```

- Use `anyhow::Result<T>` everywhere, `.context()` for debugging
- Log errors with `{:#}` to show full chain; never expose to users
- `pixiv_client/` has its own `Error` type, auto-converts to anyhow

### Shared State & Rate Limiting

- Components use `Arc<T>` for sharing, `Arc<RwLock<T>>` when mutable
- Bot uses `ThrottledBot` (teloxide's `Throttle<Bot>` adaptor) for automatic rate limiting
- No manual `sleep()` needed for Telegram API calls

### Middleware Pattern (bot/middleware.rs)

The dispatcher uses filter middleware to inject context:
```rust
// filter_user_chat() → injects UserChatContext
// filter_chat_accessible() → checks chat enabled + user role
dptree::entry()
    .branch(filter_user_chat().chain(filter_chat_accessible().chain(...)))
```

### Image Batching

**Constant**: `MAX_PER_GROUP = 10` (Telegram limit for media groups)

Caption strategy:
- First batch: full caption with title, author, tags
- Subsequent batches: `(continued 2/3)` format
- Retry logic in `scheduler/helpers.rs` must match this pattern

### Scheduler Engines

Three independent engines run as separate tokio tasks:
- `AuthorEngine`: Polls subscribed authors for new works
- `RankingEngine`: Daily ranking push at configured time
- `NameUpdateEngine`: Syncs author names with Pixiv profiles

Each engine has its own `run()` loop with configurable intervals.

## Adding Dependencies

```bash
cargo add serde --features derive  # Always use cargo add, never edit Cargo.toml manually
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
10. **Pre-commit** - Run `make ci` to catch all issues locally (see ⚠️ HIGHEST PRIORITY section)

**Common Integration Points**:
- Scheduler ↔ Notifier: Use `BatchSendResult` to track partial sends
- Handler ↔ Repo: Always use `.context()` for database operations
- Notifier ↔ Downloader: Check cache first with `cache.get()`, then download
- Bot ↔ Pixiv: Wrap client in `Arc<RwLock<>>` for safe concurrent access

## Scheduler Architecture & Retry Logic

**Orchestrator-Dispatcher-Worker Pattern** (see `scheduler/author_engine.rs`):

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
