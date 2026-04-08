# PixivBot Refactor Roadmap

## 1. Objective

Build an incremental, low-risk refactor plan for PixivBot that improves maintainability without changing user-visible behavior, adding dependencies, or rewriting the architecture.

This roadmap keeps the current layered structure:

- `bot/` — Telegram routing and interaction
- `scheduler/` — background orchestration
- `db/` — persistence
- `pixiv/` — Pixiv integration
- `cache/` — cache storage
- `utils/` — shared utilities

## 2. Current Findings

### Main hotspots

- `src/db/repo.rs` — oversized repository facade with mixed CRUD and business decisions
- `src/bot/handlers/subscription.rs` — oversized command subsystem
- `src/bot/mod.rs` — large dispatcher composition surface
- `src/bot/notifier.rs` — batching, caption, button, and send concerns combined
- `src/scheduler/author_engine.rs` — retry and state-transition heavy flow
- `src/scheduler/ranking_engine.rs` — ranking push flow with duplicated formatting and send decisions

### Cross-cutting duplication already identified

- Illust caption construction across bot and scheduler paths
- Telegram MarkdownV2 response formatting and send patterns
- Spoiler / sensitive-tag decisions
- Download button construction for channel vs non-channel chats
- Push-result / pending / retry state transitions in scheduler flows
- Message-record persistence for reply-based unsubscribe support
- Time scheduling logic in scheduler engines

### Known environment constraint

Local full `make ci` execution is currently blocked in this environment because:

- `make` is unavailable
- release build is blocked by external FFmpeg / `pkg-config` requirements

Plan validation must therefore distinguish between:

- **local gate**: `cargo fmt`, `cargo clippy`, `cargo check`, `cargo test`
- **full gate**: `make ci` or equivalent CI pipeline in a properly provisioned environment

## 3. Refactor Principles

1. Preserve behavior and user-visible message formats.
2. Prefer extraction and modularization over redesign.
3. Split file structure before introducing new abstraction layers.
4. Add tests before refactoring untested logic.
5. Keep each phase independently reviewable and reversible.
6. Do not merge `AuthorEngine` and `RankingEngine` early.
7. Do not introduce new dependencies.

## 4. Non-Goals

- No big-bang rewrite
- No framework migration
- No dependency replacement
- No immediate generic scheduler engine
- No public behavior changes in captions, commands, retries, or permissions

## 5. Execution Model

This roadmap is designed as **6 sequential PRs**.

Each PR should:

- have a single clear theme
- pass the local verification gate before merge
- avoid mixing structural refactor with feature work

### Standard local verification gate

```powershell
cargo fmt --all -- --check
$env:RUSTFLAGS = "-Dwarnings"
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --all-targets
cargo test --workspace --all-targets
```

### Full verification gate

- Run `make ci` or CI-equivalent pipeline in an environment with FFmpeg and `pkg-config` available.

## 6. PR Plan

---

## PR 1 — Extract shared caption building

### Goal

Remove duplicated illust caption construction from bot and scheduler code paths.

### Files in scope

- new `src/utils/caption.rs`
- `src/utils/mod.rs`
- `src/scheduler/helpers.rs`
- `src/scheduler/ranking_engine.rs`
- `src/bot/handler.rs`

### Work

- Create shared caption helpers for:
  - normal illust caption
  - continuation caption
  - ugoira caption
  - ranking caption
- Move any shared batching constant used for caption continuation to one location if needed.
- Replace inline caption-building logic in the three current sites.

### Required tests

- Add golden tests for exact MarkdownV2 caption output.
- Cover:
  - single-page illust
  - multi-page illust
  - continuation caption
  - ugoira caption
  - ranking caption
  - titles / author names containing Markdown-sensitive characters

### Acceptance criteria

- `src/utils/caption.rs` exists and is used by the affected files.
- Caption output stays byte-for-byte stable for covered test inputs.
- Inline illust caption formatting is removed from the targeted call sites.

### QA scenario

1. Run:

   ```powershell
   cargo test caption
   ```

   Expected result:
   - newly added caption golden tests pass
   - existing caption-related tests remain green

2. Run:

   ```powershell
   cargo fmt --all -- --check
   $env:RUSTFLAGS = "-Dwarnings"
   cargo clippy --workspace --all-targets -- -D warnings
   cargo check --workspace --all-targets
   cargo test --workspace --all-targets
   ```

   Expected result:
   - formatting passes
   - clippy passes with warnings denied
   - workspace type-check passes
   - all tests pass

3. Run a repository search for leftover inline caption construction patterns in the replaced files.

   Expected result:
   - targeted inline caption formatting logic is no longer present in `src/scheduler/helpers.rs`, `src/scheduler/ranking_engine.rs`, and `src/bot/handler.rs`

### Risk

High — caption formatting regressions are user-visible.

---

## PR 2 — Extract spoiler and download-button decisions

### Goal

Centralize repeated per-chat delivery decisions that currently appear in multiple bot/scheduler paths.

