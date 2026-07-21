# EH Queue Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `/estatus` so users can inspect the invoking chat's active EH download queue and its most recently enqueued terminal record.

**Architecture:** `Repo` performs chat-scoped status filtering and returns a minimal DTO that cannot expose queue errors, tokens, or paths. The EH handler maps stored states to stable Chinese labels, computes a bounded MarkdownV2-safe response, and sends it through the existing ordinary-command route.

**Tech Stack:** Rust 1.94, SeaORM, Tokio, teloxide, MarkdownV2

---

## File map

- Modify: `src/db/repo/eh_download_queue.rs` — minimal queue-status DTO and chat-scoped snapshot query.
- Modify/Test: `src/db/repo/eh_integration_tests.rs` — repository filtering, ordering, background state, terminal selection, and chat isolation.
- Modify/Test: `src/bot/handlers/subscription/ehentai.rs` — state labels, bounded formatter, and Telegram handler.
- Modify/Test: `src/bot/commands.rs` — `/estatus` parsing and EH-dependent menu visibility.
- Modify: `src/bot/handler.rs` — dispatch `/estatus` with the invoking `chat_id`.
- No change: `src/bot/mod.rs` — the existing command tree already applies ordinary chat-access and mention middleware.

### Task 1: Query a chat-scoped EH queue snapshot

**Files:**
- Modify: `src/db/repo/eh_download_queue.rs:37-55,686-695`
- Test: `src/db/repo/eh_integration_tests.rs`

- [ ] **Step 1: Add a deterministic failing repository test**

Replace the existing `tests_helpers` import in `src/db/repo/eh_integration_tests.rs` and add the test-only helpers:

```rust
use super::{tests_helpers, Repo};
use crate::db::entities::eh_download_queue as eh_download_queue_entity;
use crate::db::repo::eh_download_queue::{
    BACKGROUND_STATUS_PENDING, BACKGROUND_STATUS_RUNNING, SOURCE_DIRECT, STATUS_CANCELED,
    STATUS_DONE, STATUS_DOWNLOADED, STATUS_DOWNLOADING, STATUS_FAILED, STATUS_PENDING,
    STATUS_PUBLISHING, STATUS_UPLOADED, STATUS_UPLOADING,
};
use chrono::{NaiveDate, NaiveDateTime};
use sea_orm::{ActiveModelTrait, Set};

fn queue_time(second: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 7, 21)
        .unwrap()
        .and_hms_opt(0, 0, second)
        .unwrap()
}

async fn set_queue_state(
    repo: &Repo,
    model: eh_download_queue_entity::Model,
    status: &str,
    background_status: Option<&str>,
    created_at: NaiveDateTime,
    completed_at: Option<NaiveDateTime>,
    error: Option<&str>,
) {
    let mut active: eh_download_queue_entity::ActiveModel = model.into();
    active.status = Set(status.to_owned());
    active.background_download_status = Set(background_status.map(str::to_owned));
    active.created_at = Set(created_at);
    active.completed_at = Set(completed_at);
    active.error = Set(error.map(str::to_owned));
    active.update(repo.db()).await.unwrap();
}
```

Add this test at the end of the file:

```rust
#[tokio::test]
async fn test_eh_queue_status_snapshot_scopes_orders_and_selects_recent_terminal() {
    let repo = tests_helpers::setup_test_db().await.unwrap();

    let active_specs = [
        (101, STATUS_PENDING, None),
        (102, STATUS_PENDING, Some(BACKGROUND_STATUS_PENDING)),
        (103, STATUS_PENDING, Some(BACKGROUND_STATUS_RUNNING)),
        (104, STATUS_DOWNLOADING, None),
        (105, STATUS_DOWNLOADED, None),
        (106, STATUS_UPLOADING, None),
        (107, STATUS_UPLOADED, None),
        (108, STATUS_PUBLISHING, None),
    ];

    for (index, (gid, status, background)) in active_specs.into_iter().enumerate() {
        let row = repo
            .enqueue_eh_download(
                -100,
                gid,
                "token",
                &format!("Gallery {gid}"),
                false,
                SOURCE_DIRECT,
            )
            .await
            .unwrap();
        set_queue_state(
            &repo,
            row,
            status,
            background,
            queue_time((index + 1) as u32),
            None,
            None,
        )
        .await;
    }

    for (gid, status, created, completed, error) in [
        (201, STATUS_DONE, 20, 59, None),
        (202, STATUS_CANCELED, 25, 40, None),
        (203, STATUS_FAILED, 30, 31, Some("internal database secret")),
    ] {
        let row = repo
            .enqueue_eh_download(
                -100,
                gid,
                "token",
                &format!("Terminal {gid}"),
                false,
                SOURCE_DIRECT,
            )
            .await
            .unwrap();
        set_queue_state(
            &repo,
            row,
            status,
            None,
            queue_time(created),
            Some(queue_time(completed)),
            error,
        )
        .await;
    }

    for (gid, status) in [(998, STATUS_DONE), (999, STATUS_PENDING)] {
        let row = repo
            .enqueue_eh_download(-200, gid, "token", "Foreign", false, SOURCE_DIRECT)
            .await
            .unwrap();
        set_queue_state(
            &repo,
            row,
            status,
            Some(BACKGROUND_STATUS_RUNNING),
            queue_time(58),
            Some(queue_time(58)),
            None,
        )
        .await;
    }

    let snapshot = repo.get_eh_queue_snapshot(-100).await.unwrap();

    assert_eq!(
        snapshot.active.iter().map(|item| item.gid).collect::<Vec<_>>(),
        vec![101, 102, 103, 104, 105, 106, 107, 108]
    );
    assert_eq!(
        snapshot.active[1].background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_PENDING)
    );
    assert_eq!(
        snapshot.active[2].background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_RUNNING)
    );
    assert!(snapshot.active.iter().all(|item| item.gid < 200));

    let recent = snapshot.recent_terminal.as_ref().unwrap();
    assert_eq!(recent.gid, 203);
    assert_eq!(recent.status, STATUS_FAILED);
    assert!(!format!("{snapshot:?}").contains("internal database secret"));
}
```

- [ ] **Step 2: Run the test and capture RED**

Run:

```powershell
cargo test -p pixivbot test_eh_queue_status_snapshot_scopes_orders_and_selects_recent_terminal -- --nocapture
```

Expected: compilation fails because `Repo::get_eh_queue_snapshot` does not exist.

- [ ] **Step 3: Add the minimal status DTOs**

Add after the queue status/source constants in `src/db/repo/eh_download_queue.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhQueueStatusItem {
    pub gid: i64,
    pub title: String,
    pub status: String,
    pub background_download_status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhQueueSnapshot {
    pub active: Vec<EhQueueStatusItem>,
    pub recent_terminal: Option<EhQueueStatusItem>,
}

impl From<eh_download_queue::Model> for EhQueueStatusItem {
    fn from(model: eh_download_queue::Model) -> Self {
        Self {
            gid: model.gid,
            title: model.title,
            status: model.status,
            background_download_status: model.background_download_status,
        }
    }
}
```

The DTO deliberately excludes `chat_id`, token, errors, retry metadata, subscription IDs, URLs, and local paths.

- [ ] **Step 4: Implement the two-query snapshot**

Add near `count_pending_eh_downloads()`:

```rust
pub async fn get_eh_queue_snapshot(&self, chat_id: i64) -> Result<EhQueueSnapshot> {
    let active = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::ChatId.eq(chat_id))
        .filter(eh_download_queue::Column::Status.is_in([
            STATUS_PENDING,
            STATUS_DOWNLOADING,
            STATUS_DOWNLOADED,
            STATUS_UPLOADING,
            STATUS_UPLOADED,
            STATUS_PUBLISHING,
        ]))
        .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
        .all(&self.db)
        .await
        .context("Failed to fetch active EH queue snapshot")?
        .into_iter()
        .map(EhQueueStatusItem::from)
        .collect();

    let recent_terminal = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::ChatId.eq(chat_id))
        .filter(eh_download_queue::Column::Status.is_in([
            STATUS_DONE,
            STATUS_FAILED,
            STATUS_CANCELED,
        ]))
        .order_by(eh_download_queue::Column::CreatedAt, Order::Desc)
        .one(&self.db)
        .await
        .context("Failed to fetch recent terminal EH queue entry")?
        .map(EhQueueStatusItem::from);

    Ok(EhQueueSnapshot {
        active,
        recent_terminal,
    })
}
```

