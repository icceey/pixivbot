use super::*;
use crate::db::entities::eh_download_queue::{Column, Entity};
use crate::db::entities::eh_gallery_jobs;
use crate::db::repo::eh_gallery_jobs::EhDownloadQueue::{Background, Main};
use crate::db::repo::eh_gallery_jobs::{
    EhGalleryVariant, CLEANUP_STATUS_PENDING, JOB_STATUS_DOWNLOADED, JOB_STATUS_DOWNLOADING,
    JOB_STATUS_PENDING, JOB_STATUS_RETIRED, TELEGRAPH_STATUS_NOT_REQUIRED,
};
use crate::db::repo::tests_helpers;
use chrono::Duration;
use sea_orm::{sea_query::Expr, Set};

#[derive(Default)]
struct RecordingAbortUploader {
    aborts: std::sync::Mutex<Vec<(std::path::PathBuf, bool)>>,
    fail_abort: bool,
}

#[async_trait::async_trait]
impl ImageUploader for RecordingAbortUploader {
    async fn upload_images(
        &self,
        _images: &[media_publish::ImageUploadInput<'_>],
    ) -> media_publish::Result<Vec<String>> {
        Err(media_publish::Error::Other(
            "recording uploader upload_images must not be called".to_string(),
        ))
    }

    async fn abort_upload_state(
        &self,
        uploads_dir: &std::path::Path,
    ) -> Result<(), media_publish::UploadStateError> {
        self.aborts
            .lock()
            .unwrap()
            .push((uploads_dir.to_path_buf(), uploads_dir.exists()));
        if self.fail_abort {
            return Err(media_publish::UploadStateError::AbortHttp(503));
        }
        Ok(())
    }
}

async fn job_for_delivery(
    repo: &Repo,
    delivery: &eh_download_queue::Model,
) -> eh_gallery_jobs::Model {
    eh_gallery_jobs::Entity::find_by_id(delivery.job_id.unwrap())
        .one(repo.db())
        .await
        .unwrap()
        .unwrap()
}

async fn seed_publishing_archive_delivery(
    repo: &Repo,
    gid: i64,
) -> (eh_download_queue::Model, eh_gallery_jobs::Model) {
    let delivery = repo
        .enqueue_eh_download(
            -100,
            gid,
            "terminal-transition",
            "Terminal transition",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
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
        1,
        "/tmp/terminal-transition.zip",
        0,
    )
    .await
    .unwrap();
    let claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.delivery.id, delivery.id);
    (claim.delivery, claim.job)
}

async fn set_eh_job_claim_fields(
    repo: &Repo,
    job_id: i32,
    created_at: DateTime,
    next_retry_at: Option<DateTime>,
) {
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            created_at: Set(created_at),
            next_retry_at: Set(next_retry_at),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(job_id))
        .exec(repo.db())
        .await
        .unwrap();
}

#[tokio::test]
async fn stale_publish_recovery_preserves_progress_and_fences_the_previous_generation() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let (old, job) = seed_publishing_archive_delivery(&repo, 90_003).await;
    // Keep an unsent Telegraph surface so the old worker's late marker can
    // corrupt the replacement claim unless generation fencing is applied.
    eh_download_queue::Entity::update_many()
        .col_expr(eh_download_queue::Column::Telegraph, Expr::value(true))
        .filter(eh_download_queue::Column::Id.eq(old.id))
        .exec(repo.db())
        .await
        .unwrap();
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            telegraph_status: Set(TELEGRAPH_STATUS_READY.to_string()),
            telegraph_url: Set(Some("https://telegra.ph/recovered".to_string())),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    repo.mark_eh_archive_delivery_sent(&old).await.unwrap();
    let cutoff = old.started_at.unwrap() + chrono::Duration::seconds(1);
    assert_eq!(
        repo.reset_stale_eh_publish_deliveries(cutoff, &[old.id])
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        repo.reset_stale_eh_publish_deliveries(cutoff, &[])
            .await
            .unwrap(),
        1
    );
    let new = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.delivery.id, old.id);
    assert!(new.delivery.started_at > old.started_at);
    assert!(new.delivery.archive_sent_at.is_some());
    assert!(new.delivery.telegraph_sent_at.is_none());
    assert!(repo
        .get_eh_delivery_publish_claim(&old, true)
        .await
        .unwrap()
        .is_none());
    assert!(repo
        .mark_eh_telegraph_delivery_sent(&old, None)
        .await
        .is_err());
    assert!(repo.defer_eh_delivery_publish(&old, 60).await.is_err());
    assert!(repo
        .schedule_eh_delivery_retry(&old, "late failure", 0, true)
        .await
        .is_err());
    assert!(repo.mark_eh_delivery_done(&old, true).await.is_err());
    let unchanged = eh_download_queue::Entity::find_by_id(old.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged, new.delivery);
    repo.mark_eh_telegraph_delivery_sent(&new.delivery, None)
        .await
        .unwrap();
    repo.mark_eh_delivery_done(&new.delivery, true)
        .await
        .unwrap();
}