### Files in scope

- `src/utils/sensitive.rs`
- `src/bot/notifier.rs`
- `src/scheduler/helpers.rs`
- `src/scheduler/ranking_engine.rs`
- `src/bot/handler.rs`

### Work

- Add shared helper for spoiler decision based on chat settings and illust tags.
- Add shared helper or constructor for download-button configuration based on chat type.
- Replace duplicated inline logic in bot/scheduler call sites.

### Required tests

- Unit tests for spoiler decision with:
  - blur disabled
  - blur enabled but no sensitive match
  - blur enabled with sensitive match
- Unit tests for button configuration in channel vs non-channel contexts.

### Acceptance criteria

- No inline duplicate spoiler decision logic remains in targeted call sites.
- No inline duplicate channel-aware button construction remains in targeted call sites.

### QA scenario

1. Run:

   ```powershell
   cargo test sensitive
   ```

   Expected result:
   - spoiler-decision tests pass
   - button-config tests added for this PR pass

2. Run:

   ```powershell
   cargo fmt --all -- --check
   $env:RUSTFLAGS = "-Dwarnings"
   cargo clippy --workspace --all-targets -- -D warnings
   cargo check --workspace --all-targets
   cargo test --workspace --all-targets
   ```

   Expected result:
   - no formatting, lint, type-check, or test regressions

3. Search the touched files for the old duplicated inline patterns.

   Expected result:
   - spoiler decision logic is centralized in `src/utils/sensitive.rs`
   - channel-aware download-button construction is centralized behind the shared helper or constructor

### Risk

Low to medium — behavior is simple but used in multiple send paths.

---

## PR 3 — Build safety-net tests for untested hotspots

### Goal

Increase confidence before deeper refactors of notifier and scheduler internals.

### Files in scope

- `src/bot/notifier.rs`
- `src/scheduler/author_engine.rs`
- `src/scheduler/ranking_engine.rs`

### Work

- Add characterization tests for pure or mostly pure logic already present in these files.
- Cover:
  - `BatchSendResult` behavior
  - author-engine state helper behavior
  - ranking-engine scheduling edge cases

### Acceptance criteria

- Test count increases materially.
- The targeted logic becomes protected by unit tests before structural changes.

### QA scenario

1. Run:

   ```powershell
   cargo test notifier
   cargo test ranking_engine
   cargo test author_engine
   ```

   Expected result:
   - characterization tests for the targeted hotspots pass
   - no existing tests in those modules regress

2. Run:

   ```powershell
   cargo fmt --all -- --check
   $env:RUSTFLAGS = "-Dwarnings"
   cargo clippy --workspace --all-targets -- -D warnings
   cargo check --workspace --all-targets
   cargo test --workspace --all-targets
   ```

   Expected result:
   - test-only additions remain warning-free and do not break compilation

3. Compare the total test count before and after the PR.

   Expected result:
   - the repository gains additional unit coverage for previously under-tested hotspot files

### Risk

Low — test-only PR.

---

## PR 4 — Simplify scheduler shared flow boundaries

### Goal

Reduce scheduler duplication without prematurely introducing a generic engine abstraction.

### Files in scope

- `src/scheduler/helpers.rs`
- `src/scheduler/author_engine.rs`
- `src/scheduler/ranking_engine.rs`
- optional new helper modules under `src/scheduler/`

### Work

- Continue extracting shared scheduler helpers for:
  - push-result follow-up handling
  - message record persistence
  - tag filtering application
  - time scheduling helpers where duplicated
- Shrink `ranking_engine.rs` and `author_engine.rs` by moving repeated support logic to shared scheduler helpers.
- Keep orchestration differences intact between author and ranking flows.

### Acceptance criteria

- Scheduler helper boundaries are clearer.
- `author_engine.rs` and `ranking_engine.rs` lose repeated support logic but retain their distinct orchestration models.
- No new generic engine abstraction is introduced.

### QA scenario

1. Run:

   ```powershell
   cargo test author_engine
   cargo test ranking_engine
   ```

   Expected result:
   - scheduler characterization tests pass after helper extraction

2. Run:

   ```powershell
   cargo fmt --all -- --check
   $env:RUSTFLAGS = "-Dwarnings"
   cargo clippy --workspace --all-targets -- -D warnings
   cargo check --workspace --all-targets
   cargo test --workspace --all-targets
   ```

   Expected result:
   - scheduler refactor remains lint-clean and test-clean

3. Search for duplicate scheduler-side support logic that this PR is intended to remove.

   Expected result:
   - message-record persistence, tag-filter application, and duplicated scheduling helpers are reduced in the engine files and moved behind shared scheduler helpers

### Risk

Medium — multiple stateful paths involved.

---

## PR 5 — Split oversized bot modules

### Goal

Reduce complexity in the bot layer by splitting monolithic handler and notifier files into responsibility-focused submodules.

### Files in scope