Do not call the global `count_pending_eh_downloads()`; these reads intentionally form a best-effort snapshot rather than a transaction.

- [ ] **Step 5: Run the test and capture GREEN**

Run the Step 2 command again. Expected: `1 passed; 0 failed`.

- [ ] **Step 6: Record the review boundary**

Review only `src/db/repo/eh_download_queue.rs` and `src/db/repo/eh_integration_tests.rs`. Intended commit if the user later grants Git write permission: `feat: query per-chat EH queue status`.

### Task 2: Format a bounded MarkdownV2 status message

**Files:**
- Modify/Test: `src/bot/handlers/subscription/ehentai.rs:1-10,702-875`

- [ ] **Step 1: Import the snapshot types and statuses**

Replace the `SOURCE_DIRECT` import with:

```rust
use crate::db::repo::eh_download_queue::{
    EhQueueSnapshot, EhQueueStatusItem, BACKGROUND_STATUS_PENDING, BACKGROUND_STATUS_RUNNING,
    SOURCE_DIRECT, STATUS_CANCELED, STATUS_DONE, STATUS_DOWNLOADED, STATUS_DOWNLOADING,
    STATUS_FAILED, STATUS_PENDING, STATUS_PUBLISHING, STATUS_UPLOADED, STATUS_UPLOADING,
};
```

- [ ] **Step 2: Add failing pure formatter tests**

Inside the existing `tests` module, add a minimal item constructor and four tests:

```rust
fn queue_item(
    gid: i64,
    title: &str,
    status: &str,
    background: Option<&str>,
) -> EhQueueStatusItem {
    EhQueueStatusItem {
        gid,
        title: title.to_owned(),
        status: status.to_owned(),
        background_download_status: background.map(str::to_owned),
    }
}

#[test]
fn test_eh_queue_status_stage_labels() {
    for (status, background, expected) in [
        (STATUS_PENDING, Some(BACKGROUND_STATUS_RUNNING), "后台下载中"),
        (STATUS_PENDING, Some(BACKGROUND_STATUS_PENDING), "后台排队"),
        (STATUS_PENDING, None, "排队中"),
        (STATUS_DOWNLOADING, Some(BACKGROUND_STATUS_RUNNING), "下载中"),
        (STATUS_DOWNLOADED, None, "等待上传或发送"),
        (STATUS_UPLOADING, None, "上传中"),
        (STATUS_UPLOADED, None, "等待发送"),
        (STATUS_PUBLISHING, None, "发送中"),
        (STATUS_DONE, None, "已完成"),
        (STATUS_FAILED, None, "失败"),
        (STATUS_CANCELED, None, "已取消"),
        ("unexpected", None, "未知状态"),
    ] {
        assert_eq!(eh_queue_stage(status, background), expected);
    }
}

#[test]
fn test_eh_queue_status_summarizes_all_active_stages() {
    let snapshot = EhQueueSnapshot {
        active: vec![
            queue_item(1, "A", STATUS_PENDING, Some(BACKGROUND_STATUS_RUNNING)),
            queue_item(2, "B", STATUS_PENDING, Some(BACKGROUND_STATUS_PENDING)),
            queue_item(3, "C", STATUS_PENDING, None),
            queue_item(4, "D", STATUS_DOWNLOADING, None),
            queue_item(5, "E", STATUS_DOWNLOADED, None),
            queue_item(6, "F", STATUS_UPLOADING, None),
            queue_item(7, "G", STATUS_UPLOADED, None),
            queue_item(8, "H", STATUS_PUBLISHING, None),
        ],
        recent_terminal: None,
    };

    let output = format_eh_queue_status(&snapshot);
    assert!(output.contains("活动任务：`8`"));
    assert!(output.contains(
        "阶段：后台下载中 `1` · 后台排队 `1` · 排队中 `1` · 下载中 `1` · \
         等待上传或发送 `1` · 上传中 `1` · 等待发送 `1` · 发送中 `1`"
    ));
}

#[test]
fn test_eh_queue_status_formats_empty_with_recent_terminal() {
    let snapshot = EhQueueSnapshot {
        active: Vec::new(),
        recent_terminal: Some(queue_item(900, "已完成的标题", STATUS_DONE, None)),
    };

    assert_eq!(
        format_eh_queue_status(&snapshot),
        "📥 *EH 下载队列*\n\n当前聊天没有活动中的 EH 下载任务\n\n\
         *最近记录*\n• GID `900` · 已完成的标题 · 已完成"
    );
}

#[test]
fn test_eh_queue_status_limits_entries_truncates_and_escapes() {
    let mut active = (1..=21)
        .map(|gid| queue_item(gid, &format!("Gallery {gid}"), STATUS_PENDING, None))
        .collect::<Vec<_>>();
    active[0].title = "界".repeat(81);
    active[1].title = "A_*[危险].!".to_owned();
    let hidden_title = active[20].title.clone();
    let snapshot = EhQueueSnapshot {
        active,
        recent_terminal: Some(queue_item(
            999,
            "failed title",
            STATUS_FAILED,
            None,
        )),
    };

    let output = format_eh_queue_status(&snapshot);
    assert!(output.contains("活动任务：`21`"));
    assert!(output.contains("排队中 `21`"));
    assert!(output.contains("另有 `1` 项未显示"));
    assert!(output.contains("• GID `20`"));
    assert!(!output.contains("• GID `21`"));
    assert!(!output.contains(&hidden_title));
    assert!(output.contains(&"界".repeat(80)));
    assert!(!output.contains(&"界".repeat(81)));
    assert!(output.contains(r"A\_\*\[危险\]\.\!"));
    assert!(!output.contains("A_*[危险].!"));
    assert!(output.contains("• GID `999` · failed title · 失败"));
}
```