#[tokio::test]
async fn chat_lock_drop_with_queued_waiter_never_splits_and_cleans_idle_key() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let registry = Arc::new(EhChatLockRegistry::default());
    let first = registry.lock_chat(-100).await;
    let active = Arc::new(AtomicUsize::new(0));
    let second_acquired = Arc::new(tokio::sync::Notify::new());
    let release_second = Arc::new(tokio::sync::Notify::new());
    let third_acquired = Arc::new(tokio::sync::Notify::new());

    let second_registry = Arc::clone(&registry);
    let second_active = Arc::clone(&active);
    let second_acquired_signal = Arc::clone(&second_acquired);
    let second_release = Arc::clone(&release_second);
    let second = tokio::spawn(async move {
        let _guard = second_registry.lock_chat(-100).await;
        assert_eq!(second_active.fetch_add(1, Ordering::SeqCst), 0);
        second_acquired_signal.notify_one();
        second_release.notified().await;
        assert_eq!(second_active.fetch_sub(1, Ordering::SeqCst), 1);
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let waiter_is_queued = registry
                .locks
                .lock()
                .unwrap()
                .get(&-100)
                .is_some_and(|mutex| mutex.strong_count() >= 3);
            if waiter_is_queued {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second caller should queue on the first mutex");

    drop(first);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        second_acquired.notified(),
    )
    .await
    .expect("queued caller should acquire after the current guard drops");

    let third_registry = Arc::clone(&registry);
    let third_active = Arc::clone(&active);
    let third_acquired_signal = Arc::clone(&third_acquired);
    let third = tokio::spawn(async move {
        let _guard = third_registry.lock_chat(-100).await;
        assert_eq!(third_active.fetch_add(1, Ordering::SeqCst), 0);
        third_acquired_signal.notify_one();
        assert_eq!(third_active.fetch_sub(1, Ordering::SeqCst), 1);
    });
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            third_acquired.notified(),
        )
        .await
        .is_err(),
        "a third caller must wait for the queued caller's same-chat critical section"
    );

    release_second.notify_one();
    second.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), third_acquired.notified())
        .await
        .expect("third caller should acquire after the second caller releases");
    third.await.unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if registry.locks.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle chat lock should be removed from the registry");
}

