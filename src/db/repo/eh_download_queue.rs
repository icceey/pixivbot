use super::Repo;
use crate::db::entities::eh_download_queue;
use anyhow::{Context, Result};
use chrono::{Local, Timelike};
use eh_client::ArchiveArtifacts;
use sea_orm::prelude::DateTime;
use sea_orm::sea_query::{Expr, SimpleExpr};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, Order, PaginatorTrait, QueryFilter, QueryOrder,
    QueryTrait, Set,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;
use tracing::warn;

use crate::db::entities::subscriptions;

/// Serializes EH publish side effects with subscription-queue cancellation.
///
/// A database status check alone cannot prevent `/eunsub` from canceling a row
/// between the final active check and a Telegram send.  Both the cancel path
/// and the publish-send path take this process-wide lock so their effects have
/// a single observable order.
pub static EH_PUBLISH_CANCEL_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Bundled enqueue request parameters to keep helper signatures manageable
/// and avoid clippy `too_many_arguments`.
struct EhEnqueueRequest<'a> {
    chat_id: i64,
    gid: i64,
    token: &'a str,
    title: &'a str,
    telegraph: bool,
    source: &'a str,
    subscription_id: Option<i32>,
}

/// Status constants for eh_download_queue.
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_DOWNLOADING: &str = "downloading";
pub const STATUS_DONE: &str = "done";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_DOWNLOADED: &str = "downloaded";
pub const STATUS_UPLOADING: &str = "uploading";
pub const STATUS_UPLOADED: &str = "uploaded";
pub const STATUS_PUBLISHING: &str = "publishing";
pub const STATUS_CANCELED: &str = "canceled";
pub const BACKGROUND_STATUS_PENDING: &str = "pending";
pub const BACKGROUND_STATUS_RUNNING: &str = "running";
pub const TELEGRAPH_REWRITE_STATUS_PENDING: &str = "pending";
pub const TELEGRAPH_REWRITE_STATUS_REWRITING: &str = "rewriting";
pub const TELEGRAPH_REWRITE_STATUS_FAILED: &str = "failed";
const MAIN_DOWNLOAD_RECENT_WINDOW_HOURS: i64 = 2;

enum ArchivePolicyClaim {
    Main { started_at: DateTime },
    Background { started_at: DateTime },
}

/// `started_at` is a row-level claim generation after the first claim. Keeping
/// it monotonic at whole-second precision prevents same-tick ABA without a
/// schema migration, including on databases that truncate timestamp fractions.
fn next_claim_generation(now: DateTime, previous: Option<DateTime>) -> Result<DateTime> {
    let now_second = now
        .with_nanosecond(0)
        .context("Cannot normalize EH claim generation timestamp")?;
    let Some(previous) = previous else {
        return Ok(now_second);
    };
    let previous_second = previous
        .with_nanosecond(0)
        .context("Cannot normalize previous EH claim generation timestamp")?;
    let following_generation = previous_second
        .checked_add_signed(chrono::Duration::seconds(1))
        .context("EH claim generation timestamp overflow")?;

    Ok(now_second.max(following_generation))
}

fn claim_generation_filter(previous: Option<DateTime>) -> sea_orm::Condition {
    match previous {
        Some(generation) => {
            sea_orm::Condition::all().add(eh_download_queue::Column::StartedAt.eq(generation))
        }
        None => sea_orm::Condition::all().add(eh_download_queue::Column::StartedAt.is_null()),
    }
}

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

/// Source constants for eh_download_queue.
pub const SOURCE_SUBSCRIPTION: &str = "subscription";
pub const SOURCE_DIRECT: &str = "direct";

fn parse_subscription_ids(value: Option<&str>) -> BTreeSet<i32> {
    value
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| part.parse::<i32>().ok())
        .collect()
}

fn format_subscription_ids(ids: &BTreeSet<i32>) -> Option<String> {
    if ids.is_empty() {
        None
    } else {
        Some(ids.iter().map(i32::to_string).collect::<Vec<_>>().join(","))
    }
}

fn merge_subscription_ids(current: Option<&str>, new_id: Option<i32>) -> Option<String> {
    let mut ids = parse_subscription_ids(current);
    if let Some(id) = new_id {
        ids.insert(id);
    }
    format_subscription_ids(&ids)
}

fn merge_telegraph_subscription_ids(
    current: Option<&str>,
    new_id: Option<i32>,
    telegraph: bool,
) -> Option<String> {
    let mut ids = parse_subscription_ids(current);
    if telegraph {
        if let Some(id) = new_id {
            ids.insert(id);
        }
    }
    format_subscription_ids(&ids)
}

fn is_active_subscription_queue_status(status: &str) -> bool {
    matches!(
        status,
        STATUS_PENDING
            | STATUS_DOWNLOADING
            | STATUS_DOWNLOADED
            | STATUS_UPLOADING
            | STATUS_UPLOADED
            | STATUS_PUBLISHING
    )
}

fn is_cancelable_subscription_queue_status(status: &str) -> bool {
    is_active_subscription_queue_status(status)
        || matches!(status, STATUS_DONE | STATUS_FAILED | STATUS_CANCELED)
}

fn subscription_ids_filter(expected: Option<String>) -> sea_orm::sea_query::SimpleExpr {
    match expected {
        Some(value) => eh_download_queue::Column::SubscriptionIds.eq(value),
        None => eh_download_queue::Column::SubscriptionIds.is_null(),
    }
}

fn telegraph_subscription_ids_filter(expected: Option<String>) -> sea_orm::sea_query::SimpleExpr {
    match expected {
        Some(value) => eh_download_queue::Column::TelegraphSubscriptionIds.eq(value),
        None => eh_download_queue::Column::TelegraphSubscriptionIds.is_null(),
    }
}

