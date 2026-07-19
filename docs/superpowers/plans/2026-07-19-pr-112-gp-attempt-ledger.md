# PR #112 EH GP Attempt Ledger Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在任何正 GP `archiver.php` POST 前持久化 append-only spend attempt，使失败 POST 后的重试仍受 rolling GP budget 约束，同时保留 queue `gp_cost` 作为最近一次成功下载的 metadata。

**Architecture:** 新增 `eh_gp_spend_attempts` ledger，由 migration 回填历史 queue 成功支出；所有 rolling GP 聚合随后只读取 ledger。主下载和后台下载通过同一个 check-and-reserve helper，在启用 rolling budget 时于进程级 mutex 内完成预算检查和 append，随后释放 mutex 再执行网络请求；禁用 rolling budget 时仍 append，但不加锁、不查询窗口。

**Tech Stack:** Rust 1.94、SeaORM/SeaORM Migration 1.1.20、SeaQuery 0.32.7、SQLite、Tokio、Chrono、Wiremock、Anyhow。

---

## 依赖顺序

1. **Task 1**：建立 migration、entity、测试数据库 schema，并验证回填/FK/CHECK/down。
2. **Task 2**：实现 ledger append 与 rolling aggregate Repo API。
3. **Task 3**：将主/后台 worker 改为 POST 前 reservation，并加入失败路径回归测试。
4. **Task 4**：迁移旧 GP 测试、强化并发断言、更新 queue metadata 注释并执行最终质量门禁。

Task 2 依赖 Task 1；Task 3 依赖 Task 2；Task 4 依赖前三项。

## 文件地图

| 文件 | 操作 | 职责 |
|---|---|---|
| `migration/src/m20260719_000000_eh_gp_spend_attempts.rs` | Create | 建表、索引、历史回填及 down |
| `migration/src/lib.rs` | Modify | 注册 ledger migration，并保证它位于 GP queue migration 之后 |
| `src/db/entities/eh_gp_spend_attempts.rs` | Create | SeaORM ledger entity 和 nullable queue relation |
| `src/db/entities/mod.rs` | Modify | 注册 entity |
| `src/db/repo/eh_gp_spend_attempts.rs` | Create | append、窗口聚合及 migration/repo tests |
| `src/db/repo.rs` | Modify | 注册 Repo 模块，并更新 `setup_test_db()` schema |
| `src/db/repo/eh_download_queue.rs` | Modify | 删除旧 queue-based GP aggregate；保留成功 mark 的 `gp_cost` 写入 |
| `src/db/entities/eh_download_queue.rs` | Modify | 将 `gp_cost` 注释改为最近一次成功 metadata |
| `src/scheduler/eh_engine.rs` | Modify | 删除长生命周期 permit；主/后台共享 durable reservation；更新 Wiremock 和并发测试 |

不修改 `Cargo.toml`、配置文件或 retry policy。

---

### Task 1: 建立 ledger schema、entity 和 migration 回填

**Files:**
- Create: `migration/src/m20260719_000000_eh_gp_spend_attempts.rs`
- Modify: `migration/src/lib.rs`
- Create: `src/db/entities/eh_gp_spend_attempts.rs`
- Modify: `src/db/entities/mod.rs`
- Create: `src/db/repo/eh_gp_spend_attempts.rs`
- Modify: `src/db/repo.rs:4-15,32-170`

- [ ] **Step 1: 注册 Repo 测试模块并先写 migration 行为测试**

先在 `src/db/repo.rs` 的模块注册区加入以下声明，确保紧接着创建的测试文件会被 Rust 测试目标加载：

```rust
pub mod eh_gp_spend_attempts;
```

然后在 `src/db/repo/eh_gp_spend_attempts.rs` 中加入测试模块。测试使用 `migration::{Migrator, MigratorTrait}` 运行 ledger 之前的 migrations，再用原始 SQL 插入旧 schema fixture，避免最新 `eh_download_queue::ActiveModel` 与 migration 中间版本发生耦合：

```rust
#[cfg(test)]
mod migration_tests {
    use crate::db::entities::eh_gp_spend_attempts;
    use chrono::NaiveDateTime;
    use migration::{Migrator, MigratorTrait};
    use sea_orm::{
        ConnectionTrait, Database, DbBackend, EntityTrait, Statement,
    };
    use sea_orm_migration::SchemaManager;

    #[tokio::test]
    async fn test_gp_spend_attempt_migration_backfills_and_enforces_schema() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared("PRAGMA foreign_keys = ON")
            .await
            .unwrap();

        let migrations_before_ledger = (Migrator::migrations().len() - 1) as u32;
        Migrator::up(&db, Some(migrations_before_ledger))
            .await
            .unwrap();

        db.execute_unprepared(
            "INSERT INTO eh_download_queue \
                 (chat_id, gid, token, title, telegraph, source, status, \
                  file_size, gp_cost, retry_count, created_at, completed_at) \
             VALUES \
                 (-100, 1001, 'a1b2c3d4e5', 'eligible', 0, 'direct', 'done', \
                  0, 218, 0, '2026-07-19 10:00:00', '2026-07-19 10:00:00'), \
                 (-100, 1002, 'b1c2d3e4f5', 'zero-cost', 0, 'direct', 'done', \
                  0, 0, 0, '2026-07-19 10:01:00', '2026-07-19 10:01:00'), \
                 (-100, 1003, 'c1d2e3f4a5', 'not-completed', 0, 'direct', 'done', \
                  0, 500, 0, '2026-07-19 10:02:00', NULL)",
        )
        .await
        .unwrap();

        let legacy_total = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COALESCE(SUM(gp_cost), 0) AS total \
                 FROM eh_download_queue \
                 WHERE gp_cost > 0 AND completed_at IS NOT NULL"
                    .to_owned(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "total")
            .unwrap();
        assert_eq!(legacy_total, 218);

        Migrator::up(&db, Some(1)).await.unwrap();

        let attempts = eh_gp_spend_attempts::Entity::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(attempts[0].queue_id.is_some());
        assert_eq!(attempts[0].gid, 1001);
        assert_eq!(attempts[0].gp_cost, 218);
        assert_eq!(
            attempts[0].created_at,
            NaiveDateTime::parse_from_str(
                "2026-07-19 10:00:00",
                "%Y-%m-%d %H:%M:%S",
            )
            .unwrap()
        );
        assert_eq!(
            attempts.iter().map(|attempt| attempt.gp_cost).sum::<i64>(),
            legacy_total,
            "migration must preserve the pre-cutover rolling total without duplicating it"
        );

        let indexes = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA index_list('eh_gp_spend_attempts')".to_owned(),
            ))
            .await
            .unwrap();
        assert!(indexes.iter().any(|row| {
            row.try_get::<String>("", "name").unwrap()
                == "idx_eh_gp_spend_attempts_created_at"
        }));

        for (gid, invalid_cost) in [(1004, 0), (1005, -1)] {
            let invalid_insert = db
                .execute_unprepared(&format!(
                    "INSERT INTO eh_gp_spend_attempts \
                         (queue_id, gid, gp_cost, created_at) \
                     VALUES (NULL, {gid}, {invalid_cost}, CURRENT_TIMESTAMP)"
                ))
                .await;
            assert!(
                invalid_insert.is_err(),
                "database CHECK must reject gp_cost={invalid_cost}"
            );
        }

        let queue_id = attempts[0].queue_id.unwrap();
        db.execute_unprepared(&format!(
            "DELETE FROM eh_download_queue WHERE id = {queue_id}"
        ))
        .await
        .unwrap();
        let retained = eh_gp_spend_attempts::Entity::find_by_id(attempts[0].id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            retained.queue_id, None,
            "ON DELETE SET NULL must retain ledger history"
        );

        Migrator::down(&db, Some(1)).await.unwrap();
        let schema = SchemaManager::new(&db);
        assert!(!schema.has_table("eh_gp_spend_attempts").await.unwrap());

        let remaining_index_count = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM sqlite_master \
                 WHERE type = 'index' \
                   AND name = 'idx_eh_gp_spend_attempts_created_at'"
                    .to_owned(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "count")
            .unwrap();
        assert_eq!(remaining_index_count, 0);
    }
}
```