#[tokio::test]
async fn cancel_one_shared_delivery_preserves_remaining_job_ownership() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let canceled = repo
        .enqueue_eh_subscription_download(
            -100, 123, 700, "tok", "Title", true, &variant, None, true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let sibling = repo
        .enqueue_eh_subscription_download(
            -200, 456, 700, "tok", "Title", false, &variant, None, true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(canceled.job_id, sibling.job_id);

    let job = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        job.id,
        job.started_at.unwrap(),
        1,
        "/tmp/shared-cancel.zip",
        0,
    )
    .await
    .unwrap();

    repo.cancel_eh_subscription_queue_entries(123, true)
        .await
        .unwrap();

    let canceled = Entity::find_by_id(canceled.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    let sibling = Entity::find_by_id(sibling.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    let job = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(canceled.status, STATUS_CANCELED);
    assert_eq!(sibling.status, STATUS_WAITING);
    assert_ne!(job.status, JOB_STATUS_RETIRED);
    assert_eq!(job.telegraph_required, sibling.telegraph);
    assert_eq!(job.cleanup_status, CLEANUP_STATUS_NONE);
    assert_eq!(job.zip_path.as_deref(), Some("/tmp/shared-cancel.zip"));
}

#[tokio::test]
async fn test_cancel_subscription_queue_entries_cancels_only_active_subscription_rows() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let sub_row = repo
        .enqueue_eh_subscription_download(
            -100,
            123,
            40,
            "tok",
            "Title",
            false,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(sub_row.source, SOURCE_SUBSCRIPTION);
    assert_eq!(sub_row.subscription_ids.as_deref(), Some("123"));
    let other_sub_row = repo
        .enqueue_eh_subscription_download(
            -100,
            456,
            41,
            "tok",
            "Title",
            false,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let direct_row = repo
        .enqueue_eh_download(
            -100,
            42,
            "tok",
            "Title",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert!(direct_row.subscription_ids.is_none());
    let done_row = repo
        .enqueue_eh_subscription_download(
            -100,
            123,
            43,
            "tok",
            "Title",
            false,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    Entity::update_many()
        .col_expr(Column::Status, Expr::value(STATUS_DONE))
        .filter(Column::Id.eq(done_row.id))
        .exec(&repo.db)
        .await
        .unwrap();

    let changed = repo
        .cancel_eh_subscription_queue_entries(123, true)
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
    assert_eq!(
        Entity::find_by_id(other_sub_row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap()
            .status,
        other_sub_row.status
    );
    assert_eq!(
        Entity::find_by_id(direct_row.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap()
            .status,
        direct_row.status
    );
    let done = Entity::find_by_id(done_row.id)
        .one(&repo.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.status, STATUS_DONE);
    assert_eq!(done.subscription_ids, None);
}

#[tokio::test]
async fn publish_selector_reaches_ready_rows_behind_blocked_backlog() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    for gid in 10_000..10_064 {
        repo.enqueue_eh_download(
            -gid,
            gid,
            "blocked",
            "Blocked",
            true,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    }

    let first_ready = repo
        .enqueue_eh_download(
            -20_000,
            20_000,
            "first-ready",
            "First ready",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            status: Set(JOB_STATUS_DOWNLOADED.to_string()),
            zip_path: Set(Some("/tmp/first-ready.zip".to_string())),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(first_ready.job_id.unwrap()))
        .exec(repo.db())
        .await
        .unwrap();

    let second_ready = repo
        .enqueue_eh_download(
            -20_001,
            20_001,
            "second-ready",
            "Second ready",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            status: Set(JOB_STATUS_DOWNLOADED.to_string()),
            zip_path: Set(Some("/tmp/second-ready.zip".to_string())),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(second_ready.job_id.unwrap()))
        .exec(repo.db())
        .await
        .unwrap();

    let first_claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_claim.delivery.id, first_ready.id);
    repo.defer_eh_delivery_publish(&first_claim.delivery, 60)
        .await
        .unwrap();

    let second_claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second_claim.delivery.id, second_ready.id);
}

#[tokio::test]
async fn test_publish_selector_keeps_one_in_flight_delivery_per_chat() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let mut deliveries = Vec::new();
    for (chat_id, gid) in [(-100, 21_000), (-100, 21_001), (-200, 21_002)] {
        let delivery = repo
            .enqueue_eh_download(
                chat_id,
                gid,
                "token",
                "Ready",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        eh_gallery_jobs::Entity::update_many()
            .set(eh_gallery_jobs::ActiveModel {
                status: Set(JOB_STATUS_DOWNLOADED.to_string()),
                zip_path: Set(Some(format!("/tmp/{gid}.zip"))),
                ..Default::default()
            })
            .filter(eh_gallery_jobs::Column::Id.eq(delivery.job_id.unwrap()))
            .exec(repo.db())
            .await
            .unwrap();
        deliveries.push(delivery);
    }

    let first = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.delivery.id, deliveries[0].id);

    let second = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        second.delivery.id, deliveries[2].id,
        "another chat must receive the next publish slot"
    );

    repo.defer_eh_delivery_publish(&first.delivery, 60)
        .await
        .unwrap();
    let third = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        third.delivery.id, deliveries[1].id,
        "the chat may claim its next delivery after the prior claim is released"
    );
}

#[tokio::test]
async fn test_cleanup_eh_cache_orphans_aborts_orphan_upload_state_before_removal_and_preserves_active_state(
) {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let cache_dir = temp.path();

    let orphan_zip = cache_dir.join("orphan.zip");
    let orphan_part = cache_dir.join("orphan.zip.part");
    let orphan_parts = cache_dir.join("orphan.zip.parts");
    let orphan_uploads = cache_dir.join("orphan.zip.uploads");
    let orphan_selection = cache_dir.join("orphan.zip.original");
    let active_zip = cache_dir.join("active.zip");
    let active_part = cache_dir.join("active.zip.part");
    let active_parts = cache_dir.join("active.zip.parts");
    let active_uploads = cache_dir.join("active.zip.uploads");
    let active_selection = cache_dir.join("active.zip.original");
    let unrelated = cache_dir.join("notes").join("keep.txt");
    std::fs::write(&orphan_zip, b"zip").unwrap();
    std::fs::write(&orphan_part, b"partial").unwrap();
    std::fs::write(&orphan_selection, []).unwrap();
    std::fs::create_dir_all(orphan_parts.join("nested")).unwrap();
    std::fs::write(orphan_parts.join("manifest.json"), b"manifest").unwrap();
    std::fs::write(orphan_parts.join("nested").join("part-0001"), b"part").unwrap();
    std::fs::create_dir_all(orphan_uploads.join("nested")).unwrap();
    std::fs::write(orphan_uploads.join("archive.json"), b"archive").unwrap();
    std::fs::write(orphan_uploads.join("nested").join("image-0.json"), b"image").unwrap();
    std::fs::write(&active_zip, b"zip").unwrap();
    std::fs::write(&active_part, b"partial").unwrap();
    std::fs::write(&active_selection, []).unwrap();
    std::fs::create_dir_all(active_parts.join("nested")).unwrap();
    std::fs::write(active_parts.join("manifest.json"), b"manifest").unwrap();
    std::fs::write(active_parts.join("nested").join("part-0001"), b"part").unwrap();
    std::fs::create_dir_all(active_uploads.join("nested")).unwrap();
    std::fs::write(active_uploads.join("archive.json"), b"archive").unwrap();
    std::fs::write(active_uploads.join("nested").join("image-0.json"), b"image").unwrap();
    std::fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
    std::fs::write(&unrelated, b"keep").unwrap();

    let model = repo
        .enqueue_eh_download(
            -100,
            77,
            "tok",
            "Title",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let claimed = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, model.job_id.unwrap());
    repo.mark_eh_job_downloaded(
        Main,
        claimed.id,
        claimed.started_at.unwrap(),
        5000,
        active_zip.to_str().unwrap(),
        0,
    )
    .await
    .unwrap();

    let uploader = RecordingAbortUploader::default();
    repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader), true)
        .await
        .unwrap();

    assert_eq!(
        *uploader.aborts.lock().unwrap(),
        vec![(orphan_uploads.clone(), true)],
        "only the orphan upload state should be aborted before removal"
    );

    assert!(!orphan_zip.exists(), "orphan final ZIP should be removed");
    assert!(
        !orphan_selection.exists(),
        "orphan selection must be removed with its archive"
    );
    assert!(
        active_selection.exists(),
        "active selection must survive cleanup for safe retries"
    );
    assert!(
        !orphan_part.exists(),
        "orphan partial ZIP should be removed"
    );
    assert!(
        !orphan_parts.exists(),
        "orphan multipart state should be removed recursively"
    );
    assert!(
        !orphan_uploads.exists(),
        "orphan upload state should be removed recursively"
    );
    assert!(active_zip.exists(), "active final ZIP should be kept");
    assert!(
        active_part.exists(),
        "a persisted shared-job owner keeps its entire artifact family"
    );
    assert!(
        active_parts.exists(),
        "a persisted shared-job owner keeps its assembly state"
    );
    assert!(
        active_uploads.join("archive.json").exists()
            && active_uploads.join("nested").join("image-0.json").exists(),
        "active final ZIP should keep resumable upload state"
    );
    assert!(
        unrelated.exists(),
        "unrelated directories should be ignored"
    );
}