- [ ] **Step 3: Run formatter tests and capture RED**

Run:

```powershell
cargo test -p pixivbot eh_queue_status -- --nocapture
```

Expected: compilation fails because `eh_queue_stage` and `format_eh_queue_status` do not exist.

- [ ] **Step 4: Implement state mapping and formatting**

Add above `impl BotHandler`:

```rust
const MAX_VISIBLE_EH_QUEUE_ITEMS: usize = 20;
const MAX_EH_QUEUE_TITLE_CHARS: usize = 80;
const ACTIVE_EH_QUEUE_STAGES: [&str; 9] = [
    "后台下载中",
    "后台排队",
    "排队中",
    "下载中",
    "等待上传或发送",
    "上传中",
    "等待发送",
    "发送中",
    "未知状态",
];

fn eh_queue_stage(status: &str, background_status: Option<&str>) -> &'static str {
    match (status, background_status) {
        (STATUS_PENDING, Some(BACKGROUND_STATUS_RUNNING)) => "后台下载中",
        (STATUS_PENDING, Some(BACKGROUND_STATUS_PENDING)) => "后台排队",
        (STATUS_PENDING, _) => "排队中",
        (STATUS_DOWNLOADING, _) => "下载中",
        (STATUS_DOWNLOADED, _) => "等待上传或发送",
        (STATUS_UPLOADING, _) => "上传中",
        (STATUS_UPLOADED, _) => "等待发送",
        (STATUS_PUBLISHING, _) => "发送中",
        (STATUS_DONE, _) => "已完成",
        (STATUS_FAILED, _) => "失败",
        (STATUS_CANCELED, _) => "已取消",
        _ => "未知状态",
    }
}

fn format_eh_queue_item(item: &EhQueueStatusItem) -> String {
    let title = item
        .title
        .chars()
        .take(MAX_EH_QUEUE_TITLE_CHARS)
        .collect::<String>();
    let stage = eh_queue_stage(
        &item.status,
        item.background_download_status.as_deref(),
    );
    format!(
        "GID `{}` · {} · {}",
        item.gid,
        markdown::escape(&title),
        stage
    )
}

fn format_eh_queue_status(snapshot: &EhQueueSnapshot) -> String {
    let staged = snapshot
        .active
        .iter()
        .map(|item| {
            (
                item,
                eh_queue_stage(
                    &item.status,
                    item.background_download_status.as_deref(),
                ),
            )
        })
        .collect::<Vec<_>>();
    let mut output = String::from("📥 *EH 下载队列*\n\n");

    if staged.is_empty() {
        output.push_str("当前聊天没有活动中的 EH 下载任务");
    } else {
        output.push_str(&format!("活动任务：`{}`\n", staged.len()));
        let summary = ACTIVE_EH_QUEUE_STAGES
            .iter()
            .filter_map(|stage| {
                let count = staged
                    .iter()
                    .filter(|(_, item_stage)| item_stage == stage)
                    .count();
                (count > 0).then(|| format!("{stage} `{count}`"))
            })
            .collect::<Vec<_>>()
            .join(" · ");
        output.push_str(&format!("阶段：{summary}\n\n*任务*\n"));

        for (item, _) in staged.iter().take(MAX_VISIBLE_EH_QUEUE_ITEMS) {
            output.push_str("• ");
            output.push_str(&format_eh_queue_item(item));
            output.push('\n');
        }
        if staged.len() > MAX_VISIBLE_EH_QUEUE_ITEMS {
            output.push_str(&format!(
                "另有 `{}` 项未显示\n",
                staged.len() - MAX_VISIBLE_EH_QUEUE_ITEMS
            ));
        }
        output.pop();
    }

    if let Some(item) = &snapshot.recent_terminal {
        output.push_str("\n\n*最近记录*\n• ");
        output.push_str(&format_eh_queue_item(item));
    }

    output
}
```