- [ ] **Step 2: 运行 RED 测试**

Run:

```powershell
cargo test -p pixivbot db::repo::eh_gp_spend_attempts::migration_tests::test_gp_spend_attempt_migration_backfills_and_enforces_schema -- --exact --nocapture
```

Expected: FAIL，原因包括尚未注册 `eh_gp_spend_attempts` entity/module 或 migration 尚不存在；不能因找不到已有测试而显示 `running 0 tests`。

- [ ] **Step 3: 创建 SeaORM entity**

创建 `src/db/entities/eh_gp_spend_attempts.rs`：

```rust
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "eh_gp_spend_attempts")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(nullable)]
    pub queue_id: Option<i32>,
    pub gid: i64,
    pub gp_cost: i64,
    pub created_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::eh_download_queue::Entity",
        from = "Column::QueueId",
        to = "super::eh_download_queue::Column::Id",
        on_delete = "SetNull"
    )]
    Queue,
}

impl Related<super::eh_download_queue::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Queue.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
```

在 `src/db/entities/mod.rs` 增加：

```rust
pub mod eh_gp_spend_attempts;
```

`src/db/repo.rs` 的模块声明已在 Step 1 为 RED 测试注册，不要重复添加。

- [ ] **Step 4: 实现 migration**

`Cargo.lock` 锁定 `sea-query 0.32.7`；该版本支持 `ColumnDef::check(Expr::col(...).gt(0))`。创建 `migration/src/m20260719_000000_eh_gp_spend_attempts.rs`：

```rust
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(EhGpSpendAttempts::Table)
                    .col(
                        ColumnDef::new(EhGpSpendAttempts::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(EhGpSpendAttempts::QueueId).integer().null())
                    .col(
                        ColumnDef::new(EhGpSpendAttempts::Gid)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EhGpSpendAttempts::GpCost)
                            .big_integer()
                            .not_null()
                            .check(Expr::col(EhGpSpendAttempts::GpCost).gt(0)),
                    )
                    .col(
                        ColumnDef::new(EhGpSpendAttempts::CreatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_eh_gp_spend_attempts_queue")
                            .from(
                                EhGpSpendAttempts::Table,
                                EhGpSpendAttempts::QueueId,
                            )
                            .to(EhDownloadQueue::Table, EhDownloadQueue::Id)
                            .on_delete(ForeignKeyAction::SetNull),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_eh_gp_spend_attempts_created_at")
                    .table(EhGpSpendAttempts::Table)
                    .col(EhGpSpendAttempts::CreatedAt)
                    .to_owned(),
            )
            .await?;

        manager
            .get_connection()
            .execute_unprepared(
                "INSERT INTO eh_gp_spend_attempts \
                     (queue_id, gid, gp_cost, created_at) \
                 SELECT id, gid, gp_cost, completed_at \
                 FROM eh_download_queue \
                 WHERE gp_cost > 0 AND completed_at IS NOT NULL",
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(EhGpSpendAttempts::Table)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum EhGpSpendAttempts {
    Table,
    Id,
    QueueId,
    Gid,
    GpCost,
    CreatedAt,
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    Id,
}
```

此顺序必须保持为：**create table → create index → backfill**。`down` 只 drop table；SQLite 会随表移除该表拥有的 index，migration test 对 table 和 index 消失均有断言。

- [ ] **Step 5: 注册 migration**

在 `migration/src/lib.rs` 增加模块：

```rust
mod m20260719_000000_eh_gp_spend_attempts;
```

并将 migration 放在 `m20260718_000000_eh_download_gp_cost` 后：

```rust
Box::new(m20260718_000000_eh_download_gp_cost::Migration),
Box::new(m20260719_000000_eh_gp_spend_attempts::Migration),
```

- [ ] **Step 6: 更新 `setup_test_db()`**

在连接建立后、建表前启用 SQLite foreign keys：

```rust
db.execute_unprepared("PRAGMA foreign_keys = ON").await?;
```

在创建 `eh_download_queue` 后增加：

```rust
db.execute(Statement::from_string(
    DbBackend::Sqlite,
    r#"
    CREATE TABLE eh_gp_spend_attempts (
        id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
        queue_id INTEGER,
        gid INTEGER NOT NULL,
        gp_cost INTEGER NOT NULL CHECK (gp_cost > 0),
        created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (queue_id)
            REFERENCES eh_download_queue(id)
            ON DELETE SET NULL
    )
    "#,
))
.await?;

db.execute(Statement::from_string(
    DbBackend::Sqlite,
    r#"
    CREATE INDEX idx_eh_gp_spend_attempts_created_at
    ON eh_gp_spend_attempts(created_at)
    "#,
))
.await?;
```

- [ ] **Step 7: 运行 GREEN 测试**

Run:

```powershell
cargo test -p pixivbot db::repo::eh_gp_spend_attempts::migration_tests::test_gp_spend_attempt_migration_backfills_and_enforces_schema -- --exact --nocapture
cargo test -p migration --all-targets
```

Expected: 两个命令均 PASS；migration test 验证：
- 只回填 `gp_cost > 0 AND completed_at IS NOT NULL`；
- `created_at` 等于旧 `completed_at`；
- 回填前后总额相等；
- `created_at` index 存在；
- `gp_cost=0` 与 `gp_cost=-1` 均被 schema CHECK 拒绝；
- 删除 queue 后 ledger 保留且 `queue_id=NULL`；
- down 删除表，SQLite 同时删除 index。

- [ ] **Step 8: 建议 review boundary**

不执行 `git add`、commit 或其他 Git 写操作。建议语义提交边界：

```text
feat: add EH GP spend attempt ledger schema
```

范围：Task 1 的六个文件。

---

### Task 2: 实现 append 与 ledger-only rolling aggregate

**Files:**
- Modify: `src/db/repo/eh_gp_spend_attempts.rs`
- Modify: `src/db/repo/eh_download_queue.rs:663-699`

- [ ] **Step 1: 写 Repo API 的 RED 测试**

在 `src/db/repo/eh_gp_spend_attempts.rs` 增加：

```rust
#[cfg(test)]
mod tests {
    use crate::db::entities::{eh_download_queue, eh_gp_spend_attempts};
    use crate::db::repo::eh_download_queue::{SOURCE_DIRECT, STATUS_FAILED};
    use crate::db::repo::tests_helpers::setup_test_db;
    use chrono::Local;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

    async fn insert_queue(
        repo: &super::Repo,
        gid: i64,
        status: &str,
        gp_cost: i64,
    ) -> eh_download_queue::Model {
        eh_download_queue::ActiveModel {
            chat_id: Set(-100),
            gid: Set(gid),
            token: Set(format!("{gid:010x}")),
            title: Set(format!("queue-{gid}")),
            telegraph: Set(false),
            source: Set(SOURCE_DIRECT.to_owned()),
            status: Set(status.to_owned()),
            gp_cost: Set(gp_cost),
            created_at: Set(Local::now().naive_local()),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_append_eh_gp_spend_attempt_rejects_non_positive_cost() {
        let repo = setup_test_db().await.unwrap();
        let queue = insert_queue(&repo, 2001, STATUS_FAILED, 0).await;

        for invalid in [0, -1] {
            let error = repo
                .append_eh_gp_spend_attempt(queue.id, queue.gid, invalid)
                .await
                .expect_err("non-positive GP cost must be rejected");
            assert!(error.to_string().contains("must be positive"));
        }

        let attempts = eh_gp_spend_attempts::Entity::find()
            .all(repo.db())
            .await
            .unwrap();
        assert!(attempts.is_empty());
    }

    #[tokio::test]
    async fn test_get_eh_gp_cost_in_window_reads_only_recent_ledger_rows() {
        let repo = setup_test_db().await.unwrap();

        // A queue metadata value alone must not contribute after cutover.
        let queue = insert_queue(&repo, 2002, STATUS_FAILED, 900).await;

        repo.append_eh_gp_spend_attempt(queue.id, queue.gid, 100)
            .await
            .unwrap();
        repo.append_eh_gp_spend_attempt(queue.id, queue.gid, 250)
            .await
            .unwrap();

        eh_gp_spend_attempts::ActiveModel {
            queue_id: Set(Some(queue.id)),
            gid: Set(queue.gid),
            gp_cost: Set(500),
            created_at: Set(
                Local::now().naive_local() - chrono::Duration::hours(48),
            ),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap();

        assert_eq!(repo.get_eh_gp_cost_in_window(24).await.unwrap(), 350);

        let attempts = eh_gp_spend_attempts::Entity::find()
            .all(repo.db())
            .await
            .unwrap();
        assert_eq!(attempts.len(), 3);
    }
}
```

- [ ] **Step 2: 运行 RED 测试**

Run:

```powershell
cargo test -p pixivbot db::repo::eh_gp_spend_attempts::tests -- --nocapture
```

Expected: FAIL，提示 `append_eh_gp_spend_attempt` 尚不存在，且旧 `get_eh_gp_cost_in_window` 仍读取 queue。

- [ ] **Step 3: 实现 Repo API**

在测试模块之前加入：

```rust
use super::Repo;
use crate::db::entities::eh_gp_spend_attempts;
use anyhow::{bail, Context, Result};
use chrono::Local;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set,
};

impl Repo {
    pub async fn append_eh_gp_spend_attempt(
        &self,
        queue_id: i32,
        gid: i64,
        gp_cost: i64,
    ) -> Result<eh_gp_spend_attempts::Model> {
        if gp_cost <= 0 {
            bail!("EH GP spend attempt cost must be positive, got {gp_cost}");
        }

        eh_gp_spend_attempts::ActiveModel {
            queue_id: Set(Some(queue_id)),
            gid: Set(gid),
            gp_cost: Set(gp_cost),
            created_at: Set(Local::now().naive_local()),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .with_context(|| {
            format!(
                "Failed to append EH GP spend attempt for queue_id={queue_id}, gid={gid}, gp_cost={gp_cost}"
            )
        })
    }

    pub async fn get_eh_gp_cost_in_window(&self, hours: u64) -> Result<i64> {
        let cutoff =
            Local::now().naive_local() - chrono::Duration::hours(hours as i64);

        let attempts = eh_gp_spend_attempts::Entity::find()
            .filter(eh_gp_spend_attempts::Column::CreatedAt.gte(cutoff))
            .all(&self.db)
            .await
            .context("Failed to fetch EH GP spend attempts in window")?;

        attempts.into_iter().try_fold(0_i64, |total, attempt| {
            total
                .checked_add(attempt.gp_cost)
                .context("EH GP spend total overflowed i64")
        })
    }
}
```

该 append 是独立数据库写入；成功返回即表示 reservation durable。它不得依赖后续 queue mark。

- [ ] **Step 4: 删除旧 queue aggregate**

从 `src/db/repo/eh_download_queue.rs` 删除原有：

```rust
pub async fn get_eh_gp_cost_in_window(&self, hours: u64) -> Result<i64>
```

完整删除其 queue `completed_at` 查询。不要保留 queue+ledger 汇总或兼容 fallback。

保留：
- `get_eh_downloaded_bytes_in_window`；
- `mark_eh_download_downloaded(..., gp_cost: i64)`；
- `mark_eh_background_download_downloaded(..., gp_cost: i64)`；
- 两个 mark 中对 queue `GpCost` 的写入。