#[tokio::test]
async fn test_cleanup_eh_cache_orphans_preserves_family_when_abort_fails() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let cache_dir = temp.path();
    let orphan_zip = cache_dir.join("abort-fails.zip");
    let orphan_part = cache_dir.join("abort-fails.zip.part");
    let orphan_uploads = cache_dir.join("abort-fails.zip.uploads");
    let manifest = orphan_uploads.join("archive.json");
    std::fs::write(&orphan_zip, b"zip").unwrap();
    std::fs::write(&orphan_part, b"partial").unwrap();
    std::fs::create_dir_all(&orphan_uploads).unwrap();
    std::fs::write(&manifest, b"manifest").unwrap();
    let uploader = RecordingAbortUploader {
        fail_abort: true,
        ..Default::default()
    };

    repo.cleanup_eh_cache_orphans(cache_dir, Some(&uploader), true)
        .await
        .unwrap();

    assert_eq!(
        *uploader.aborts.lock().unwrap(),
        vec![(orphan_uploads.clone(), true)],
        "Abort must observe the manifest directory before cleanup considers deletion"
    );
    assert!(
        orphan_zip.exists(),
        "Abort failure must retain the final ZIP"
    );
    assert!(
        orphan_part.exists(),
        "Abort failure must retain family scratch"
    );
    assert!(
        orphan_uploads.exists(),
        "Abort failure must retain upload state"
    );
    assert!(
        manifest.exists(),
        "Abort failure must retain the upload manifest"
    );
}

