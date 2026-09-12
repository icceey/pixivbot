use super::*;

#[tokio::test]
async fn orphan_cleanup_retries_after_startup_failure_without_restart() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let temp = tempfile::tempdir().unwrap();
    let cache_dir = temp.path().join("eh_cache");
    std::fs::create_dir(&cache_dir).unwrap();
    let artifacts = seed_archive_artifact_family(&cache_dir.join("orphan.zip"));
    let uploader = Arc::new(TerminalCleanupMockUploader {
        fail_abort: true,
        ..Default::default()
    });
    repo.cleanup_eh_cache_orphans(&cache_dir, Some(uploader.as_ref()), true)
        .await
        .unwrap();
    uploader.cleanup_attempted.notified().await;

    let server = MockServer::start().await;
    let worker = EhDownloadWorker::new(
        repo,
        make_eh_client(&server),
        Arc::new(EhentaiConfig::default()),
        temp.path().to_path_buf(),
        Main,
        Some(uploader.clone()),
    );
    // Pause only around the maintenance timer, not SQLite's connection setup
    // or filesystem I/O, which run on real threads.
    tokio::time::pause();
    let worker = tokio::spawn(worker.run());
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    let retried = tokio::time::timeout(
        Duration::from_secs(5),
        uploader.cleanup_attempted.notified(),
    )
    .await;
    worker.abort();
    let _ = worker.await;
    retried.expect("the running worker must retry a failed startup orphan cleanup");
    assert_eq!(uploader.cleanup_calls.lock().unwrap().len(), 2);
    assert!(artifacts.uploads_dir().join("archive.json").exists());
}

#[tokio::test]
async fn shared_zip_survives_first_delivery_and_is_removed_after_final_consumer() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("shared-consumers.zip");
    create_test_zip(&zip_path, 1);
    let (job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        881,
        "consumers",
        "Shared consumers",
        &zip_path,
        &[(-100, false, "First"), (-200, false, "Second")],
    )
    .await;

    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(DELIVERY_STATUS_DONE),
        )
        .filter(eh_download_queue::Column::Id.eq(deliveries[0].id))
        .exec(repo.db())
        .await
        .unwrap();
    repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
    let after_first = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_first.cleanup_status, CLEANUP_STATUS_NONE);
    assert!(
        zip_path.exists(),
        "the second active consumer still owns the ZIP"
    );

    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(DELIVERY_STATUS_DONE),
        )
        .filter(eh_download_queue::Column::Id.eq(deliveries[1].id))
        .exec(repo.db())
        .await
        .unwrap();
    repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
    let scheduled = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(scheduled.cleanup_status, CLEANUP_STATUS_PENDING);
    assert!(
        zip_path.exists(),
        "liveness schedules but never removes artifacts"
    );

    assert_eq!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), None, 1, true)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::CleanRetired)
    );
    assert!(
        !zip_path.exists(),
        "maintenance removes the final consumer ZIP"
    );
}

