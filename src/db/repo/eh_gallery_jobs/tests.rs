use super::*;
use crate::db::entities::{eh_download_completions, eh_gallery_push_ledger};
use crate::db::repo::eh_gallery_jobs::EhDownloadQueue::{Background, Main};
use crate::db::repo::eh_gallery_results::upsert_eh_gallery_result_in_txn;
use crate::db::repo::tests_helpers;
use sea_orm::{
    sea_query::Expr, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, TransactionTrait,
};

const INITIAL_SOURCE_GENERATION: i64 = 1;

#[allow(clippy::too_many_arguments)]
async fn seed_cached_result(
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
        Some("[{\"name\":\"001.jpg\",\"cid\":\"bafk-cached\"}]"),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
}

async fn load_eh_job(repo: &Repo, job_id: i32) -> eh_gallery_jobs::Model {
    eh_gallery_jobs::Entity::find_by_id(job_id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap()
}

async fn seed_push_ledger(
    repo: &Repo,
    chat_id: i64,
    gid: i64,
    archive_sent_at: Option<DateTime>,
    telegraph_sent_at: Option<DateTime>,
) -> eh_gallery_push_ledger::Model {
    eh_gallery_push_ledger::ActiveModel {
        chat_id: Set(chat_id),
        gid: Set(gid),
        archive_sent_at: Set(archive_sent_at),
        telegraph_sent_at: Set(telegraph_sent_at),
        updated_at: Set(Local::now().naive_local()),
        ..Default::default()
    }
    .insert(repo.db())
    .await
    .unwrap()
}

#[tokio::test]
async fn subscription_enqueue_skips_fully_delivered_gallery() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let archive_sent_at = Local::now().naive_local();
    let telegraph_sent_at = archive_sent_at + chrono::Duration::seconds(1);
    seed_push_ledger(
        &repo,
        -100,
        8801,
        Some(archive_sent_at),
        Some(telegraph_sent_at),
    )
    .await;

    let delivery = repo
        .enqueue_eh_subscription_download(
            -100,
            101,
            8801,
            "token",
            "Delivered Gallery",
            true,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap();

    assert!(delivery.is_none());
    assert_eq!(
        eh_download_queue::Entity::find()
            .count(repo.db())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        eh_gallery_jobs::Entity::find()
            .count(repo.db())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn subscription_enqueue_publishes_only_the_missing_surface() {
    for archive_already_sent in [true, false] {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let sent_at = Local::now().naive_local();
        let archive_sent_at = archive_already_sent.then_some(sent_at);
        let telegraph_sent_at = (!archive_already_sent).then_some(sent_at);
        seed_push_ledger(&repo, -100, 8802, archive_sent_at, telegraph_sent_at).await;
        let delivery = repo
            .enqueue_eh_subscription_download(
                -100,
                101,
                8802,
                "token",
                "Partly delivered Gallery",
                true,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("the missing surface must create a delivery wave");
        assert_eq!(delivery.archive_sent_at, archive_sent_at);
        assert_eq!(delivery.telegraph_sent_at, telegraph_sent_at);
        let job_id = delivery.job_id.unwrap();
        let mut ready = eh_gallery_jobs::ActiveModel {
            status: Set(JOB_STATUS_DOWNLOADED.to_string()),
            ..Default::default()
        };
        if archive_already_sent {
            ready.telegraph_status = Set(TELEGRAPH_STATUS_READY.to_string());
            ready.telegraph_url = Set(Some("https://example.com/gallery".to_string()));
        } else {
            assert!(!load_eh_job(&repo, job_id).await.telegraph_required);
            assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
            ready.zip_path = Set(Some("shared-8802.zip".to_string()));
        }
        eh_gallery_jobs::Entity::update_many()
            .set(ready)
            .filter(eh_gallery_jobs::Column::Id.eq(job_id))
            .exec(repo.db())
            .await
            .unwrap();
        let claimed = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .expect("only the missing surface should be claimable");
        assert_eq!(claimed.delivery.id, delivery.id);
        assert_eq!(claimed.delivery.archive_sent_at, archive_sent_at);
        assert_eq!(claimed.delivery.telegraph_sent_at, telegraph_sent_at);
        if archive_already_sent {
            repo.mark_eh_telegraph_delivery_sent(&claimed.delivery, None)
                .await
                .unwrap();
        } else {
            repo.mark_eh_archive_delivery_sent(&claimed.delivery)
                .await
                .unwrap();
        }
        let ledger = eh_gallery_push_ledger::Entity::find()
            .filter(eh_gallery_push_ledger::Column::ChatId.eq(-100))
            .filter(eh_gallery_push_ledger::Column::Gid.eq(8802))
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(ledger.archive_sent_at.is_some());
        assert!(ledger.telegraph_sent_at.is_some());
        if archive_already_sent {
            assert_eq!(ledger.archive_sent_at, archive_sent_at);
        } else {
            assert_eq!(ledger.telegraph_sent_at, telegraph_sent_at);
        }
    }
}

#[tokio::test]
async fn direct_enqueue_bypasses_ledger_dedup() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let sent_at = Local::now().naive_local();
    seed_push_ledger(&repo, -100, 8804, Some(sent_at), Some(sent_at)).await;

    let delivery = repo
        .enqueue_eh_download(
            -100,
            8804,
            "token",
            "Direct Gallery",
            true,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("direct enqueue must not be suppressed by the subscription ledger");
    assert!(delivery.archive_sent_at.is_none());
    assert!(delivery.telegraph_sent_at.is_none());
    assert_eq!(delivery.source, SOURCE_DIRECT);
}

async fn seed_rewrite_ready_job(
    repo: &Repo,
    gid: i64,
) -> (eh_gallery_jobs::Model, eh_download_queue::Model) {
    let delivery = repo
        .enqueue_eh_download(
            -100,
            gid,
            "token",
            "Gallery",
            true,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let job_id = delivery.job_id.unwrap();
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            status: Set(JOB_STATUS_DOWNLOADED.to_string()),
            telegraph_status: Set(TELEGRAPH_STATUS_READY.to_string()),
            telegraph_url: Set(Some("https://telegra.ph/Gallery".to_string())),
            telegraph_rewrite_data: Set(Some("{\"pages\":[]}".to_string())),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(job_id))
        .exec(repo.db())
        .await
        .unwrap();
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(DELIVERY_STATUS_PUBLISHING),
        )
        .filter(eh_download_queue::Column::Id.eq(delivery.id))
        .exec(repo.db())
        .await
        .unwrap();
    let job = load_eh_job(repo, job_id).await;
    let delivery = eh_download_queue::Entity::find_by_id(delivery.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    (job, delivery)
}

fn assert_telegraph_rewrite_state_preserved(
    expected: &eh_gallery_jobs::Model,
    actual: &eh_gallery_jobs::Model,
) {
    assert_eq!(actual.telegraph_status, expected.telegraph_status);
    assert_eq!(actual.telegraph_url, expected.telegraph_url);
    assert_eq!(
        actual.telegraph_rewrite_data,
        expected.telegraph_rewrite_data
    );
    assert_eq!(
        actual.telegraph_rewrite_status,
        expected.telegraph_rewrite_status
    );
    assert_eq!(
        actual.telegraph_rewrite_after,
        expected.telegraph_rewrite_after
    );
    assert_eq!(
        actual.telegraph_rewrite_started_at,
        expected.telegraph_rewrite_started_at
    );
    assert_eq!(
        actual.telegraph_rewrite_next_retry_at,
        expected.telegraph_rewrite_next_retry_at
    );
    assert_eq!(
        actual.telegraph_rewrite_retry_count,
        expected.telegraph_rewrite_retry_count
    );
    assert_eq!(
        actual.telegraph_rewrite_error,
        expected.telegraph_rewrite_error
    );
    assert_eq!(
        actual.telegraph_rewritten_at,
        expected.telegraph_rewritten_at
    );
}

#[tokio::test]
async fn late_older_ready_does_not_overwrite_newer_generation_result() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let older = repo
        .enqueue_eh_download(
            -38104,
            28104,
            "token",
            "Generation A",
            true,
            SOURCE_DIRECT,
            &variant,
            Some("fingerprint-a"),
            true,
        )
        .await
        .unwrap()
        .expect("older generation should be enqueued");
    let newer = repo
        .enqueue_eh_download(
            -48104,
            28104,
            "token",
            "Generation B",
            true,
            SOURCE_DIRECT,
            &variant,
            Some("fingerprint-b"),
            true,
        )
        .await
        .unwrap()
        .expect("newer generation should be enqueued");
    let older_job = load_eh_job(&repo, older.job_id.unwrap()).await;
    let newer_job = load_eh_job(&repo, newer.job_id.unwrap()).await;
    assert_ne!(older_job.id, newer_job.id);
    assert_eq!(
        older_job.source_fingerprint.as_deref(),
        Some("fingerprint-a")
    );
    assert_eq!(
        newer_job.source_fingerprint.as_deref(),
        Some("fingerprint-b")
    );
    assert_eq!(older_job.source_generation, 1);
    assert_eq!(newer_job.source_generation, 2);

    let first_download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .expect("one generation should be claimable");
    let second_download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .expect("the other generation should also be claimable");
    let (older_download, newer_download) = if first_download.id == older_job.id {
        (first_download, second_download)
    } else {
        (second_download, first_download)
    };
    assert_eq!(older_download.id, older_job.id);
    assert_eq!(newer_download.id, newer_job.id);

    repo.mark_eh_job_downloaded(
        Main,
        newer_download.id,
        newer_download.started_at.unwrap(),
        123,
        "generation-b.zip",
        0,
    )
    .await
    .unwrap();
    let newer_upload = repo
        .get_next_eh_job_for_upload()
        .await
        .unwrap()
        .expect("newer generation should need an upload");
    assert_eq!(newer_upload.id, newer_job.id);
    repo.mark_eh_job_telegraph_ready(
        newer_upload.id,
        newer_upload.started_at.unwrap(),
        "https://telegra.ph/generation-b",
        None,
        Some("[{\"name\":\"001.jpg\",\"cid\":\"bafk-b\"}]"),
        true,
    )
    .await
    .unwrap();

    repo.mark_eh_job_downloaded(
        Main,
        older_download.id,
        older_download.started_at.unwrap(),
        123,
        "generation-a.zip",
        0,
    )
    .await
    .unwrap();
    let older_upload = repo
        .get_next_eh_job_for_upload()
        .await
        .unwrap()
        .expect("older generation should still settle successfully");
    assert_eq!(older_upload.id, older_job.id);
    repo.mark_eh_job_telegraph_ready(
        older_upload.id,
        older_upload.started_at.unwrap(),
        "https://telegra.ph/generation-a",
        None,
        Some("[{\"name\":\"001.jpg\",\"cid\":\"bafk-a\"}]"),
        true,
    )
    .await
    .unwrap();

    let txn = repo.db().begin().await.unwrap();
    let cached = find_eh_gallery_result_in_txn(&txn, 28104, "token", &variant)
        .await
        .unwrap()
        .expect("newer result should remain cached");
    txn.commit().await.unwrap();
    assert_eq!(cached.source_generation, newer_job.source_generation);
    assert_eq!(cached.source_fingerprint, "fingerprint-b");
    assert_eq!(cached.telegraph_url, "https://telegra.ph/generation-b");
    assert_eq!(
        cached.media_cids.as_deref(),
        Some("[{\"name\":\"001.jpg\",\"cid\":\"bafk-b\"}]")
    );
}

#[tokio::test]
async fn new_fingerprint_preserves_ready_job_and_existing_result() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let first = repo
        .enqueue_eh_download(
            -18105,
            18105,
            "token",
            "Generation A",
            true,
            SOURCE_DIRECT,
            &variant,
            Some("fingerprint-a"),
            true,
        )
        .await
        .unwrap()
        .expect("first generation should be enqueued");
    let download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        download.id,
        download.started_at.unwrap(),
        123,
        "generation-a.zip",
        0,
    )
    .await
    .unwrap();
    let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    repo.mark_eh_job_telegraph_ready(
        upload.id,
        upload.started_at.unwrap(),
        "https://telegra.ph/generation-a",
        None,
        Some("[{\"name\":\"001.jpg\",\"cid\":\"bafk-a\"}]"),
        true,
    )
    .await
    .unwrap();
    let ready_a = load_eh_job(&repo, first.job_id.unwrap()).await;

    let second = repo
        .enqueue_eh_download(
            -28105,
            18105,
            "token",
            "Generation B",
            true,
            SOURCE_DIRECT,
            &variant,
            Some("fingerprint-b"),
            true,
        )
        .await
        .unwrap()
        .expect("second generation should be enqueued");
    let pending_b = load_eh_job(&repo, second.job_id.unwrap()).await;
    assert_ne!(pending_b.id, ready_a.id);
    assert_eq!(
        pending_b.source_fingerprint.as_deref(),
        Some("fingerprint-b")
    );
    assert_eq!(pending_b.status, JOB_STATUS_PENDING);
    assert!(pending_b.zip_path.is_none());
    assert_eq!(load_eh_job(&repo, ready_a.id).await, ready_a);

    let txn = repo.db().begin().await.unwrap();
    let result = find_eh_gallery_result_in_txn(&txn, 18105, "token", &variant)
        .await
        .unwrap()
        .expect("first upload result should remain cached");
    txn.commit().await.unwrap();
    assert_eq!(result.source_fingerprint, "fingerprint-a");
    assert_eq!(result.telegraph_url, "https://telegra.ph/generation-a");
}

#[tokio::test]
async fn late_telegraph_demand_applies_cached_result() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    seed_cached_result(
        &repo,
        8104,
        "token",
        &variant,
        "fingerprint",
        INITIAL_SOURCE_GENERATION,
        "https://telegra.ph/late",
        None,
    )
    .await;
    let archive = repo
        .enqueue_eh_download(
            -8104,
            8104,
            "token",
            "Gallery",
            false,
            SOURCE_DIRECT,
            &variant,
            Some("fingerprint"),
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let pending = load_eh_job(&repo, archive.job_id.unwrap()).await;
    assert_eq!(pending.status, JOB_STATUS_PENDING);
    assert_eq!(pending.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
    assert!(pending.telegraph_url.is_none());
    let download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(download.id, pending.id);
    repo.mark_eh_job_downloaded(
        Main,
        download.id,
        download.started_at.unwrap(),
        123,
        "late.zip",
        0,
    )
    .await
    .unwrap();

    let late = repo
        .enqueue_eh_download(
            -18104,
            8104,
            "token",
            "Gallery",
            true,
            SOURCE_DIRECT,
            &variant,
            Some("fingerprint"),
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(late.job_id, archive.job_id);
    let ready = load_eh_job(&repo, archive.job_id.unwrap()).await;
    assert_eq!(ready.status, JOB_STATUS_DOWNLOADED);
    assert!(ready.telegraph_required);
    assert_eq!(ready.telegraph_status, TELEGRAPH_STATUS_READY);
    assert_eq!(
        ready.telegraph_url.as_deref(),
        Some("https://telegra.ph/late")
    );
    assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
}

#[tokio::test]
async fn claimed_cleanup_reactivates_archive_work_without_clearing_ready_telegraph_state() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let first = repo
        .enqueue_eh_download(
            -100,
            7213,
            "token",
            "Gallery",
            true,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        download.id,
        download.started_at.unwrap(),
        123,
        "claimed-cleanup.zip",
        0,
    )
    .await
    .unwrap();
    let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    repo.mark_eh_job_telegraph_ready(
        upload.id,
        upload.started_at.unwrap(),
        "https://telegra.ph/claimed-cleanup",
        Some("{\"pages\":[]}"),
        None,
        true,
    )
    .await
    .unwrap();
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(DELIVERY_STATUS_CANCELED),
        )
        .filter(eh_download_queue::Column::Id.eq(first.id))
        .exec(repo.db())
        .await
        .unwrap();
    repo.evaluate_eh_job_liveness(download.id, true)
        .await
        .unwrap();
    let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
    let cleanup_generation = cleanup.cleanup_started_at.unwrap();

    let rewrite_generation = Local::now().naive_local();
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            telegraph_rewrite_status: Set(Some(TELEGRAPH_REWRITE_STATUS_PENDING.to_string())),
            telegraph_rewrite_after: Set(Some(rewrite_generation)),
            telegraph_rewrite_started_at: Set(Some(rewrite_generation)),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(download.id))
        .exec(repo.db())
        .await
        .unwrap();
    let rebound = repo
        .enqueue_eh_download(
            -200,
            7213,
            "token",
            "Gallery",
            true,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(rebound.job_id, Some(download.id));
    assert!(repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .is_none());

    assert_eq!(
        repo.finalize_eh_job_cleanup(download.id, cleanup_generation, true)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::ReactivatedPending)
    );
    let settled = load_eh_job(&repo, download.id).await;
    assert_eq!(settled.status, JOB_STATUS_PENDING);
    assert_eq!(settled.cleanup_status, CLEANUP_STATUS_NONE);
    assert!(settled.zip_path.is_none());
    assert_eq!(settled.file_size, 0);
    assert_eq!(settled.gp_cost, 0);
    assert_eq!(settled.retry_count, 0);
    assert!(settled.next_retry_at.is_none());
    assert_eq!(settled.telegraph_status, TELEGRAPH_STATUS_READY);
    assert_eq!(
        settled.telegraph_url.as_deref(),
        Some("https://telegra.ph/claimed-cleanup")
    );
    assert_eq!(
        settled.telegraph_rewrite_data.as_deref(),
        Some("{\"pages\":[]}")
    );
    assert_eq!(
        settled.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_PENDING)
    );
    assert_eq!(settled.telegraph_rewrite_after, Some(rewrite_generation));
    assert_eq!(
        settled.telegraph_rewrite_started_at,
        Some(rewrite_generation)
    );
    assert_eq!(
        repo.get_next_eh_delivery_for_publish(false)
            .await
            .unwrap()
            .unwrap()
            .delivery
            .id,
        rebound.id
    );
    assert_eq!(
        repo.claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap()
            .id,
        download.id
    );
}