#[tokio::test]
async fn test_publish_markers_survive_delivery_retry_and_marker_safe_reenqueue() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let model = repo
        .enqueue_eh_download(
            -100,
            20,
            "tok",
            "Title",
            true,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");

    let claimed = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        claimed.id,
        claimed.started_at.unwrap(),
        5000,
        "/tmp/20.zip",
        0,
    )
    .await
    .unwrap();
    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            telegraph_url: Set(Some("https://telegra.ph/20".to_string())),
            telegraph_status: Set(
                crate::db::repo::eh_gallery_jobs::TELEGRAPH_STATUS_READY.to_string()
            ),
            telegraph_required: Set(true),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(model.job_id.unwrap()))
        .exec(repo.db())
        .await
        .unwrap();

    let claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.delivery.id, model.id);
    repo.mark_eh_archive_delivery_sent(&claim.delivery)
        .await
        .unwrap();
    repo.mark_eh_telegraph_delivery_sent(&claim.delivery, None)
        .await
        .unwrap();

    repo.schedule_eh_delivery_retry(&claim.delivery, "telegram failed", 3, true)
        .await
        .unwrap();
    let preserved = Entity::find_by_id(model.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert!(preserved.archive_sent_at.is_some());
    assert!(preserved.telegraph_sent_at.is_some());

    let rebound = repo
        .enqueue_eh_download(
            -100,
            20,
            "tok",
            "New",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(rebound.job_id, model.job_id);
    assert!(rebound.archive_sent_at.is_some());
    assert!(rebound.telegraph_sent_at.is_some());
}

#[tokio::test]
async fn test_deferred_item_not_claimable_before_delay_expires() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let delivery = repo
        .enqueue_eh_download(
            -100,
            35,
            "tok",
            "Title",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let claimed = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, delivery.job_id.unwrap());

    // Defer with a long delay — the item should not be picked up
    repo.defer_eh_job_download(claimed.id, 3600).await.unwrap();

    let next = repo.claim_eh_download_job(Main, true).await.unwrap();
    assert!(
        next.is_none(),
        "deferred item should not be claimable before delay expires"
    );

    let reloaded = job_for_delivery(&repo, &delivery).await;
    assert_eq!(reloaded.status, JOB_STATUS_PENDING);
    assert_eq!(reloaded.retry_count, 0);
    assert!(reloaded.next_retry_at.is_some());
}