#[tokio::test]
async fn missing_zip_cancellation_recovers_cleanup_before_reenqueue() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let temp_dir = tempfile::tempdir().unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    repo.enqueue_eh_subscription_download(
        -100,
        555,
        882,
        "abort",
        "Abort first",
        false,
        &variant,
        None,
        true,
    )
    .await
    .unwrap()
    .expect("delivery should be enqueued");
    let downloaded = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    let zip_path = archive_artifacts_for_job(temp_dir.path(), &downloaded)
        .final_zip()
        .to_path_buf();
    std::fs::create_dir_all(zip_path.parent().unwrap()).unwrap();
    create_test_zip(&zip_path, 1);
    repo.mark_eh_job_downloaded(
        Main,
        downloaded.id,
        downloaded.started_at.unwrap(),
        10,
        &zip_path.to_string_lossy(),
        0,
    )
    .await
    .unwrap();
    let artifacts = seed_archive_artifact_family(&zip_path);
    std::fs::remove_file(&zip_path).unwrap();
    repo.reset_eh_job_for_missing_zip(
        downloaded.id,
        downloaded.started_at.unwrap(),
        zip_path.to_str().unwrap(),
        3,
    )
    .await
    .unwrap();
    repo.cancel_eh_subscription_queue_entries(555, true)
        .await
        .unwrap();
    // The reset lost zip_path, so cancellation alone could not enqueue cleanup.
    // The scan must reclaim the remaining family before a new delivery arrives.
    repo.cleanup_eh_cache_orphans(zip_path.parent().unwrap(), None, true)
        .await
        .unwrap();

    let rebound = repo
        .enqueue_eh_download(
            -200,
            882,
            "abort",
            "Abort first",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(rebound.job_id, Some(downloaded.id));
    let failing_uploader = TerminalCleanupMockUploader {
        fail_abort: true,
        ..Default::default()
    };
    assert!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(&failing_uploader), 0, true)
            .await
            .is_err()
    );
    assert_terminal_cleanup_precedes_local_removal(&failing_uploader, &artifacts);
    let failed = eh_gallery_jobs::Entity::find_by_id(downloaded.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.cleanup_status, CLEANUP_STATUS_FAILED);
    assert!(failed.cleanup_next_retry_at.is_some());
    assert!(artifacts.uploads_dir().join("archive.json").exists());
    assert!(repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .is_none());

    let successful_uploader = TerminalCleanupMockUploader::default();
    assert_eq!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(&successful_uploader), 0, true)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::ReactivatedPending)
    );
    assert!(!artifacts.uploads_dir().exists());
    let replacement = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        replacement.id,
        replacement.started_at.unwrap(),
        20,
        "/tmp/replacement.zip",
        0,
    )
    .await
    .unwrap();
    assert_eq!(
        eh_download_completions::Entity::find()
            .filter(eh_download_completions::Column::JobId.eq(downloaded.id))
            .count(repo.db())
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn download_tick_continues_after_due_cleanup_abort_failure() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let dirty_zip = temp.path().join("dirty-cleanup.zip");
    create_test_zip(&dirty_zip, 1);
    let artifacts = seed_archive_artifact_family(&dirty_zip);
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;
    setup_chat(&repo, -300, true).await;

    let dirty_delivery = repo
        .enqueue_eh_download(
            -100,
            883,
            "dirty",
            "Dirty cleanup",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let dirty_claim = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        dirty_claim.id,
        dirty_claim.started_at.unwrap(),
        10,
        &dirty_zip.to_string_lossy(),
        0,
    )
    .await
    .unwrap();
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(STATUS_CANCELED),
        )
        .filter(eh_download_queue::Column::Id.eq(dirty_delivery.id))
        .exec(repo.db())
        .await
        .unwrap();
    repo.evaluate_eh_job_liveness(dirty_claim.id, true)
        .await
        .unwrap();

    let first = repo
        .enqueue_eh_download(
            -200,
            884,
            "abcd000001",
            "First valid job",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    mock_archiver(
        &eh_server,
        884,
        "abcd000001",
        ArchiverPage {
            keyed: true,
            ..Default::default()
        },
    )
    .await;
    let first_download_url = format!("{}/archive/884/token/0", eh_server.uri());
    mock_eh_archiver_post(&eh_server, &first_download_url).await;
    let first_source_zip = temp.path().join("first-source.zip");
    create_test_zip(&first_source_zip, 1);
    mock_eh_archive_download(
        &eh_server,
        "/archive/884/token/0",
        std::fs::read(&first_source_zip).unwrap(),
    )
    .await;

    let failing_uploader = Arc::new(TerminalCleanupMockUploader {
        fail_abort: true,
        ..Default::default()
    });
    let mut config = make_config();
    config.background_download_enabled = false;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Main,
        Some(failing_uploader.clone()),
    );

    worker.tick().await.unwrap();
    let first_job = job_for_delivery(&repo, &first).await;
    assert_eq!(
        first_job.status, STATUS_DOWNLOADED,
        "unrelated job must complete after cleanup failure: {first_job:#?}"
    );

    let second = repo
        .enqueue_eh_download(
            -300,
            885,
            "abcd000002",
            "Second valid job",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    mock_archiver(
        &eh_server,
        885,
        "abcd000002",
        ArchiverPage {
            keyed: true,
            ..Default::default()
        },
    )
    .await;
    let second_download_url = format!("{}/archive/885/token/0", eh_server.uri());
    mock_eh_archiver_post(&eh_server, &second_download_url).await;
    let second_source_zip = temp.path().join("second-source.zip");
    create_test_zip(&second_source_zip, 1);
    mock_eh_archive_download(
        &eh_server,
        "/archive/885/token/0",
        std::fs::read(&second_source_zip).unwrap(),
    )
    .await;
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::CleanupNextRetryAt,
            Expr::value(Some(
                Local::now().naive_local() - chrono::Duration::seconds(1),
            )),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(dirty_claim.id))
        .exec(repo.db())
        .await
        .unwrap();

    worker.tick().await.unwrap();
    assert_eq!(
        job_for_delivery(&repo, &second).await.status,
        STATUS_DOWNLOADED
    );

    let dirty_job = eh_gallery_jobs::Entity::find_by_id(dirty_claim.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dirty_job.cleanup_status, CLEANUP_STATUS_FAILED);
    assert!(dirty_job.cleanup_next_retry_at.is_some());
    assert!(dirty_job.cleanup_error.is_some());
    assert_eq!(
        *failing_uploader.cleanup_calls.lock().unwrap(),
        vec![
            (artifacts.uploads_dir().to_path_buf(), true),
            (artifacts.uploads_dir().to_path_buf(), true),
        ],
        "each due cleanup must Abort before preserving local artifacts"
    );
    assert!(artifacts.final_zip().exists());
    assert!(artifacts.uploads_dir().join("archive.json").exists());
    assert!(
        repo.claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .is_none(),
        "failed cleanup remains nonclaimable after unrelated jobs progress"
    );
}