#[tokio::test]
async fn active_telegraph_rewrite_releases_zip_without_retiring() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let (job, delivery) = seed_rewrite_ready_job(&repo, 7214).await;
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            zip_path: Set(Some("rewrite-pending.zip".to_string())),
            file_size: Set(123_i64),
            gp_cost: Set(4_i64),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    repo.mark_eh_archive_delivery_sent(&delivery).await.unwrap();
    repo.mark_eh_telegraph_delivery_sent(&delivery, Some(0))
        .await
        .unwrap();
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(DELIVERY_STATUS_DONE),
        )
        .filter(eh_download_queue::Column::Id.eq(delivery.id))
        .exec(repo.db())
        .await
        .unwrap();

    repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
    let pending = load_eh_job(&repo, job.id).await;
    assert_eq!(pending.cleanup_status, CLEANUP_STATUS_PENDING);
    assert_eq!(pending.zip_path.as_deref(), Some("rewrite-pending.zip"));
    assert_eq!(
        pending.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_PENDING)
    );
    assert_eq!(
        pending.telegraph_rewrite_data.as_deref(),
        Some("{\"pages\":[]}")
    );

    let rewrite = repo
        .get_next_eh_job_for_telegraph_rewrite()
        .await
        .unwrap()
        .unwrap();
    let rewrite_generation = rewrite.telegraph_rewrite_started_at.unwrap();
    let rewrite_after = rewrite.telegraph_rewrite_after;
    let cleanup = repo.get_next_eh_job_for_cleanup().await.unwrap().unwrap();
    assert_eq!(cleanup.id, job.id);
    assert_eq!(
        repo.finalize_eh_job_cleanup(job.id, cleanup.cleanup_started_at.unwrap(), true)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::RetainedForRewrite)
    );
    let cleaned = load_eh_job(&repo, job.id).await;
    assert_eq!(cleaned.status, JOB_STATUS_DOWNLOADED);
    assert_eq!(cleaned.cleanup_status, CLEANUP_STATUS_NONE);
    assert!(cleaned.zip_path.is_none());
    assert_eq!(cleaned.file_size, 0);
    assert_eq!(cleaned.gp_cost, 0);
    assert_eq!(
        cleaned.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_REWRITING)
    );
    assert_eq!(
        cleaned.telegraph_rewrite_data.as_deref(),
        Some("{\"pages\":[]}")
    );
    assert_eq!(cleaned.telegraph_rewrite_after, rewrite_after);
    assert_eq!(
        cleaned.telegraph_rewrite_started_at,
        Some(rewrite_generation)
    );
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            error: Set(Some("stale download error".to_string())),
            retry_count: Set(3_i32),
            next_retry_at: Set(Some(Local::now().naive_local())),
            completed_at: Set(Some(Local::now().naive_local())),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    let cleaned = load_eh_job(&repo, job.id).await;

    let late_archive = repo
        .enqueue_eh_download(
            -200,
            7214,
            "token",
            "Gallery",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(late_archive.job_id, Some(job.id));
    assert!(repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .is_none());
    let recovered = load_eh_job(&repo, job.id).await;
    assert_eq!(recovered.status, JOB_STATUS_PENDING);
    assert_eq!(recovered.cleanup_status, CLEANUP_STATUS_NONE);
    assert!(recovered.zip_path.is_none());
    assert_eq!(recovered.started_at, cleaned.started_at);
    assert!(recovered.error.is_none());
    assert_eq!(recovered.retry_count, 0);
    assert!(recovered.next_retry_at.is_none());
    assert!(recovered.completed_at.is_none());
    assert!(recovered.background_download_status.is_none());
    assert_telegraph_rewrite_state_preserved(&cleaned, &recovered);
    assert_eq!(
        repo.claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap()
            .id,
        job.id
    );

    assert!(!repo
        .schedule_eh_job_telegraph_rewrite_retry(
            job.id,
            rewrite_generation,
            "temporary rewrite failure",
            1,
        )
        .await
        .unwrap());
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteNextRetryAt,
            Expr::value(None::<DateTime>),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    let retry = repo
        .get_next_eh_job_for_telegraph_rewrite()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retry.telegraph_rewrite_data.as_deref(),
        Some("{\"pages\":[]}")
    );
    assert!(retry.telegraph_rewrite_started_at.unwrap() > rewrite_generation);
    assert!(repo
        .schedule_eh_job_telegraph_rewrite_retry(
            job.id,
            retry.telegraph_rewrite_started_at.unwrap(),
            "terminal rewrite failure",
            0,
        )
        .await
        .unwrap());
    let terminal = load_eh_job(&repo, job.id).await;
    assert_eq!(
        terminal.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_FAILED)
    );
    assert_eq!(
        terminal.telegraph_rewrite_data.as_deref(),
        Some("{\"pages\":[]}")
    );
}