#[tokio::test]
async fn test_main_download_claim_prioritizes_recent_fifo_then_old_lifo() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let anchor = chrono::Local::now().naive_local();

    let recent_first = repo
        .enqueue_eh_download(
            -100,
            100,
            "tok",
            "Recent first",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let recent_second = repo
        .enqueue_eh_download(
            -100,
            101,
            "tok",
            "Recent second",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let recent_newer = repo
        .enqueue_eh_download(
            -100,
            102,
            "tok",
            "Recent newer",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let cutoff_first = repo
        .enqueue_eh_download(
            -100,
            200,
            "tok",
            "Cutoff first",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let cutoff_second = repo
        .enqueue_eh_download(
            -100,
            201,
            "tok",
            "Cutoff second",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let old = repo
        .enqueue_eh_download(
            -100,
            300,
            "tok",
            "Old",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let future_retry = repo
        .enqueue_eh_download(
            -100,
            400,
            "tok",
            "Future retry",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let recent_first_job = job_for_delivery(&repo, &recent_first).await;
    let recent_second_job = job_for_delivery(&repo, &recent_second).await;
    let recent_newer_job = job_for_delivery(&repo, &recent_newer).await;
    let cutoff_first_job = job_for_delivery(&repo, &cutoff_first).await;
    let cutoff_second_job = job_for_delivery(&repo, &cutoff_second).await;
    let old_job = job_for_delivery(&repo, &old).await;
    let future_retry_job = job_for_delivery(&repo, &future_retry).await;

    set_eh_job_claim_fields(
        &repo,
        recent_first_job.id,
        anchor - Duration::minutes(90),
        None,
    )
    .await;
    set_eh_job_claim_fields(
        &repo,
        recent_second_job.id,
        anchor - Duration::minutes(90),
        None,
    )
    .await;
    set_eh_job_claim_fields(
        &repo,
        recent_newer_job.id,
        anchor - Duration::minutes(30),
        None,
    )
    .await;
    set_eh_job_claim_fields(
        &repo,
        cutoff_first_job.id,
        anchor - Duration::hours(2),
        None,
    )
    .await;
    set_eh_job_claim_fields(
        &repo,
        cutoff_second_job.id,
        anchor - Duration::hours(2),
        None,
    )
    .await;
    set_eh_job_claim_fields(&repo, old_job.id, anchor - Duration::hours(3), None).await;
    set_eh_job_claim_fields(
        &repo,
        future_retry_job.id,
        anchor - Duration::hours(4),
        Some(anchor + Duration::minutes(1)),
    )
    .await;

    assert!(recent_first_job.id < recent_second_job.id);
    assert!(cutoff_first_job.id < cutoff_second_job.id);
    for expected_gid in [100, 101, 102, 201, 200, 300] {
        let claimed = repo
            .claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.gid, expected_gid);
        assert_eq!(claimed.status, JOB_STATUS_DOWNLOADING);
    }

    assert!(repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .is_none());
    let deferred = job_for_delivery(&repo, &future_retry).await;
    assert_eq!(deferred.status, JOB_STATUS_PENDING);
    assert_eq!(deferred.next_retry_at, Some(anchor + Duration::minutes(1)));
}

#[tokio::test]
async fn test_background_download_lifecycle_success_retry_and_stale_reset() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let model = repo
        .enqueue_eh_download(
            -100,
            45,
            "tok",
            "Title",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");

    let claimed = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, model.job_id.unwrap());
    let main_generation = claimed.started_at;
    let handed_off = repo
        .schedule_eh_job_background_download(claimed.id, JOB_STATUS_DOWNLOADING, "slow")
        .await
        .unwrap();
    assert_eq!(handed_off.started_at, main_generation);

    let bg_claim = repo
        .claim_eh_download_job(Background, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bg_claim.id, model.job_id.unwrap());
    assert_eq!(
        bg_claim.background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_RUNNING)
    );
    assert!(bg_claim.started_at > main_generation);
    let background_generation = bg_claim.started_at;

    let (retry, permanent) = repo
        .schedule_eh_job_download_retry(
            Background,
            bg_claim.id,
            bg_claim.started_at.unwrap(),
            "still slow",
            6,
        )
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

    eh_gallery_jobs::Entity::update_many()
        .set(eh_gallery_jobs::ActiveModel {
            background_download_status: Set(Some(BACKGROUND_STATUS_RUNNING.to_string())),
            background_download_started_at: Set(Some(
                Local::now().naive_local() - chrono::Duration::seconds(7200),
            )),
            ..Default::default()
        })
        .filter(eh_gallery_jobs::Column::Id.eq(model.job_id.unwrap()))
        .exec(&repo.db)
        .await
        .unwrap();
    let reset = repo.reset_stale_eh_shared_work(3600, 3600).await.unwrap();
    assert_eq!(reset.backgrounds, 1);
    let reset_row = eh_gallery_jobs::Entity::find_by_id(model.job_id.unwrap())
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

    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
            Expr::value(Some(
                Local::now().naive_local() - chrono::Duration::seconds(1),
            )),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(model.job_id.unwrap()))
        .exec(&repo.db)
        .await
        .unwrap();

    let bg_claim = repo
        .claim_eh_download_job(Background, true)
        .await
        .unwrap()
        .unwrap();
    assert!(bg_claim.started_at > background_generation);
    let done = repo
        .mark_eh_job_downloaded(
            Background,
            bg_claim.id,
            bg_claim.started_at.unwrap(),
            1234,
            "/tmp/bg.zip",
            0,
        )
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
async fn startup_reconciliation_repairs_consumerless_crash_state_idempotently() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let (delivery, job) = seed_publishing_archive_delivery(&repo, 904).await;
    Entity::update_many()
        .col_expr(Column::Status, Expr::value(STATUS_DONE))
        .filter(Column::Id.eq(delivery.id))
        .exec(repo.db())
        .await
        .unwrap();

    assert_eq!(
        repo.reconcile_eh_shared_job_liveness(true).await.unwrap(),
        1
    );
    let first = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.status, JOB_STATUS_RETIRED);
    assert_eq!(first.cleanup_status, CLEANUP_STATUS_PENDING);

    assert_eq!(
        repo.reconcile_eh_shared_job_liveness(true).await.unwrap(),
        1
    );
    let second = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second, first);
}