- [ ] **Step 5: 运行 GREEN 测试**

Run:

```powershell
cargo test -p pixivbot db::repo::eh_gp_spend_attempts::tests -- --nocapture
cargo test -p pixivbot db::repo::eh_gp_spend_attempts::migration_tests -- --nocapture
```

Expected: PASS。窗口总额必须为 `350`，而不是 queue metadata `900`、ledger 全部 `850` 或 queue+ledger 的组合值。

- [ ] **Step 6: 建议 review boundary**

不执行 `git add`、commit 或其他 Git 写操作。建议语义提交边界：

```text
feat: add EH GP spend attempt repository
```

范围：
- `src/db/repo/eh_gp_spend_attempts.rs`
- `src/db/repo/eh_download_queue.rs`

---

### Task 3: 在 POST 前共享 check-and-reserve，并覆盖失败 reservation

**Files:**
- Modify: `src/scheduler/eh_engine.rs:17-190,262-385,1108-1180`
- Test: `src/scheduler/eh_engine.rs` 内 `integration_tests`

- [ ] **Step 1: 增加 ledger test helper/import**

将 integration test entity import 改为：

```rust
use crate::db::entities::{eh_download_queue, eh_gp_spend_attempts};
```

增加：

```rust
async fn load_gp_attempts(repo: &Repo) -> Vec<eh_gp_spend_attempts::Model> {
    eh_gp_spend_attempts::Entity::find()
        .all(repo.db())
        .await
        .unwrap()
}
```

- [ ] **Step 2: 写 main malformed redirect RED 测试**

测试 token 使用现有 fixture 接受的十六进制格式：

```rust
#[tokio::test]
async fn test_main_paid_attempt_is_retained_after_malformed_redirect() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, true).await;
    let entry = insert_queue_entry(
        &repo,
        -100,
        6101,
        "a1b2c3d4e5",
        "Malformed paid main archive",
        false,
        STATUS_PENDING,
        None,
        None,
    )
    .await;

    mock_eh_archiver_page_with_cost(
        &eh_server,
        6101,
        "a1b2c3d4e5",
        "218 GP",
        "218 GP",
    )
    .await;
    mock_eh_metadata(&eh_server, 6101, "a1b2c3d4e5", 10 * 1024 * 1024)
        .await;

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<html><body>archive request accepted</body></html>"),
        )
        .expect(1)
        .mount(&eh_server)
        .await;

    let mut config = make_config();
    config.background_download_enabled = false;
    config.max_archive_gp_cost = 218;
    config.gp_rate_limit = 0;

    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
    );
    worker.tick().await.unwrap();

    let updated = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, STATUS_PENDING);
    assert_eq!(updated.retry_count, 1);
    assert_eq!(updated.gp_cost, 0);

    let attempts = load_gp_attempts(&repo).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].queue_id, Some(entry.id));
    assert_eq!(attempts[0].gid, entry.gid);
    assert_eq!(attempts[0].gp_cost, 218);
}
```

- [ ] **Step 3: 写 background malformed redirect RED 测试**

```rust
#[tokio::test]
async fn test_background_paid_attempt_is_retained_after_malformed_redirect() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    let entry = insert_queue_entry(
        &repo,
        -100,
        6102,
        "b1c2d3e4f5",
        "Malformed paid background archive",
        false,
        STATUS_PENDING,
        None,
        None,
    )
    .await;
    repo.schedule_eh_background_download_from(
        entry.id,
        STATUS_PENDING,
        "test setup",
    )
    .await
    .unwrap();

    mock_eh_archiver_page_with_cost(
        &eh_server,
        6102,
        "b1c2d3e4f5",
        "218 GP",
        "218 GP",
    )
    .await;
    mock_eh_metadata(&eh_server, 6102, "b1c2d3e4f5", 10 * 1024 * 1024)
        .await;

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<html><body>archive request accepted</body></html>"),
        )
        .expect(1)
        .mount(&eh_server)
        .await;

    let mut config = make_config();
    config.background_download_enabled = true;
    config.background_download_concurrency = 1;
    config.max_archive_gp_cost = 218;
    config.gp_rate_limit = 0;

    let worker = EhBackgroundDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
    );
    worker.tick().await.unwrap();

    let updated = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, STATUS_PENDING);
    assert_eq!(
        updated.background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_PENDING)
    );
    assert_eq!(updated.background_download_attempt_count, 1);
    assert_eq!(updated.gp_cost, 0);

    let attempts = load_gp_attempts(&repo).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].queue_id, Some(entry.id));
    assert_eq!(attempts[0].gid, entry.gid);
    assert_eq!(attempts[0].gp_cost, 218);
}
```

- [ ] **Step 4: 写 reservation DB failure、POST=0 RED 测试**

```rust
#[tokio::test]
async fn test_paid_reservation_insert_failure_sends_no_archive_post() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, true).await;
    let entry = insert_queue_entry(
        &repo,
        -100,
        6103,
        "c1d2e3f4a5",
        "Reservation failure",
        false,
        STATUS_PENDING,
        None,
        None,
    )
    .await;

    mock_eh_archiver_page_with_cost(
        &eh_server,
        6103,
        "c1d2e3f4a5",
        "218 GP",
        "218 GP",
    )
    .await;
    mock_eh_metadata(&eh_server, 6103, "c1d2e3f4a5", 10 * 1024 * 1024)
        .await;

    // gp_rate_limit=0 means the append is the first ledger operation.
    repo.db()
        .execute_unprepared("DROP TABLE eh_gp_spend_attempts")
        .await
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&eh_server)
        .await;

    let mut config = make_config();
    config.background_download_enabled = false;
    config.max_archive_gp_cost = 218;
    config.gp_rate_limit = 0;

    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
    );
    worker.tick().await.unwrap();

    let updated = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, STATUS_PENDING);
    assert_eq!(updated.retry_count, 1);
    assert_eq!(updated.gp_cost, 0);
    assert!(updated
        .error
        .as_deref()
        .unwrap()
        .contains("Failed to reserve EH GP spend attempt"));

    let post_count = eh_server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| {
            request.method.as_str() == "POST"
                && request.url.path() == "/archiver.php"
        })
        .count();
    assert_eq!(post_count, 0);
}
```

- [ ] **Step 5: 替换 permit 测试为原子 check-and-reserve 测试**