#[tokio::test]
async fn stale_consumerless_upload_keeps_owned_family_through_abort_failure() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("stale-upload-owned.zip");
    create_test_zip(&zip_path, 1);
    let artifacts = seed_archive_artifact_family(&zip_path);
    let variant = EhGalleryVariant::archive("1280x");
    let canceled = repo
        .enqueue_eh_subscription_download(
            -100,
            8821,
            8822,
            "stale-upload",
            "Stale upload",
            true,
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
        10,
        &zip_path.to_string_lossy(),
        0,
    )
    .await
    .unwrap();
    let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    repo.cancel_eh_subscription_queue_entries(8821, true)
        .await
        .unwrap();
    assert_eq!(
        eh_gallery_jobs::Entity::find_by_id(upload.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .cleanup_status,
        CLEANUP_STATUS_NONE
    );

    assert_eq!(
        repo.reset_stale_eh_shared_work(60, 60)
            .await
            .unwrap()
            .uploads,
        1
    );
    let stale = eh_gallery_jobs::Entity::find_by_id(upload.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stale.telegraph_status, TELEGRAPH_STATUS_NOT_REQUIRED);
    assert_eq!(stale.cleanup_status, CLEANUP_STATUS_PENDING);
    assert_eq!(stale.zip_path.as_deref(), zip_path.to_str());

    let rebound = repo
        .enqueue_eh_download(
            -200,
            8822,
            "stale-upload",
            "Stale upload",
            true,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(rebound.job_id, Some(upload.id));
    assert!(repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .is_none());
    assert!(repo
        .get_next_eh_delivery_for_publish(false)
        .await
        .unwrap()
        .is_none());

    let failing_uploader = TerminalCleanupMockUploader {
        fail_abort: true,
        ..Default::default()
    };
    assert!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(&failing_uploader), 0, true)
            .await
            .is_err()
    );
    let failed = eh_gallery_jobs::Entity::find_by_id(upload.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.cleanup_status, CLEANUP_STATUS_FAILED);
    assert_eq!(failed.zip_path.as_deref(), zip_path.to_str());
    assert!(artifacts.final_zip().exists());
    assert!(artifacts.uploads_dir().exists());
    assert!(repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .is_none());

    let successful_uploader = TerminalCleanupMockUploader::default();
    assert_eq!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(&successful_uploader), 0, true,)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::ReactivatedPending)
    );
    assert!(!artifacts.final_zip().exists());
    assert!(!artifacts.uploads_dir().exists());
    assert_eq!(
        repo.claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap()
            .id,
        upload.id
    );
    assert_eq!(canceled.job_id, Some(upload.id));
}

