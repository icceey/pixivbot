# PixivBot Agent Guide

## Checks

- Rust is pinned to 1.94 in `rust-toolchain.toml`; builds need FFmpeg development libraries plus `pkg-config` (`brew install ffmpeg pkg-config` on macOS, CI installs `libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libswresample-dev pkg-config`).
- Before completing Rust/code changes, run `make ci`; it runs `fmt-check -> clippy` with `RUSTFLAGS=-Dwarnings` `-> check -> test -> release build`.
- Use `make quick` for a faster local loop, `make fmt` to format, and focused tests such as `cargo test -p pixivbot link_handler` or `cargo test -p booru_client <filter>`.
- H.264/ugoira encoder tests are behind `--features ffmpeg-codec`; only run them when the local FFmpeg has a working H.264 encoder.
- Markdown-only docs can be verified with `git diff --check -- <path>`; do not run the full Rust CI for docs-only edits unless code or generated files changed.

## Source Of Truth

- Prefer executable files over prose when facts conflict: `Cargo.toml`, `rust-toolchain.toml`, `Makefile`, CI workflows, `Dockerfile`, and source code beat README-style summaries.
- `config.toml.example` is the public config reference; local `/config.toml` may contain bot tokens, Pixiv credentials, and other secrets.
- Do not read, print, copy, or commit `/config.toml`; `.gitignore` intentionally excludes it along with `/data`, `/logs`, and `target/`.
- There are no repo-local OpenCode/Cursor/Copilot instruction files besides `AGENTS.md` and scoped `src/bot/notifier/AGENTS.md` as of the last audit.

## Workspace

- Workspace crates are root `pixivbot` app, `pixiv_client` low-level Pixiv API client, `booru_client` booru API client, and `migration` SeaORM migrations.
- Root `pixivbot` owns Telegram bot wiring, scheduler engines, persistence, Pixiv downloads, and booru integration.
- `pixiv_client` is the low-level Pixiv API client; `booru_client` is the booru API client; keep protocol/client changes inside those crates when possible.
- `migration` owns SeaORM migrations and is invoked from the app at startup, not by an external migration runner.
- `src/main.rs` is the real wiring entrypoint: load config, run migrations, build `Repo`/Pixiv client/downloader/notifier, spawn author/ranking/name-update/optional booru engines, then start Telegram.

## Configuration

- `Config::load()` reads optional `config.toml` and env overrides using prefix `PIX` with `__` separators, so `PIX__TELEGRAM__BOT_TOKEN` maps into nested config.
- `telegram.owner_id` is optional; if absent, the first user to talk to the bot can become owner, so preserve the warning in `config.toml.example`.
- `telegram.bot_mode` controls private/public access, `api_url` can point teloxide at a custom Telegram API, and `require_mention_in_group` is the global group default.
- `download_threshold()` clamps configured values to `1..=10`; keep that aligned with user-facing config docs.
- Config changes usually need `src/config.rs`, `config.toml.example`, and explicit threading through `src/main.rs`/constructors; this repo avoids global config lookups.

## Bot And Telegram

- `src/bot` owns Telegram commands, middleware, settings dialogue state, link parsing, callback handlers, and command routing.
- `BotHandler` stores shared dependencies and routes commands after middleware injects `UserChatContext`; unauthorized commands are intentionally ignored.
- `build_handler_tree()` has separate callback, command, link/message, settings-dialogue, and cancel branches; preserve branch ordering when adding handlers.
- Admin enable/disable commands intentionally bypass normal chat-access and mention checks so admins can enable disabled chats.
- Link/message handling still passes through mention and chat-access filters; do not weaken group gating accidentally.
- User-visible booru commands are only exposed when a booru registry is configured.

## Telegram Safety

- Log internal failures with `tracing` and `{:#}` error chains, but send short friendly user messages; do not expose raw `anyhow`, DB, Telegram, Pixiv, or booru errors to users.
- User-visible formatted text usually uses `ParseMode::MarkdownV2`; escape dynamic text with `teloxide::utils::markdown` or existing helpers before interpolation.
- Several tests assert exact MarkdownV2 strings, so treat escaping and punctuation changes as behavior changes.
- `Owner` implies admin via `UserRole::is_admin()`; keep owner/admin private-chat behavior intact when changing access checks.
- Group behavior is controlled by global `require_mention_in_group` plus per-chat `allow_without_mention`; admin enable/disable commands intentionally bypass the normal chat-access branch.