删除 `test_archive_gp_permit_holds_lock_until_future_completes` 和旧 `test_check_archive_cost_serializes_paid_budget_until_spend_is_recorded`，先加入使用新签名的测试：

```rust
#[tokio::test]
async fn test_check_and_reserve_allows_only_one_concurrent_paid_attempt() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let first = insert_queue_entry(
        &repo, -100, 6201, "d1e2f3a4b5", "first", false,
        STATUS_PENDING, None, None,
    )
    .await;
    let second = insert_queue_entry(
        &repo, -100, 6202, "e1f2a3b4c5", "second", false,
        STATUS_PENDING, None, None,
    )
    .await;

    let mut config = make_config();
    config.max_archive_gp_cost = 200;
    config.gp_rate_limit = 300;
    let config = Arc::new(config);

    let (first_result, second_result) = tokio::join!(
        check_and_reserve_archive_cost_or_defer(
            repo.as_ref(),
            config.as_ref(),
            first.id,
            first.gid,
            &DownloadCost::Gp(200),
        ),
        check_and_reserve_archive_cost_or_defer(
            repo.as_ref(),
            config.as_ref(),
            second.id,
            second.gid,
            &DownloadCost::Gp(200),
        ),
    );

    let results = [first_result.unwrap(), second_result.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, ArchiveCostCheck::Proceed))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, ArchiveCostCheck::Defer { .. }))
            .count(),
        1
    );

    let attempts = load_gp_attempts(&repo).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].gp_cost, 200);
    assert!([Some(first.id), Some(second.id)].contains(&attempts[0].queue_id));
}

#[tokio::test]
async fn test_free_unlocked_zero_and_disabled_budget_reservation_rules() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let entry = insert_queue_entry(
        &repo, -100, 6203, "f1a2b3c4d5", "retry", false,
        STATUS_PENDING, None, None,
    )
    .await;

    let _budget_lock = Arc::clone(&*EH_GP_BUDGET_LOCK).lock_owned().await;

    let mut disabled = make_config();
    disabled.max_archive_gp_cost = 200;
    disabled.gp_rate_limit = 0;

    for _ in 0..2 {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            check_and_reserve_archive_cost_or_defer(
                repo.as_ref(),
                &disabled,
                entry.id,
                entry.gid,
                &DownloadCost::Gp(200),
            ),
        )
        .await
        .expect("disabled rolling budget must not wait for the mutex")
        .unwrap();
        assert!(matches!(result, ArchiveCostCheck::Proceed));
    }

    let mut enabled = make_config();
    enabled.max_archive_gp_cost = 200;
    enabled.gp_rate_limit = 300;

    for cost in [
        DownloadCost::Free,
        DownloadCost::Unlocked,
        DownloadCost::Gp(0),
    ] {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            check_and_reserve_archive_cost_or_defer(
                repo.as_ref(),
                &enabled,
                entry.id,
                entry.gid,
                &cost,
            ),
        )
        .await
        .expect("free, unlocked and zero-GP costs must bypass the mutex")
        .unwrap();
        assert!(
            matches!(result, ArchiveCostCheck::Proceed),
            "Gp(0) is allowed when max_archive_gp_cost > 0 but must not be recorded"
        );
    }

    let attempts = load_gp_attempts(&repo).await;
    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().all(|attempt| {
        attempt.queue_id == Some(entry.id) && attempt.gp_cost == 200
    }));
    assert_eq!(repo.get_eh_gp_cost_in_window(24).await.unwrap(), 400);
}
```

两次 disabled-budget 调用代表同一 queue 的两个获准尝试，必须生成两行，不能覆盖或复用第一行。`DownloadCost::Gp(0)` 在 `max_archive_gp_cost > 0` 时必须 `Proceed`，但因为不是正 GP，ledger 行数不得增加。

- [ ] **Step 6: 运行 scheduler RED 测试**

Run:

```powershell
cargo test -p pixivbot test_main_paid_attempt_is_retained_after_malformed_redirect -- --nocapture
cargo test -p pixivbot test_background_paid_attempt_is_retained_after_malformed_redirect -- --nocapture
cargo test -p pixivbot test_paid_reservation_insert_failure_sends_no_archive_post -- --nocapture
cargo test -p pixivbot test_check_and_reserve_allows_only_one_concurrent_paid_attempt -- --nocapture
```

Expected:
- 在旧实现上 malformed redirect 测试 FAIL，因为 ledger 没有 reservation；
- DB failure 测试 FAIL，因为旧代码仍会 POST；
- 新 helper 测试在函数名/签名或 `Proceed` variant 上编译失败。

- [ ] **Step 7: 删除 permit 并实现共享 helper**

删除：
- `OwnedMutexGuard` import；
- `ArchiveGpPermit`；
- `ArchiveGpPermit::hold_until`；
- `ArchiveCostCheck::Proceed { permit: ... }` payload。
- 旧 `assert_check_bypasses_gp_budget_lock` helper；
- 旧 `test_check_archive_cost_bypasses_lock_for_free_and_disabled_gp_rate_limit`，其 Free、Unlocked 和禁用 rolling budget 覆盖由 Step 5 的 `test_free_unlocked_zero_and_disabled_budget_reservation_rules` 完整替代。

类型改为：

```rust
enum ArchiveCostCheck {
    Proceed,
    Defer { delay_secs: i64, reason: String },
}
```

共享 helper 改为：