#[tokio::test]
async fn normal_late_completion_keeps_owned_family_until_cleanup_reactivates() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let temp_dir = tempfile::tempdir().unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let first = repo
        .enqueue_eh_download(
            -100,
            883,
            "normal-late",
            "Normal late completion",
            false,
            SOURCE_DIRECT,
            &variant,
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
    let zip_path = archive_artifacts_for_job(temp_dir.path(), &claimed)
        .final_zip()
        .to_path_buf();
    std::fs::create_dir_all(zip_path.parent().unwrap()).unwrap();
    create_test_zip(&zip_path, 1);
    let artifacts = seed_archive_artifact_family(&zip_path);
    assert!(repo
        .persist_eh_job_archive_artifact_ownership(
            claimed.id,
            claimed.started_at.unwrap(),
            &zip_path.to_string_lossy(),
            false,
        )
        .await
        .unwrap());
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(STATUS_CANCELED),
        )
        .filter(eh_download_queue::Column::Id.eq(first.id))
        .exec(repo.db())
        .await
        .unwrap();

    repo.evaluate_eh_job_liveness(claimed.id, true)
        .await
        .unwrap();
    let in_flight = job_for_delivery(&repo, &first).await;
    assert_eq!(
        in_flight.status, JOB_STATUS_DOWNLOADING,
        "cancellation must not retire an in-flight normal writer"
    );
    assert_eq!(
        in_flight.cleanup_status, CLEANUP_STATUS_NONE,
        "cleanup must not race a normal writer"
    );

    repo.mark_eh_job_downloaded(
        Main,
        claimed.id,
        claimed.started_at.unwrap(),
        10,
        &zip_path.to_string_lossy(),
        0,
    )
    .await
    .unwrap();
    let settled = job_for_delivery(&repo, &first).await;
    assert_eq!(settled.cleanup_status, CLEANUP_STATUS_PENDING);

    let rebound = repo
        .enqueue_eh_download(
            -200,
            883,
            "normal-late",
            "Normal late completion",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(rebound.job_id, Some(claimed.id));
    assert!(repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .is_none());

    let uploader = TerminalCleanupMockUploader::default();
    assert_eq!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(&uploader), 0, true)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::ReactivatedPending)
    );
    assert!(!artifacts.final_zip().exists());
    assert!(!artifacts.assembly_scratch().exists());
    assert!(!artifacts.parts_dir().exists());
    assert!(!artifacts.uploads_dir().exists());
    assert_eq!(
        repo.claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap()
            .id,
        claimed.id
    );
}

#[tokio::test]
async fn background_late_completion_keeps_owned_family_until_cleanup_reactivates() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let temp_dir = tempfile::tempdir().unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let first = repo
        .enqueue_eh_download(
            -100,
            884,
            "background-late",
            "Background late completion",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let normal_claim = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.schedule_eh_job_background_download(
        normal_claim.id,
        normal_claim.status.as_str(),
        "test handoff",
    )
    .await
    .unwrap();
    let claimed = repo
        .claim_eh_download_job(Background, true)
        .await
        .unwrap()
        .unwrap();
    let zip_path = archive_artifacts_for_job(temp_dir.path(), &claimed)
        .final_zip()
        .to_path_buf();
    std::fs::create_dir_all(zip_path.parent().unwrap()).unwrap();
    create_test_zip(&zip_path, 1);
    let artifacts = seed_archive_artifact_family(&zip_path);
    assert!(repo
        .persist_eh_job_archive_artifact_ownership(
            claimed.id,
            claimed.started_at.unwrap(),
            &zip_path.to_string_lossy(),
            true,
        )
        .await
        .unwrap());
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(STATUS_CANCELED),
        )
        .filter(eh_download_queue::Column::Id.eq(first.id))
        .exec(repo.db())
        .await
        .unwrap();

    repo.evaluate_eh_job_liveness(claimed.id, true)
        .await
        .unwrap();
    let in_flight = job_for_delivery(&repo, &first).await;
    assert_eq!(
        in_flight.status, JOB_STATUS_PENDING,
        "cancellation must not retire an in-flight background writer"
    );
    assert_eq!(
        in_flight.background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_RUNNING)
    );
    assert_eq!(
        in_flight.cleanup_status, CLEANUP_STATUS_NONE,
        "cleanup must not race a background writer"
    );

    repo.mark_eh_job_downloaded(
        Background,
        claimed.id,
        claimed.started_at.unwrap(),
        10,
        &zip_path.to_string_lossy(),
        0,
    )
    .await
    .unwrap();
    let settled = job_for_delivery(&repo, &first).await;
    assert_eq!(settled.status, JOB_STATUS_RETIRED);
    assert_eq!(settled.cleanup_status, CLEANUP_STATUS_PENDING);
    assert_eq!(
        eh_download_queue::Entity::find_by_id(first.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .status,
        STATUS_CANCELED
    );

    let rebound = repo
        .enqueue_eh_download(
            -200,
            884,
            "background-late",
            "Background late completion",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(rebound.job_id, Some(claimed.id));
    assert!(repo
        .claim_eh_download_job(Background, true)
        .await
        .unwrap()
        .is_none());

    let uploader = TerminalCleanupMockUploader::default();
    assert_eq!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(&uploader), 0, true)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::ReactivatedPending)
    );
    assert!(!artifacts.final_zip().exists());
    assert!(!artifacts.assembly_scratch().exists());
    assert!(!artifacts.parts_dir().exists());
    assert!(!artifacts.uploads_dir().exists());
    assert_eq!(
        repo.claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap()
            .id,
        claimed.id
    );
}