Counts use every active item; only the detail section is limited to 20. Background state overrides only primary `pending`, and titles are truncated before escaping.

- [ ] **Step 5: Run formatter tests and capture GREEN**

Run the Step 3 command again. Expected: all four `eh_queue_status` tests pass.

- [ ] **Step 6: Record the review boundary**

Review only formatter behavior and tests in `src/bot/handlers/subscription/ehentai.rs`. Intended commit if permission is granted: `feat: format EH queue status`.

### Task 3: Register, route, and send `/estatus`

**Files:**
- Modify/Test: `src/bot/commands.rs:4-140,143-247`
- Modify: `src/bot/handler.rs:118-175`
- Modify: `src/bot/handlers/subscription/ehentai.rs`

- [ ] **Step 1: Add failing command parsing and visibility tests**

Inside `src/bot/commands.rs` tests, import the parsing trait and add:

```rust
use teloxide::utils::command::BotCommands;

#[test]
fn estatus_parses_as_no_argument_command() {
    assert!(matches!(Command::parse("/estatus", ""), Ok(Command::EStatus)));
    assert!(Command::parse("/estatus unexpected", "").is_err());
}

#[test]
fn estatus_visibility_follows_eh_configuration_for_all_roles() {
    for commands in [
        Command::user_commands(false, false),
        Command::admin_commands(false, false),
        Command::owner_commands(false, false),
    ] {
        assert!(!command_names(commands).iter().any(|name| name == "estatus"));
    }

    for commands in [
        Command::user_commands(false, true),
        Command::admin_commands(false, true),
        Command::owner_commands(false, true),
    ] {
        assert!(command_names(commands).iter().any(|name| name == "estatus"));
    }
}
```

Also add `"estatus"` to both existing EH include/omit arrays.

- [ ] **Step 2: Run command tests and capture RED**

Run:

```powershell
cargo test -p pixivbot estatus -- --nocapture
```

Expected: compilation fails because `Command::EStatus` does not exist.

- [ ] **Step 3: Add the command variant and menu item**

Add beside the existing EH variants:

```rust
#[command(description = "查看当前聊天的 E-Hentai 下载队列")]
EStatus,
```

Add to the `has_ehentai` menu extension:

```rust
BotCommand::new("estatus", "查看当前聊天的EH下载队列"),
```

Admin and owner menus inherit it through `user_commands()`.

- [ ] **Step 4: Add the Telegram handler**

Add to the EH `impl BotHandler` in `src/bot/handlers/subscription/ehentai.rs`:

```rust
pub async fn handle_estatus(
    &self,
    bot: ThrottledBot,
    chat_id: ChatId,
) -> ResponseResult<()> {
    if self.eh_client.is_none() {
        let _ = bot.send_message(chat_id, "E-Hentai 功能未启用").await;
        return Ok(());
    }

    let snapshot = match self.repo.get_eh_queue_snapshot(chat_id.0).await {
        Ok(snapshot) => snapshot,
        Err(e) => {
            error!(
                "Failed to get EH queue status for chat {}: {:#}",
                chat_id, e
            );
            let _ = bot
                .send_message(chat_id, "❌ 获取 EH 下载队列状态失败，请稍后重试")
                .await;
            return Ok(());
        }
    };

    let _ = bot
        .send_message(chat_id, format_eh_queue_status(&snapshot))
        .parse_mode(ParseMode::MarkdownV2)
        .await;
    Ok(())
}
```

The handler accepts no target-chat argument, logs the complete repository error chain, and never includes it in the user response.

- [ ] **Step 5: Route the command**

Add in the EH section of `BotHandler::dispatch_command()`:

```rust
Command::EStatus => self.handle_estatus(bot, chat_id).await,
```

Do not modify `build_handler_tree()` or add a middleware bypass.

- [ ] **Step 6: Run command tests and capture GREEN**

Run the Step 2 command again. Expected: both `estatus` tests pass.

- [ ] **Step 7: Run all focused feature tests**

Run:

```powershell
cargo test -p pixivbot eh_queue_status -- --nocapture
```

Expected: repository and formatter/status tests pass with no failures.

- [ ] **Step 8: Record the review boundary**

Review command visibility, dispatch, and the handler together. Intended commit if permission is granted: `feat: add EH queue status command`.

### Task 4: Verify the complete change

**Files:**
- Verify all modified Rust files and both design/plan documents.

- [ ] **Step 1: Format the workspace**

Run:

```powershell
cargo fmt --all
```

Expected: exit code 0.

- [ ] **Step 2: Run the focused repository test**

```powershell
cargo test -p pixivbot test_eh_queue_status_snapshot_scopes_orders_and_selects_recent_terminal -- --nocapture
```

Expected: `1 passed; 0 failed`.

- [ ] **Step 3: Run all queue-status tests**

```powershell
cargo test -p pixivbot eh_queue_status -- --nocapture
```

Expected: all queue snapshot and formatting tests pass.

- [ ] **Step 4: Run command registration tests**

```powershell
cargo test -p pixivbot estatus -- --nocapture
```

Expected: command parsing and role-menu visibility tests pass.

- [ ] **Step 5: Run the repository quality gate**

```powershell
make ci
```

Expected: format check, clippy with warnings denied, check, tests, and release build all succeed. If the local FFmpeg development environment prevents the release build, preserve the exact failure and still report the focused test results rather than claiming full verification.

- [ ] **Step 6: Check patch hygiene**

```powershell
git diff --check
git status --short
```

Expected: `git diff --check` has no output; status lists only the intended implementation, tests, spec, and plan. Do not stage or commit without explicit user permission.

## Final review correction applied

- Treat 20 active details as an upper bound and rebuild the complete output with successively fewer details until `message.encode_utf16().count() <= 4096`.
- Keep the recent terminal record visible and calculate omitted rows as `snapshot.active.len() - visible_active_count` so budget-driven omissions remain accurate.
- Cover 20 maximum-length non-BMP active titles plus a maximum-length recent title; assert the final UTF-16 length, visible-plus-hidden count, and recent record.
- Propagate the final successful `send_message` result with `await?` instead of silently discarding a Telegram rejection.

## Self-review

- Spec coverage: Tasks 1-3 implement current-chat isolation, all active stages, background precedence, a recent terminal record, bounded details, MarkdownV2, generic errors, and EH-dependent visibility.
- Placeholder scan: every edit step contains exact code, commands, and expected outcomes; there are no deferred implementation sections.
- Type consistency: repository and handler consistently use `EhQueueSnapshot`, `EhQueueStatusItem`, `active`, and `recent_terminal`; `/estatus` maps to `Command::EStatus` and `handle_estatus()`.
- Scope: no migrations, dependencies, config, worker state changes, queue mutation, global view, cancellation, retry, or network test is included.
- Git policy: commit messages are recorded only as future boundaries; no Git write is authorized by this plan.