impl Repo {
    /// Recover from a failed insert in `enqueue_eh_download` by re-selecting
    /// and merging into the existing row.  Extracted as a private helper so
    /// tests can exercise the fallback logic deterministically without relying
    /// on fragile concurrency.
    async fn recover_eh_enqueue_after_insert_error(
        &self,
        req: EhEnqueueRequest<'_>,
        db_err: sea_orm::DbErr,
    ) -> Result<eh_download_queue::Model> {
        match eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(req.chat_id))
            .filter(eh_download_queue::Column::Gid.eq(req.gid))
            .one(&self.db)
            .await
        {
            Ok(Some(raced)) => {
                self.merge_eh_download(
                    raced,
                    req.token,
                    req.title,
                    req.telegraph,
                    req.source,
                    req.subscription_id,
                )
                .await
            }
            Ok(None) => Err(db_err).context("Failed to enqueue eh download"),
            Err(select_err) => Err(select_err).context("Failed to re-select after insert conflict"),
        }
    }

    /// Enqueue a download request. Returns the created/updated model.
    ///
    /// If an entry for the same (chat_id, gid) already exists:
    /// - If it's `done` or `failed`, reset to `pending` (re-download).
    /// - Otherwise, return the existing entry (already in queue).
    pub async fn enqueue_eh_download(
        &self,
        chat_id: i64,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
        source: &str,
    ) -> Result<eh_download_queue::Model> {
        self.enqueue_eh_download_request(EhEnqueueRequest {
            chat_id,
            gid,
            token,
            title,
            telegraph,
            source,
            subscription_id: None,
        })
        .await
    }

    /// Enqueue a scheduler-created EH subscription download and remember the
    /// originating subscription id so unsubscribe can cancel queued work.
    pub async fn enqueue_eh_subscription_download(
        &self,
        chat_id: i64,
        subscription_id: i32,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
    ) -> Result<eh_download_queue::Model> {
        self.enqueue_eh_download_request(EhEnqueueRequest {
            chat_id,
            gid,
            token,
            title,
            telegraph,
            source: SOURCE_SUBSCRIPTION,
            subscription_id: Some(subscription_id),
        })
        .await
    }

    async fn enqueue_eh_download_request(
        &self,
        req: EhEnqueueRequest<'_>,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();

        // Check for existing entry
        let existing = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(req.chat_id))
            .filter(eh_download_queue::Column::Gid.eq(req.gid))
            .one(&self.db)
            .await?;

        if let Some(model) = existing {
            return self
                .merge_eh_download(
                    model,
                    req.token,
                    req.title,
                    req.telegraph,
                    req.source,
                    req.subscription_id,
                )
                .await;
        }

        // No existing entry — insert new; handle race on unique conflict
        let entry = eh_download_queue::ActiveModel {
            chat_id: Set(req.chat_id),
            gid: Set(req.gid),
            token: Set(req.token.to_string()),
            title: Set(req.title.to_string()),
            telegraph: Set(req.telegraph),
            source: Set(req.source.to_string()),
            subscription_ids: Set(req.subscription_id.map(|id| id.to_string())),
            telegraph_subscription_ids: Set(if req.telegraph {
                req.subscription_id.map(|id| id.to_string())
            } else {
                None
            }),
            status: Set(STATUS_PENDING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        };

        match entry.insert(&self.db).await {
            Ok(model) => Ok(model),
            Err(db_err) => {
                // Race: another caller may have inserted the same (chat_id, gid).
                // Re-select and merge instead of failing on unique constraint.
                self.recover_eh_enqueue_after_insert_error(req, db_err)
                    .await
            }
        }
    }

    /// Merge an existing queue entry with new request parameters.
    ///
    /// - Terminal (`done`/`failed`): reset to `pending` with full transient clear.
    /// - Non-terminal: update token/title, merge telegraph (OR) and source (direct wins).
    ///   If the merge upgrades source to direct or adds telegraph to an already-uploaded
    ///   entry, reset to `pending` with full transient clear.
    ///
    /// Uses a retry loop with CAS guards: the in-place update checks that the row is
    /// still in the expected status (and expected telegraph for downloaded rows).
    /// If a concurrent worker changed the row between select and update, re-read and
    /// recompute the merge decision up to 3 attempts.
    async fn merge_eh_download(
        &self,
        existing: eh_download_queue::Model,
        token: &str,
        title: &str,
        telegraph: bool,
        source: &str,
        subscription_id: Option<i32>,
    ) -> Result<eh_download_queue::Model> {
        const MAX_RETRIES: usize = 3;
        let mut current = existing;

        for attempt in 0..MAX_RETRIES {
            let is_terminal = matches!(
                current.status.as_str(),
                STATUS_DONE | STATUS_FAILED | STATUS_CANCELED
            );
            let merged_source = if current.source == SOURCE_DIRECT || source == SOURCE_DIRECT {
                SOURCE_DIRECT
            } else {
                SOURCE_SUBSCRIPTION
            };
            let merged_subscription_ids = if merged_source == SOURCE_DIRECT {
                None
            } else {
                merge_subscription_ids(current.subscription_ids.as_deref(), subscription_id)
            };
            let merged_telegraph_subscription_ids = if merged_source == SOURCE_DIRECT {
                None
            } else {
                merge_telegraph_subscription_ids(
                    current.telegraph_subscription_ids.as_deref(),
                    subscription_id,
                    telegraph,
                )
            };
            let merged_telegraph = if merged_source == SOURCE_SUBSCRIPTION {
                merged_telegraph_subscription_ids.is_some()
            } else {
                current.telegraph || telegraph
            };
            let source_upgraded_to_direct =
                current.source != SOURCE_DIRECT && merged_source == SOURCE_DIRECT;
            let telegraph_upgraded = !current.telegraph && merged_telegraph;
            let reset_for_new_requirement = source_upgraded_to_direct
                || (telegraph_upgraded
                    && matches!(current.status.as_str(), STATUS_UPLOADED | STATUS_PUBLISHING));

            if is_terminal || reset_for_new_requirement {
                // Full reset to pending — CAS-guarded so a stale snapshot does not
                // blindly overwrite a row that was changed by another worker.
                let id = current.id;
                let expected_status = current.status.clone();
                let expected_telegraph = current.telegraph;
                let expected_source = current.source.clone();
                let expected_subscription_ids = current.subscription_ids.clone();

                let result = eh_download_queue::Entity::update_many()
                    .col_expr(
                        eh_download_queue::Column::Status,
                        Expr::value(STATUS_PENDING),
                    )
                    .col_expr(
                        eh_download_queue::Column::Token,
                        Expr::value(token.to_string()),
                    )
                    .col_expr(
                        eh_download_queue::Column::Title,
                        Expr::value(title.to_string()),
                    )
                    .col_expr(
                        eh_download_queue::Column::Telegraph,
                        Expr::value(merged_telegraph),
                    )
                    .col_expr(
                        eh_download_queue::Column::Source,
                        Expr::value(merged_source.to_string()),
                    )
                    .col_expr(
                        eh_download_queue::Column::SubscriptionIds,
                        Expr::value(merged_subscription_ids.clone()),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphSubscriptionIds,
                        Expr::value(merged_telegraph_subscription_ids.clone()),
                    )
                    .col_expr(eh_download_queue::Column::FileSize, Expr::value(0))
                    .col_expr(
                        eh_download_queue::Column::Error,
                        Expr::value(None::<String>),
                    )
                    .col_expr(eh_download_queue::Column::RetryCount, Expr::value(0))
                    .col_expr(
                        eh_download_queue::Column::CompletedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::ZipPath,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphUrl,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::NextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::ArchiveSentAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphSentAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::BackgroundDownloadStatus,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::BackgroundDownloadStartedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::BackgroundDownloadAttemptCount,
                        Expr::value(0),
                    )
                    .col_expr(
                        eh_download_queue::Column::BackgroundDownloadError,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteData,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteStatus,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteAfter,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteStartedAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                        Expr::value(None::<DateTime>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteRetryCount,
                        Expr::value(0),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewriteError,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        eh_download_queue::Column::TelegraphRewrittenAt,
                        Expr::value(None::<DateTime>),
                    )
                    .filter(eh_download_queue::Column::Id.eq(id))
                    .filter(eh_download_queue::Column::Status.eq(&expected_status))
                    .filter(eh_download_queue::Column::Telegraph.eq(expected_telegraph))
                    .filter(eh_download_queue::Column::Source.eq(&expected_source))
                    .filter(subscription_ids_filter(expected_subscription_ids))
                    .exec(&self.db)
                    .await
                    .context("Failed to reset eh download for re-enqueue")?;

                if result.rows_affected == 1 {
                    let model = eh_download_queue::Entity::find_by_id(id)
                        .one(&self.db)
                        .await?
                        .context("Entry disappeared after reset")?;
                    return Ok(model);
                }

                // CAS failed — row changed between select and update.
                // Re-read and retry.
                if attempt + 1 < MAX_RETRIES {
                    current = match eh_download_queue::Entity::find_by_id(id)
                        .one(&self.db)
                        .await?
                    {
                        Some(fresh) => fresh,
                        None => anyhow::bail!("EH download {} disappeared during merge", id),
                    };
                    continue;
                }

                anyhow::bail!(
                    "Failed to reset EH download {} after {} attempts: row changed too frequently",
                    id,
                    MAX_RETRIES
                );
            }

            // Non-terminal: conditional update with CAS on expected status.
            // For downloaded rows, also guard on telegraph to prevent racing with
            // a publish worker that claimed the row between our select and update.
            let id = current.id;
            let expected_status = current.status.clone();
            let expected_telegraph = current.telegraph;
            let expected_source = current.source.clone();
            let expected_subscription_ids = current.subscription_ids.clone();

            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Token,
                    Expr::value(token.to_string()),
                )
                .col_expr(
                    eh_download_queue::Column::Title,
                    Expr::value(title.to_string()),
                )
                .col_expr(
                    eh_download_queue::Column::Telegraph,
                    Expr::value(merged_telegraph),
                )
                .col_expr(
                    eh_download_queue::Column::Source,
                    Expr::value(merged_source.to_string()),
                )
                .col_expr(
                    eh_download_queue::Column::SubscriptionIds,
                    Expr::value(merged_subscription_ids.clone()),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    Expr::value(merged_telegraph_subscription_ids.clone()),
                )
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(eh_download_queue::Column::Status.eq(&expected_status))
                .filter(eh_download_queue::Column::Telegraph.eq(expected_telegraph))
                .filter(eh_download_queue::Column::Source.eq(&expected_source))
                .filter(subscription_ids_filter(expected_subscription_ids))
                .exec(&self.db)
                .await
                .context("Failed to update eh download in place")?;

            if result.rows_affected == 1 {
                // Success — re-read and return
                let model = eh_download_queue::Entity::find_by_id(id)
                    .one(&self.db)
                    .await?
                    .context("Entry disappeared after merge update")?;
                return Ok(model);
            }

            // CAS failed — row was changed between our select and update.
            // Re-read and retry.
            if attempt + 1 < MAX_RETRIES {
                current = match eh_download_queue::Entity::find_by_id(id)
                    .one(&self.db)
                    .await?
                {
                    Some(fresh) => fresh,
                    None => anyhow::bail!("EH download {} disappeared during merge", id),
                };
            }
        }

        anyhow::bail!(
            "Failed to merge EH download {} after {} attempts: row changed too frequently",
            current.id,
            MAX_RETRIES
        );
    }

    /// Get the next pending download, atomically marking it as "downloading".
    ///
    /// Returns None if no pending downloads exist.
    #[allow(dead_code)]
    pub async fn get_next_pending_eh_download(&self) -> Result<Option<eh_download_queue::Model>> {
        let entry = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch pending eh download")?;

        if let Some(model) = entry {
            let now = Local::now().naive_local();
            let generation = next_claim_generation(now, model.started_at)?;
            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(STATUS_DOWNLOADING),
                )
                .col_expr(
                    eh_download_queue::Column::StartedAt,
                    Expr::value(generation),
                )
                .filter(eh_download_queue::Column::Id.eq(model.id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
                .filter(claim_generation_filter(model.started_at))
                .exec(&self.db)
                .await
                .context("Failed to mark eh download as downloading")?;
            if result.rows_affected == 0 {
                return Ok(None);
            }

            let updated = eh_download_queue::Entity::find()
                .filter(eh_download_queue::Column::Id.eq(model.id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
                .filter(eh_download_queue::Column::StartedAt.eq(generation))
                .one(&self.db)
                .await
                .context("Failed to re-fetch legacy EH download claim")?;
            Ok(updated)
        } else {
            Ok(None)
        }
    }

    /// Mark a publish as completed successfully (archive and/or Telegraph sent).
    /// Only allowed when current status is `STATUS_PUBLISHING`.
    /// Preserves `completed_at` from the download stage if already set;
    /// sets it to now only if it hasn't been set yet.
    pub async fn mark_eh_download_done(
        &self,
        id: i32,
        file_size: i64,
    ) -> Result<eh_download_queue::Model> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch eh download")?
            .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

        let now = Local::now().naive_local();
        let completed_at = entry.completed_at.unwrap_or(now);

        let result = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Status, Expr::value(STATUS_DONE))
            .col_expr(eh_download_queue::Column::FileSize, Expr::value(file_size))
            .col_expr(
                eh_download_queue::Column::CompletedAt,
                Expr::value(completed_at),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .exec(&self.db)
            .await
            .context("Failed to mark eh download as done")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark EH download {} as done: expected status '{}', but it was changed by another worker",
                id,
                STATUS_PUBLISHING
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after mark done")?;
        Ok(model)
    }

    /// Mark a download as failed.
    #[allow(dead_code)]
    pub async fn mark_eh_download_failed(
        &self,
        id: i32,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch eh download")?
            .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

        let now = Local::now().naive_local();
        let new_retry_count = entry.retry_count + 1;
        let mut active: eh_download_queue::ActiveModel = entry.into();
        active.status = Set(STATUS_FAILED.to_string());
        active.error = Set(Some(error.to_string()));
        active.completed_at = Set(Some(now));
        active.retry_count = Set(new_retry_count);
        active
            .update(&self.db)
            .await
            .context("Failed to mark eh download as failed")
    }

    /// Permanently fail a main download after its archive cost violates the
    /// configured policy. The original claim generation prevents a stale worker
    /// from overwriting a newer re-enqueued claim.
    pub async fn fail_eh_download_for_archive_policy(
        &self,
        entry: &eh_download_queue::Model,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        let started_at = entry
            .started_at
            .context("Cannot fail EH download for archive policy: missing main claim started_at")?;
        self.fail_eh_download_for_archive_policy_claim(
            entry.id,
            error,
            ArchivePolicyClaim::Main { started_at },
        )
        .await
    }

    /// Permanently fail a claimed background download after its archive cost
    /// violates the configured policy. The original row claim generation preserves
    /// newer cancellation or re-enqueue decisions.
    pub async fn fail_eh_background_download_for_archive_policy(
        &self,
        entry: &eh_download_queue::Model,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        let started_at = entry.started_at.context(
            "Cannot fail EH download for archive policy: missing background claim started_at",
        )?;
        self.fail_eh_download_for_archive_policy_claim(
            entry.id,
            error,
            ArchivePolicyClaim::Background { started_at },
        )
        .await
    }

    async fn fail_eh_download_for_archive_policy_claim(
        &self,
        id: i32,
        error: &str,
        claim: ArchivePolicyClaim,
    ) -> Result<eh_download_queue::Model> {
        let (expected_claim, claim_name): (SimpleExpr, &str) = match claim {
            ArchivePolicyClaim::Main { started_at } => (
                sea_orm::Condition::all()
                    .add(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
                    .add(eh_download_queue::Column::StartedAt.eq(started_at))
                    .into(),
                "main downloading claim",
            ),
            ArchivePolicyClaim::Background { started_at } => (
                sea_orm::Condition::all()
                    .add(eh_download_queue::Column::Status.eq(STATUS_PENDING))
                    .add(
                        eh_download_queue::Column::BackgroundDownloadStatus
                            .eq(BACKGROUND_STATUS_RUNNING),
                    )
                    .add(eh_download_queue::Column::StartedAt.eq(started_at))
                    .into(),
                "background running claim",
            ),
        };
        let now = Local::now().naive_local();
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_FAILED),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(expected_claim)
            .exec(&self.db)
            .await
            .context("Failed to fail EH download for archive policy")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot fail EH download {} for archive policy: expected {} claim, but it was changed by another worker",
                id,
                claim_name
            );
        }

        eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch EH download after archive policy failure")?
            .context("Entry disappeared after archive policy failure")
    }

    /// Get total bytes downloaded in the last `hours` window.
    /// Uses `completed_at` from the download stage (not overwritten by upload/publish stages).
    /// Uses SQL aggregate for efficiency.
    pub async fn get_eh_downloaded_bytes_in_window(&self, hours: u64) -> Result<i64> {
        let cutoff = Local::now().naive_local() - chrono::Duration::hours(hours as i64);

        let result = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
                STATUS_DONE,
                STATUS_FAILED,
            ]))
            .filter(eh_download_queue::Column::CompletedAt.gte(cutoff))
            .all(&self.db)
            .await
            .context("Failed to fetch eh downloads in window")?;

        Ok(result.iter().map(|e| e.file_size).sum())
    }

    /// Get the current queue status for one chat.
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
            .context("Failed to fetch active EH queue entries")?
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

    /// Count pending downloads in the queue.
    #[allow(dead_code)]
    pub async fn count_pending_eh_downloads(&self) -> Result<u64> {
        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .count(&self.db)
            .await
            .context("Failed to count pending eh downloads")
    }

    /// Cancel queued/in-flight EH downloads that were created for a subscription
    /// that has just been removed. Direct downloads and terminal rows are left
    /// untouched; rows with other active subscription owners keep running.
    pub async fn cancel_eh_subscription_queue_entries(&self, subscription_id: i32) -> Result<u64> {
        let _guard = EH_PUBLISH_CANCEL_LOCK.lock().await;
        self.cancel_eh_subscription_queue_entries_inner(subscription_id)
            .await
    }

    /// Cancel legacy subscription queue rows that predate `subscription_ids`.
    ///
    /// They cannot be attributed to a specific subscription safely, so they are
    /// canceled instead of being published after an unsubscribe or after the
    /// migration introduces owner-aware queue semantics.  Already-canceled
    /// legacy rows are included so startup can repair rows from an older
    /// migration attempt that canceled them without clearing Telegraph state.
    pub async fn cancel_legacy_eh_subscription_queue_entries(&self) -> Result<u64> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_CANCELED),
            )
            .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
            .filter(eh_download_queue::Column::SubscriptionIds.is_null())
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_PENDING,
                STATUS_DOWNLOADING,
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
                STATUS_CANCELED,
                STATUS_DONE,
                STATUS_FAILED,
            ]))
            .exec(&self.db)
            .await
            .context("Failed to cancel legacy EH subscription queue entries")?;
        Ok(result.rows_affected)
    }

    /// Delete an EH subscription and cancel/prune its queued work in one
    /// publish/cancel critical section.
    pub async fn delete_eh_subscription_and_cancel_queue(
        &self,
        subscription_id: i32,
    ) -> Result<()> {
        let _guard = EH_PUBLISH_CANCEL_LOCK.lock().await;
        self.delete_subscription(subscription_id).await?;
        self.cancel_eh_subscription_queue_entries_inner(subscription_id)
            .await?;
        Ok(())
    }

    async fn cancel_eh_subscription_queue_entries_inner(
        &self,
        subscription_id: i32,
    ) -> Result<u64> {
        let active_rows = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
            .filter(eh_download_queue::Column::SubscriptionIds.is_not_null())
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_PENDING,
                STATUS_DOWNLOADING,
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
                STATUS_DONE,
                STATUS_FAILED,
                STATUS_CANCELED,
            ]))
            .all(&self.db)
            .await
            .context("Failed to cancel eh subscription queue entries")?;

        let mut changed = 0u64;
        for row in active_rows {
            changed += self
                .remove_subscription_owner_from_eh_row(row, subscription_id)
                .await?;
        }
        Ok(changed)
    }

    async fn remove_subscription_owner_from_eh_row(
        &self,
        mut row: eh_download_queue::Model,
        subscription_id: i32,
    ) -> Result<u64> {
        const MAX_RETRIES: usize = 3;
        for attempt in 0..MAX_RETRIES {
            if row.source != SOURCE_SUBSCRIPTION
                || !is_cancelable_subscription_queue_status(&row.status)
            {
                return Ok(0);
            }

            let mut ids = parse_subscription_ids(row.subscription_ids.as_deref());
            if !ids.remove(&subscription_id) {
                return Ok(0);
            }
            let remaining_ids = format_subscription_ids(&ids);
            let mut telegraph_ids =
                parse_subscription_ids(row.telegraph_subscription_ids.as_deref());
            telegraph_ids.remove(&subscription_id);
            let remaining_telegraph_ids = format_subscription_ids(&telegraph_ids);
            let canceled = remaining_ids.is_none();
            let telegraph_still_required = remaining_telegraph_ids.is_some();
            let new_status = if canceled && is_active_subscription_queue_status(&row.status) {
                STATUS_CANCELED.to_string()
            } else if !telegraph_still_required
                && matches!(
                    row.status.as_str(),
                    STATUS_UPLOADING | STATUS_UPLOADED | STATUS_PUBLISHING
                )
            {
                STATUS_DOWNLOADED.to_string()
            } else {
                row.status.clone()
            };
            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::SubscriptionIds,
                    Expr::value(remaining_ids),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    Expr::value(remaining_telegraph_ids),
                )
                .col_expr(eh_download_queue::Column::Status, Expr::value(new_status))
                .col_expr(
                    eh_download_queue::Column::Telegraph,
                    Expr::value(telegraph_still_required),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphUrl,
                    Expr::value(if telegraph_still_required {
                        row.telegraph_url.clone()
                    } else {
                        None::<String>
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::ArchiveSentAt,
                    Expr::value(if canceled {
                        None::<DateTime>
                    } else {
                        row.archive_sent_at
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSentAt,
                    Expr::value(if telegraph_still_required {
                        row.telegraph_sent_at
                    } else {
                        None::<DateTime>
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::NextRetryAt,
                    Expr::value(if telegraph_still_required {
                        row.next_retry_at
                    } else {
                        None::<DateTime>
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadStatus,
                    Expr::value(if canceled {
                        None::<String>
                    } else {
                        row.background_download_status.clone()
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadStartedAt,
                    Expr::value(if canceled {
                        None::<DateTime>
                    } else {
                        row.background_download_started_at
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                    Expr::value(if canceled {
                        None::<DateTime>
                    } else {
                        row.background_download_next_retry_at
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadAttemptCount,
                    Expr::value(if canceled {
                        0
                    } else {
                        row.background_download_attempt_count
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadError,
                    Expr::value(if canceled {
                        None::<String>
                    } else {
                        row.background_download_error.clone()
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteData,
                    Expr::value(if telegraph_still_required {
                        row.telegraph_rewrite_data.clone()
                    } else {
                        None::<String>
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStatus,
                    Expr::value(if telegraph_still_required {
                        row.telegraph_rewrite_status.clone()
                    } else {
                        None::<String>
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteAfter,
                    Expr::value(if telegraph_still_required {
                        row.telegraph_rewrite_after
                    } else {
                        None::<DateTime>
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStartedAt,
                    Expr::value(if telegraph_still_required {
                        row.telegraph_rewrite_started_at
                    } else {
                        None::<DateTime>
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                    Expr::value(if telegraph_still_required {
                        row.telegraph_rewrite_next_retry_at
                    } else {
                        None::<DateTime>
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteRetryCount,
                    Expr::value(if telegraph_still_required {
                        row.telegraph_rewrite_retry_count
                    } else {
                        0
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteError,
                    Expr::value(if telegraph_still_required {
                        row.telegraph_rewrite_error.clone()
                    } else {
                        None::<String>
                    }),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewrittenAt,
                    Expr::value(if telegraph_still_required {
                        row.telegraph_rewritten_at
                    } else {
                        None::<DateTime>
                    }),
                )
                .filter(eh_download_queue::Column::Id.eq(row.id))
                .filter(eh_download_queue::Column::Status.eq(&row.status))
                .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
                .filter(eh_download_queue::Column::SubscriptionIds.eq(row.subscription_ids.clone()))
                .filter(eh_download_queue::Column::Telegraph.eq(row.telegraph))
                .filter(telegraph_subscription_ids_filter(
                    row.telegraph_subscription_ids.clone(),
                ))
                .exec(&self.db)
                .await
                .context("Failed to cancel eh subscription queue entry")?;
            if result.rows_affected == 1 {
                return Ok(1);
            }

            if attempt + 1 == MAX_RETRIES {
                anyhow::bail!(
                    "Failed to cancel EH subscription queue entry {} after {} attempts: row changed too frequently",
                    row.id,
                    MAX_RETRIES
                );
            }

            match eh_download_queue::Entity::find_by_id(row.id)
                .one(&self.db)
                .await?
            {
                Some(fresh) => row = fresh,
                None => return Ok(0),
            }
        }
        Ok(0)
    }

    /// True if a claimed queue row is still active and has not been canceled
    /// after its originating subscription was removed.
    pub async fn eh_download_is_active(&self, id: i32, expected_status: &str) -> Result<bool> {
        let row = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(expected_status))
            .one(&self.db)
            .await
            .context("Failed to check eh download activity")?;
        let Some(row) = row else {
            return Ok(false);
        };
        if row.source != SOURCE_SUBSCRIPTION {
            return Ok(true);
        }
        self.eh_download_has_live_owner_or_cancel(row, expected_status)
            .await
    }

    async fn eh_download_has_live_owner_or_cancel(
        &self,
        mut row: eh_download_queue::Model,
        expected_status: &str,
    ) -> Result<bool> {
        const MAX_RETRIES: usize = 3;
        for attempt in 0..MAX_RETRIES {
            if row.status != expected_status || row.source != SOURCE_SUBSCRIPTION {
                return Ok(false);
            }
            let ids = parse_subscription_ids(row.subscription_ids.as_deref());
            let alive = if ids.is_empty() {
                false
            } else {
                subscriptions::Entity::find()
                    .filter(subscriptions::Column::Id.is_in(ids.iter().copied()))
                    .count(&self.db)
                    .await
                    .context("Failed to check EH subscription owners")?
                    > 0
            };
            if alive {
                return Ok(true);
            }

            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(STATUS_CANCELED),
                )
                .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
                .col_expr(
                    eh_download_queue::Column::SubscriptionIds,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSubscriptionIds,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphUrl,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::ArchiveSentAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphSentAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteData,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStatus,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteAfter,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteRetryCount,
                    Expr::value(0),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteError,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewrittenAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(eh_download_queue::Column::Id.eq(row.id))
                .filter(eh_download_queue::Column::Status.eq(&row.status))
                .filter(eh_download_queue::Column::Source.eq(SOURCE_SUBSCRIPTION))
                .filter(eh_download_queue::Column::Telegraph.eq(row.telegraph))
                .filter(subscription_ids_filter(row.subscription_ids.clone()))
                .filter(telegraph_subscription_ids_filter(
                    row.telegraph_subscription_ids.clone(),
                ))
                .exec(&self.db)
                .await
                .context("Failed to soft-cancel inactive EH download")?;
            if result.rows_affected == 1 {
                return Ok(false);
            }

            if attempt + 1 == MAX_RETRIES {
                anyhow::bail!(
                    "Failed to soft-cancel inactive EH download {} after {} attempts: row changed too frequently",
                    row.id,
                    MAX_RETRIES
                );
            }

            match eh_download_queue::Entity::find_by_id(row.id)
                .one(&self.db)
                .await?
            {
                Some(fresh) => row = fresh,
                None => return Ok(false),
            }
        }
        Ok(false)
    }

    /// Reset stale in-flight entries back to their pre-claim status (crash recovery).
    ///
    /// Resets ALL three transient statuses:
    /// - `downloading` → `pending`
    /// - `uploading`   → `downloaded`
    /// - `publishing`  → `downloaded` (if telegraph=false) or `uploaded` (if telegraph_url is set)
    ///
    /// Should be called once at startup before any worker begins.
    pub async fn reset_stale_eh_downloads(&self) -> Result<u64> {
        let mut count = 0u64;

        // downloading → pending
        let stale_downloading = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
            .all(&self.db)
            .await
            .context("Failed to fetch stale downloading entries")?;
        for entry in stale_downloading {
            let mut active: eh_download_queue::ActiveModel = entry.into();
            active.status = Set(STATUS_PENDING.to_string());
            active
                .update(&self.db)
                .await
                .context("Failed to reset stale downloading entry")?;
            count += 1;
        }

        // uploading → downloaded
        let stale_uploading = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_UPLOADING))
            .all(&self.db)
            .await
            .context("Failed to fetch stale uploading entries")?;
        for entry in stale_uploading {
            let mut active: eh_download_queue::ActiveModel = entry.into();
            active.status = Set(STATUS_DOWNLOADED.to_string());
            active
                .update(&self.db)
                .await
                .context("Failed to reset stale uploading entry")?;
            count += 1;
        }

        // publishing → downloaded (telegraph=false) or uploaded (telegraph_url set)
        let stale_publishing = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .all(&self.db)
            .await
            .context("Failed to fetch stale publishing entries")?;
        for entry in stale_publishing {
            let target = if entry.telegraph_url.is_some() {
                STATUS_UPLOADED
            } else {
                STATUS_DOWNLOADED
            };
            let mut active: eh_download_queue::ActiveModel = entry.into();
            active.status = Set(target.to_string());
            active
                .update(&self.db)
                .await
                .context("Failed to reset stale publishing entry")?;
            count += 1;
        }

        Ok(count)
    }

    /// Reset stale Telegraph rewrite claims back to pending rewrite work.
    pub async fn reset_stale_eh_telegraph_rewrites(&self, stale_sec: i64) -> Result<u64> {
        let cutoff = Local::now().naive_local() - chrono::Duration::seconds(stale_sec);
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .filter(
                eh_download_queue::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .filter(eh_download_queue::Column::TelegraphRewriteData.is_not_null())
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::TelegraphRewriteStartedAt.is_null())
                    .add(eh_download_queue::Column::TelegraphRewriteStartedAt.lte(cutoff)),
            )
            .exec(&self.db)
            .await
            .context("Failed to reset stale EH Telegraph rewrites")?;

        Ok(result.rows_affected)
    }

    /// Reset failed downloads back to pending if they haven't exceeded max_retry_count.
    #[allow(dead_code)]
    pub async fn retry_failed_eh_downloads(&self, max_retry_count: u8) -> Result<u64> {
        let failed = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_FAILED))
            .filter(eh_download_queue::Column::RetryCount.lte(max_retry_count as i32))
            .all(&self.db)
            .await
            .context("Failed to fetch failed eh downloads")?;

        let count = failed.len();
        for entry in failed {
            let mut active: eh_download_queue::ActiveModel = entry.into();
            active.status = Set(STATUS_PENDING.to_string());
            active.completed_at = Set(None);
            active
                .update(&self.db)
                .await
                .context("Failed to reset failed eh download")?;
        }

        Ok(count as u64)
    }

    /// Calculate exponential backoff delay (seconds) for a given retry count.
    /// 1→60s, 2→300s, 3→900s, beyond→3600s.
    pub fn backoff_delay_secs(retry_count: i32) -> i64 {
        match retry_count {
            0 | 1 => 60,
            2 => 300,
            3 => 900,
            _ => 3600,
        }
    }

    /// Mark a download as downloaded (ZIP saved to cache). Transitions to `downloaded` status.
    /// Only allowed when current status is `STATUS_DOWNLOADING`.
    ///
    /// `gp_cost` is compatibility/display metadata for the most recent successful
    /// archive download (0 for free / unlocked). The append-only
    /// `eh_gp_spend_attempts` ledger calculates rolling GP budgets.
    pub async fn mark_eh_download_downloaded(
        &self,
        id: i32,
        file_size: i64,
        zip_path: &str,
        gp_cost: i64,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADED),
            )
            .col_expr(eh_download_queue::Column::FileSize, Expr::value(file_size))
            .col_expr(eh_download_queue::Column::GpCost, Expr::value(gp_cost))
            .col_expr(
                eh_download_queue::Column::ZipPath,
                Expr::value(Some(zip_path.to_string())),
            )
            .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
            .exec(&self.db)
            .await
            .context("Failed to mark eh download as downloaded")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark EH download {} as downloaded: expected status '{}', but it was changed by another worker",
                id,
                STATUS_DOWNLOADING
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after mark downloaded")?;
        Ok(model)
    }

    /// Mark a download as uploaded (Telegraph page created). Transitions to `uploaded` status.
    /// Only allowed when current status is `STATUS_UPLOADING`.
    #[allow(dead_code)]
    pub async fn mark_eh_download_uploaded(
        &self,
        id: i32,
        telegraph_url: &str,
    ) -> Result<eh_download_queue::Model> {
        self.mark_eh_download_uploaded_with_rewrite(id, telegraph_url, None)
            .await
    }

    /// Mark a download as uploaded and store optional post-send Telegraph rewrite metadata.
    pub async fn mark_eh_download_uploaded_with_rewrite(
        &self,
        id: i32,
        telegraph_url: &str,
        rewrite_data_json: Option<&str>,
    ) -> Result<eh_download_queue::Model> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_UPLOADED),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(Some(telegraph_url.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(rewrite_data_json.map(str::to_string)),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_UPLOADING))
            .exec(&self.db)
            .await
            .context("Failed to mark eh download as uploaded")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark EH download {} as uploaded: expected status '{}', but it was changed by another worker",
                id,
                STATUS_UPLOADING
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after mark uploaded")?;
        Ok(model)
    }

    /// Fallback a permanently failed Telegraph upload to archive-only delivery.
    /// Sets telegraph=false, status=downloaded, clears next_retry_at,
    /// telegraph_url, archive_sent_at, and telegraph_sent_at so publish
    /// workers do not send stale Telegraph links.
    /// Only updates rows currently in `STATUS_UPLOADING`.
    pub async fn fallback_eh_upload_to_archive(
        &self,
        id: i32,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(eh_download_queue::Column::RetryCount, Expr::value(0))
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_UPLOADING))
            .exec(&self.db)
            .await?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot fallback EH upload {} to archive: expected status '{}', but it was changed",
                id,
                STATUS_UPLOADING
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after upload fallback")?;
        Ok(model)
    }

    /// Disable Telegraph delivery for rows that have not produced a Telegraph URL yet.
    ///
    /// This is used at startup when no Telegraph token is configured.  Rows with an
    /// existing `telegraph_url` are left untouched because they are already publishable;
    /// rows without a URL are downgraded to archive-only so they can be downloaded or
    /// published without an upload worker.  Terminal rows have only their Telegraph flag
    /// cleared so a later plain re-enqueue does not OR-merge the stale preference back in.
    pub async fn disable_eh_telegraph_for_unuploaded_entries(&self) -> Result<u64> {
        let mut changed = 0u64;

        // Pre-download in-flight work should restart from the download queue.
        let pending = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_PENDING),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(eh_download_queue::Column::TelegraphUrl.is_null())
            .filter(eh_download_queue::Column::Status.is_in([STATUS_PENDING, STATUS_DOWNLOADING]))
            .exec(&self.db)
            .await
            .context("Failed to disable unuploaded EH Telegraph pending entries")?;
        changed += pending.rows_affected;

        // ZIP already exists or should exist: publish as archive-only.
        let downloaded = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADED),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphUrl,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(eh_download_queue::Column::TelegraphUrl.is_null())
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
            ]))
            .exec(&self.db)
            .await
            .context("Failed to disable unuploaded EH Telegraph downloaded entries")?;
        changed += downloaded.rows_affected;

        // Terminal rows do not need status changes, but clearing the stale flag prevents
        // future plain `/edl` re-enqueues from OR-merging Telegraph back to true.
        let terminal = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Telegraph, Expr::value(false))
            .col_expr(
                eh_download_queue::Column::TelegraphSubscriptionIds,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(eh_download_queue::Column::Status.is_in([
                STATUS_DONE,
                STATUS_FAILED,
                STATUS_CANCELED,
            ]))
            .exec(&self.db)
            .await
            .context("Failed to disable unuploaded EH Telegraph terminal entries")?;
        changed += terminal.rows_affected;

        Ok(changed)
    }

    /// Get next entry for the download stage: status=pending, next_retry_at is NULL or <= now.
    /// Uses a conditional UPDATE to atomically claim the entry.
    pub async fn get_next_for_download(&self) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        self.get_next_for_download_at(now).await
    }

    async fn get_next_for_download_at(
        &self,
        now: DateTime,
    ) -> Result<Option<eh_download_queue::Model>> {
        let cutoff = now - chrono::Duration::hours(MAIN_DOWNLOAD_RECENT_WINDOW_HOURS);
        let is_recent = Expr::col(eh_download_queue::Column::CreatedAt).gt(cutoff);
        let recent_priority: SimpleExpr = Expr::case(is_recent.clone(), 0).finally(1).into();
        let recent_created_at: SimpleExpr = Expr::case(
            is_recent.clone(),
            Expr::col(eh_download_queue::Column::CreatedAt),
        )
        .finally(Expr::value(None::<DateTime>))
        .into();
        let recent_id: SimpleExpr =
            Expr::case(is_recent.clone(), Expr::col(eh_download_queue::Column::Id))
                .finally(Expr::value(None::<i32>))
                .into();
        let old_created_at: SimpleExpr =
            Expr::case(is_recent.clone(), Expr::value(None::<DateTime>))
                .finally(Expr::col(eh_download_queue::Column::CreatedAt))
                .into();
        let old_id: SimpleExpr = Expr::case(is_recent, Expr::value(None::<i32>))
            .finally(Expr::col(eh_download_queue::Column::Id))
            .into();
        let mut query = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_null())
            .filter(
                eh_download_queue::Column::NextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::NextRetryAt.lte(now)),
            );
        QueryTrait::query(&mut query)
            .order_by_expr(recent_priority, Order::Asc)
            .order_by_expr(recent_created_at, Order::Asc)
            .order_by_expr(recent_id, Order::Asc)
            .order_by_expr(old_created_at, Order::Desc)
            .order_by_expr(old_id, Order::Desc);
        let entry = query
            .one(&self.db)
            .await
            .context("Failed to fetch next for download")?;

        let Some(model) = entry else {
            return Ok(None);
        };

        self.claim_main_download_from_snapshot_at(&model, now).await
    }

    async fn claim_main_download_from_snapshot_at(
        &self,
        model: &eh_download_queue::Model,
        now: DateTime,
    ) -> Result<Option<eh_download_queue::Model>> {
        let generation = next_claim_generation(now, model.started_at)?;
        // Atomic claim: only flip if still pending with valid next_retry_at
        // and the selected previous generation (guards stale selectors too).
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADING),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(generation),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_null())
            .filter(claim_generation_filter(model.started_at))
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::NextRetryAt.is_null())
                    .add(eh_download_queue::Column::NextRetryAt.lte(now)),
            )
            .exec(&self.db)
            .await
            .context("Failed to atomically claim download entry")?;

        if result.rows_affected == 0 {
            return Ok(None); // someone else claimed it
        }

        // Confirm this worker's status and generation survived until readback.
        let updated = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_null())
            .filter(eh_download_queue::Column::StartedAt.eq(generation))
            .one(&self.db)
            .await?;
        Ok(updated)
    }

    /// Get next entry for the upload stage: status=downloaded, telegraph=true, next_retry_at ok.
    /// Uses a conditional UPDATE to atomically claim the entry.
    pub async fn get_next_for_upload(&self) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        let entry = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADED))
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(
                eh_download_queue::Column::NextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::NextRetryAt.lte(now)),
            )
            .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next for upload")?;

        let Some(model) = entry else {
            return Ok(None);
        };

        let generation = next_claim_generation(now, model.started_at)?;
        // Atomic claim: only flip if still downloaded+telegraph with valid next_retry_at
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_UPLOADING),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(generation),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADED))
            .filter(eh_download_queue::Column::Telegraph.eq(true))
            .filter(claim_generation_filter(model.started_at))
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::NextRetryAt.is_null())
                    .add(eh_download_queue::Column::NextRetryAt.lte(now)),
            )
            .exec(&self.db)
            .await
            .context("Failed to atomically claim upload entry")?;

        if result.rows_affected == 0 {
            return Ok(None);
        }

        let updated = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_UPLOADING))
            .filter(eh_download_queue::Column::StartedAt.eq(generation))
            .one(&self.db)
            .await?;
        Ok(updated)
    }

    /// Get next entry for the publish stage: either (downloaded, telegraph=false) or (uploaded).
    /// Uses a conditional UPDATE to atomically claim the entry.
    pub async fn get_next_for_publish(&self) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        let entry = eh_download_queue::Entity::find()
            .filter(
                sea_orm::Condition::any()
                    .add(
                        eh_download_queue::Column::Status
                            .eq(STATUS_DOWNLOADED)
                            .and(eh_download_queue::Column::Telegraph.eq(false)),
                    )
                    .add(eh_download_queue::Column::Status.eq(STATUS_UPLOADED)),
            )
            .filter(
                eh_download_queue::Column::NextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::NextRetryAt.lte(now)),
            )
            .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next for publish")?;

        let Some(model) = entry else {
            return Ok(None);
        };

        let generation = next_claim_generation(now, model.started_at)?;
        // Atomically claim: only flip if status is still the original AND next_retry_at is valid.
        // Also guard against row changes between select and update (telegraph toggle, re-enqueue).
        let original_status = model.status.clone();
        let status_filter = if original_status == STATUS_DOWNLOADED {
            // Must still be downloaded with telegraph=false (prevent claim of upgraded row)
            sea_orm::Condition::all()
                .add(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADED))
                .add(eh_download_queue::Column::Telegraph.eq(false))
        } else {
            // Must still be uploaded
            sea_orm::Condition::all().add(eh_download_queue::Column::Status.eq(STATUS_UPLOADED))
        };
        let retry_filter = sea_orm::Condition::any()
            .add(eh_download_queue::Column::NextRetryAt.is_null())
            .add(eh_download_queue::Column::NextRetryAt.lte(now));

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_PUBLISHING),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(generation),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(status_filter)
            .filter(claim_generation_filter(model.started_at))
            .filter(retry_filter)
            .exec(&self.db)
            .await
            .context("Failed to atomically claim publish entry")?;

        if result.rows_affected == 0 {
            return Ok(None);
        }

        let updated = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .filter(eh_download_queue::Column::StartedAt.eq(generation))
            .one(&self.db)
            .await?;
        Ok(updated)
    }

    /// Mark the archive ZIP as sent (publish stage progress marker).
    /// Only updates rows currently in `STATUS_PUBLISHING`.
    pub async fn mark_eh_archive_sent(&self, id: i32) -> Result<()> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(Local::now().naive_local()),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .exec(&self.db)
            .await?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark archive sent for EH download {}: expected status '{}', but it was changed",
                id,
                STATUS_PUBLISHING
            );
        }
        Ok(())
    }

    /// Mark the Telegraph link as sent (publish stage progress marker).
    /// Only updates rows currently in `STATUS_PUBLISHING`.
    #[allow(dead_code)]
    pub async fn mark_eh_telegraph_sent(&self, id: i32) -> Result<()> {
        self.mark_eh_telegraph_sent_and_schedule_rewrite(id, None)
            .await
    }

    /// Mark the Telegraph link as sent and schedule rewrite metadata in the same DB update.
    pub async fn mark_eh_telegraph_sent_and_schedule_rewrite(
        &self,
        id: i32,
        rewrite_delay_secs: Option<i64>,
    ) -> Result<()> {
        let now = Local::now().naive_local();
        if rewrite_delay_secs.is_none() {
            let result = eh_download_queue::Entity::update_many()
                .col_expr(eh_download_queue::Column::TelegraphSentAt, Expr::value(now))
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .exec(&self.db)
                .await?;

            if result.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot mark telegraph sent for EH download {}: expected status '{}', but it was changed",
                    id,
                    STATUS_PUBLISHING
                );
            }
            return Ok(());
        }

        if let Some(delay_secs) = rewrite_delay_secs {
            let result = eh_download_queue::Entity::update_many()
                .col_expr(eh_download_queue::Column::TelegraphSentAt, Expr::value(now))
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStatus,
                    Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteAfter,
                    Expr::value(now + chrono::Duration::seconds(delay_secs)),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteRetryCount,
                    Expr::value(0),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteError,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewrittenAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
                .filter(eh_download_queue::Column::TelegraphRewriteData.is_not_null())
                .exec(&self.db)
                .await
                .context("Failed to mark EH Telegraph sent and schedule rewrite")?;
            if result.rows_affected == 1 {
                return Ok(());
            }
        }

        let result = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::TelegraphSentAt, Expr::value(now))
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .filter(eh_download_queue::Column::TelegraphRewriteData.is_null())
            .exec(&self.db)
            .await?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark telegraph sent for EH download {}: expected status '{}', but it was changed",
                id,
                STATUS_PUBLISHING
            );
        }
        Ok(())
    }

    /// Schedule stored Telegraph rewrite data after the link has been sent.
    pub async fn schedule_eh_telegraph_rewrite_after_send(
        &self,
        id: i32,
        delay_secs: i64,
    ) -> Result<()> {
        let now = Local::now().naive_local();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(now + chrono::Duration::seconds(delay_secs)),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .filter(eh_download_queue::Column::TelegraphSentAt.is_not_null())
            .filter(eh_download_queue::Column::TelegraphRewriteData.is_not_null())
            .filter(eh_download_queue::Column::TelegraphRewriteStatus.is_null())
            .filter(eh_download_queue::Column::TelegraphRewriteAfter.is_null())
            .filter(eh_download_queue::Column::TelegraphRewriteNextRetryAt.is_null())
            .filter(eh_download_queue::Column::TelegraphRewrittenAt.is_null())
            .exec(&self.db)
            .await
            .context("Failed to schedule EH Telegraph rewrite")?;
        Ok(())
    }

    /// Claim the next due Telegraph rewrite job.
    pub async fn get_next_for_telegraph_rewrite(&self) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        let entry = eh_download_queue::Entity::find()
            .filter(
                eh_download_queue::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_PENDING),
            )
            .filter(eh_download_queue::Column::TelegraphRewriteData.is_not_null())
            .filter(eh_download_queue::Column::TelegraphSentAt.is_not_null())
            .filter(eh_download_queue::Column::TelegraphRewrittenAt.is_null())
            .filter(
                eh_download_queue::Column::TelegraphRewriteAfter
                    .is_null()
                    .or(eh_download_queue::Column::TelegraphRewriteAfter.lte(now)),
            )
            .filter(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::TelegraphRewriteNextRetryAt.lte(now)),
            )
            .order_by(eh_download_queue::Column::TelegraphRewriteAfter, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next EH Telegraph rewrite")?;

        let Some(model) = entry else {
            return Ok(None);
        };

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(now),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(
                eh_download_queue::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_PENDING),
            )
            .filter(eh_download_queue::Column::TelegraphRewriteData.is_not_null())
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::TelegraphRewriteAfter.is_null())
                    .add(eh_download_queue::Column::TelegraphRewriteAfter.lte(now)),
            )
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::TelegraphRewriteNextRetryAt.is_null())
                    .add(eh_download_queue::Column::TelegraphRewriteNextRetryAt.lte(now)),
            )
            .exec(&self.db)
            .await
            .context("Failed to atomically claim EH Telegraph rewrite")?;

        if result.rows_affected == 0 {
            return Ok(None);
        }

        let updated = eh_download_queue::Entity::find_by_id(model.id)
            .one(&self.db)
            .await?
            .context("EH Telegraph rewrite entry disappeared after claim")?;
        Ok(Some(updated))
    }

    /// Mark a claimed Telegraph rewrite as complete and clear rewrite payload.
    pub async fn mark_eh_telegraph_rewritten(&self, id: i32) -> Result<()> {
        let now = Local::now().naive_local();
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteData,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteAfter,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewrittenAt,
                Expr::value(Some(now)),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(
                eh_download_queue::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .exec(&self.db)
            .await
            .context("Failed to mark EH Telegraph rewrite complete")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot mark EH Telegraph rewrite {} complete: expected status '{}'",
                id,
                TELEGRAPH_REWRITE_STATUS_REWRITING
            );
        }

        Ok(())
    }

    /// Retry a claimed Telegraph rewrite with backoff, or stop retrying after `max_retry_count`.
    pub async fn schedule_eh_telegraph_rewrite_retry(
        &self,
        id: i32,
        error: &str,
        max_retry_count: u8,
    ) -> Result<bool> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch EH Telegraph rewrite for retry")?
            .ok_or_else(|| anyhow::anyhow!("EH Telegraph rewrite {} not found", id))?;
        let retry_count = entry.telegraph_rewrite_retry_count + 1;
        let is_permanent = retry_count > max_retry_count as i32;
        let now = Local::now().naive_local();

        if is_permanent {
            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStatus,
                    Expr::value(Some(TELEGRAPH_REWRITE_STATUS_FAILED.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteRetryCount,
                    Expr::value(retry_count),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteError,
                    Expr::value(Some(error.to_string())),
                )
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(
                    eh_download_queue::Column::TelegraphRewriteStatus
                        .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
                )
                .exec(&self.db)
                .await
                .context("Failed to mark EH Telegraph rewrite failed")?;

            if result.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot fail EH Telegraph rewrite {}: expected status '{}'",
                    id,
                    TELEGRAPH_REWRITE_STATUS_REWRITING
                );
            }
            return Ok(true);
        }

        let delay = Self::backoff_delay_secs(retry_count);
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                Expr::value(now + chrono::Duration::seconds(delay)),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteRetryCount,
                Expr::value(retry_count),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphRewriteError,
                Expr::value(Some(error.to_string())),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(
                eh_download_queue::Column::TelegraphRewriteStatus
                    .eq(TELEGRAPH_REWRITE_STATUS_REWRITING),
            )
            .exec(&self.db)
            .await
            .context("Failed to schedule EH Telegraph rewrite retry")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot retry EH Telegraph rewrite {}: expected status '{}'",
                id,
                TELEGRAPH_REWRITE_STATUS_REWRITING
            );
        }

        Ok(false)
    }

    /// Defer an entry: set status to `target_status` and delay next retry by `delay_secs`.
    /// Does NOT increment `retry_count` and does NOT set `error`.
    ///
    /// Legal target statuses: `STATUS_PENDING`, `STATUS_DOWNLOADED`, `STATUS_UPLOADED`.
    /// Current-status guards:
    /// - target `STATUS_PENDING`: current must be `STATUS_DOWNLOADING`.
    /// - target `STATUS_DOWNLOADED`: current must be `STATUS_UPLOADING` or `STATUS_PUBLISHING`.
    /// - target `STATUS_UPLOADED`: current must be `STATUS_PUBLISHING`.
    pub async fn defer_eh_download(
        &self,
        id: i32,
        target_status: &str,
        delay_secs: i64,
    ) -> Result<()> {
        let current_filter = match target_status {
            STATUS_PENDING => eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING),
            STATUS_DOWNLOADED => eh_download_queue::Column::Status
                .is_in([STATUS_UPLOADING, STATUS_PUBLISHING]),
            STATUS_UPLOADED => eh_download_queue::Column::Status.eq(STATUS_PUBLISHING),
            _ => anyhow::bail!(
                "defer_eh_download: invalid target status '{}' (expected pending, downloaded, or uploaded)",
                target_status
            ),
        };

        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(target_status),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(Local::now().naive_local() + chrono::Duration::seconds(delay_secs)),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(current_filter)
            .exec(&self.db)
            .await?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot defer EH download {} to '{}': expected in-flight status, but it was changed by another worker",
                id,
                target_status
            );
        }
        Ok(())
    }

    /// Schedule a retry for an entry: set status back to target_status, increment retry_count,
    /// set next_retry_at to now + backoff. If retry_count exceeds max, set status=failed.
    /// Returns (model, is_permanent_failure).
    ///
    /// Compatibility wrapper for unambiguous retry targets.  Use
    /// `schedule_eh_retry_from()` when multiple stages can retry to the same
    /// target status.
    #[allow(dead_code)]
    pub async fn schedule_eh_retry(
        &self,
        id: i32,
        target_status: &str,
        error: &str,
        max_retry_count: u8,
    ) -> Result<(eh_download_queue::Model, bool)> {
        let expected_status = match target_status {
            STATUS_PENDING => STATUS_DOWNLOADING,
            STATUS_UPLOADED => STATUS_PUBLISHING,
            STATUS_DOWNLOADED => anyhow::bail!(
                "schedule_eh_retry target '{}' is ambiguous; use schedule_eh_retry_from with the claimed status",
                target_status
            ),
            _ => anyhow::bail!(
                "schedule_eh_retry: invalid target status '{}'",
                target_status
            ),
        };
        self.schedule_eh_retry_from(id, expected_status, target_status, error, max_retry_count)
            .await
    }

    /// Schedule a retry from a specific in-flight status.  The explicit
    /// `expected_status` is required because both upload and publish failures can
    /// target `downloaded`; without it a stale worker could overwrite a newer
    /// in-flight stage that happens to share the same retry target.
    pub async fn schedule_eh_retry_from(
        &self,
        id: i32,
        expected_status: &str,
        target_status: &str,
        error: &str,
        max_retry_count: u8,
    ) -> Result<(eh_download_queue::Model, bool)> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch eh download")?
            .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

        let new_retry_count = entry.retry_count + 1;
        let is_permanent = new_retry_count > max_retry_count as i32;
        let now = Local::now().naive_local();

        // Determine the valid current-status filter for the expected stage and
        // retry target (same for transient and permanent failure).
        let current_filter = match (expected_status, target_status) {
            (STATUS_DOWNLOADING, STATUS_PENDING) => {
                eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING)
            }
            (STATUS_UPLOADING, STATUS_DOWNLOADED) => {
                eh_download_queue::Column::Status.eq(STATUS_UPLOADING)
            }
            (STATUS_PUBLISHING, STATUS_DOWNLOADED) => {
                eh_download_queue::Column::Status.eq(STATUS_PUBLISHING)
            }
            (STATUS_PUBLISHING, STATUS_UPLOADED) => {
                eh_download_queue::Column::Status.eq(STATUS_PUBLISHING)
            }
            (STATUS_PUBLISHING, STATUS_PENDING) => {
                eh_download_queue::Column::Status.eq(STATUS_PUBLISHING)
            }
            _ => anyhow::bail!(
                "schedule_eh_retry_from: invalid transition from '{}' to '{}'",
                expected_status,
                target_status
            ),
        };

        if is_permanent {
            // Permanent failure: CAS-guarded so stale workers don't overwrite re-enqueued rows.
            let result = eh_download_queue::Entity::update_many()
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(STATUS_FAILED),
                )
                .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
                .col_expr(
                    eh_download_queue::Column::Error,
                    Expr::value(Some(error.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::RetryCount,
                    Expr::value(new_retry_count),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteData,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStatus,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteAfter,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteRetryCount,
                    Expr::value(0),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewriteError,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::TelegraphRewrittenAt,
                    Expr::value(None::<DateTime>),
                )
                .filter(eh_download_queue::Column::Id.eq(id))
                .filter(current_filter)
                .exec(&self.db)
                .await
                .context("Failed to schedule retry (permanent)")?;

            if result.rows_affected != 1 {
                anyhow::bail!(
                    "Cannot schedule permanent retry for EH download {}: expected in-flight status, but it was changed by another worker",
                    id
                );
            }

            let model = eh_download_queue::Entity::find_by_id(id)
                .one(&self.db)
                .await?
                .context("Entry disappeared after retry")?;
            return Ok((model, true));
        }

        let delay = Self::backoff_delay_secs(new_retry_count);
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(target_status),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(now + chrono::Duration::seconds(delay)),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::RetryCount,
                Expr::value(new_retry_count),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(current_filter)
            .exec(&self.db)
            .await
            .context("Failed to schedule retry")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot schedule retry for EH download {} to '{}': expected in-flight status, but it was changed by another worker",
                id,
                target_status
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after retry")?;
        Ok((model, false))
    }

    pub async fn schedule_eh_background_download_from(
        &self,
        id: i32,
        expected_status: &str,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_PENDING),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(expected_status))
            .exec(&self.db)
            .await
            .context("Failed to schedule EH background download")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot schedule EH background download {}: expected status '{}', but it was changed",
                id,
                expected_status
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after background handoff")?;
        Ok(model)
    }

    /// Defer a background download that is currently running without treating
    /// the defer as a retryable failure.
    ///
    /// Sets `background_download_status` back to `pending`, schedules
    /// `background_download_next_retry_at = now + delay_secs`, and crucially
    /// does NOT increment `background_download_attempt_count` and does NOT
    /// mark the entry as failed. Used by the background worker when the GP /
    /// byte-rate guard defers a download - the entry should wait out the
    /// configured window, not burn retry attempts.
    ///
    /// Requires `status = STATUS_PENDING` and
    /// `background_download_status = BACKGROUND_STATUS_RUNNING` (i.e. the
    /// background worker currently owns the entry). Returns the updated model.
    pub async fn defer_eh_background_download(
        &self,
        id: i32,
        delay_secs: i64,
        reason: &str,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(Some(now + chrono::Duration::seconds(delay_secs))),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(Some(reason.to_string())),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
            )
            .exec(&self.db)
            .await
            .context("Failed to defer EH background download")?;

        if result.rows_affected != 1 {
            anyhow::bail!(
                "Cannot defer EH background download {}: expected status '{}' with background_download_status='{}', but it was changed by another worker",
                id,
                STATUS_PENDING,
                BACKGROUND_STATUS_RUNNING
            );
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after background defer")?;
        Ok(model)
    }

    pub async fn reset_stale_background_downloads(&self, stale_sec: u64) -> Result<u64> {
        let cutoff = Local::now().naive_local() - chrono::Duration::seconds(stale_sec as i64);
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
            )
            .filter(
                eh_download_queue::Column::BackgroundDownloadStartedAt
                    .is_null()
                    .or(eh_download_queue::Column::BackgroundDownloadStartedAt.lte(cutoff)),
            )
            .exec(&self.db)
            .await
            .context("Failed to reset stale EH background downloads")?;
        Ok(result.rows_affected)
    }

    pub async fn release_background_downloads_to_main_queue(&self) -> Result<u64> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_not_null())
            .exec(&self.db)
            .await
            .context("Failed to release EH background downloads to main queue")?;
        Ok(result.rows_affected)
    }

    async fn clear_background_download_if_inactive(&self, id: i32) -> Result<()> {
        let Some(row) = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
        else {
            return Ok(());
        };
        if row.background_download_status.is_none()
            || matches!(row.status.as_str(), STATUS_PENDING | STATUS_DOWNLOADING)
        {
            return Ok(());
        }
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(row.status))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_not_null())
            .exec(&self.db)
            .await
            .context("Failed to clear stale EH background download state")?;
        Ok(())
    }

    pub async fn get_next_for_background_download(
        &self,
    ) -> Result<Option<eh_download_queue::Model>> {
        let now = Local::now().naive_local();
        self.get_next_for_background_download_at(now).await
    }

    async fn get_next_for_background_download_at(
        &self,
        now: DateTime,
    ) -> Result<Option<eh_download_queue::Model>> {
        let entry = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_PENDING),
            )
            .filter(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::BackgroundDownloadNextRetryAt.lte(now)),
            )
            .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
            .one(&self.db)
            .await
            .context("Failed to fetch next background EH download")?;

        let Some(model) = entry else {
            return Ok(None);
        };

        let generation = next_claim_generation(now, model.started_at)?;
        let lease_started_at = next_claim_generation(now, None)?;
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_RUNNING.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(Some(lease_started_at)),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(generation),
            )
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_PENDING),
            )
            .filter(claim_generation_filter(model.started_at))
            .filter(
                sea_orm::Condition::any()
                    .add(eh_download_queue::Column::BackgroundDownloadNextRetryAt.is_null())
                    .add(eh_download_queue::Column::BackgroundDownloadNextRetryAt.lte(now)),
            )
            .exec(&self.db)
            .await
            .context("Failed to atomically claim background EH download")?;

        if result.rows_affected == 0 {
            return Ok(None);
        }

        let updated = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Id.eq(model.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
            )
            .filter(eh_download_queue::Column::StartedAt.eq(generation))
            .filter(eh_download_queue::Column::BackgroundDownloadStartedAt.eq(lease_started_at))
            .one(&self.db)
            .await?;
        Ok(updated)
    }

    pub async fn mark_eh_background_download_downloaded(
        &self,
        id: i32,
        file_size: i64,
        zip_path: &str,
        gp_cost: i64,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();
        let result = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_DOWNLOADED),
            )
            .col_expr(eh_download_queue::Column::FileSize, Expr::value(file_size))
            .col_expr(eh_download_queue::Column::GpCost, Expr::value(gp_cost))
            .col_expr(
                eh_download_queue::Column::ZipPath,
                Expr::value(Some(zip_path.to_string())),
            )
            .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
            )
            .exec(&self.db)
            .await
            .context("Failed to mark background EH download as downloaded")?;

        if result.rows_affected != 1 {
            self.clear_background_download_if_inactive(id).await?;
            anyhow::bail!("Cannot mark background EH download {} as downloaded", id);
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after mark background downloaded")?;
        Ok(model)
    }

    pub async fn schedule_eh_background_download_retry(
        &self,
        id: i32,
        error: &str,
        max_attempts: u8,
    ) -> Result<(eh_download_queue::Model, bool)> {
        let entry = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .context("Failed to fetch background EH download")?
            .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;
        let new_attempts = entry.background_download_attempt_count + 1;
        let permanent = new_attempts >= max_attempts as i32;
        let now = Local::now().naive_local();

        let mut update = eh_download_queue::Entity::update_many();
        if permanent {
            update = update
                .col_expr(
                    eh_download_queue::Column::Status,
                    Expr::value(STATUS_FAILED),
                )
                .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
                .col_expr(
                    eh_download_queue::Column::Error,
                    Expr::value(Some(error.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadStatus,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadError,
                    Expr::value(None::<String>),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadAttemptCount,
                    Expr::value(0),
                );
        } else {
            let delay = Self::backoff_delay_secs(new_attempts);
            update = update
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadStatus,
                    Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadStartedAt,
                    Expr::value(None::<DateTime>),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                    Expr::value(Some(now + chrono::Duration::seconds(delay))),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadError,
                    Expr::value(Some(error.to_string())),
                )
                .col_expr(
                    eh_download_queue::Column::BackgroundDownloadAttemptCount,
                    Expr::value(new_attempts),
                );
        }

        let result = update
            .filter(eh_download_queue::Column::Id.eq(id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(
                eh_download_queue::Column::BackgroundDownloadStatus.eq(BACKGROUND_STATUS_RUNNING),
            )
            .exec(&self.db)
            .await
            .context("Failed to schedule background EH download retry")?;

        if result.rows_affected != 1 {
            self.clear_background_download_if_inactive(id).await?;
            anyhow::bail!("Cannot schedule background EH download retry {}", id);
        }

        let model = eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after background retry")?;
        Ok((model, permanent))
    }

    /// Delete ZIP/partial ZIP files in the cache dir that have no corresponding
    /// active or retryable queue entry.
    pub async fn cleanup_eh_cache_orphans(&self, cache_dir: &std::path::Path) -> Result<()> {
        if !cache_dir.exists() {
            return Ok(());
        }

        let active_final_identities: HashSet<std::path::PathBuf> =
            eh_download_queue::Entity::find()
                .filter(eh_download_queue::Column::Status.is_in([
                    STATUS_PENDING,
                    STATUS_DOWNLOADING,
                    STATUS_DOWNLOADED,
                    STATUS_UPLOADING,
                    STATUS_UPLOADED,
                    STATUS_PUBLISHING,
                ]))
                .all(&self.db)
                .await?
                .into_iter()
                .flat_map(|entry| expected_eh_cache_zip_paths(cache_dir, entry))
                .collect();

        let mut artifact_families: HashMap<std::path::PathBuf, ArchiveArtifacts> = HashMap::new();
        for entry in std::fs::read_dir(cache_dir).context("Failed to read eh_cache dir")? {
            let entry = entry?;
            let path = entry.path();
            let Some(artifacts) = ArchiveArtifacts::from_member(&path) else {
                continue;
            };
            artifact_families
                .entry(artifacts.final_zip().to_path_buf())
                .or_insert(artifacts);
        }

        for (final_zip, artifacts) in artifact_families {
            let result = if !active_final_identities.contains(&final_zip) {
                artifacts.remove_all().await
            } else if final_zip.exists() {
                artifacts.remove_multipart_state().await
            } else {
                continue;
            };
            if let Err(e) = result {
                warn!("Failed to cleanup EH archive artifacts: {}", e);
            }
        }

        Ok(())
    }
}

fn expected_eh_cache_zip_paths(
    cache_dir: &std::path::Path,
    entry: eh_download_queue::Model,
) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(zip_path) = entry.zip_path {
        paths.push(std::path::PathBuf::from(zip_path));
    }
    if matches!(entry.status.as_str(), STATUS_PENDING | STATUS_DOWNLOADING) {
        paths.push(cache_dir.join(format!("{}_{}.zip", entry.gid, entry.token)));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entities::eh_download_queue::{Column, Entity};
    use crate::db::repo::tests_helpers;
    use chrono::{Duration, NaiveDate, Utc};
    use sea_orm::{sea_query::Expr, ConnectionTrait, DbBackend, Statement};

    async fn set_download_claim_fields(
        repo: &Repo,
        id: i32,
        created_at: DateTime,
        next_retry_at: Option<DateTime>,
        background_download_status: Option<&str>,
    ) {
        Entity::update_many()
            .col_expr(Column::CreatedAt, Expr::value(created_at))
            .col_expr(Column::NextRetryAt, Expr::value(next_retry_at))
            .col_expr(
                Column::BackgroundDownloadStatus,
                Expr::value(background_download_status.map(str::to_owned)),
            )
            .filter(Column::Id.eq(id))
            .exec(&repo.db)
            .await
            .unwrap();
    }

    #[test]
    fn test_next_claim_generation_is_monotonic_at_second_precision() {
        let second = NaiveDate::from_ymd_opt(2026, 7, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let fractional_now = second + Duration::milliseconds(900);
        let fractional_previous = second + Duration::milliseconds(100);

        assert_eq!(next_claim_generation(fractional_now, None).unwrap(), second);
        assert_eq!(
            next_claim_generation(fractional_now, Some(fractional_previous)).unwrap(),
            second + Duration::seconds(1)
        );
        assert_eq!(
            next_claim_generation(second, Some(second + Duration::seconds(8))).unwrap(),
            second + Duration::seconds(9)
        );
        assert_eq!(
            next_claim_generation(second + Duration::seconds(8), Some(second)).unwrap(),
            second + Duration::seconds(8)
        );
        assert!(next_claim_generation(DateTime::MAX, Some(DateTime::MAX)).is_err());
    }

    #[tokio::test]
    async fn test_subscription_enqueue_records_origin_subscription() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_subscription_download(-100, 123, 40, "tok", "Title", false)
            .await
            .unwrap();

        assert_eq!(model.source, SOURCE_SUBSCRIPTION);
        assert_eq!(model.subscription_ids.as_deref(), Some("123"));

        let direct = repo
            .enqueue_eh_download(-100, 41, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(direct.subscription_ids, None);
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_cancels_only_active_subscription_rows() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let sub_row = repo
            .enqueue_eh_subscription_download(-100, 123, 40, "tok", "Title", false)
            .await
            .unwrap();
        let other_sub_row = repo
            .enqueue_eh_subscription_download(-100, 456, 41, "tok", "Title", false)
            .await
            .unwrap();
        let direct_row = repo
            .enqueue_eh_download(-100, 42, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let done_row = repo
            .enqueue_eh_subscription_download(-100, 123, 43, "tok", "Title", false)
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_DONE))
            .filter(Column::Id.eq(done_row.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let changed = repo
            .cancel_eh_subscription_queue_entries(123)
            .await
            .unwrap();
        assert_eq!(changed, 2);
        let canceled = Entity::find_by_id(sub_row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canceled.status, STATUS_CANCELED);
        assert_eq!(canceled.subscription_ids, None);
        assert!(Entity::find_by_id(other_sub_row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .is_some());
        assert!(Entity::find_by_id(direct_row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .is_some());
        let done = Entity::find_by_id(done_row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, STATUS_DONE);
        assert_eq!(done.subscription_ids, None);
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_keeps_other_subscription_owners() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let row = repo
            .enqueue_eh_subscription_download(-100, 123, 44, "tok", "Title", false)
            .await
            .unwrap();
        let merged = repo
            .enqueue_eh_subscription_download(-100, 456, 44, "tok2", "Title 2", false)
            .await
            .unwrap();
        assert_eq!(merged.id, row.id);
        assert_eq!(merged.subscription_ids.as_deref(), Some("123,456"));

        let changed = repo
            .cancel_eh_subscription_queue_entries(123)
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let remaining = Entity::find_by_id(row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(remaining.status, STATUS_PENDING);
        assert_eq!(remaining.subscription_ids.as_deref(), Some("456"));

        let changed = repo
            .cancel_eh_subscription_queue_entries(456)
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let canceled = Entity::find_by_id(row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canceled.status, STATUS_CANCELED);
        assert_eq!(canceled.subscription_ids, None);
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_removes_stale_telegraph_requirement() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let telegraph_owner = repo
            .enqueue_eh_subscription_download(-100, 123, 52, "tok", "Title", true)
            .await
            .unwrap();
        let merged = repo
            .enqueue_eh_subscription_download(-100, 456, 52, "tok2", "Title 2", false)
            .await
            .unwrap();
        assert_eq!(merged.id, telegraph_owner.id);
        assert!(merged.telegraph);
        assert_eq!(merged.subscription_ids.as_deref(), Some("123,456"));
        assert_eq!(merged.telegraph_subscription_ids.as_deref(), Some("123"));
        Entity::update_many()
            .col_expr(
                Column::NextRetryAt,
                Expr::value(Some(
                    chrono::Local::now().naive_local() + chrono::Duration::hours(1),
                )),
            )
            .filter(Column::Id.eq(merged.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let changed = repo
            .cancel_eh_subscription_queue_entries(123)
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let row = Entity::find_by_id(telegraph_owner.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert_eq!(row.subscription_ids.as_deref(), Some("456"));
        assert_eq!(row.telegraph_subscription_ids, None);
        assert!(!row.telegraph);
        assert!(row.next_retry_at.is_none());
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_preserves_concurrent_telegraph_upgrade() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        repo.enqueue_eh_subscription_download(-100, 123, 54, "tok", "Title", false)
            .await
            .unwrap();
        let stale = repo
            .enqueue_eh_subscription_download(-100, 456, 54, "tok2", "Title 2", false)
            .await
            .unwrap();
        assert_eq!(stale.subscription_ids.as_deref(), Some("123,456"));
        assert_eq!(stale.telegraph_subscription_ids, None);

        Entity::update_many()
            .col_expr(Column::Telegraph, Expr::value(true))
            .col_expr(
                Column::TelegraphSubscriptionIds,
                Expr::value(Some("456".to_string())),
            )
            .filter(Column::Id.eq(stale.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let row_id = stale.id;
        let changed = repo
            .remove_subscription_owner_from_eh_row(stale, 123)
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let row = Entity::find_by_id(row_id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.subscription_ids.as_deref(), Some("456"));
        assert_eq!(row.telegraph_subscription_ids.as_deref(), Some("456"));
        assert!(row.telegraph);
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_scrubs_terminal_telegraph_owner() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let row = repo
            .enqueue_eh_subscription_download(-100, 123, 53, "tok", "Title", true)
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_DONE))
            .col_expr(
                Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/old".to_string())),
            )
            .filter(Column::Id.eq(row.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let changed = repo
            .cancel_eh_subscription_queue_entries(123)
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let scrubbed = Entity::find_by_id(row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scrubbed.status, STATUS_DONE);
        assert_eq!(scrubbed.subscription_ids, None);
        assert_eq!(scrubbed.telegraph_subscription_ids, None);
        assert!(!scrubbed.telegraph);
        assert!(scrubbed.telegraph_url.is_none());

        let reenqueued = repo
            .enqueue_eh_download(-100, 53, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reenqueued.status, STATUS_PENDING);
        assert!(!reenqueued.telegraph);
    }

    #[tokio::test]
    async fn test_merge_preserves_concurrent_subscription_owner_updates() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let row = repo
            .enqueue_eh_subscription_download(-100, 123, 45, "tok", "Title", false)
            .await
            .unwrap();

        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::SubscriptionIds,
                Expr::value(Some("123,789".to_string())),
            )
            .filter(eh_download_queue::Column::Id.eq(row.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let merged = repo
            .merge_eh_download(
                row,
                "tok2",
                "Title 2",
                false,
                SOURCE_SUBSCRIPTION,
                Some(456),
            )
            .await
            .unwrap();
        assert_eq!(merged.subscription_ids.as_deref(), Some("123,456,789"));
    }

    #[tokio::test]
    async fn test_inactive_check_preserves_concurrent_live_subscription_owner() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let now = chrono::Local::now().naive_local();
        repo.upsert_chat(-100, "private".to_string(), None, true, Default::default())
            .await
            .unwrap();
        let task = crate::db::entities::tasks::ActiveModel {
            r#type: Set(crate::db::types::TaskType::Ehentai),
            value: Set("eh:test".to_string()),
            author_name: Set(None),
            next_poll_at: Set(now),
            last_polled_at: Set(None),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let live_sub = crate::db::entities::subscriptions::ActiveModel {
            chat_id: Set(-100),
            task_id: Set(task.id),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let row = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(46i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(Some("123".to_string())),
            status: Set(STATUS_PUBLISHING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();

        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::SubscriptionIds,
                Expr::value(Some(format!("123,{}", live_sub.id))),
            )
            .filter(eh_download_queue::Column::Id.eq(row.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let row_id = row.id;
        let active = repo
            .eh_download_has_live_owner_or_cancel(row, STATUS_PUBLISHING)
            .await
            .unwrap();
        assert!(active, "fresh live owner should prevent soft cancel");
        let persisted = Entity::find_by_id(row_id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, STATUS_PUBLISHING);
    }

    #[tokio::test]
    async fn test_inactive_check_cancels_row_without_live_subscription_owner() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let now = chrono::Local::now().naive_local();
        let row = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(47i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(Some("123".to_string())),
            status: Set(STATUS_PUBLISHING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let row_id = row.id;

        let active = repo
            .eh_download_has_live_owner_or_cancel(row, STATUS_PUBLISHING)
            .await
            .unwrap();
        assert!(!active, "missing owner should make row inactive");
        let persisted = Entity::find_by_id(row_id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, STATUS_CANCELED);
        assert_eq!(persisted.subscription_ids, None);
    }

    #[tokio::test]
    async fn test_cancel_legacy_subscription_queue_entries_without_owner_tracking() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let now = chrono::Local::now().naive_local();
        let legacy = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(48i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(true),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(None),
            status: Set(STATUS_DOWNLOADED.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            telegraph_url: Set(Some("https://telegra.ph/stale".to_string())),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let direct = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(49i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_DIRECT.to_string()),
            subscription_ids: Set(None),
            status: Set(STATUS_DOWNLOADED.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let terminal = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(50i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(None),
            status: Set(STATUS_DONE.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();
        let already_canceled = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(51i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(true),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(None),
            status: Set(STATUS_CANCELED.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            telegraph_url: Set(Some("https://telegra.ph/stale-canceled".to_string())),
            ..Default::default()
        }
        .insert(&repo.db)
        .await
        .unwrap();

        let count = repo
            .cancel_legacy_eh_subscription_queue_entries()
            .await
            .unwrap();
        assert_eq!(count, 3);
        let legacy = Entity::find_by_id(legacy.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(legacy.status, STATUS_CANCELED);
        assert!(!legacy.telegraph);
        assert!(legacy.telegraph_url.is_none());
        let already_canceled = Entity::find_by_id(already_canceled.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(already_canceled.status, STATUS_CANCELED);
        assert!(!already_canceled.telegraph);
        assert!(already_canceled.telegraph_url.is_none());
        assert_eq!(
            Entity::find_by_id(direct.id)
                .one(&repo.db)
                .await
                .unwrap()
                .unwrap()
                .status,
            STATUS_DOWNLOADED
        );
        assert_eq!(
            Entity::find_by_id(terminal.id)
                .one(&repo.db)
                .await
                .unwrap()
                .unwrap()
                .status,
            STATUS_CANCELED
        );
    }

    #[tokio::test]
    async fn test_reenqueue_during_downloading_blocks_stale_download_completion() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 40, "tok", "Title", false, SOURCE_SUBSCRIPTION)
            .await
            .unwrap();

        // Claim for download (status -> downloading)
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        assert_eq!(claimed.status, STATUS_DOWNLOADING);

        // Re-enqueue with source upgrade causes full reset (status -> pending)
        let reset = repo
            .enqueue_eh_download(-100, 40, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reset.id, model.id);
        assert_eq!(reset.status, STATUS_PENDING);

        // Stale worker tries to mark the old claimed row as downloaded — must fail
        let err = repo
            .mark_eh_download_downloaded(claimed.id, 9999, "/tmp/40.zip", 0)
            .await;
        assert!(
            err.is_err(),
            "stale downloaded completion should be blocked"
        );

        // Verify final state is still pending, not overwritten
        let final_row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_row.status, STATUS_PENDING);
        assert_ne!(final_row.file_size, 9999);
    }

    #[tokio::test]
    async fn test_publish_claim_requires_telegraph_false_for_downloaded() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 45, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        // Download stage: claim -> downloading -> downloaded
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 5000, "/tmp/45.zip", 0)
            .await
            .unwrap();

        // Row is now downloaded, telegraph=false — publish should claim it
        let pub_claimed = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(pub_claimed.id, model.id);
        assert_eq!(pub_claimed.status, STATUS_PUBLISHING);
        // Reset back to downloaded for the next check
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_DOWNLOADED))
            .col_expr(Column::Telegraph, Expr::value(true))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();

        // Now row is downloaded, telegraph=true — publish should NOT claim it
        let none = repo.get_next_for_publish().await.unwrap();
        assert!(
            none.is_none(),
            "publish should not claim downloaded row with telegraph=true"
        );
    }

    #[tokio::test]
    async fn test_marker_methods_require_publishing_status() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 50, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        // Row is pending — archive marker should fail
        let err = repo.mark_eh_archive_sent(model.id).await;
        assert!(
            err.is_err(),
            "mark_eh_archive_sent should fail on non-publishing row"
        );

        // Telegraph marker should also fail
        let err = repo.mark_eh_telegraph_sent(model.id).await;
        assert!(
            err.is_err(),
            "mark_eh_telegraph_sent should fail on non-publishing row"
        );

        // Move to publishing
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_PUBLISHING))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();

        // Now markers should succeed
        repo.mark_eh_archive_sent(model.id).await.unwrap();
        repo.mark_eh_telegraph_sent(model.id).await.unwrap();
    }

    #[tokio::test]
    async fn test_defer_rejects_invalid_status_transition() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 55, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        // Defer from pending to publishing — invalid (not an in-flight status)
        let err = repo
            .defer_eh_download(model.id, STATUS_PUBLISHING, 60)
            .await;
        assert!(err.is_err(), "defer to publishing from pending should fail");

        // Defer from pending to failed — invalid (not a legal target)
        let err = repo.defer_eh_download(model.id, STATUS_FAILED, 60).await;
        assert!(
            err.is_err(),
            "defer to failed should be rejected as invalid target"
        );

        // Defer from pending to pending — invalid (must be from downloading)
        let err = repo.defer_eh_download(model.id, STATUS_PENDING, 60).await;
        assert!(
            err.is_err(),
            "defer to pending from pending should fail (must be from downloading)"
        );
    }

    #[tokio::test]
    async fn test_enqueue_merges_telegraph_and_direct_source() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let first = repo
            .enqueue_eh_download(-100, 10, "old", "Old", false, SOURCE_SUBSCRIPTION)
            .await
            .unwrap();
        let merged = repo
            .enqueue_eh_download(-100, 10, "new", "New", true, SOURCE_DIRECT)
            .await
            .unwrap();

        assert_eq!(first.id, merged.id);
        assert!(merged.telegraph);
        assert_eq!(merged.source, SOURCE_DIRECT);
        assert_eq!(merged.token, "new");
        assert_eq!(merged.title, "New");
    }

    #[tokio::test]
    async fn test_downloaded_bytes_window_counts_all_downloaded_states() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        for (gid, status, size) in [
            (1, STATUS_DOWNLOADED, 100),
            (2, STATUS_UPLOADING, 200),
            (3, STATUS_UPLOADED, 300),
            (4, STATUS_PUBLISHING, 400),
            (5, STATUS_DONE, 500),
            (6, STATUS_FAILED, 600),
        ] {
            let model = repo
                .enqueue_eh_download(-100, gid, "tok", "Title", false, SOURCE_DIRECT)
                .await
                .unwrap();
            Entity::update_many()
                .col_expr(Column::Status, Expr::value(status))
                .col_expr(Column::FileSize, Expr::value(size))
                .col_expr(Column::CompletedAt, Expr::value(Utc::now().naive_utc()))
                .filter(Column::Id.eq(model.id))
                .exec(&repo.db)
                .await
                .unwrap();
        }

        let bytes = repo.get_eh_downloaded_bytes_in_window(24).await.unwrap();
        assert_eq!(bytes, 2100);
    }

    #[tokio::test]
    async fn test_cleanup_eh_cache_orphans_removes_partial_without_active_zip() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path();

        let orphan_zip = cache_dir.join("orphan.zip");
        let orphan_part = cache_dir.join("orphan.zip.part");
        let orphan_parts = cache_dir.join("orphan.zip.parts");
        let active_zip = cache_dir.join("active.zip");
        let active_part = cache_dir.join("active.zip.part");
        let active_parts = cache_dir.join("active.zip.parts");
        let unrelated = cache_dir.join("notes").join("keep.txt");
        std::fs::write(&orphan_zip, b"zip").unwrap();
        std::fs::write(&orphan_part, b"partial").unwrap();
        std::fs::create_dir_all(orphan_parts.join("nested")).unwrap();
        std::fs::write(orphan_parts.join("manifest.json"), b"manifest").unwrap();
        std::fs::write(orphan_parts.join("nested").join("part-0001"), b"part").unwrap();
        std::fs::write(&active_zip, b"zip").unwrap();
        std::fs::write(&active_part, b"partial").unwrap();
        std::fs::create_dir_all(active_parts.join("nested")).unwrap();
        std::fs::write(active_parts.join("manifest.json"), b"manifest").unwrap();
        std::fs::write(active_parts.join("nested").join("part-0001"), b"part").unwrap();
        std::fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        let model = repo
            .enqueue_eh_download(-100, 77, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 5000, active_zip.to_str().unwrap(), 0)
            .await
            .unwrap();

        repo.cleanup_eh_cache_orphans(cache_dir).await.unwrap();

        assert!(!orphan_zip.exists(), "orphan final ZIP should be removed");
        assert!(
            !orphan_part.exists(),
            "orphan partial ZIP should be removed"
        );
        assert!(
            !orphan_parts.exists(),
            "orphan multipart state should be removed recursively"
        );
        assert!(active_zip.exists(), "active final ZIP should be kept");
        assert!(
            !active_part.exists(),
            "active final ZIP should discard stale assembly scratch"
        );
        assert!(
            !active_parts.exists(),
            "active final ZIP should discard stale multipart state"
        );
        assert!(
            unrelated.exists(),
            "unrelated directories should be ignored"
        );
    }

    #[tokio::test]
    async fn test_cleanup_eh_cache_orphans_keeps_pending_resume_partial() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path();

        let model = repo
            .enqueue_eh_download(-100, 88, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);

        let part = cache_dir.join("88_tok.zip.part");
        let parts_dir = cache_dir.join("88_tok.zip.parts");
        std::fs::write(&part, b"partial").unwrap();
        std::fs::create_dir_all(parts_dir.join("nested")).unwrap();
        std::fs::write(parts_dir.join("manifest.json"), b"manifest").unwrap();
        std::fs::write(parts_dir.join("nested").join("part-0001"), b"part").unwrap();

        let reset = repo.reset_stale_eh_downloads().await.unwrap();
        assert_eq!(reset, 1);
        repo.cleanup_eh_cache_orphans(cache_dir).await.unwrap();

        assert!(
            part.exists(),
            "pending retry partial should be kept for resumable download"
        );
        assert!(
            parts_dir.exists(),
            "pending retry multipart state should be kept for resumable download"
        );

        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_CANCELED))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();

        repo.cleanup_eh_cache_orphans(cache_dir).await.unwrap();
        assert!(
            !part.exists(),
            "canceled queue partial should be removed as orphan"
        );
        assert!(
            !parts_dir.exists(),
            "canceled queue multipart state should be removed recursively"
        );
    }

    #[tokio::test]
    async fn test_publish_markers_survive_stale_reset_and_clear_on_terminal_reset() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 20, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        // Move to publishing via the normal pipeline so CAS guards pass.
        // download: claim -> downloading -> downloaded
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 5000, "/tmp/20.zip", 0)
            .await
            .unwrap();

        // Simulate upload: set telegraph_url so publish claims from uploaded branch
        Entity::update_many()
            .col_expr(
                Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/20".to_string())),
            )
            .col_expr(Column::Status, Expr::value(STATUS_UPLOADED))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();

        // Claim for publish -> publishing
        let pub_claimed = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(pub_claimed.id, model.id);
        assert_eq!(pub_claimed.status, STATUS_PUBLISHING);

        // Mark archive and telegraph sent while publishing
        repo.mark_eh_archive_sent(model.id).await.unwrap();
        repo.mark_eh_telegraph_sent(model.id).await.unwrap();

        // Defer back to publishing (simulates stale worker)
        repo.defer_eh_download(model.id, STATUS_UPLOADED, 60)
            .await
            .unwrap();
        // Move back to publishing manually for reset
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_PUBLISHING))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();

        repo.reset_stale_eh_downloads().await.unwrap();
        let preserved = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        // Both markers survive stale reset (publishing with telegraph_url -> uploaded)
        assert!(preserved.archive_sent_at.is_some());
        assert!(preserved.telegraph_sent_at.is_some());

        // Terminal reset clears both markers
        repo.mark_eh_download_failed(model.id, "failed")
            .await
            .unwrap();
        let reset = repo
            .enqueue_eh_download(-100, 20, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert!(reset.archive_sent_at.is_none());
        assert!(reset.telegraph_sent_at.is_none());
    }

    #[tokio::test]
    async fn test_defer_does_not_increment_retry_count() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 30, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        // Claim to downloading so defer-to-pending passes the CAS guard.
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        assert_eq!(claimed.status, STATUS_DOWNLOADING);

        repo.defer_eh_download(model.id, STATUS_PENDING, 60)
            .await
            .unwrap();
        let deferred = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deferred.status, STATUS_PENDING);
        assert_eq!(deferred.retry_count, 0);
        assert!(deferred.next_retry_at.is_some());
    }

    #[tokio::test]
    async fn test_deferred_item_not_claimable_before_delay_expires() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 35, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        // Claim to downloading so defer-to-pending passes the CAS guard.
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);

        // Defer with a long delay — the item should not be picked up
        repo.defer_eh_download(model.id, STATUS_PENDING, 3600)
            .await
            .unwrap();

        // get_next_for_download filters on next_retry_at <= now, so should return None
        let next = repo.get_next_for_download().await.unwrap();
        assert!(
            next.is_none(),
            "deferred item should not be claimable before delay expires"
        );

        let reloaded = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.retry_count, 0);
        assert!(reloaded.next_retry_at.is_some());
    }

    #[tokio::test]
    async fn test_background_owned_item_is_excluded_from_main_download_queue() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let slow = repo
            .enqueue_eh_download(-100, 40, "slow", "Slow", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let fast = repo
            .enqueue_eh_download(-100, 41, "fast", "Fast", false, SOURCE_DIRECT)
            .await
            .unwrap();

        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, slow.id);
        let background = repo
            .schedule_eh_background_download_from(claimed.id, STATUS_DOWNLOADING, "too slow")
            .await
            .unwrap();
        assert_eq!(background.status, STATUS_PENDING);
        assert_eq!(
            background.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );

        let next = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(next.id, fast.id);
    }

    #[tokio::test]
    async fn test_main_download_claim_prioritizes_recent_fifo_then_old_lifo() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let anchor = NaiveDate::from_ymd_opt(2026, 7, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();

        let recent_first = repo
            .enqueue_eh_download(-100, 100, "tok", "Recent first", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let recent_second = repo
            .enqueue_eh_download(-100, 101, "tok", "Recent second", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let recent_newer = repo
            .enqueue_eh_download(-100, 102, "tok", "Recent newer", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let cutoff_first = repo
            .enqueue_eh_download(-100, 200, "tok", "Cutoff first", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let cutoff_second = repo
            .enqueue_eh_download(-100, 201, "tok", "Cutoff second", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let old = repo
            .enqueue_eh_download(-100, 300, "tok", "Old", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let future_retry = repo
            .enqueue_eh_download(-100, 400, "tok", "Future retry", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let background_old = repo
            .enqueue_eh_download(-100, 500, "tok", "Background old", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let background_recent = repo
            .enqueue_eh_download(-100, 501, "tok", "Background recent", false, SOURCE_DIRECT)
            .await
            .unwrap();

        set_download_claim_fields(
            &repo,
            recent_first.id,
            anchor - Duration::minutes(90),
            None,
            None,
        )
        .await;
        set_download_claim_fields(
            &repo,
            recent_second.id,
            anchor - Duration::minutes(90),
            None,
            None,
        )
        .await;
        set_download_claim_fields(
            &repo,
            recent_newer.id,
            anchor - Duration::minutes(30),
            None,
            None,
        )
        .await;
        set_download_claim_fields(
            &repo,
            cutoff_first.id,
            anchor - Duration::hours(2),
            None,
            None,
        )
        .await;
        set_download_claim_fields(
            &repo,
            cutoff_second.id,
            anchor - Duration::hours(2),
            None,
            None,
        )
        .await;
        set_download_claim_fields(&repo, old.id, anchor - Duration::hours(3), None, None).await;
        set_download_claim_fields(
            &repo,
            future_retry.id,
            anchor - Duration::hours(4),
            Some(anchor + Duration::minutes(1)),
            None,
        )
        .await;
        set_download_claim_fields(
            &repo,
            background_old.id,
            anchor - Duration::hours(3),
            None,
            Some(BACKGROUND_STATUS_PENDING),
        )
        .await;
        set_download_claim_fields(
            &repo,
            background_recent.id,
            anchor - Duration::hours(1),
            None,
            Some(BACKGROUND_STATUS_PENDING),
        )
        .await;

        assert!(recent_first.id < recent_second.id);
        assert!(cutoff_first.id < cutoff_second.id);
        for expected_gid in [100, 101, 102, 201, 200, 300] {
            let claimed = repo
                .get_next_for_download_at(anchor)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(claimed.gid, expected_gid);
            assert_eq!(claimed.status, STATUS_DOWNLOADING);
        }

        assert!(repo
            .get_next_for_download_at(anchor)
            .await
            .unwrap()
            .is_none());
        let deferred = Entity::find_by_id(future_retry.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deferred.status, STATUS_PENDING);
        assert_eq!(deferred.next_retry_at, Some(anchor + Duration::minutes(1)));

        let first_background = repo
            .get_next_for_background_download()
            .await
            .unwrap()
            .unwrap();
        let second_background = repo
            .get_next_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_background.gid, background_old.gid);
        assert_eq!(second_background.gid, background_recent.gid);
    }

    #[tokio::test]
    async fn test_background_download_lifecycle_success_retry_and_stale_reset() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 45, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        let main_generation = claimed.started_at;
        let handed_off = repo
            .schedule_eh_background_download_from(claimed.id, STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        assert_eq!(handed_off.started_at, main_generation);

        let bg_claim = repo
            .get_next_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bg_claim.id, model.id);
        assert_eq!(
            bg_claim.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );
        assert!(bg_claim.started_at > main_generation);
        let background_generation = bg_claim.started_at;

        let (retry, permanent) = repo
            .schedule_eh_background_download_retry(bg_claim.id, "still slow", 6)
            .await
            .unwrap();
        assert!(!permanent);
        assert_eq!(retry.status, STATUS_PENDING);
        assert_eq!(retry.background_download_attempt_count, 1);
        assert_eq!(
            retry.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );
        assert!(retry.background_download_next_retry_at.is_some());
        assert_eq!(retry.started_at, background_generation);

        Entity::update_many()
            .col_expr(
                Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_RUNNING.to_string())),
            )
            .col_expr(
                Column::BackgroundDownloadStartedAt,
                Expr::value(Some(
                    Local::now().naive_local() - chrono::Duration::seconds(7200),
                )),
            )
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        let reset = repo.reset_stale_background_downloads(3600).await.unwrap();
        assert_eq!(reset, 1);
        let reset_row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reset_row.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );
        assert!(reset_row.background_download_started_at.is_none());
        assert_eq!(reset_row.started_at, background_generation);

        Entity::update_many()
            .col_expr(
                Column::BackgroundDownloadNextRetryAt,
                Expr::value(Some(
                    Local::now().naive_local() - chrono::Duration::seconds(1),
                )),
            )
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let bg_claim = repo
            .get_next_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert!(bg_claim.started_at > background_generation);
        let done = repo
            .mark_eh_background_download_downloaded(bg_claim.id, 1234, "/tmp/bg.zip", 0)
            .await
            .unwrap();
        assert_eq!(done.status, STATUS_DOWNLOADED);
        assert_eq!(done.file_size, 1234);
        assert_eq!(done.zip_path.as_deref(), Some("/tmp/bg.zip"));
        assert!(done.background_download_status.is_none());
        assert!(done.background_download_error.is_none());
        assert_eq!(done.started_at, bg_claim.started_at);
    }

    #[tokio::test]
    async fn test_release_background_downloads_to_main_queue_clears_pending_background_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 46, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        repo.schedule_eh_background_download_from(claimed.id, STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();

        let released = repo
            .release_background_downloads_to_main_queue()
            .await
            .unwrap();
        assert_eq!(released, 1);
        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert!(row.background_download_status.is_none());
        assert_eq!(row.background_download_attempt_count, 0);

        let next = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(next.id, model.id);
    }

    #[tokio::test]
    async fn test_cancel_subscription_queue_entries_clears_background_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_subscription_download(-100, 123, 47, "tok", "Title", false)
            .await
            .unwrap();

        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        repo.schedule_eh_background_download_from(claimed.id, STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();

        repo.cancel_eh_subscription_queue_entries(123)
            .await
            .unwrap();
        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_CANCELED);
        assert!(row.background_download_status.is_none());
        assert_eq!(row.background_download_attempt_count, 0);
    }

    #[tokio::test]
    async fn test_reenqueue_terminal_row_clears_background_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let generation = NaiveDate::from_ymd_opt(2026, 7, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let model = repo
            .enqueue_eh_download(-100, 48, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_FAILED))
            .col_expr(Column::StartedAt, Expr::value(Some(generation)))
            .col_expr(
                Column::BackgroundDownloadStatus,
                Expr::value(Some(BACKGROUND_STATUS_PENDING.to_string())),
            )
            .col_expr(Column::BackgroundDownloadAttemptCount, Expr::value(5))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let reenqueued = repo
            .enqueue_eh_download(-100, 48, "tok2", "Title 2", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reenqueued.status, STATUS_PENDING);
        assert_eq!(reenqueued.started_at, Some(generation));
        assert!(reenqueued.background_download_status.is_none());
        assert_eq!(reenqueued.background_download_attempt_count, 0);
    }

    #[tokio::test]
    async fn test_background_completion_cleans_canceled_race_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 49, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        repo.schedule_eh_background_download_from(claimed.id, STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        let bg_claim = repo
            .get_next_for_background_download()
            .await
            .unwrap()
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_CANCELED))
            .filter(Column::Id.eq(bg_claim.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let err = repo
            .mark_eh_background_download_downloaded(bg_claim.id, 10, "/tmp/bg.zip", 0)
            .await
            .expect_err("canceled row should not be overwritten by background completion");
        assert!(err
            .to_string()
            .contains("Cannot mark background EH download"));

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_CANCELED);
        assert!(row.background_download_status.is_none());
        assert_eq!(row.background_download_attempt_count, 0);
    }

    #[tokio::test]
    async fn test_background_retry_permanent_failure_clears_background_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 50, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        repo.schedule_eh_background_download_from(claimed.id, STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        let bg_claim = repo
            .get_next_for_background_download()
            .await
            .unwrap()
            .unwrap();

        let (failed, permanent) = repo
            .schedule_eh_background_download_retry(bg_claim.id, "exhausted", 1)
            .await
            .unwrap();
        assert!(permanent);
        assert_eq!(failed.status, STATUS_FAILED);
        assert_eq!(failed.error.as_deref(), Some("exhausted"));
        assert!(failed.background_download_status.is_none());
        assert!(failed.background_download_started_at.is_none());
        assert!(failed.background_download_next_retry_at.is_none());
        assert!(failed.background_download_error.is_none());
        assert_eq!(failed.background_download_attempt_count, 0);

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.background_download_attempt_count, 0);
    }

    #[tokio::test]
    async fn test_enqueue_and_get_next_pending() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let model = repo
            .enqueue_eh_download(
                -100123,
                123456,
                "abcdef0123",
                "Test Gallery",
                false,
                SOURCE_SUBSCRIPTION,
            )
            .await
            .unwrap();

        assert_eq!(model.chat_id, -100123);
        assert_eq!(model.gid, 123456);
        assert_eq!(model.status, STATUS_PENDING);
        assert_eq!(model.source, SOURCE_SUBSCRIPTION);

        // get_next_pending should mark it as downloading
        let next = repo.get_next_pending_eh_download().await.unwrap().unwrap();
        assert_eq!(next.id, model.id);
        assert_eq!(next.status, STATUS_DOWNLOADING);
        assert!(next.started_at.is_some());

        // No more pending
        let none = repo.get_next_pending_eh_download().await.unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn test_mark_done() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let model = repo
            .enqueue_eh_download(-100, 1, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        // Download stage: claim -> downloading -> downloaded
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 50000, "/tmp/1.zip", 0)
            .await
            .unwrap();

        // Publish stage: claim -> publishing -> done
        let pub_claimed = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(pub_claimed.id, model.id);
        assert_eq!(pub_claimed.status, STATUS_PUBLISHING);

        let done = repo.mark_eh_download_done(model.id, 50000).await.unwrap();

        assert_eq!(done.status, STATUS_DONE);
        assert_eq!(done.file_size, 50000);
        assert!(done.completed_at.is_some());
        assert!(done.error.is_none());
    }

    #[tokio::test]
    async fn test_mark_failed() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let model = repo
            .enqueue_eh_download(-100, 1, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        repo.get_next_pending_eh_download().await.unwrap();
        let failed = repo
            .mark_eh_download_failed(model.id, "network error")
            .await
            .unwrap();

        assert_eq!(failed.status, STATUS_FAILED);
        assert_eq!(failed.error, Some("network error".to_string()));
        assert_eq!(failed.retry_count, 1);
        assert!(failed.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_downloaded_bytes_in_window() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        // Enqueue and complete two downloads through the full pipeline
        let m1 = repo
            .enqueue_eh_download(-100, 1, "tok1", "T1", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let c1 = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(c1.id, m1.id);
        repo.mark_eh_download_downloaded(m1.id, 10000, "/tmp/1.zip", 0)
            .await
            .unwrap();
        let p1 = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(p1.id, m1.id);
        repo.mark_eh_download_done(m1.id, 10000).await.unwrap();

        let m2 = repo
            .enqueue_eh_download(-100, 2, "tok2", "T2", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let c2 = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(c2.id, m2.id);
        repo.mark_eh_download_downloaded(m2.id, 20000, "/tmp/2.zip", 0)
            .await
            .unwrap();
        let p2 = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(p2.id, m2.id);
        repo.mark_eh_download_done(m2.id, 20000).await.unwrap();

        let bytes = repo.get_eh_downloaded_bytes_in_window(24).await.unwrap();
        assert_eq!(bytes, 30000);
    }

    #[tokio::test]
    async fn test_reset_stale_downloads() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        let m = repo
            .enqueue_eh_download(-100, 1, "tok", "T", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let first_claim = repo.get_next_pending_eh_download().await.unwrap().unwrap(); // marks as downloading

        // Simulate crash: entry is stuck in "downloading"
        let reset_count = repo.reset_stale_eh_downloads().await.unwrap();
        assert_eq!(reset_count, 1);
        let reset = Entity::find_by_id(m.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reset.status, STATUS_PENDING);
        assert_eq!(reset.started_at, first_claim.started_at);

        // Should be pending again
        let next = repo.get_next_pending_eh_download().await.unwrap().unwrap();
        assert_eq!(next.id, m.id);
        assert_eq!(next.status, STATUS_DOWNLOADING); // got_next marks it downloading again
        assert!(next.started_at > first_claim.started_at);
    }

    #[tokio::test]
    async fn test_main_claim_generation_survives_retry_handoff_and_completion() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let claim_now = NaiveDate::from_ymd_opt(2026, 7, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let entry = repo
            .enqueue_eh_download(-100, 69, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        let first_claim = repo
            .get_next_for_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        let (retry, permanent) = repo
            .schedule_eh_retry_from(entry.id, STATUS_DOWNLOADING, STATUS_PENDING, "temporary", 5)
            .await
            .unwrap();
        assert!(!permanent);
        assert_eq!(retry.started_at, first_claim.started_at);

        Entity::update_many()
            .col_expr(Column::NextRetryAt, Expr::value(None::<DateTime>))
            .filter(Column::Id.eq(entry.id))
            .exec(&repo.db)
            .await
            .unwrap();
        let second_claim = repo
            .get_next_for_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            second_claim.started_at,
            first_claim
                .started_at
                .map(|generation| generation + Duration::seconds(1))
        );

        let handed_off = repo
            .schedule_eh_background_download_from(entry.id, STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        assert_eq!(handed_off.started_at, second_claim.started_at);
        assert_eq!(
            repo.release_background_downloads_to_main_queue()
                .await
                .unwrap(),
            1
        );
        let released = Entity::find_by_id(entry.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(released.started_at, second_claim.started_at);

        let third_claim = repo
            .get_next_for_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            third_claim.started_at,
            second_claim
                .started_at
                .map(|generation| generation + Duration::seconds(1))
        );
        let downloaded = repo
            .mark_eh_download_downloaded(entry.id, 1024, "/tmp/69.zip", 0)
            .await
            .unwrap();
        assert_eq!(downloaded.started_at, third_claim.started_at);

        let publishing = repo.get_next_for_publish().await.unwrap().unwrap();
        assert!(publishing.started_at > downloaded.started_at);
        let done = repo.mark_eh_download_done(entry.id, 1024).await.unwrap();
        assert_eq!(done.started_at, publishing.started_at);
    }

    #[tokio::test]
    async fn test_count_pending() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        repo.enqueue_eh_download(-100, 1, "tok1", "T1", false, SOURCE_DIRECT)
            .await
            .unwrap();
        repo.enqueue_eh_download(-100, 2, "tok2", "T2", false, SOURCE_DIRECT)
            .await
            .unwrap();

        let count = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_queue_schema_has_publish_marker_columns() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let entry = repo
            .enqueue_eh_download(-100, 42, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert!(entry.archive_sent_at.is_none());
        assert!(entry.telegraph_sent_at.is_none());
    }

    /// Permanent retry must not overwrite a row that was re-enqueued between
    /// the initial select and the CAS update.
    #[tokio::test]
    async fn test_schedule_permanent_retry_does_not_fail_reenqueued_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 60, "tok", "Title", false, SOURCE_SUBSCRIPTION)
            .await
            .unwrap();

        // Claim for download (status -> downloading)
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        assert_eq!(claimed.status, STATUS_DOWNLOADING);

        // Re-enqueue with source upgrade (subscription -> direct) triggers full
        // reset to pending, simulating a re-enqueue that changes the row's status.
        let reenq = repo
            .enqueue_eh_download(-100, 60, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reenq.id, model.id);
        assert_eq!(reenq.status, STATUS_PENDING);

        // A stale worker (still holding the old "downloading" snapshot) tries to
        // permanently fail the row — must be rejected because current status is
        // no longer "downloading".
        let err = repo
            .schedule_eh_retry(claimed.id, STATUS_PENDING, "stale error", 0)
            .await;
        assert!(
            err.is_err(),
            "permanent retry with stale snapshot must be rejected"
        );

        // Verify the row is still pending, not overwritten as failed
        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert_ne!(row.status, STATUS_FAILED);
    }

    /// A policy decision made by a main worker must not overwrite a row that
    /// was re-enqueued after that worker claimed it.
    #[tokio::test]
    async fn test_main_archive_policy_failure_does_not_fail_reenqueued_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 62, "tok", "Title", false, SOURCE_SUBSCRIPTION)
            .await
            .unwrap();

        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        assert_eq!(claimed.status, STATUS_DOWNLOADING);

        let reenqueued = repo
            .enqueue_eh_download(-100, 62, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reenqueued.id, model.id);
        assert_eq!(reenqueued.status, STATUS_PENDING);

        let err = repo
            .fail_eh_download_for_archive_policy(&claimed, "policy reject")
            .await
            .expect_err("stale policy decision must not fail a re-enqueued row");
        assert!(err.to_string().contains("archive policy"));

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert_ne!(row.status, STATUS_FAILED);
    }

    /// A policy decision made by a background worker must not overwrite a row
    /// canceled after its running claim.
    #[tokio::test]
    async fn test_background_archive_policy_failure_does_not_fail_canceled_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_subscription_download(-100, 123, 63, "tok", "Title", false)
            .await
            .unwrap();

        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        repo.schedule_eh_background_download_from(claimed.id, STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        let background_claim = repo
            .get_next_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            background_claim.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );

        repo.cancel_eh_subscription_queue_entries(123)
            .await
            .unwrap();

        let err = repo
            .fail_eh_background_download_for_archive_policy(&background_claim, "policy reject")
            .await
            .expect_err("stale policy decision must not fail a canceled row");
        assert!(err.to_string().contains("archive policy"));

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_CANCELED);
        assert_ne!(row.status, STATUS_FAILED);
    }

    #[tokio::test]
    async fn test_archive_policy_failure_rejects_missing_claim_timestamp() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let entry = repo
            .enqueue_eh_download(-100, 66, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();

        let main_err = repo
            .fail_eh_download_for_archive_policy(&entry, "policy reject")
            .await
            .expect_err("main policy failure requires a claim timestamp");
        assert!(main_err
            .to_string()
            .contains("missing main claim started_at"));

        let background_err = repo
            .fail_eh_background_download_for_archive_policy(&entry, "policy reject")
            .await
            .expect_err("background policy failure requires a claim timestamp");
        assert!(background_err
            .to_string()
            .contains("missing background claim started_at"));
    }

    /// A main policy transition must be bound to the original worker claim, not
    /// merely to a status that a newer worker can claim again.
    #[tokio::test]
    async fn test_main_archive_policy_aba_does_not_fail_reclaimed_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let claim_now = NaiveDate::from_ymd_opt(2026, 7, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let model = repo
            .enqueue_eh_download(-100, 64, "tok", "Title", false, SOURCE_SUBSCRIPTION)
            .await
            .unwrap();

        let first_claim = repo
            .get_next_for_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_claim.status, STATUS_DOWNLOADING);
        assert!(first_claim.started_at.is_some());

        let reenqueued = repo
            .enqueue_eh_download(-100, 64, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reenqueued.status, STATUS_PENDING);
        assert_eq!(reenqueued.started_at, first_claim.started_at);
        let second_claim = repo
            .get_next_for_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second_claim.status, STATUS_DOWNLOADING);
        assert_eq!(
            second_claim.started_at,
            first_claim
                .started_at
                .map(|generation| generation + Duration::seconds(1))
        );

        let err = repo
            .fail_eh_download_for_archive_policy(&first_claim, "policy reject")
            .await
            .expect_err("stale main claim must not fail the newer claim");
        assert!(err.to_string().contains("archive policy"));

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_DOWNLOADING);
        assert_eq!(row.started_at, second_claim.started_at);
        assert_ne!(row.status, STATUS_FAILED);
    }

    /// A background policy transition must be bound to the original running
    /// claim, even after cancellation and a second background handoff/claim.
    #[tokio::test]
    async fn test_background_archive_policy_aba_does_not_fail_reclaimed_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let claim_now = Local::now().naive_local() + Duration::minutes(1);
        let model = repo
            .enqueue_eh_subscription_download(-100, 124, 65, "tok", "Title", false)
            .await
            .unwrap();

        let first_main_claim = repo
            .get_next_for_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        repo.schedule_eh_background_download_from(first_main_claim.id, STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        let first_background_claim = repo
            .get_next_for_background_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first_background_claim.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );
        assert!(first_background_claim
            .background_download_started_at
            .is_some());
        assert!(first_background_claim.started_at.is_some());

        repo.cancel_eh_subscription_queue_entries(124)
            .await
            .unwrap();
        let reenqueued = repo
            .enqueue_eh_download(-100, 65, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reenqueued.status, STATUS_PENDING);
        assert_eq!(reenqueued.started_at, first_background_claim.started_at);
        let second_main_claim = repo
            .get_next_for_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        repo.schedule_eh_background_download_from(second_main_claim.id, STATUS_DOWNLOADING, "slow")
            .await
            .unwrap();
        let second_background_claim = repo
            .get_next_for_background_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            second_background_claim
                .background_download_status
                .as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );
        assert_eq!(
            first_background_claim.background_download_started_at,
            second_background_claim.background_download_started_at
        );
        assert_eq!(
            second_background_claim.started_at,
            first_background_claim
                .started_at
                .map(|generation| generation + Duration::seconds(2))
        );

        let err = repo
            .fail_eh_background_download_for_archive_policy(
                &first_background_claim,
                "policy reject",
            )
            .await
            .expect_err("stale background claim must not fail the newer claim");
        assert!(err.to_string().contains("archive policy"));

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert_eq!(
            row.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );
        assert_eq!(
            row.background_download_started_at,
            second_background_claim.background_download_started_at
        );
        assert_eq!(row.started_at, second_background_claim.started_at);
        assert_ne!(row.status, STATUS_FAILED);
    }

    #[tokio::test]
    async fn test_main_download_claim_rejects_stale_previous_generation() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let claim_now = NaiveDate::from_ymd_opt(2026, 7, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let entry = repo
            .enqueue_eh_download(-100, 67, "tok", "Title", false, SOURCE_SUBSCRIPTION)
            .await
            .unwrap();
        let stale_snapshot = Entity::find_by_id(entry.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();

        let first_claim = repo
            .get_next_for_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        repo.enqueue_eh_download(-100, 67, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let second_claim = repo
            .get_next_for_download_at(claim_now)
            .await
            .unwrap()
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_PENDING))
            .filter(Column::Id.eq(entry.id))
            .filter(Column::Status.eq(STATUS_DOWNLOADING))
            .exec(&repo.db)
            .await
            .unwrap();

        assert_eq!(
            repo.claim_main_download_from_snapshot_at(&stale_snapshot, claim_now)
                .await
                .unwrap(),
            None,
            "the stale selector must not claim after a later generation is released"
        );
        let row = Entity::find_by_id(entry.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert_eq!(
            second_claim.started_at,
            first_claim
                .started_at
                .map(|generation| generation + Duration::seconds(1))
        );
        assert_eq!(row.started_at, second_claim.started_at);
    }

    #[tokio::test]
    async fn test_main_download_claim_refetch_detects_lost_claim() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let claim_now = NaiveDate::from_ymd_opt(2026, 7, 22)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let entry = repo
            .enqueue_eh_download(-100, 68, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        repo.db
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                "CREATE TRIGGER release_eh_main_claim AFTER UPDATE OF status ON eh_download_queue \
                 WHEN NEW.status = 'downloading' BEGIN \
                     UPDATE eh_download_queue SET status = 'pending' WHERE id = NEW.id; \
                 END;",
            ))
            .await
            .unwrap();

        assert!(
            repo.get_next_for_download_at(claim_now)
                .await
                .unwrap()
                .is_none(),
            "a claim lost before refetch must not be returned to its original worker"
        );
        let row = Entity::find_by_id(entry.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert_eq!(row.started_at, Some(claim_now));
    }

    #[tokio::test]
    async fn test_stale_upload_retry_does_not_overwrite_publishing_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 61, "tok", "Title", true, SOURCE_DIRECT)
            .await
            .unwrap();

        let download_claim = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(download_claim.status, STATUS_DOWNLOADING);
        repo.mark_eh_download_downloaded(model.id, 5000, "/tmp/61.zip", 0)
            .await
            .unwrap();

        let upload_claim = repo.get_next_for_upload().await.unwrap().unwrap();
        assert_eq!(upload_claim.status, STATUS_UPLOADING);
        repo.mark_eh_download_uploaded(model.id, "https://telegra.ph/61")
            .await
            .unwrap();

        let publish_claim = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(publish_claim.status, STATUS_PUBLISHING);

        let stale_upload_retry = repo
            .schedule_eh_retry_from(
                upload_claim.id,
                STATUS_UPLOADING,
                STATUS_DOWNLOADED,
                "stale upload failure",
                3,
            )
            .await;
        assert!(
            stale_upload_retry.is_err(),
            "stale upload retry must not move a publishing row back to downloaded"
        );

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PUBLISHING);
        assert_eq!(row.retry_count, 0);
    }

    /// When merge runs on a row that was claimed by a publish worker between
    /// select and update, the CAS guard detects the status change, re-reads,
    /// and correctly decides to reset (telegraph was upgraded while the row is
    /// in a post-download state where telegraph matters).
    #[tokio::test]
    async fn test_merge_rechecks_after_publish_claim_race() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_subscription_download(-100, 123, 65, "tok", "Title", false)
            .await
            .unwrap();

        // Download: claim -> downloading -> downloaded (telegraph=false)
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 5000, "/tmp/65.zip", 0)
            .await
            .unwrap();

        // Row is now downloaded, telegraph=false — publish worker claims it
        let pub_claimed = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(pub_claimed.id, model.id);
        assert_eq!(pub_claimed.status, STATUS_PUBLISHING);

        // Now enqueue with telegraph=true.  The merge sees the row is
        // `publishing` and telegraph was upgraded → resets to pending so the
        // new telegraph requirement is not lost.
        let merged = repo
            .enqueue_eh_subscription_download(-100, 456, 65, "newtok", "NewTitle", true)
            .await
            .unwrap();

        // Merge correctly detected the telegraph upgrade on a publishing row
        // and reset to pending so the download pipeline will re-process with
        // telegraph=true.
        assert_eq!(merged.id, model.id);
        assert_eq!(merged.status, STATUS_PENDING);
        assert!(merged.telegraph, "reset must preserve telegraph=true");
        assert_eq!(merged.subscription_ids.as_deref(), Some("123,456"));
        assert_eq!(merged.telegraph_subscription_ids.as_deref(), Some("456"));
        assert_eq!(merged.token, "newtok");
        assert_eq!(merged.title, "NewTitle");

        // Re-download and verify the pipeline now respects telegraph=true
        let re_claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(re_claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 5000, "/tmp/65b.zip", 0)
            .await
            .unwrap();

        // Upload stage should now claim it (telegraph=true, downloaded)
        let up_claimed = repo.get_next_for_upload().await.unwrap().unwrap();
        assert_eq!(up_claimed.id, model.id);
        assert_eq!(up_claimed.status, STATUS_UPLOADING);
    }

    /// When an insert conflicts on (chat_id, gid) unique constraint, the
    /// insert-error path re-selects and calls merge_eh_download.  Verify that
    /// merge correctly handles a row inserted directly into the DB (simulating
    /// a concurrent insert that won the race).
    #[tokio::test]
    async fn test_enqueue_insert_error_reselect_merge_helper() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        // Simulate a concurrent caller that inserted the row first.
        let now = chrono::Local::now().naive_local();
        let conflict = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(70i64),
            token: Set("other".to_string()),
            title: Set("Other".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            status: Set(STATUS_PENDING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        };
        conflict.insert(&repo.db).await.unwrap();

        // Now enqueue the "real" request — SELECT finds the directly-inserted
        // row and merges via merge_eh_download.
        let merged = repo
            .enqueue_eh_download(-100, 70, "tok2", "Title2", true, SOURCE_DIRECT)
            .await
            .unwrap();

        assert_eq!(merged.chat_id, -100);
        assert_eq!(merged.gid, 70);
        // Merge should have applied the new values and OR'd telegraph + source upgrade.
        assert_eq!(merged.token, "tok2");
        assert_eq!(merged.title, "Title2");
        assert!(merged.telegraph, "telegraph should be OR-merged to true");
        assert_eq!(
            merged.source, SOURCE_DIRECT,
            "source should be upgraded to direct"
        );

        // No duplicate rows
        let all: Vec<_> = Entity::find()
            .filter(Column::ChatId.eq(-100))
            .filter(Column::Gid.eq(70))
            .all(&repo.db)
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
    }

    /// When a stale subscription merge races with a direct-source upgrade,
    /// the CAS source guard prevents the stale snapshot from overwriting
    /// the direct upgrade.  The retry loop re-reads and recomputes so the
    /// final source stays `SOURCE_DIRECT`.
    #[tokio::test]
    async fn test_merge_source_guard_preserves_direct_upgrade() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        // Insert a row with SOURCE_SUBSCRIPTION, status=pending
        let model = repo
            .enqueue_eh_download(-100, 80, "tok", "Title", false, SOURCE_SUBSCRIPTION)
            .await
            .unwrap();
        assert_eq!(model.source, SOURCE_SUBSCRIPTION);
        assert_eq!(model.status, STATUS_PENDING);

        // Snapshot A: the old subscription row
        let snap_a = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap_a.source, SOURCE_SUBSCRIPTION);

        // Apply a direct upgrade via enqueue (full reset path)
        let upgraded = repo
            .enqueue_eh_download(-100, 80, "direct_tok", "Direct Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(upgraded.id, model.id);
        assert_eq!(upgraded.source, SOURCE_DIRECT);

        // Snapshot B: the upgraded row (still pending after reset)
        let snap_b = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap_b.source, SOURCE_DIRECT);

        // Apply stale subscription merge from snapshot A.
        // The CAS guard must detect that source changed from SUBSCRIPTION to
        // DIRECT, fail the update, re-read, and recompute -> source stays DIRECT.
        let merged = repo
            .merge_eh_download(
                snap_a,
                "stale_tok",
                "Stale Title",
                false,
                SOURCE_SUBSCRIPTION,
                None,
            )
            .await
            .unwrap();
        assert_eq!(merged.id, model.id);
        assert_eq!(
            merged.source, SOURCE_DIRECT,
            "stale subscription merge must not overwrite direct upgrade"
        );
        // Token may be updated by the stale merge (normal in-place update after
        // the CAS retry re-reads the row with the correct source).  The key
        // invariant is that source stays SOURCE_DIRECT.
        assert_eq!(merged.token, "stale_tok");
    }

    /// Verify that `recover_eh_enqueue_after_insert_error` reselects the
    /// existing row and merges it when called after a simulated insert error.
    /// The existing test only exercised the SELECT-found path before the
    /// insert; this test exercises the fallback branch through the private
    /// helper directly.
    #[tokio::test]
    async fn test_enqueue_insert_error_reselect_merge_fallback() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        // Pre-insert a row so the helper's re-select finds it
        let now = chrono::Local::now().naive_local();
        let conflict = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(71i64),
            token: Set("other".to_string()),
            title: Set("Other".to_string()),
            telegraph: Set(false),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            status: Set(STATUS_PENDING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        };
        conflict.insert(&repo.db).await.unwrap();

        // Call the private helper with a synthetic DbErr — the error value
        // is not inspected, only used for logging in production.
        let synthetic_err = sea_orm::DbErr::Custom("simulated insert conflict".to_string());
        let recovered = repo
            .recover_eh_enqueue_after_insert_error(
                EhEnqueueRequest {
                    chat_id: -100,
                    gid: 71,
                    token: "real_tok",
                    title: "Real Title",
                    telegraph: true,
                    source: SOURCE_DIRECT,
                    subscription_id: None,
                },
                synthetic_err,
            )
            .await
            .unwrap();

        assert_eq!(recovered.chat_id, -100);
        assert_eq!(recovered.gid, 71);
        assert_eq!(recovered.token, "real_tok");
        assert_eq!(recovered.title, "Real Title");
        assert!(recovered.telegraph, "telegraph should be OR-merged");
        assert_eq!(recovered.source, SOURCE_DIRECT, "source should be upgraded");
    }

    /// When re-select after insert conflict returns Ok(None) (row doesn't
    /// exist at all), the helper must propagate the original insert DbErr
    /// rather than a generic message.
    #[tokio::test]
    async fn test_enqueue_insert_error_reselect_none_preserves_original_error() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        // No row exists for (chat_id, gid) — re-select will return Ok(None).
        let synthetic_err = sea_orm::DbErr::Custom("unique constraint violation".to_string());
        let err = repo
            .recover_eh_enqueue_after_insert_error(
                EhEnqueueRequest {
                    chat_id: -999,
                    gid: 999,
                    token: "tok",
                    title: "Title",
                    telegraph: false,
                    source: SOURCE_DIRECT,
                    subscription_id: None,
                },
                synthetic_err,
            )
            .await
            .unwrap_err();

        let chain = format!("{:#}", err);
        assert!(
            chain.contains("unique constraint violation"),
            "original DbErr should appear in error chain: {chain}"
        );
        assert!(
            chain.contains("Failed to enqueue eh download"),
            "context message should be present: {chain}"
        );
    }

    /// After fallback, stale Telegraph URL and sent markers must be cleared
    /// so publish workers do not send stale Telegraph links.
    #[tokio::test]
    async fn test_fallback_clears_stale_telegraph_state() {
        let repo = tests_helpers::setup_test_db().await.unwrap();

        // Construct a STATUS_UPLOADING row with stale Telegraph state
        let now = chrono::Local::now().naive_local();
        let active = eh_download_queue::ActiveModel {
            chat_id: Set(-100i64),
            gid: Set(90i64),
            token: Set("tok".to_string()),
            title: Set("Title".to_string()),
            telegraph: Set(true),
            source: Set(SOURCE_DIRECT.to_string()),
            status: Set(STATUS_UPLOADING.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(2),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(Some(now)),
            zip_path: Set(Some("/tmp/90.zip".to_string())),
            telegraph_url: Set(Some("https://telegra.ph/stale".to_string())),
            archive_sent_at: Set(Some(now)),
            telegraph_sent_at: Set(Some(now)),
            next_retry_at: Set(Some(now)),
            ..Default::default()
        };
        let model = active.insert(&repo.db).await.unwrap();

        // Perform fallback
        let result = repo
            .fallback_eh_upload_to_archive(model.id, "permanent upload failure")
            .await
            .unwrap();

        // Assert: status downgraded to downloaded, telegraph cleared
        assert_eq!(result.status, STATUS_DOWNLOADED);
        assert!(!result.telegraph);

        // Assert: stale Telegraph state cleared
        assert!(
            result.telegraph_url.is_none(),
            "telegraph_url must be cleared after fallback"
        );
        assert!(
            result.archive_sent_at.is_none(),
            "archive_sent_at must be cleared after fallback"
        );
        assert!(
            result.telegraph_sent_at.is_none(),
            "telegraph_sent_at must be cleared after fallback"
        );

        // Assert: retry state reset
        assert_eq!(result.retry_count, 0);
        assert!(
            result.next_retry_at.is_none(),
            "next_retry_at must be cleared after fallback"
        );

        // Assert: error recorded
        assert_eq!(result.error.as_deref(), Some("permanent upload failure"));

        // Assert: ZIP path preserved
        assert_eq!(result.zip_path.as_deref(), Some("/tmp/90.zip"));
    }

    #[tokio::test]
    async fn test_disable_telegraph_without_token_downgrades_unuploaded_downloaded_rows() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 91, "tok", "Title", true, SOURCE_DIRECT)
            .await
            .unwrap();
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 5000, "/tmp/91.zip", 0)
            .await
            .unwrap();

        let changed = repo
            .disable_eh_telegraph_for_unuploaded_entries()
            .await
            .unwrap();
        assert_eq!(changed, 1);

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_DOWNLOADED);
        assert!(!row.telegraph);
        assert!(row.telegraph_url.is_none());

        assert!(repo.get_next_for_upload().await.unwrap().is_none());
        let publish = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(publish.id, model.id);
        assert_eq!(publish.status, STATUS_PUBLISHING);
    }

    #[tokio::test]
    async fn test_disable_telegraph_without_token_preserves_uploaded_rows_with_url() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 92, "tok", "Title", true, SOURCE_DIRECT)
            .await
            .unwrap();
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 5000, "/tmp/92.zip", 0)
            .await
            .unwrap();
        let upload = repo.get_next_for_upload().await.unwrap().unwrap();
        assert_eq!(upload.id, model.id);
        repo.mark_eh_download_uploaded(model.id, "https://telegra.ph/92")
            .await
            .unwrap();

        let changed = repo
            .disable_eh_telegraph_for_unuploaded_entries()
            .await
            .unwrap();
        assert_eq!(changed, 0);

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert!(row.telegraph);
        assert_eq!(row.status, STATUS_UPLOADED);
        assert_eq!(row.telegraph_url.as_deref(), Some("https://telegra.ph/92"));
    }

    #[tokio::test]
    async fn test_disable_telegraph_without_token_clears_terminal_stale_flag() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 93, "tok", "Title", true, SOURCE_DIRECT)
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_FAILED))
            .col_expr(Column::Error, Expr::value(Some("old failure".to_string())))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        let canceled_model = repo
            .enqueue_eh_download(-100, 94, "tok", "Title", true, SOURCE_DIRECT)
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_CANCELED))
            .filter(Column::Id.eq(canceled_model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        let done_with_url = repo
            .enqueue_eh_download(-100, 95, "tok", "Title", true, SOURCE_DIRECT)
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(STATUS_DONE))
            .col_expr(
                Column::TelegraphUrl,
                Expr::value(Some("https://telegra.ph/old".to_string())),
            )
            .filter(Column::Id.eq(done_with_url.id))
            .exec(&repo.db)
            .await
            .unwrap();

        let changed = repo
            .disable_eh_telegraph_for_unuploaded_entries()
            .await
            .unwrap();
        assert_eq!(changed, 3);

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_FAILED);
        assert!(!row.telegraph);
        let canceled = Entity::find_by_id(canceled_model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canceled.status, STATUS_CANCELED);
        assert!(!canceled.telegraph);
        let done = Entity::find_by_id(done_with_url.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, STATUS_DONE);
        assert!(!done.telegraph);
        assert_eq!(
            done.telegraph_url.as_deref(),
            Some("https://telegra.ph/old")
        );

        let reenqueued = repo
            .enqueue_eh_download(-100, 93, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reenqueued.status, STATUS_PENDING);
        assert!(!reenqueued.telegraph);
        let reenqueued_done_url = repo
            .enqueue_eh_download(-100, 95, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reenqueued_done_url.status, STATUS_PENDING);
        assert!(!reenqueued_done_url.telegraph);
    }

    #[tokio::test]
    async fn test_telegraph_rewrite_lifecycle_schedule_retry_stale_and_success() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 96, "tok", "Title", true, SOURCE_DIRECT)
            .await
            .unwrap();

        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        repo.mark_eh_download_downloaded(model.id, 5000, "/tmp/96.zip", 0)
            .await
            .unwrap();

        let upload = repo.get_next_for_upload().await.unwrap().unwrap();
        assert_eq!(upload.status, STATUS_UPLOADING);
        let uploaded = repo
            .mark_eh_download_uploaded_with_rewrite(
                model.id,
                "https://telegra.ph/96",
                Some("{\"pages\":[]}"),
            )
            .await
            .unwrap();
        assert_eq!(uploaded.status, STATUS_UPLOADED);
        assert_eq!(
            uploaded.telegraph_rewrite_data.as_deref(),
            Some("{\"pages\":[]}")
        );
        assert!(uploaded.telegraph_rewrite_status.is_none());

        let publishing = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(publishing.status, STATUS_PUBLISHING);
        repo.mark_eh_telegraph_sent_and_schedule_rewrite(model.id, Some(0))
            .await
            .unwrap();
        let scheduled = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        let first_rewrite_after = scheduled.telegraph_rewrite_after;
        repo.schedule_eh_telegraph_rewrite_after_send(model.id, 3600)
            .await
            .unwrap();
        let still_scheduled = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still_scheduled.telegraph_rewrite_after, first_rewrite_after);

        let rewrite = repo
            .get_next_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rewrite.id, model.id);
        assert_eq!(
            rewrite.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_REWRITING)
        );
        assert!(rewrite.telegraph_rewrite_started_at.is_some());

        let permanent = repo
            .schedule_eh_telegraph_rewrite_retry(model.id, "gateway not ready", 3)
            .await
            .unwrap();
        assert!(!permanent);
        let retry = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            retry.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_PENDING)
        );
        assert_eq!(retry.telegraph_rewrite_retry_count, 1);
        assert!(retry.telegraph_rewrite_next_retry_at.is_some());

        Entity::update_many()
            .col_expr(
                Column::TelegraphRewriteStatus,
                Expr::value(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
            )
            .col_expr(
                Column::TelegraphRewriteStartedAt,
                Expr::value(Some(
                    Local::now().naive_local() - chrono::Duration::seconds(7200),
                )),
            )
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        let reset = repo.reset_stale_eh_telegraph_rewrites(3600).await.unwrap();
        assert_eq!(reset, 1);

        Entity::update_many()
            .col_expr(
                Column::TelegraphRewriteNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();
        let rewrite = repo
            .get_next_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rewrite.id, model.id);
        repo.mark_eh_telegraph_rewritten(model.id).await.unwrap();

        let done = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert!(done.telegraph_rewrite_data.is_none());
        assert!(done.telegraph_rewrite_status.is_none());
        assert_eq!(done.telegraph_rewrite_retry_count, 0);
        assert!(done.telegraph_rewritten_at.is_some());
    }

    #[tokio::test]
    async fn test_telegraph_rewrite_retry_exhaustion_marks_failed() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(-100, 97, "tok", "Title", true, SOURCE_DIRECT)
            .await
            .unwrap();

        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        repo.mark_eh_download_downloaded(claimed.id, 5000, "/tmp/97.zip", 0)
            .await
            .unwrap();
        repo.get_next_for_upload().await.unwrap().unwrap();
        repo.mark_eh_download_uploaded_with_rewrite(
            model.id,
            "https://telegra.ph/97",
            Some("{\"pages\":[]}"),
        )
        .await
        .unwrap();
        repo.get_next_for_publish().await.unwrap().unwrap();
        repo.mark_eh_telegraph_sent_and_schedule_rewrite(model.id, Some(0))
            .await
            .unwrap();
        repo.get_next_for_telegraph_rewrite()
            .await
            .unwrap()
            .unwrap();

        let permanent = repo
            .schedule_eh_telegraph_rewrite_retry(model.id, "edit denied", 0)
            .await
            .unwrap();
        assert!(permanent);

        let failed = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            failed.telegraph_rewrite_status.as_deref(),
            Some(TELEGRAPH_REWRITE_STATUS_FAILED)
        );
        assert_eq!(failed.telegraph_rewrite_retry_count, 1);
        assert!(failed.telegraph_rewrite_next_retry_at.is_none());
        assert!(repo
            .get_next_for_telegraph_rewrite()
            .await
            .unwrap()
            .is_none());
    }
}