#[tokio::test]
async fn missing_zip_reset_stops_at_retry_limit_and_fails_deliveries() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let delivery = repo
        .enqueue_eh_download(
            -100,
            731,
            "token",
            "Gallery",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        download.id,
        download.started_at.unwrap(),
        123,
        "at-limit.zip",
        0,
    )
    .await
    .unwrap();
    let generation = download.started_at.unwrap();
    let second = repo
        .enqueue_eh_download(
            -200,
            731,
            "token",
            "Gallery",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(second.job_id, Some(download.id));

    // Below the limit: one more shared redownload is still scheduled.
    eh_gallery_jobs::Entity::update_many()
        .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(2_i32))
        .filter(eh_gallery_jobs::Column::Id.eq(download.id))
        .exec(repo.db())
        .await
        .unwrap();
    assert_eq!(
        repo.reset_eh_job_for_missing_zip(download.id, generation, "at-limit.zip", 3)
            .await
            .unwrap(),
        EhMissingZipResetOutcome::Reset
    );
    let reset = load_eh_job(&repo, download.id).await;
    assert_eq!(reset.status, JOB_STATUS_PENDING);
    assert_eq!(reset.retry_count, 3);
    assert!(reset.next_retry_at.is_some());
    assert!(reset.zip_path.is_none());
    assert_eq!(
        eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .status,
        DELIVERY_STATUS_WAITING
    );

    // Back to downloaded state at the limit for the terminal scenario.
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::NextRetryAt,
            Expr::value(None::<DateTime>),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(download.id))
        .exec(repo.db())
        .await
        .unwrap();
    let redownload = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(redownload.id, download.id);
    repo.mark_eh_job_downloaded(
        Main,
        redownload.id,
        redownload.started_at.unwrap(),
        123,
        "at-limit.zip",
        0,
    )
    .await
    .unwrap();
    let at_limit_generation = redownload.started_at.unwrap();
    eh_gallery_jobs::Entity::update_many()
        .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(3_i32))
        .filter(eh_gallery_jobs::Column::Id.eq(download.id))
        .exec(repo.db())
        .await
        .unwrap();

    assert_eq!(
        repo.reset_eh_job_for_missing_zip(download.id, at_limit_generation, "at-limit.zip", 3)
            .await
            .unwrap(),
        EhMissingZipResetOutcome::Exhausted
    );

    let failed = load_eh_job(&repo, download.id).await;
    assert_eq!(failed.status, JOB_STATUS_FAILED);
    assert_eq!(failed.retry_count, 4);
    assert_eq!(failed.error.as_deref(), Some("cached EH ZIP is missing"));
    assert!(failed.next_retry_at.is_none());
    assert!(failed.completed_at.is_some());
    assert_eq!(failed.zip_path.as_deref(), Some("at-limit.zip"));
    assert_eq!(failed.cleanup_status, CLEANUP_STATUS_PENDING);

    for delivery_id in [delivery.id, second.id] {
        let failed_delivery = eh_download_queue::Entity::find_by_id(delivery_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed_delivery.status, DELIVERY_STATUS_FAILED);
        assert!(failed_delivery.next_retry_at.is_none());
    }

    // A repeated reset attempt is stale: the failed generation is gone.
    assert_eq!(
        repo.reset_eh_job_for_missing_zip(download.id, at_limit_generation, "at-limit.zip", 3)
            .await
            .unwrap(),
        EhMissingZipResetOutcome::Stale
    );
    // Neither the normal nor the background source selector ever claims the
    // failed job again.
    assert!(repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .is_none());
    assert!(repo
        .claim_eh_download_job(Background, true)
        .await
        .unwrap()
        .is_none());
    // The durable cleanup lane now owns the leftover archive family.
    assert_eq!(
        repo.get_next_eh_job_for_cleanup()
            .await
            .unwrap()
            .unwrap()
            .id,
        download.id
    );
}