```rust
async fn check_and_reserve_archive_cost_or_defer(
    repo: &Repo,
    config: &EhentaiConfig,
    queue_id: i32,
    gid: i64,
    cost: &DownloadCost,
) -> Result<ArchiveCostCheck> {
    let downloaded_bytes = repo
        .get_eh_downloaded_bytes_in_window(config.download_rate_window_hours)
        .await?;
    if downloaded_bytes >= config.download_rate_limit_bytes() as i64 {
        return Ok(ArchiveCostCheck::Defer {
            delay_secs: config.download_poll_interval_sec.max(60) as i64,
            reason: format!(
                "EH byte rate limit reached ({} bytes in last {}h)",
                downloaded_bytes, config.download_rate_window_hours
            ),
        });
    }

    let positive_gp = match cost {
        DownloadCost::Gp(gp) if *gp > 0 => Some(
            i64::try_from(*gp)
                .context("EH archive GP cost cannot be represented as i64")?,
        ),
        _ => None,
    };

    if let Some(gp_cost) = positive_gp {
        if config.gp_rate_limit > 0 {
            let _budget_guard =
                Arc::clone(&*EH_GP_BUDGET_LOCK).lock_owned().await;
            let window_hours = config.gp_rate_window_hours_clamped();
            let spent = repo.get_eh_gp_cost_in_window(window_hours).await?;
            let projected = i128::from(spent) + i128::from(gp_cost);
            let limit = i128::from(config.gp_rate_limit);

            if projected > limit {
                return Ok(ArchiveCostCheck::Defer {
                    delay_secs: window_hours as i64 * 3600 / 4,
                    reason: format!(
                        "EH GP rate limit would be exceeded ({} + {} > {} in last {}h)",
                        spent, gp_cost, config.gp_rate_limit, window_hours
                    ),
                });
            }

            if !config.allows_archive_gp_cost(cost) {
                return Ok(ArchiveCostCheck::Defer {
                    delay_secs: config.download_poll_interval_sec.max(60) as i64,
                    reason: format!(
                        "EH archive GP cost {:?} exceeds configured max_archive_gp_cost={}",
                        cost, config.max_archive_gp_cost
                    ),
                });
            }

            repo.append_eh_gp_spend_attempt(queue_id, gid, gp_cost)
                .await
                .with_context(|| {
                    format!(
                        "Failed to reserve EH GP spend attempt for queue_id={queue_id}, gid={gid}, gp_cost={gp_cost}"
                    )
                })?;
            return Ok(ArchiveCostCheck::Proceed);
        }
    }

    if !config.allows_archive_gp_cost(cost) {
        return Ok(ArchiveCostCheck::Defer {
            delay_secs: config.download_poll_interval_sec.max(60) as i64,
            reason: format!(
                "EH archive GP cost {:?} exceeds configured max_archive_gp_cost={}",
                cost, config.max_archive_gp_cost
            ),
        });
    }

    if let Some(gp_cost) = positive_gp {
        repo.append_eh_gp_spend_attempt(queue_id, gid, gp_cost)
            .await
            .with_context(|| {
                format!(
                    "Failed to reserve EH GP spend attempt for queue_id={queue_id}, gid={gid}, gp_cost={gp_cost}"
                )
            })?;
    }

    Ok(ArchiveCostCheck::Proceed)
}
```

类型和溢出约束：
- `DownloadCost::Gp` 内部值为 `u64`；只通过 `i64::try_from(*gp)` 进入 ledger API；
- Repo ledger `gp_cost` 为 `i64`，且方法边界拒绝 `<= 0`；
- rolling budget 使用 `i128` 计算 `spent + gp_cost`，不会在比较前发生 `i64`/`u64` 溢出；
- `Gp(0)` 不进入 `positive_gp`，仍经过 `allows_archive_gp_cost`，因此 `max_archive_gp_cost > 0` 时 `Proceed` 但不 append。

行为边界：
- rolling enabled：mutex 内执行 sum、单次上限检查和 append；
- append 成功后 helper 返回，mutex 立即释放；
- rolling disabled：不碰 mutex、不读窗口，但仍 append 正 GP；
- Free、Unlocked、`Gp(0)` 不 append；
- append error 直接返回，调用方不能进入 POST；
- reservation 不随后续失败删除。

- [ ] **Step 8: 更新后台 worker**

`BackgroundDownloadOutcome::Completed` 删除 `permit` 字段：

```rust
Completed {
    file_size: u64,
    zip_path: std::path::PathBuf,
    gp_cost: i64,
}
```

在 `download_claimed` 中：

```rust
match check_and_reserve_archive_cost_or_defer(
    self.repo.as_ref(),
    self.config.as_ref(),
    entry.id,
    entry.gid,
    archive_request.cost(),
)
.await?
{
    ArchiveCostCheck::Proceed => {}
    ArchiveCostCheck::Defer { delay_secs, reason } => {
        self.repo
            .defer_eh_background_download(entry.id, delay_secs, &reason)
            .await?;
        return Ok(BackgroundDownloadOutcome::Deferred { reason });
    }
}
```

返回 tuple 改为：

```rust
let gp_cost = archive_request
    .cost()
    .gp_amount()
    .map(i64::try_from)
    .transpose()
    .context("EH archive GP cost cannot be represented as i64")?
    .unwrap_or(0);
(downloaded_file_size, gp_cost)
```

未登录分支改为：

```rust
(file_size, 0)
```

`process_claimed` 成功分支直接 mark：

```rust
self.repo
    .mark_eh_background_download_downloaded(
        entry.id,
        file_size as i64,
        &zip_path.to_string_lossy(),
        gp_cost,
    )
    .await?;
```

- [ ] **Step 9: 更新主 worker**

主 worker tuple 改为：

```rust
let (file_size, gp_cost) = if self.client.is_logged_in() {
```

调用 helper：

```rust
match check_and_reserve_archive_cost_or_defer(
    self.repo.as_ref(),
    self.config.as_ref(),
    entry.id,
    entry.gid,
    archive_request.cost(),
)
.await?
{
    ArchiveCostCheck::Proceed => {}
    ArchiveCostCheck::Defer { delay_secs, reason } => {
        info!(
            "Deferring EH download for gid={} ({}), no GP spent",
            gid, reason
        );
        self.repo
            .defer_eh_download(entry.id, STATUS_PENDING, delay_secs)
            .await?;
        return Ok(());
    }
}
```

下载成功后返回：

```rust
let gp_cost = archive_request
    .cost()
    .gp_amount()
    .map(i64::try_from)
    .transpose()
    .context("EH archive GP cost cannot be represented as i64")?
    .unwrap_or(0);
(downloaded_file_size, gp_cost)
```

未登录分支返回：

```rust
(file_size, 0)
```

成功 mark 不再通过 permit：

```rust
self.repo
    .mark_eh_download_downloaded(
        entry.id,
        file_size as i64,
        &zip_path_str,
        gp_cost,
    )
    .await?;
```

- [ ] **Step 10: 运行 GREEN 测试**

Run:

```powershell
cargo test -p pixivbot test_main_paid_attempt_is_retained_after_malformed_redirect -- --nocapture
cargo test -p pixivbot test_background_paid_attempt_is_retained_after_malformed_redirect -- --nocapture
cargo test -p pixivbot test_paid_reservation_insert_failure_sends_no_archive_post -- --nocapture
cargo test -p pixivbot test_check_and_reserve_allows_only_one_concurrent_paid_attempt -- --nocapture
cargo test -p pixivbot test_free_unlocked_zero_and_disabled_budget_reservation_rules -- --nocapture
```

