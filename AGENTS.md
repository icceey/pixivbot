# PixivBot Agent Guide

## Checks

- Rust is pinned to 1.94 in `rust-toolchain.toml`; builds need FFmpeg development libraries plus `pkg-config` (`brew install ffmpeg pkg-config` on macOS, CI installs `libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libswresample-dev pkg-config`).
- Before completing Rust/code changes, run `make ci`; it runs `fmt-check -> clippy` with `RUSTFLAGS=-Dwarnings` `-> check -> test -> release build`.
- Use `make quick` for a faster local loop, `make fmt` to format, and focused tests such as `cargo test -p pixivbot link_handler` or `cargo test -p booru_client <filter>`.
- H.264/ugoira encoder tests are behind `--features ffmpeg-codec`; only run them when the local FFmpeg has a working H.264 encoder.
- Markdown-only docs can be verified with `git diff --check -- <path>`; do not run the full Rust CI for docs-only edits unless code or generated files changed.

## Pre-Delivery Self-Review (Required)

- Before delivering code, review the final diff and affected code paths against every applicable instruction in this file, scoped `AGENTS.md` files, and the user's requirements. This review is mandatory even when formatting, compilation, tests, and CI pass.
- Check for unnecessary abstractions, pass-through wrappers, redundant defensive checks, speculative APIs, dead code, unused imports/parameters, and incomplete feature removal. Verify references across the workspace and relevant feature-gated paths before removing code; preserve implicit protocol, persistence, and resource-lifetime contracts.
- Review the necessity of every added or modified test using the Testing Notes below. Remove low-value and redundant tests with their exclusive fixtures/helpers; do not justify keeping them by production-code calls, regression labels, case counts, or green results.
- Verify that behavior changes stay within the requested scope and that affected configuration, documentation, migrations, error messages, and shared call paths remain consistent. Check changed code and fixtures for real credentials or private configuration; do not inspect the local `config.toml` to perform this check.
- Confirm the actual toolchain used and complete the checks required for the change. Fix every identified instruction violation before handoff, then review the resulting diff again. Report the self-review outcome and validation performed in the delivery response, with any unresolved limitation stated explicitly; do not claim compliance while known violations remain.

## Code Cleanup Constraints

- Keep implementations direct: do not add speculative APIs, pass-through wrappers, unused parameters, no-op branches, or duplicate branches with identical behavior.
- Do not keep retired business logic alive with tests, `#[cfg(test)]`, underscore names, or `#[allow(dead_code)]`. Genuine test fixtures, synchronization hooks, and fault injection may use `#[cfg(test)]`; obsolete production implementations may not.
- Before declaring code dead, check references across all workspace crates, entrypoints, tests, scripts, and feature-gated builds. Account for trait/macro-generated use and public client APIs; absence of a call in one module is not sufficient evidence.
- An unread field or parameter can still enforce a contract through deserialization, database schema, dependency injection, or resource lifetime. In particular, an optional deserialized field can reject duplicate or malformed input. Preserve these effects and document non-obvious reasons for retaining the declaration.
- Remove confirmed dead implementations together with their exclusive tests, private helpers, mocks, imports, and stale comments. Do not retain compatibility wrappers or tests for a removed feature; tests of persisted formats that are still supported remain valid.
- Cleanup must preserve accepted/rejected inputs, defaults, errors and formatting, operation order, and state transitions. Review the diff against the original behavior; passing tests alone does not establish equivalence. Use the checks above and remove any leftovers before delivery.

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

- Add only necessary unit tests. If no additional unit test is necessary, add none; delivering a change with zero new tests is acceptable. Never add tests to pursue coverage, raise line/branch/function coverage percentages, cover every changed path, or satisfy a test-count target.
- A test must protect against a specific, non-trivial failure that code review, the compiler, and existing coverage do not adequately catch. A new feature, a changed branch, an uncovered code path, or a hypothetical future regression is not sufficient justification.
- Calling real production code is necessary but does not make a test valuable. Mocks may replace external boundaries, but do not copy an algorithm into a test or turn a trivial assertion into an apparent integration test by calling it through more layers.
- Do not add or retain tests that only check source text, file/symbol existence, function signatures, type fields, fixed configuration/constant values, trivial accessors, or data assignment. Serialization tests must protect an actual protocol or persistence contract, not merely a derived round trip or field presence.
- Treat tests that merely restate simple enum comparisons, boolean switches, tag membership, case normalization, string concatenation, or established include/exclude priority as low-value. Enumerating missing/default/known/unknown values or combinations of switches does not by itself justify a test.
- Specifically, checking that an AI flag adds a label, repeating that label assertion through several caption builders, or checking that existing tag filters and blur switches react to the label are low-value tests. Do not retain them merely because they call production functions or are described as behavior/regression coverage.
- Large synthetic fixtures, hand-picked result lists, and caption prefix/suffix assertions do not add value when the assertion still mirrors a straightforward implementation. Do not replace rejected unit tests with larger mocks, table-driven cases, or renamed integration tests that make the same assertions.
- Before adding or retaining a test, identify the actual failure mechanism and the observable consequence it uniquely checks. Examples that can justify a test include malformed protocol input accepted unexpectedly, lost or duplicate work after partial failure/restart, broken transaction guarantees, or a reproducible concurrency race. Broad labels such as "boundary case", "state transition", "real behavior", or "regression" are not a justification on their own.
- Check existing coverage first. Extend an existing justified test when appropriate; remove redundant or low-value tests together with their exclusive fixtures, helpers, mocks, and imports before delivery. Do not defend tests by their count, number of cases, passing result, or coverage percentage.
- Do not create test-only constructors or business wrappers to preserve outdated call sites. Exercise the current production entrypoint with explicit inputs. Keep test setup limited to state and mocks that the test actually uses.
- Synchronize concurrent tests on observable events or completion rather than arbitrary sleeps. Test servers must actually receive the intended request before asserting transport behavior; retries or longer waits must not mask a broken fixture.
- Parsing, state transitions, caption/Markdown output, and repo behavior are subject to the same necessity threshold. Exact output assertions and colocated `#[cfg(test)]` organization do not exempt a test from these rules.
- Link parser tests cover Pixiv ordering and booru engine-specific URL support; update them when changing supported URL forms.
- `BooruTaskKey` tests cover task-value encoding and filter signatures; adjust tests when task sharing semantics change.
- Apply the same threshold to config and access-control changes. Do not automatically add default-value, role-predicate, or command-visibility truth tables; a security test must demonstrate a concrete unauthorized-access risk through the relevant production checks.

## Release And Runtime

- `Dockerfile` uses Rust 1.94 cargo-chef, builds with `--locked`, and installs FFmpeg development/runtime libraries; keep dependency changes compatible with container builds.
- `docker-compose.yml` mounts `./config.toml:/app/config.toml:ro` and `./data:/app/data`, with `TZ=Asia/Shanghai` by default.
- Release workflow builds Linux, macOS, and Windows targets; Windows FFmpeg comes from vcpkg while Linux/macOS install FFmpeg dev packages.