## Database And Migrations

- `src/db` owns SeaORM entities, custom DB types, and `Repo`; application code should use `Repo` methods instead of scattering SeaORM queries.
- Schema changes require a new migration file in `migration/src` and registration in `migration/src/lib.rs` inside `MigratorTrait::migrations()`.
- `subscriptions.latest_data` persists scheduler progress as `SubscriptionState` variants; update state transitions and tests together.
- Repo unit tests often create in-memory SQLite schemas manually, so do not assume migrations have run inside unit tests.

## Pixiv And Booru

- `src/pixiv` wraps `pixiv_client` for auth, cache-aware downloads, Pixiv referer handling, and ugoira ZIP to MP4 conversion.
- Downloader paths should prefer cached files before network fetches and keep pximg referer behavior intact.
- Ugoira/H.264 tests behind `ffmpeg-codec` require a working encoder; avoid enabling them in routine checks unless local FFmpeg supports it.
- `src/booru` builds one shared `Arc<BooruSiteRegistry>` from configured sites; empty registries disable booru scheduler work and user-visible booru commands.
- `BooruSiteRegistry` lowercases lookup keys and applies per-site auth plus optional bypass/FlareSolverr configuration.
- `BooruTaskKey` encodes values as `site:tags|o=...|r=...|i=...|f=...`; filter signatures encode which filters exist, not threshold values.

## Notifier And Batching

- Read `src/bot/notifier/AGENTS.md` before changing notifier internals; it documents invariants for captioning and continuation numbering.
- Telegram media groups cap at `utils::caption::MAX_PER_GROUP = 10`; scheduler retry math, `ContinuationNumbering`, and notifier chunking must stay aligned with that constant.
- All send paths, including the single-image path during resumed sends, must go through notifier caption helpers so partial retries use `\(continued N/M\)` consistently.
- `BatchSendResult` drives scheduler retry state; preserve succeeded/failed indices and `first_message_id` semantics when touching send code.
- Bot API calls use teloxide `Throttle<Bot>` via `ThrottledBot`; do not add manual Telegram rate-limit sleeps unless a non-Telegram backoff is being modeled.

## Scheduler State

- `src/scheduler` owns `AuthorEngine`, `RankingEngine`, `NameUpdateEngine`, and optional `BooruEngine`; scheduler decisions should not move into Telegram handlers.
- `get_chat_if_should_notify()` skips disabled chats except admin/owner private chats; reuse it for scheduler notification eligibility.
- Author tasks fetch one Pixiv author list once, then process each subscription independently; pending `PendingIllust { sent_pages, retry_count }` is retried before new work.
- Ranking tasks run at configured local `HH:MM` and process all ranking tasks, not just currently pending DB tasks.
- Booru engine caps grace/ranking sends per tick and uses short drain polls for pending queues; do not simplify this into sending every pending post in one tick.
- Keep `INTER_SUBSCRIPTION_DELAY_MS`, pending retry counts, and `first_message_id` semantics aligned with persisted scheduler state.

## Testing Notes

- Add small colocated `#[cfg(test)]` tests for parsing, state transitions, caption/Markdown output, and repo behavior; several tests assert exact MarkdownV2 strings.
- Link parser tests cover Pixiv ordering and booru engine-specific URL support; update them when changing supported URL forms.
- `BooruTaskKey` tests cover task-value encoding and filter signatures; adjust tests when task sharing semantics change.
- For config or access-control changes, prefer focused unit tests around parsing, role checks, middleware filters, or command visibility instead of broad integration tests.

## Release And Runtime

- `Dockerfile` uses Rust 1.94 cargo-chef, builds with `--locked`, and installs FFmpeg development/runtime libraries; keep dependency changes compatible with container builds.
- `docker-compose.yml` mounts `./config.toml:/app/config.toml:ro` and `./data:/app/data`, with `TZ=Asia/Shanghai` by default.
- Release workflow builds Linux, macOS, and Windows targets; Windows FFmpeg comes from vcpkg while Linux/macOS install FFmpeg dev packages.