Expected: 全部 PASS。两个 malformed redirect 测试各有一条 ledger row 且 queue `gp_cost=0`；DB failure 测试 POST count 为零；并发 helper 只写一行；disabled budget 的两个调用写两行；Free、Unlocked、`Gp(0)` 均 `Proceed` 且不增加 ledger 行数。

- [ ] **Step 11: 建议 review boundary**

不执行 `git add`、commit 或其他 Git 写操作。建议语义提交边界：

```text
fix: reserve EH GP before archive POST
```

范围：`src/scheduler/eh_engine.rs`。

---

### Task 4: 迁移旧测试、强化并发证明并执行完整质量门禁

**Files:**
- Modify: `src/scheduler/eh_engine.rs:4555-5325`
- Modify: `src/db/entities/eh_download_queue.rs:32-38`
- Modify: `src/db/repo/eh_download_queue.rs:1336-1347`

- [ ] **Step 1: 运行旧假设的 RED 测试**

Run:

```powershell
cargo test -p pixivbot test_get_eh_gp_cost_in_window -- --nocapture
cargo test -p pixivbot test_download_worker_gp_rate_limit_defers_without_post -- --nocapture
```

Expected:
- 旧 queue aggregate 测试 FAIL，因为 queue rows 已不再是 rolling source；
- rate-limit 测试 FAIL 或错误地 POST，因为测试只给 queue `gp_cost` 赋值而没有 ledger row。

这确认旧测试必须迁移，不能通过恢复 queue fallback 修复。

- [ ] **Step 2: 删除已被 Repo 测试取代的 queue aggregate tests**

从 `src/scheduler/eh_engine.rs` 删除：
- `insert_gp_cost_entry`；
- `test_get_eh_gp_cost_in_window_counts_pending_completed_spend`；
- `test_get_eh_gp_cost_in_window_excludes_null_completed_at`；
- `test_get_eh_gp_cost_in_window_excludes_old_entries`。

这些行为已由 Task 2 的 ledger Repo tests 覆盖。

- [ ] **Step 3: 将 rate-limit 预填改为 ledger append**

在 `test_download_worker_gp_rate_limit_defers_without_post` 中保留历史 queue row，但取得 insert 结果并追加 ledger：

```rust
let spent = spent.insert(repo.db()).await.unwrap();
repo.append_eh_gp_spend_attempt(spent.id, spent.gid, 1000)
    .await
    .unwrap();
```

保持：
- 新请求 cost `218`；
- `gp_rate_limit=1000`；
- POST `.expect(0)`；
- queue defer 且不增加 retry。

再增加：

```rust
assert_eq!(load_gp_attempts(&repo).await.len(), 1);
```

确保被 defer 的新请求没有写第二行。

- [ ] **Step 4: 更新成功、免费和单次上限测试断言**

在 `test_download_worker_free_cost_proceeds_with_post` 增加：

```rust
assert!(load_gp_attempts(&repo).await.is_empty());
```

在 `test_download_worker_gp_cost_within_limit_proceeds` 增加：

```rust
let attempts = load_gp_attempts(&repo).await;
assert_eq!(attempts.len(), 1);
assert_eq!(attempts[0].queue_id, Some(entry.id));
assert_eq!(attempts[0].gp_cost, 218);
```

在以下 defer tests 增加 ledger 为空断言：

```rust
assert!(load_gp_attempts(&repo).await.is_empty());
```

目标测试：
- `test_download_worker_gp_cost_exceeds_limit_defers_without_post`
- `test_background_worker_gp_cost_exceeds_defers_without_retry_increment`
- `test_download_worker_unknown_cost_defers_without_post`
- `test_download_worker_original_resolution_gp_cost_defers`

- [ ] **Step 5: 强化现有后台并发测试**

在 `test_background_gp_rate_limit_allows_only_one_post` 中保留：
- 恰好一个 queue 进入 `STATUS_DOWNLOADED`；
- 另一个保持可重试；
- aggregate 为 `218`；
- POST count 为 `1`。

增加：

```rust
let attempts = load_gp_attempts(&repo).await;
assert_eq!(attempts.len(), 1);
assert_eq!(attempts[0].gp_cost, 218);
assert!(
    [Some(first.id), Some(second.id)].contains(&attempts[0].queue_id)
);
```

- [ ] **Step 6: 强化主/后台共享锁并发测试**

在 `test_main_and_background_gp_rate_limit_allows_only_one_post` 增加：

```rust
let attempts = load_gp_attempts(&repo).await;
assert_eq!(attempts.len(), 1);
assert_eq!(attempts[0].gp_cost, 218);
assert!(
    [Some(main_entry.id), Some(background_entry.id)]
        .contains(&attempts[0].queue_id)
);
```

保留现有：
- 恰好一个 worker 下载；
- 另一个 worker 被 defer 且仍可处理；
- aggregate 为 `218`；
- POST count 为 `1`。

这证明 mutex 保护的是 **ledger check+append**，而不是网络请求或 queue mark。

- [ ] **Step 7: 更新 queue `gp_cost` 语义注释**

在 `src/db/entities/eh_download_queue.rs` 改为：

```rust
/// GP cost of the most recent successful archive download
/// (0 for free / unlocked downloads).
/// Rolling GP budget accounting uses `eh_gp_spend_attempts`, not this metadata.
#[sea_orm(default = 0)]
pub gp_cost: i64,
```

在 `mark_eh_download_downloaded` 上方改为：

```rust
/// `gp_cost` records the most recent successful archive download as queue
/// metadata. Rolling GP budget accounting is sourced from
/// `eh_gp_spend_attempts` before the archive POST.
```

不要删除两个成功 mark 中对 `eh_download_queue::Column::GpCost` 的更新。

- [ ] **Step 8: 运行 focused GREEN 测试**

Run:

```powershell
cargo test -p pixivbot db::repo::eh_gp_spend_attempts -- --nocapture
cargo test -p pixivbot test_download_worker_gp_cost -- --nocapture
cargo test -p pixivbot test_download_worker_free_cost_proceeds_with_post -- --nocapture
cargo test -p pixivbot test_download_worker_gp_rate_limit_defers_without_post -- --nocapture
cargo test -p pixivbot test_main_paid_attempt_is_retained_after_malformed_redirect -- --nocapture
cargo test -p pixivbot test_background_paid_attempt_is_retained_after_malformed_redirect -- --nocapture
cargo test -p pixivbot test_paid_reservation_insert_failure_sends_no_archive_post -- --nocapture
cargo test -p pixivbot test_background_gp_rate_limit_allows_only_one_post -- --nocapture
cargo test -p pixivbot test_main_and_background_gp_rate_limit_allows_only_one_post -- --nocapture
cargo test -p migration --all-targets
```