#[tokio::test]
async fn stale_download_completion_cannot_overwrite_a_new_claim_or_append_a_ledger_row() {
    for queue in [Main, Background] {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let variant = EhGalleryVariant::archive("1280x");
        let delivery = repo
            .enqueue_eh_download(
                -100, 78, "token", "Gallery", false, "direct", &variant, None, true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job_id = delivery.job_id.unwrap();
        let normal = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        let first_claim = if queue == Background {
            repo.schedule_eh_job_background_download(normal.id, &normal.status, "test handoff")
                .await
                .unwrap();
            repo.claim_eh_download_job(queue, true)
                .await
                .unwrap()
                .unwrap()
        } else {
            normal
        };
        let first_started_at = first_claim.started_at.unwrap();
        if queue == Main {
            assert_eq!(
                repo.reset_stale_eh_shared_work(3600, 3600)
                    .await
                    .unwrap()
                    .downloads,
                1
            );
        } else {
            eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::BackgroundDownloadStartedAt,
                    Expr::value(Some(
                        Local::now().naive_local() - chrono::Duration::seconds(2),
                    )),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job_id))
                .exec(repo.db())
                .await
                .unwrap();
            assert_eq!(
                repo.reset_stale_eh_shared_work(1, 1)
                    .await
                    .unwrap()
                    .backgrounds,
                1
            );
        }
        if queue == Main {
            let reset = load_eh_job(&repo, job_id).await;
            assert_eq!(reset.status, JOB_STATUS_PENDING);
            assert_eq!(reset.started_at, first_claim.started_at);
        }
        let replacement_claim = repo
            .claim_eh_download_job(queue, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replacement_claim.id, first_claim.id);
        assert!(replacement_claim.started_at.unwrap() > first_started_at);
        let before_completion = load_eh_job(&repo, job_id).await;

        let error = repo
            .mark_eh_job_downloaded(
                queue,
                job_id,
                first_started_at,
                100,
                "/tmp/stale-background.zip",
                0,
            )
            .await
            .expect_err("stale download claim must not complete a newer generation");
        assert!(error.to_string().contains("claim changed concurrently"));
        assert!(eh_download_completions::Entity::find()
            .all(repo.db())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(load_eh_job(&repo, job_id).await, before_completion);
    }
}