- `src/bot/handlers/subscription.rs`
- `src/bot/notifier.rs`
- `src/bot/mod.rs`
- optional new modules under `src/bot/handlers/subscription/` and `src/bot/notifier/`

### Work

- Split `subscription.rs` into focused submodules such as:
  - parsing
  - subscription command execution
  - listing / pagination
  - response formatting
- Split `notifier.rs` into focused submodules such as:
  - batching
  - caption integration
  - button configuration
  - send orchestration
- If useful, reorganize dispatcher assembly in `bot/mod.rs` into smaller route-builder helpers, but do not change routing behavior.

### Acceptance criteria

- `subscription.rs` no longer acts as a single monolithic command subsystem file.
- `notifier.rs` responsibilities are visibly separated.
- Dispatcher behavior remains unchanged.

### QA scenario

1. Run targeted module tests after the split:

   ```powershell
   cargo test subscription
   cargo test notifier
   ```

   Expected result:
   - moved bot-layer logic still passes its targeted tests

2. Run:

   ```powershell
   cargo fmt --all -- --check
   $env:RUSTFLAGS = "-Dwarnings"
   cargo clippy --workspace --all-targets -- -D warnings
   cargo check --workspace --all-targets
   cargo test --workspace --all-targets
   ```

   Expected result:
   - file moves and module splits do not introduce import or lint regressions

3. Inspect the resulting module structure.

   Expected result:
   - subscription parsing, execution, listing, and formatting concerns are separated
   - notifier batching, caption integration, button configuration, and send orchestration are separated
   - dispatcher assembly remains behavior-preserving

### Risk

Medium — mostly structural, but bot entry surfaces are sensitive.

---

## PR 6 — Split `db/repo.rs` internally while preserving API

### Goal

Reduce `Repo` cohesion problems without forcing a broad caller rewrite.

### Files in scope

- `src/db/repo.rs` or `src/db/repo/mod.rs`
- new submodules under `src/db/repo/`

### Work

- Split repository implementation by domain while keeping the `Repo` facade stable.
- Candidate submodules:
  - `user_repo`
  - `chat_repo`
  - `subscription_repo`
  - `task_repo`
  - `message_repo`
  - `stats_repo`
- Preserve external call sites through the existing `Repo` API surface.

### Acceptance criteria

- `Repo` remains the external entrypoint.
- Internal file structure reflects domain boundaries.
- No behavior change at call sites.

### QA scenario

1. Run targeted repository tests after the split:

   ```powershell
   cargo test repo
   ```

   Expected result:
   - repository tests pass without caller-side behavior changes

2. Run:

   ```powershell
   cargo fmt --all -- --check
   $env:RUSTFLAGS = "-Dwarnings"
   cargo clippy --workspace --all-targets -- -D warnings
   cargo check --workspace --all-targets
   cargo test --workspace --all-targets
   ```

   Expected result:
   - module moves preserve compile-time compatibility across the workspace

3. Inspect external call sites for `Repo` usage.

   Expected result:
   - callers still use the same outward-facing `Repo` facade without broad mechanical rewrites

### Risk

Medium — broad compile surface, but low intended behavior change.

## 7. Recommended Ordering Rationale

Recommended sequence:

1. PR 1 — shared caption building
2. PR 2 — spoiler/button decisions
3. PR 3 — safety-net tests
4. PR 4 — scheduler shared-flow cleanup
5. PR 5 — bot module splitting
6. PR 6 — repo splitting

Why this order:

- PR 1 and PR 2 remove duplicated cross-cutting logic with high reuse value.
- PR 3 increases safety before deeper structural work.
- PR 4 builds on the extracted helpers while staying within one architectural layer.
- PR 5 and PR 6 are bigger structural reorganizations and are safer once shared logic is already centralized.

## 8. Explicit Risks and Mitigations

### Risk: Caption regressions

Mitigation:

- exact-output tests
- avoid changing punctuation, emoji, or escape style

### Risk: MarkdownV2 breakage

Mitigation:

- include special-character test cases
- centralize escaping in shared helpers

### Risk: Scheduler behavior drift

Mitigation:

- do not unify author and ranking engines too early
- keep state-machine responsibilities explicit
- add characterization tests before deeper cleanup

### Risk: Repo split causes broad breakage

Mitigation:

- preserve `Repo` as the outward-facing facade
- perform internal file movement first, API redesign later if ever needed

## 9. Deferred Work

These are intentionally out of scope for the first roadmap cycle:

- introducing a dedicated `services/` layer
- generic scheduler engine abstraction
- moving domain logic out of `utils/` into richer domain modules
- redesigning dispatcher architecture beyond structural decomposition

These may become a second-cycle roadmap after the first six PRs land cleanly.

## 10. Definition of Success

This roadmap is successful if, after the six PRs:

- the main hotspot files are materially smaller or modularized
- duplicated delivery/caption logic is centralized
- scheduler support logic is clearer without losing explicit orchestration
- repository internals are easier to navigate
- CI remains green in a fully provisioned environment
- no user-visible behavior changes are introduced