Expected: 全部 PASS；每个带测试过滤器的命令必须实际运行至少一个目标测试，不能静默得到 `0 tests`。

- [ ] **Step 9: 执行 Rustfmt 和 LSP diagnostics**

Run:

```powershell
cargo fmt --all -- --check
```

Expected: exit code 0，无格式 diff。

对以下文件运行 `lsp_diagnostics`，severity 使用 `all`：

```text
migration/src/m20260719_000000_eh_gp_spend_attempts.rs
migration/src/lib.rs
src/db/entities/eh_gp_spend_attempts.rs
src/db/entities/eh_download_queue.rs
src/db/entities/mod.rs
src/db/repo/eh_gp_spend_attempts.rs
src/db/repo/eh_download_queue.rs
src/db/repo.rs
src/scheduler/eh_engine.rs
```

Expected: 每个文件均为零 diagnostics。若当前环境没有已连接的 Rust LSP，必须明确记录“LSP unavailable”，并以随后 `cargo check`、clippy 和 tests 作为编译证据，不能宣称 LSP 已通过。

- [ ] **Step 10: 执行 workspace clippy/check/tests**

PowerShell：

```powershell
$previousRustflags = $env:RUSTFLAGS
$env:RUSTFLAGS = "-Dwarnings"
cargo clippy --workspace --all-targets -- -D warnings
$clippyExit = $LASTEXITCODE
$env:RUSTFLAGS = $previousRustflags
if ($clippyExit -ne 0) { exit $clippyExit }
```

Expected: exit code 0，零 warnings。

随后：

```powershell
cargo check --workspace --all-targets
cargo test --workspace --all-targets
```

Expected: 两个命令均 exit code 0；migration、Repo、scheduler 和其他 workspace tests 全部 PASS。

- [ ] **Step 11: 执行 release build 和 diff 检查**

默认 feature release build：

```powershell
cargo build --release --workspace
```

Expected: exit code 0。

检查 whitespace：

```powershell
git diff --check
```

Expected: 无输出且 exit code 0。

`Makefile` 的 release gate 额外启用 `ffmpeg-codec`。仅当本机已有 FFmpeg development libraries、`pkg-config` 和可用 H.264 环境时运行：

```powershell
cargo build --release --workspace --features ffmpeg-codec
```

Expected when dependencies are available: exit code 0。若因缺少 FFmpeg native development libraries 或 `pkg-config` 失败：
- 不安装任何软件；
- 记录具体 native dependency 限制；
- 保留默认 release build、workspace check/tests 和 CI 作为验证证据；
- 不把缺失本机 FFmpeg 环境误报为 Rust 代码失败。

这组命令等价展开 `make ci` 的 fmt-check、clippy、check、test 和 release build 阶段，不要求 Windows 本机存在 `make`。

- [ ] **Step 12: 最终行为验收**

确认以下可观测结果均有自动化证据：

1. 历史 positive/completed queue rows 被精确回填一次。
2. 切换后 rolling aggregate 只读 ledger。
3. 正 GP POST 前已经存在 ledger row。
4. malformed redirect 后 ledger row 保留而 queue `gp_cost` 仍为 `0`。
5. append 失败时 POST count 为 `0`。
6. rolling enabled 的并发 worker 只有一行 ledger 和一次 POST。
7. rolling disabled 的同一 queue 两次获准尝试产生两行。
8. Free、Unlocked、`Gp(0)` 均可 `Proceed` 且不写 ledger。
9. 删除 queue 后 ledger 保留并把 `queue_id` 置空。
10. queue 成功 mark 仍写最近一次成功的 `gp_cost` metadata。

- [ ] **Step 13: 建议 review boundary**

不执行 `git add`、commit 或其他 Git 写操作。建议语义提交边界：

```text
test: cover EH GP spend attempt accounting
```

范围：
- `src/scheduler/eh_engine.rs`
- `src/db/entities/eh_download_queue.rs`
- `src/db/repo/eh_download_queue.rs`

---

## 明确排除范围

实施中不得增加：

- ledger `state` 字段或状态机；
- refund/reconciliation；
- cross-process transaction 或分布式锁；
- redirect persistence；
- remote idempotency key；
- retry policy、backoff 或最大尝试次数变更；
- queue+ledger 双重汇总；
- 新依赖或软件安装；
- 对 `config.toml` 的读取或修改；
- `git add`、commit、push、tag 等 Git 写操作。

## Scope / Type Consistency Self-Review

- **规格覆盖：** migration、backfill、FK/SET NULL、positive CHECK、ledger-only aggregate、POST 前 durable append、rolling mutex、disabled-budget append、Free/Unlocked/`Gp(0)` exclusion、permit 删除、失败 reservation 保留、重试新增行及 queue metadata 均映射到明确任务和测试。
- **类型一致性：** `queue_id: i32`、`gid: i64`、ledger `gp_cost: i64`、entity `queue_id: Option<i32>`、`created_at: DateTime` 与现有类型一致；`DownloadCost::Gp(u64)` 只通过 `i64::try_from(*gp)` 转换，budget projection 使用 `i128`。
- **API 一致性：** 全计划只定义一组 Repo API：`append_eh_gp_spend_attempt(&self, i32, i64, i64) -> Result<Model>` 和 `get_eh_gp_cost_in_window(&self, u64) -> Result<i64>`。
- **Migration 一致性：** app-crate migration test 使用 `migration::{Migrator, MigratorTrait}`；pre-ledger fixture 使用原始 SQL，不依赖最新 queue entity；SeaQuery 0.32.7 的 `ColumnDef::check`、nullable FK/SET NULL、index、backfill 和 drop-table down 均有具体验证。
- **测试稳定性：** 新 scheduler fixture token 全部使用十六进制字符串；malformed redirect 失败发生在 POST 后；删除 ledger table 且设置 `gp_rate_limit=0` 可稳定制造 pre-POST append failure。
- **范围检查：** 单一目标是修复 PR review 指出的失败 POST 记账缺口；未引入退款、跨进程协调、remote idempotency 或 retry redesign。
- **占位检查：** 扫描通过；所有步骤均给出具体文件、符号、代码、命令与预期结果。

目标计划路径：`docs/superpowers/plans/2026-07-19-pr-112-gp-attempt-ledger.md`
执行顺序：Task 1 → Task 2 → Task 3 → Task 4
正式 plan-critic receipt：`[OKAY-UNAMBIGUOUS]`。