#[tokio::test]
async fn shared_job_crash_recovery_resets_each_claim_once() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let now = Local::now().naive_local() - chrono::Duration::seconds(120);
    let variant = EhGalleryVariant::archive("1280x");
    let download = repo
        .enqueue_eh_download(
            -100,
            911,
            "download",
            "Download",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let upload = repo
        .enqueue_eh_download(
            -101,
            912,
            "upload",
            "Upload",
            true,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let background = repo
        .enqueue_eh_download(
            -102,
            913,
            "background",
            "Background",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let rewrite = repo
        .enqueue_eh_download(
            -103,
            914,
            "rewrite",
            "Rewrite",
            true,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let cleanup = repo
        .enqueue_eh_download(
            -104,
            915,
            "cleanup",
            "Cleanup",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");

    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            status: Set(JOB_STATUS_DOWNLOADING.to_string()),
            started_at: Set(Some(now)),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(download.job_id.unwrap()))
        .exec(repo.db())
        .await
        .unwrap();
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            status: Set(JOB_STATUS_DOWNLOADED.to_string()),
            telegraph_status: Set(TELEGRAPH_STATUS_UPLOADING.to_string()),
            started_at: Set(Some(now)),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(upload.job_id.unwrap()))
        .exec(repo.db())
        .await
        .unwrap();
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            background_download_status: Set(Some(BACKGROUND_STATUS_RUNNING.to_string())),
            background_download_started_at: Set(Some(now)),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(background.job_id.unwrap()))
        .exec(repo.db())
        .await
        .unwrap();
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            telegraph_rewrite_status: Set(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
            telegraph_rewrite_data: Set(Some("payload".to_string())),
            telegraph_rewrite_started_at: Set(Some(now)),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(rewrite.job_id.unwrap()))
        .exec(repo.db())
        .await
        .unwrap();
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            cleanup_status: Set(CLEANUP_STATUS_RUNNING.to_string()),
            cleanup_started_at: Set(Some(now)),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(cleanup.job_id.unwrap()))
        .exec(repo.db())
        .await
        .unwrap();
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(DELIVERY_STATUS_PUBLISHING),
        )
        .filter(eh_download_queue::Column::Id.eq(upload.id))
        .exec(repo.db())
        .await
        .unwrap();

    assert_eq!(
        repo.reset_stale_eh_shared_work(60, 60).await.unwrap(),
        EhStaleResetCounts {
            downloads: 1,
            uploads: 1,
            backgrounds: 1,
            rewrites: 1,
            cleanups: 1,
            deliveries: 1,
        }
    );
    let reset_download = load_eh_job(&repo, download.job_id.unwrap()).await;
    let reset_upload = load_eh_job(&repo, upload.job_id.unwrap()).await;
    let reset_background = load_eh_job(&repo, background.job_id.unwrap()).await;
    let reset_rewrite = load_eh_job(&repo, rewrite.job_id.unwrap()).await;
    let reset_cleanup = load_eh_job(&repo, cleanup.job_id.unwrap()).await;
    assert_eq!(reset_download.status, JOB_STATUS_PENDING);
    assert_eq!(reset_upload.telegraph_status, TELEGRAPH_STATUS_PENDING);
    assert_eq!(
        reset_background.background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_PENDING)
    );
    assert_eq!(
        reset_rewrite.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_PENDING)
    );
    assert_eq!(
        reset_rewrite.telegraph_rewrite_data.as_deref(),
        Some("payload")
    );
    assert_eq!(reset_cleanup.cleanup_status, CLEANUP_STATUS_PENDING);
    assert_eq!(
        eh_download_queue::Entity::find_by_id(upload.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .status,
        DELIVERY_STATUS_WAITING
    );
    assert_eq!(
        repo.reset_stale_eh_shared_work(60, 60).await.unwrap(),
        EhStaleResetCounts::default()
    );
}

