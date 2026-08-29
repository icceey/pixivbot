use crate::db::entities::{eh_gallery_jobs, eh_gallery_results};
use crate::db::repo::eh_gallery_jobs::EhGalleryVariant;
use anyhow::{Context, Result};
use chrono::Local;
use sea_orm::{
    sea_query::Expr, ColumnTrait, ConnectionTrait, DatabaseTransaction, EntityTrait, QueryFilter,
    Statement,
};

#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub async fn upsert_eh_gallery_result_in_txn(
    txn: &DatabaseTransaction,
    gid: i64,
    token: &str,
    variant: &EhGalleryVariant,
    fingerprint: &str,
    source_generation: i64,
    telegraph_url: &str,
    rewrite_data: Option<&str>,
    media_cids: Option<&str>,
) -> Result<()> {
    let now = Local::now().naive_local();
    let statement = Statement::from_sql_and_values(
        txn.get_database_backend(),
        r#"
        INSERT INTO eh_gallery_results (
            gid, token, download_mode, resolution,
            source_fingerprint, source_generation, telegraph_url,
            telegraph_rewrite_data, media_cids,
            created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(gid, token, download_mode, resolution) DO UPDATE SET
            source_fingerprint = excluded.source_fingerprint,
            source_generation = excluded.source_generation,
            telegraph_url = excluded.telegraph_url,
            telegraph_rewrite_data = excluded.telegraph_rewrite_data,
            media_cids = excluded.media_cids,
            updated_at = excluded.updated_at
        WHERE excluded.source_generation >= eh_gallery_results.source_generation
        "#,
        vec![
            gid.into(),
            token.to_string().into(),
            variant.download_mode.clone().into(),
            variant.resolution.clone().into(),
            fingerprint.to_string().into(),
            source_generation.into(),
            telegraph_url.to_string().into(),
            rewrite_data.map(str::to_string).into(),
            media_cids.map(str::to_string).into(),
            now.into(),
            now.into(),
        ],
    );
    txn.execute(statement)
        .await
        .context("Failed to upsert cached EH gallery result")?;
    Ok(())
}

pub async fn find_eh_gallery_result_in_txn(
    txn: &DatabaseTransaction,
    gid: i64,
    token: &str,
    variant: &EhGalleryVariant,
) -> Result<Option<eh_gallery_results::Model>> {
    eh_gallery_results::Entity::find()
        .filter(eh_gallery_results::Column::Gid.eq(gid))
        .filter(eh_gallery_results::Column::Token.eq(token))
        .filter(eh_gallery_results::Column::DownloadMode.eq(&variant.download_mode))
        .filter(eh_gallery_results::Column::Resolution.eq(&variant.resolution))
        .one(txn)
        .await
        .context("Failed to find cached EH gallery result")
}

pub async fn clear_rewritten_eh_gallery_result_in_txn(
    txn: &DatabaseTransaction,
    job: &eh_gallery_jobs::Model,
) -> Result<()> {
    let (Some(fingerprint), Some(telegraph_url), Some(rewrite_data)) = (
        job.source_fingerprint.as_deref(),
        job.telegraph_url.as_deref(),
        job.telegraph_rewrite_data.as_deref(),
    ) else {
        return Ok(());
    };
    eh_gallery_results::Entity::update_many()
        .col_expr(
            eh_gallery_results::Column::TelegraphRewriteData,
            Expr::value(None::<String>),
        )
        .filter(eh_gallery_results::Column::Gid.eq(job.gid))
        .filter(eh_gallery_results::Column::Token.eq(&job.token))
        .filter(eh_gallery_results::Column::DownloadMode.eq(&job.download_mode))
        .filter(eh_gallery_results::Column::Resolution.eq(&job.resolution))
        .filter(eh_gallery_results::Column::SourceFingerprint.eq(fingerprint))
        .filter(eh_gallery_results::Column::SourceGeneration.eq(job.source_generation))
        .filter(eh_gallery_results::Column::TelegraphUrl.eq(telegraph_url))
        .filter(eh_gallery_results::Column::TelegraphRewriteData.eq(rewrite_data))
        .exec(txn)
        .await
        .context("Failed to clear completed cached EH Telegraph rewrite")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{find_eh_gallery_result_in_txn, upsert_eh_gallery_result_in_txn};
    use crate::db::entities::{eh_download_queue, eh_gallery_jobs, eh_gallery_results};
    use crate::db::repo::eh_gallery_jobs::{
        try_apply_cached_eh_result_in_txn, EhGalleryVariant, BACKGROUND_STATUS_RUNNING,
        CLEANUP_STATUS_NONE, DELIVERY_STATUS_WAITING, JOB_STATUS_DOWNLOADED,
        JOB_STATUS_DOWNLOADING, JOB_STATUS_PENDING, TELEGRAPH_REWRITE_STATUS_PENDING,
        TELEGRAPH_STATUS_NOT_REQUIRED, TELEGRAPH_STATUS_READY,
    };
    use crate::db::repo::tests_helpers::setup_test_db;
    use crate::db::repo::Repo;
    use chrono::{Duration, Local};
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};

    async fn create_job(
        repo: &Repo,
        gid: i64,
        token: &str,
        variant: &EhGalleryVariant,
        fingerprint: Option<&str>,
    ) -> eh_gallery_jobs::Model {
        eh_gallery_jobs::ActiveModel {
            gid: Set(gid),
            token: Set(token.to_string()),
            download_mode: Set(variant.download_mode.clone()),
            resolution: Set(variant.resolution.clone()),
            source_fingerprint: Set(fingerprint.map(str::to_string)),
            source_generation: Set(1),
            title: Set(format!("Gallery {gid}")),
            status: Set(JOB_STATUS_PENDING.to_string()),
            telegraph_status: Set(TELEGRAPH_STATUS_NOT_REQUIRED.to_string()),
            telegraph_required: Set(false),
            cleanup_status: Set(CLEANUP_STATUS_NONE.to_string()),
            created_at: Set(Local::now().naive_local()),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap()
    }

    async fn create_delivery(
        repo: &Repo,
        job: &eh_gallery_jobs::Model,
        telegraph: bool,
        archive_sent_at: Option<chrono::NaiveDateTime>,
    ) -> eh_download_queue::Model {
        eh_download_queue::ActiveModel {
            job_id: Set(Some(job.id)),
            chat_id: Set(-job.id as i64),
            gid: Set(job.gid),
            token: Set(job.token.clone()),
            title: Set(job.title.clone()),
            telegraph: Set(telegraph),
            source: Set("direct".to_string()),
            status: Set(DELIVERY_STATUS_WAITING.to_string()),
            created_at: Set(Local::now().naive_local()),
            archive_sent_at: Set(archive_sent_at),
            ..Default::default()
        }
        .insert(repo.db())
        .await
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_cached_result(
        repo: &Repo,
        gid: i64,
        token: &str,
        variant: &EhGalleryVariant,
        fingerprint: &str,
        source_generation: i64,
        telegraph_url: &str,
        rewrite_data: Option<&str>,
    ) {
        let txn = repo.db().begin().await.unwrap();
        upsert_eh_gallery_result_in_txn(
            &txn,
            gid,
            token,
            variant,
            fingerprint,
            source_generation,
            telegraph_url,
            rewrite_data,
            None,
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();
    }

    async fn apply_cached_result(repo: &Repo, job_id: i32, send_archive: bool) -> bool {
        let txn = repo.db().begin().await.unwrap();
        let applied = try_apply_cached_eh_result_in_txn(&txn, job_id, send_archive)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        applied
    }

    async fn load_job(repo: &Repo, job_id: i32) -> eh_gallery_jobs::Model {
        eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn upsert_preserves_newer_generation_and_updates_equal_generation() {
        let repo = setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let txn = repo.db().begin().await.unwrap();

        upsert_eh_gallery_result_in_txn(
            &txn,
            101,
            "token",
            &variant,
            "fingerprint-v1",
            1,
            "https://telegra.ph/v1",
            Some("rewrite-v1"),
            Some("cid-v1"),
        )
        .await
        .unwrap();
        let first = find_eh_gallery_result_in_txn(&txn, 101, "token", &variant)
            .await
            .unwrap()
            .unwrap();
        let mut older: eh_gallery_results::ActiveModel = first.clone().into();
        older.updated_at = Set(first.updated_at - Duration::seconds(1));
        let first = older.update(&txn).await.unwrap();

        upsert_eh_gallery_result_in_txn(
            &txn,
            101,
            "token",
            &variant,
            "fingerprint-v2",
            2,
            "https://telegra.ph/v2",
            Some("rewrite-v2"),
            Some("cid-v2"),
        )
        .await
        .unwrap();
        let result = find_eh_gallery_result_in_txn(&txn, 101, "token", &variant)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result.id, first.id);
        assert_eq!(result.created_at, first.created_at);
        assert!(result.updated_at > first.updated_at);
        assert_eq!(result.source_fingerprint, "fingerprint-v2");
        assert_eq!(result.telegraph_url, "https://telegra.ph/v2");
        assert_eq!(result.telegraph_rewrite_data.as_deref(), Some("rewrite-v2"));
        assert_eq!(result.media_cids.as_deref(), Some("cid-v2"));

        upsert_eh_gallery_result_in_txn(
            &txn,
            101,
            "token",
            &variant,
            "fingerprint-v1-late",
            1,
            "https://telegra.ph/v1-late",
            Some("rewrite-v1-late"),
            Some("cid-v1-late"),
        )
        .await
        .unwrap();
        let result = find_eh_gallery_result_in_txn(&txn, 101, "token", &variant)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.source_fingerprint, "fingerprint-v2");
        assert_eq!(result.telegraph_url, "https://telegra.ph/v2");
        assert_eq!(result.telegraph_rewrite_data.as_deref(), Some("rewrite-v2"));
        assert_eq!(result.media_cids.as_deref(), Some("cid-v2"));

        upsert_eh_gallery_result_in_txn(
            &txn,
            101,
            "token",
            &variant,
            "fingerprint-v2-retry",
            2,
            "https://telegra.ph/v2-retry",
            Some("rewrite-v2-retry"),
            Some("cid-v2-retry"),
        )
        .await
        .unwrap();
        let result = find_eh_gallery_result_in_txn(&txn, 101, "token", &variant)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.source_fingerprint, "fingerprint-v2-retry");
        assert_eq!(result.telegraph_url, "https://telegra.ph/v2-retry");
        assert_eq!(
            result.telegraph_rewrite_data.as_deref(),
            Some("rewrite-v2-retry")
        );
        assert_eq!(result.media_cids.as_deref(), Some("cid-v2-retry"));
        assert_eq!(
            eh_gallery_results::Entity::find()
                .filter(eh_gallery_results::Column::Gid.eq(101))
                .filter(eh_gallery_results::Column::Token.eq("token"))
                .all(&txn)
                .await
                .unwrap()
                .len(),
            1
        );

        txn.commit().await.unwrap();
    }

    #[tokio::test]
    async fn apply_cached_result_makes_telegraph_only_job_zipless_ready() {
        let repo = setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let job = create_job(&repo, 102, "token", &variant, Some("fingerprint")).await;
        create_delivery(&repo, &job, true, None).await;
        insert_cached_result(
            &repo,
            job.gid,
            &job.token,
            &variant,
            "fingerprint",
            job.source_generation,
            "https://telegra.ph/cached",
            Some("cached-rewrite"),
        )
        .await;

        let mut stale: eh_gallery_jobs::ActiveModel = job.clone().into();
        let then = Local::now().naive_local() - Duration::minutes(1);
        stale.telegraph_rewrite_data = Set(Some("stale-rewrite".to_string()));
        stale.telegraph_rewrite_status = Set(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string()));
        stale.telegraph_rewrite_after = Set(Some(then));
        stale.telegraph_rewrite_started_at = Set(Some(then));
        stale.telegraph_rewrite_next_retry_at = Set(Some(then));
        stale.telegraph_rewrite_retry_count = Set(3);
        stale.telegraph_rewrite_error = Set(Some("stale error".to_string()));
        stale.telegraph_rewritten_at = Set(Some(then));
        stale.update(repo.db()).await.unwrap();

        assert!(apply_cached_result(&repo, job.id, false).await);

        let updated = load_job(&repo, job.id).await;
        assert_eq!(updated.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(updated.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            updated.telegraph_url.as_deref(),
            Some("https://telegra.ph/cached")
        );
        assert_eq!(
            updated.telegraph_rewrite_data.as_deref(),
            Some("cached-rewrite")
        );
        assert!(updated.zip_path.is_none());
        assert_eq!(updated.file_size, 0);
        assert_eq!(updated.gp_cost, 0);
        assert!(updated.completed_at.is_some());
        assert!(updated.telegraph_rewrite_status.is_none());
        assert!(updated.telegraph_rewrite_after.is_none());
        assert!(updated.telegraph_rewrite_started_at.is_none());
        assert!(updated.telegraph_rewrite_next_retry_at.is_none());
        assert_eq!(updated.telegraph_rewrite_retry_count, 0);
        assert!(updated.telegraph_rewrite_error.is_none());
        assert!(updated.telegraph_rewritten_at.is_none());
    }

    #[tokio::test]
    async fn apply_with_archive_demand_keeps_pending() {
        let repo = setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let job = create_job(&repo, 103, "token", &variant, Some("fingerprint")).await;
        create_delivery(&repo, &job, true, None).await;
        insert_cached_result(
            &repo,
            job.gid,
            &job.token,
            &variant,
            "fingerprint",
            job.source_generation,
            "https://telegra.ph/cached",
            None,
        )
        .await;

        assert!(apply_cached_result(&repo, job.id, true).await);

        let updated = load_job(&repo, job.id).await;
        assert_eq!(updated.status, JOB_STATUS_PENDING);
        assert_eq!(updated.telegraph_status, TELEGRAPH_STATUS_READY);
        assert!(updated.zip_path.is_none());
        assert_eq!(updated.file_size, 0);
        assert_eq!(updated.gp_cost, 0);
        assert!(updated.completed_at.is_none());
    }

    #[tokio::test]
    async fn apply_rejects_fingerprint_mismatch_or_missing_record_or_null_job_fingerprint() {
        let repo = setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");

        let missing = create_job(&repo, 104, "missing", &variant, Some("fingerprint")).await;
        assert!(!apply_cached_result(&repo, missing.id, false).await);
        assert_eq!(load_job(&repo, missing.id).await, missing);

        let mismatch = create_job(&repo, 105, "mismatch", &variant, Some("fingerprint")).await;
        insert_cached_result(
            &repo,
            mismatch.gid,
            &mismatch.token,
            &variant,
            "other-fingerprint",
            mismatch.source_generation,
            "https://telegra.ph/mismatch",
            None,
        )
        .await;
        assert!(!apply_cached_result(&repo, mismatch.id, false).await);
        assert_eq!(load_job(&repo, mismatch.id).await, mismatch);

        let null_fingerprint = create_job(&repo, 106, "null", &variant, None).await;
        insert_cached_result(
            &repo,
            null_fingerprint.gid,
            &null_fingerprint.token,
            &variant,
            "fingerprint",
            null_fingerprint.source_generation,
            "https://telegra.ph/null",
            None,
        )
        .await;
        assert!(!apply_cached_result(&repo, null_fingerprint.id, false).await);
        assert_eq!(load_job(&repo, null_fingerprint.id).await, null_fingerprint);
    }

    #[tokio::test]
    async fn apply_variant_isolation() {
        let repo = setup_test_db().await.unwrap();
        let job_variant = EhGalleryVariant::archive("1280x");
        let other_variant = EhGalleryVariant::images();
        let job = create_job(&repo, 107, "token", &job_variant, Some("fingerprint")).await;
        insert_cached_result(
            &repo,
            job.gid,
            &job.token,
            &other_variant,
            "fingerprint",
            job.source_generation,
            "https://telegra.ph/images",
            None,
        )
        .await;

        assert!(!apply_cached_result(&repo, job.id, false).await);
        let updated = load_job(&repo, job.id).await;
        assert_eq!(updated.status, JOB_STATUS_PENDING);
        assert_eq!(updated.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
    }

    #[tokio::test]
    async fn apply_preserves_normal_and_background_claim_state() {
        let repo = setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let then = Local::now().naive_local() - Duration::minutes(1);

        let normal = create_job(&repo, 108, "normal", &variant, Some("fingerprint")).await;
        let mut normal_active: eh_gallery_jobs::ActiveModel = normal.into();
        normal_active.status = Set(JOB_STATUS_DOWNLOADING.to_string());
        normal_active.started_at = Set(Some(then));
        normal_active.zip_path = Set(Some("normal.zip".to_string()));
        normal_active.file_size = Set(123);
        normal_active.gp_cost = Set(45);
        let normal = normal_active.update(repo.db()).await.unwrap();
        insert_cached_result(
            &repo,
            normal.gid,
            &normal.token,
            &variant,
            "fingerprint",
            normal.source_generation,
            "https://telegra.ph/normal",
            Some("normal-rewrite"),
        )
        .await;

        assert!(apply_cached_result(&repo, normal.id, false).await);
        let updated_normal = load_job(&repo, normal.id).await;
        assert_eq!(updated_normal.status, JOB_STATUS_DOWNLOADING);
        assert_eq!(updated_normal.started_at, normal.started_at);
        assert_eq!(updated_normal.zip_path, normal.zip_path);
        assert_eq!(updated_normal.file_size, normal.file_size);
        assert_eq!(updated_normal.gp_cost, normal.gp_cost);
        assert_eq!(updated_normal.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            updated_normal.telegraph_rewrite_data.as_deref(),
            Some("normal-rewrite")
        );

        let background = create_job(&repo, 109, "background", &variant, Some("fingerprint")).await;
        let mut background_active: eh_gallery_jobs::ActiveModel = background.into();
        background_active.started_at = Set(Some(then));
        background_active.background_download_status =
            Set(Some(BACKGROUND_STATUS_RUNNING.to_string()));
        background_active.background_download_started_at = Set(Some(then));
        background_active.background_download_next_retry_at = Set(Some(then));
        background_active.zip_path = Set(Some("background.zip".to_string()));
        background_active.file_size = Set(456);
        background_active.gp_cost = Set(78);
        let background = background_active.update(repo.db()).await.unwrap();
        insert_cached_result(
            &repo,
            background.gid,
            &background.token,
            &variant,
            "fingerprint",
            background.source_generation,
            "https://telegra.ph/background",
            Some("background-rewrite"),
        )
        .await;

        assert!(apply_cached_result(&repo, background.id, false).await);
        let updated_background = load_job(&repo, background.id).await;
        assert_eq!(updated_background.status, JOB_STATUS_PENDING);
        assert_eq!(updated_background.started_at, background.started_at);
        assert_eq!(
            updated_background.background_download_status,
            background.background_download_status
        );
        assert_eq!(
            updated_background.background_download_started_at,
            background.background_download_started_at
        );
        assert_eq!(
            updated_background.background_download_next_retry_at,
            background.background_download_next_retry_at
        );
        assert_eq!(updated_background.zip_path, background.zip_path);
        assert_eq!(updated_background.file_size, background.file_size);
        assert_eq!(updated_background.gp_cost, background.gp_cost);
        assert_eq!(updated_background.telegraph_status, TELEGRAPH_STATUS_READY);
        assert_eq!(
            updated_background.telegraph_rewrite_data.as_deref(),
            Some("background-rewrite")
        );
    }
}