#[tokio::test]
async fn disable_telegraph_without_token_downgrades_only_unuploaded_jobs() {
    for phase in [
        JOB_STATUS_PENDING,
        JOB_STATUS_DOWNLOADED,
        TELEGRAPH_STATUS_READY,
    ] {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_download(
                -100,
                91,
                "tok",
                "Title",
                true,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let initial_job = job_for_delivery(&repo, &model).await;
        assert_eq!(initial_job.status, JOB_STATUS_PENDING);
        assert_eq!(initial_job.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
        if phase != JOB_STATUS_PENDING {
            let download = repo
                .claim_eh_download_job(Main, true)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(download.id, initial_job.id);
            repo.mark_eh_job_downloaded(
                Main,
                download.id,
                download.started_at.unwrap(),
                5000,
                "/tmp/91.zip",
                0,
            )
            .await
            .unwrap();
        }
        let uploaded = phase == TELEGRAPH_STATUS_READY;
        if uploaded {
            let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
            assert_eq!(upload.id, initial_job.id);
            repo.mark_eh_job_telegraph_ready(
                upload.id,
                upload.started_at.unwrap(),
                "https://example.com/gallery",
                None,
                None,
                true,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            repo.disable_eh_telegraph_for_unuploaded_jobs()
                .await
                .unwrap(),
            u64::from(!uploaded),
            "{phase}"
        );
        let delivery = Entity::find_by_id(model.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.status, STATUS_WAITING);
        assert_eq!(delivery.telegraph, uploaded, "{phase}");
        assert!(delivery.telegraph_url.is_none());
        assert!(delivery.telegraph_subscription_ids.is_none());
        let job = job_for_delivery(&repo, &delivery).await;
        if uploaded {
            assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_READY);
            assert_eq!(
                job.telegraph_url.as_deref(),
                Some("https://example.com/gallery")
            );
        } else {
            assert!(!job.telegraph_required);
            assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
        }
        if phase == JOB_STATUS_PENDING {
            let download = repo
                .claim_eh_download_job(Main, true)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(download.id, job.id);
            repo.mark_eh_job_downloaded(
                Main,
                download.id,
                download.started_at.unwrap(),
                5000,
                "/tmp/91.zip",
                0,
            )
            .await
            .unwrap();
            assert!(
                repo.get_next_eh_job_for_upload().await.unwrap().is_none(),
                "downgraded work must not become upload-claimable after download"
            );
        }
    }
}

#[tokio::test]
async fn shared_enqueue_is_atomic_across_job_and_delivery_unique_constraints() {
    let repo = std::sync::Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let variant = EhGalleryVariant::archive("980x");

    let (first, second, same_chat_upgrade) = tokio::join!(
        repo.enqueue_eh_download(
            -100,
            700,
            "token",
            "Gallery",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        ),
        repo.enqueue_eh_download(
            -200,
            700,
            "token",
            "Gallery",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        ),
        repo.enqueue_eh_download(
            -100,
            700,
            "token",
            "Gallery",
            true,
            SOURCE_DIRECT,
            &variant,
            None,
            true
        ),
    );
    let first = first.unwrap().expect("delivery should be enqueued");
    let second = second.unwrap().expect("delivery should be enqueued");
    let same_chat_upgrade = same_chat_upgrade
        .unwrap()
        .expect("delivery should be enqueued");

    assert_eq!(first.job_id, second.job_id);
    assert_eq!(first.id, same_chat_upgrade.id);
    assert!(same_chat_upgrade.telegraph);
    assert_eq!(
        eh_gallery_jobs::Entity::find()
            .filter(eh_gallery_jobs::Column::Gid.eq(700))
            .count(repo.db())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        Entity::find()
            .filter(Column::Gid.eq(700))
            .count(repo.db())
            .await
            .unwrap(),
        2
    );
}