#[tokio::test]
async fn terminal_delivery_reenqueue_starts_a_clean_requested_wave() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let initial = repo
        .enqueue_eh_download(
            -100,
            917,
            "old-token",
            "Old title",
            true,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("980x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let initial_created_at = initial.created_at;
    let old_job_id = initial.job_id.unwrap();
    let now = Local::now().naive_local();
    eh_download_queue::Entity::update_many()
        .set(eh_download_queue::ActiveModel {
            status: Set(DELIVERY_STATUS_DONE.to_string()),
            file_size: Set(123_i64),
            gp_cost: Set(45_i64),
            error: Set(Some("exhausted error".to_string())),
            retry_count: Set(7_i32),
            started_at: Set(Some(now)),
            completed_at: Set(Some(now)),
            zip_path: Set(Some("old.zip".to_string())),
            telegraph_url: Set(Some("https://telegra.ph/old".to_string())),
            next_retry_at: Set(Some(now)),
            archive_sent_at: Set(Some(now)),
            telegraph_sent_at: Set(Some(now)),
            background_download_status: Set(Some(BACKGROUND_STATUS_RUNNING.to_string())),
            background_download_started_at: Set(Some(now)),
            background_download_next_retry_at: Set(Some(now)),
            background_download_attempt_count: Set(2_i32),
            background_download_error: Set(Some("background error".to_string())),
            telegraph_rewrite_data: Set(Some("rewrite data".to_string())),
            telegraph_rewrite_status: Set(Some(TELEGRAPH_REWRITE_STATUS_REWRITING.to_string())),
            telegraph_rewrite_after: Set(Some(now)),
            telegraph_rewrite_started_at: Set(Some(now)),
            telegraph_rewrite_next_retry_at: Set(Some(now)),
            telegraph_rewrite_retry_count: Set(3_i32),
            telegraph_rewrite_error: Set(Some("rewrite error".to_string())),
            telegraph_rewritten_at: Set(Some(now)),
            ..Default::default()
        })
        .filter(eh_download_queue::Column::Id.eq(initial.id))
        .exec(repo.db())
        .await
        .unwrap();

    let reenqueued = repo
        .enqueue_eh_download(
            -100,
            917,
            "new-token",
            "New title",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("original"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");

    assert_eq!(reenqueued.id, initial.id);
    assert_eq!(reenqueued.status, DELIVERY_STATUS_WAITING);
    assert_ne!(reenqueued.job_id, Some(old_job_id));
    assert_eq!(reenqueued.token, "new-token");
    assert_eq!(reenqueued.title, "New title");
    assert!(!reenqueued.telegraph);
    assert_eq!(reenqueued.source, SOURCE_DIRECT);
    assert_eq!(reenqueued.created_at, initial_created_at);
    assert_eq!(reenqueued.file_size, 0);
    assert_eq!(reenqueued.gp_cost, 0);
    assert!(reenqueued.error.is_none());
    assert_eq!(reenqueued.retry_count, 0);
    assert_eq!(reenqueued.started_at, Some(now));
    assert!(reenqueued.completed_at.is_none());
    assert!(reenqueued.zip_path.is_none());
    assert!(reenqueued.telegraph_url.is_none());
    assert!(reenqueued.next_retry_at.is_none());
    assert!(reenqueued.archive_sent_at.is_none());
    assert!(reenqueued.telegraph_sent_at.is_none());
    assert!(reenqueued.background_download_status.is_none());
    assert!(reenqueued.background_download_started_at.is_none());
    assert!(reenqueued.background_download_next_retry_at.is_none());
    assert_eq!(reenqueued.background_download_attempt_count, 0);
    assert!(reenqueued.background_download_error.is_none());
    assert!(reenqueued.telegraph_rewrite_data.is_none());
    assert!(reenqueued.telegraph_rewrite_status.is_none());
    assert!(reenqueued.telegraph_rewrite_after.is_none());
    assert!(reenqueued.telegraph_rewrite_started_at.is_none());
    assert!(reenqueued.telegraph_rewrite_next_retry_at.is_none());
    assert_eq!(reenqueued.telegraph_rewrite_retry_count, 0);
    assert!(reenqueued.telegraph_rewrite_error.is_none());
    assert!(reenqueued.telegraph_rewritten_at.is_none());
    assert_eq!(
        repo.claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap()
            .id,
        reenqueued.job_id.unwrap(),
        "the clean terminal re-enqueue must start a claimable new wave"
    );
}
