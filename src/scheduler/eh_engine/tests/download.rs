use super::*;

#[tokio::test]
async fn two_chats_share_one_download_purchase_artifact_and_completion() {
    for background in [false, true] {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let variant = EhGalleryVariant::archive("1280x");

        setup_chat(&repo, -100, true).await;
        setup_chat(&repo, -200, true).await;
        let first = repo
            .enqueue_eh_download(
                -100,
                123456,
                "abcdef0123",
                "Shared paid gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let second = repo
            .enqueue_eh_download(
                -200,
                123456,
                "abcdef0123",
                "Shared paid gallery",
                false,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        assert_eq!(first.job_id, second.job_id);

        mock_archiver(
            &eh_server,
            123456,
            "abcdef0123",
            ArchiverPage {
                original_cost: "218 GP",
                resample_cost: "218 GP",
                ..Default::default()
            },
        )
        .await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;
        let zip_temp = tempfile::tempdir().unwrap();
        let source_zip = zip_temp.path().join("shared.zip");
        create_test_zip(&source_zip, 1);
        let zip_bytes = std::fs::read(source_zip).unwrap();
        mock_eh_archive_download(&eh_server, "/archive/123456/token/0", zip_bytes.clone()).await;

        let mut config = make_config();
        config.max_archive_gp_cost = 218;
        config.background_download_enabled = background;
        config.background_download_concurrency = 2;
        let queue = if background {
            handoff_job_to_background(&repo, &first).await;
            Background
        } else {
            Main
        };
        EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            queue,
            None,
        )
        .tick()
        .await
        .unwrap();

        let job_id = first.job_id.unwrap();
        let job = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, "downloaded");
        assert_eq!(job.gp_cost, 218);
        assert!(std::path::Path::new(job.zip_path.as_deref().unwrap()).exists());
        let deliveries = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::JobId.eq(job_id))
            .all(repo.db())
            .await
            .unwrap();
        assert_eq!(deliveries.len(), 2);
        assert!(deliveries
            .iter()
            .all(|delivery| delivery.status == "waiting"));
        let attempts = gp_attempts(repo.as_ref()).await;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].job_id, Some(job_id));
        assert_eq!(attempts[0].queue_id, None);
        let completions = eh_download_completions::Entity::find()
            .filter(eh_download_completions::Column::JobId.eq(job_id))
            .all(repo.db())
            .await
            .unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(
            eh_server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|request| {
                    request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
                })
                .count(),
            1
        );
        assert_eq!(completions[0].job_id, Some(job.id));
        assert_eq!(
            repo.get_eh_downloaded_bytes_in_window(24).await.unwrap(),
            completions[0].file_size
        );
        assert_eq!(
            completions[0].file_size,
            i64::try_from(zip_bytes.len()).unwrap()
        );
    }
}

#[tokio::test]
async fn test_download_worker_rate_limit_skips() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, true).await;

    // Pre-fill the completion ledger to hit the shared rate limit.
    let now = Local::now().naive_local();
    eh_download_completions::ActiveModel {
        job_id: Set(None),
        gid: Set(999999),
        file_size: Set(11_000_000_000),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(repo.db())
    .await
    .unwrap();

    let entry = seed_delivery(
        &repo,
        -100,
        (123456, "abcdef0123", "Test"),
        Default::default(),
    )
    .await;

    let mut config = make_config();
    config.background_download_enabled = false;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Main,
        None,
    );
    worker.tick().await.unwrap();

    let updated = job_for_delivery(&repo, &entry).await;
    assert_eq!(
        updated.status, "pending",
        "should remain pending due to rate limit"
    );
    assert_eq!(updated.retry_count, 0);
    assert!(eh_server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_download_worker_chat_disabled_schedules_retry() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, false).await; // disabled
    let entry = seed_delivery(
        &repo,
        -100,
        (123456, "abcdef0123", "Test"),
        Default::default(),
    )
    .await;

    let mut config = make_config();
    config.background_download_enabled = false;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Main,
        None,
    );
    worker.tick().await.unwrap();

    let updated = job_for_delivery(&repo, &entry).await;
    // Chat disabled → shared job goes back to pending with retry scheduled.
    assert_eq!(
        updated.status, "pending",
        "should be pending for retry, not silently done"
    );
    assert_eq!(
        updated.retry_count, 0,
        "chat disabled defer should not increment retry_count"
    );
    assert!(
        updated.next_retry_at.is_some(),
        "should have next_retry_at set"
    );
}

#[tokio::test]
async fn test_download_worker_failure_schedules_retry() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, true).await;
    let entry = seed_delivery(
        &repo,
        -100,
        (123456, "abcdef0123", "Test"),
        Default::default(),
    )
    .await;

    mock_archiver(
        &eh_server,
        123456,
        "abcdef0123",
        ArchiverPage {
            keyed: true,
            ..Default::default()
        },
    )
    .await;
    // archiver.php POST returns 500
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&eh_server)
        .await;

    let mut config = make_config();
    config.background_download_enabled = false;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Main,
        None,
    );
    worker.tick().await.unwrap();

    let updated = job_for_delivery(&repo, &entry).await;
    assert_eq!(
        updated.status, "pending",
        "should be back to pending for retry"
    );
    assert_eq!(updated.retry_count, 1);
    assert!(
        updated.next_retry_at.is_some(),
        "should have next_retry_at set"
    );
}

#[tokio::test]
async fn download_workers_schedule_durable_cleanup_after_permanent_failure() {
    for background in [false, true] {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = seed_delivery(
            &repo,
            -100,
            (123456, "abcdef0123", "Test"),
            Default::default(),
        )
        .await;

        let job = job_for_delivery(&repo, &entry).await;
        if !background {
            eh_gallery_jobs::Entity::update_many()
                .col_expr(
                    eh_gallery_jobs::Column::RetryCount,
                    Expr::value(make_config().max_retry_count as i32),
                )
                .filter(eh_gallery_jobs::Column::Id.eq(job.id))
                .exec(repo.db())
                .await
                .unwrap();
        }

        let eh_cache = temp.path().join("eh_cache");
        std::fs::create_dir_all(&eh_cache).unwrap();
        let zip_path = archive_artifacts_for_job(temp.path(), &job)
            .final_zip()
            .to_path_buf();
        let part_path = zip_path.with_extension("zip.part");
        let parts_dir = zip_path.with_extension("zip.parts");
        std::fs::write(&zip_path, b"PK\x03\x04stale").unwrap();
        std::fs::write(&part_path, b"PK\x03\x04partial").unwrap();
        std::fs::create_dir_all(parts_dir.join("nested")).unwrap();
        std::fs::write(parts_dir.join("manifest.json"), b"manifest").unwrap();
        std::fs::write(parts_dir.join("nested").join("part-0001"), b"part").unwrap();

        mock_archiver(
            &eh_server,
            123456,
            "abcdef0123",
            ArchiverPage {
                keyed: true,
                ..Default::default()
            },
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_max_attempts = 1;
        config.background_download_enabled = background;
        config.background_download_concurrency = 2;
        let queue = if background {
            handoff_job_to_background(&repo, &entry).await;
            Background
        } else {
            Main
        };
        EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
            queue,
            None,
        )
        .tick()
        .await
        .unwrap();

        let updated = job_for_delivery(&repo, &entry).await;
        assert_eq!(updated.status, JOB_STATUS_RETIRED);
        assert_eq!(updated.cleanup_status, CLEANUP_STATUS_PENDING);
        assert!(
            !zip_path.exists(),
            "invalid final ZIP is discarded as a cache miss before the fresh prepare"
        );
        assert!(
            part_path.exists() && parts_dir.exists(),
            "durable cleanup owns the remaining partial artifacts before local removal"
        );
        assert_eq!(
            run_eh_job_cleanup_maintenance_once(repo.as_ref(), None, 0, true)
                .await
                .unwrap(),
            Some(EhCleanupFinalizeOutcome::CleanRetired)
        );
        assert!(!zip_path.exists(), "final ZIP should be cleaned");
        assert!(!part_path.exists(), "partial ZIP should be cleaned");
        assert!(
            !parts_dir.exists(),
            "multipart parts directory should be removed recursively"
        );
        assert_eq!(updated.background_download_status, None);
    }
}

#[tokio::test]
async fn test_download_worker_progress_failure_defers_without_retry() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, true).await;
    let entry = seed_delivery(
        &repo,
        -100,
        (123456, "abcdef0123", "Test"),
        Default::default(),
    )
    .await;
    let job = job_for_delivery(&repo, &entry).await;

    mock_archiver(
        &eh_server,
        123456,
        "abcdef0123",
        ArchiverPage {
            keyed: true,
            ..Default::default()
        },
    )
    .await;
    let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
    mock_eh_archiver_post(&eh_server, &download_url).await;

    // Pre-seed 1-byte .part so the 206 response takes the append path and
    // runs validate_content_range. Content-Range start=1 matches existing_len=1.
    let eh_cache = temp.path().join("eh_cache");
    tokio::fs::create_dir_all(&eh_cache).await.unwrap();
    let part_path = archive_artifacts_for_job(temp.path(), &job)
        .final_zip()
        .with_extension("zip.part");
    tokio::fs::write(&part_path, b"x").await.unwrap();

    // 206 with valid Content-Range (start=1==existing_len, end+1==total → validate passes)
    // but body smaller than claimed (>10KB) → written < expected_total → error
    // after writing >10KB → made_progress=true → DownloadInProgress.
    // Note: the mock returns the same fixed Content-Range on every attempt. After the
    // first append the start no longer matches existing_len, so validate_content_range
    // fails before writing further bytes; only the first attempt appends 20000 bytes.
    let partial_body = vec![0u8; 20000];
    Mock::given(method("GET"))
        .and(path("/archive/123456/token/0"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 1-99999/100000")
                .set_body_bytes(partial_body.clone()),
        )
        // 4 attempts per ARCHIVE_DOWNLOAD_MAX_ATTEMPTS
        .expect(4)
        .mount(&eh_server)
        .await;

    let mut config = make_config();
    config.background_download_enabled = false;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Main,
        None,
    );
    worker.tick().await.unwrap();

    let updated = job_for_delivery(&repo, &entry).await;
    assert_eq!(
        updated.status, STATUS_PENDING,
        "should be pending for deferred retry"
    );
    assert_eq!(
        updated.retry_count, 0,
        "DownloadInProgress should NOT increment retry_count"
    );
    assert!(
        updated.next_retry_at.is_some(),
        "should have next_retry_at set by defer_eh_job_download"
    );

    // .part file should be preserved for resumption.
    assert!(
        part_path.exists(),
        ".part file should be preserved for resumption"
    );
    let part_size = std::fs::metadata(&part_path).unwrap().len();
    assert_eq!(
        part_size, 20001,
        ".part should contain 20001 bytes (1 pre-seeded + 20000 written on first attempt), got {}",
        part_size
    );
}

#[tokio::test]
async fn test_download_worker_slow_progress_hands_off_shared_job_to_background() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, true).await;
    let entry = seed_delivery(
        &repo,
        -100,
        (123456, "abcdef0123", "Test"),
        Default::default(),
    )
    .await;
    let job = job_for_delivery(&repo, &entry).await;

    mock_archiver(
        &eh_server,
        123456,
        "abcdef0123",
        ArchiverPage {
            keyed: true,
            ..Default::default()
        },
    )
    .await;
    let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
    mock_eh_archiver_post(&eh_server, &download_url).await;

    let eh_cache = temp.path().join("eh_cache");
    tokio::fs::create_dir_all(&eh_cache).await.unwrap();
    let part_path = archive_artifacts_for_job(temp.path(), &job)
        .final_zip()
        .with_extension("zip.part");
    tokio::fs::write(&part_path, b"x").await.unwrap();

    Mock::given(method("GET"))
        .and(path("/archive/123456/token/0"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 1-99999/100000")
                .set_body_bytes(vec![0u8; 20000]),
        )
        .expect(4)
        .mount(&eh_server)
        .await;

    let mut config = make_config();
    config.background_download_enabled = true;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Main,
        None,
    );
    worker.tick().await.unwrap();

    let updated = job_for_delivery(&repo, &entry).await;
    assert_eq!(updated.status, STATUS_PENDING);
    assert_eq!(updated.retry_count, 0);
    assert_eq!(
        updated.background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_PENDING)
    );
    assert!(updated.background_download_next_retry_at.is_some());
    assert!(updated.next_retry_at.is_none());
    assert!(part_path.exists());
}

#[tokio::test]
async fn test_download_size_limit_blocks_oversized_selected_archive_before_post() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let eh_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();

    mock_archiver(
        &eh_server,
        900,
        "abcdef0123",
        ArchiverPage {
            sizes: Some(("400.0 MiB", "300.01 MiB")),
            ..Default::default()
        },
    )
    .await;
    // Any POST to /archiver.php (the paid archive request) must never happen.
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("unexpected"))
        .expect(0)
        .mount(&eh_server)
        .await;

    let mut cfg = make_config();
    cfg.max_archive_size_mb = 300;
    cfg.max_retry_count = 0;
    let entry = seed_delivery(
        &repo,
        -100,
        (900, "abcdef0123", "Title"),
        Default::default(),
    )
    .await;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(cfg),
        temp_dir.path().to_path_buf(),
        Main,
        None,
    );

    worker.tick().await.unwrap();

    let model = job_for_delivery(&repo, &entry).await;
    assert_eq!(model.status, JOB_STATUS_RETIRED);
    assert_eq!(model.cleanup_status, CLEANUP_STATUS_PENDING);
    assert!(
        model
            .error
            .as_ref()
            .is_some_and(|e| e.contains("selected EH archive size is too large")),
        "error should mention the configured limit, got: {:?}",
        model.error
    );
    assert_eq!(
        eh_server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.method.as_str() == "POST" && request.url.path() == "/api.php")
            .count(),
        0,
        "selected archive-size checks must not request gallery metadata"
    );
}

#[tokio::test]
async fn test_download_size_limit_allows_small_selected_resample_without_metadata() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let eh_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();

    // The selected 1280x archive is small even though the original archive
    // estimate is above the configured limit. No gallery metadata route is
    // mounted: a metadata request is a regression.
    mock_archiver(
        &eh_server,
        903,
        "abcdef0123",
        ArchiverPage {
            sizes: Some(("301.0 MiB", "2.33 MiB")),
            ..Default::default()
        },
    )
    .await;
    let download_url = format!("{}/archive/903/token/0", eh_server.uri());
    mock_eh_archiver_post(&eh_server, &download_url).await;
    let zip_temp = tempfile::tempdir().unwrap();
    let zip_path = zip_temp.path().join("small_resample.zip");
    create_test_zip(&zip_path, 2);
    mock_eh_archive_download(
        &eh_server,
        "/archive/903/token/0",
        std::fs::read(zip_path).unwrap(),
    )
    .await;

    let mut cfg = make_config();
    cfg.max_archive_size_mb = 300;
    cfg.background_download_enabled = false;
    let entry = seed_delivery(
        &repo,
        -100,
        (903, "abcdef0123", "Title"),
        Default::default(),
    )
    .await;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(cfg),
        temp_dir.path().to_path_buf(),
        Main,
        None,
    );

    worker.tick().await.unwrap();

    let model = job_for_delivery(&repo, &entry).await;
    assert_eq!(model.status, STATUS_DOWNLOADED);
    let requests = eh_server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method.as_str() == "POST"
                && request.url.path() == "/archiver.php")
            .count(),
        1,
        "the prepared selected archive should be posted"
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.method.as_str() == "POST" && request.url.path() == "/api.php"),
        "selected archive-size checks must not request gallery metadata"
    );
}

#[tokio::test]
async fn test_download_size_limit_allows_equal_size() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let eh_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();

    // A selected archive equal to the limit must be allowed (strict `>` rejects).
    mock_archiver(
        &eh_server,
        901,
        "abcdef0123",
        ArchiverPage {
            sizes: Some(("400.0 MiB", "300.0 MiB")),
            ..Default::default()
        },
    )
    .await;
    let download_url = format!("{}/archive/901/token/0", eh_server.uri());
    mock_eh_archiver_post(&eh_server, &download_url).await;

    let zip_temp = tempfile::tempdir().unwrap();
    let zip_path = zip_temp.path().join("equal_size.zip");
    create_test_zip(&zip_path, 2);
    let zip_bytes = std::fs::read(&zip_path).unwrap();
    mock_eh_archive_download(&eh_server, "/archive/901/token/0", zip_bytes).await;

    let mut cfg = make_config();
    cfg.max_archive_size_mb = 300;
    cfg.background_download_enabled = false;
    let entry = seed_delivery(
        &repo,
        -100,
        (901, "abcdef0123", "Title"),
        Default::default(),
    )
    .await;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(cfg),
        temp_dir.path().to_path_buf(),
        Main,
        None,
    );

    worker.tick().await.unwrap();

    let model = job_for_delivery(&repo, &entry).await;
    assert_eq!(model.status, STATUS_DOWNLOADED);
    assert!(model.zip_path.is_some());
    assert!(model.file_size > 0);
}

#[tokio::test]
async fn download_workers_resume_saved_artifacts_without_another_purchase() {
    for background in [false, true] {
        for complete_zip in [false, true] {
            let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
            let eh_server = MockServer::start().await;
            let temp = tempfile::tempdir().unwrap();
            setup_chat(&repo, -100, true).await;
            let entry = seed_delivery(
                &repo,
                -100,
                (4053259, "persisted-manifest", "Resumed Gallery"),
                Default::default(),
            )
            .await;
            let job = job_for_delivery(&repo, &entry).await;
            let artifacts = archive_artifacts_for_job(temp.path(), &job);
            let zip_temp = tempfile::tempdir().unwrap();
            let source_zip = zip_temp.path().join("source.zip");
            create_test_zip(&source_zip, 2);
            let zip_bytes = std::fs::read(&source_zip).unwrap();
            if complete_zip {
                std::fs::create_dir_all(artifacts.final_zip().parent().unwrap()).unwrap();
                std::fs::write(artifacts.final_zip(), &zip_bytes).unwrap();
            }
            std::fs::create_dir_all(artifacts.parts_dir()).unwrap();
            std::fs::write(
                artifacts.parts_dir().join("part-0000000000000000"),
                &zip_bytes,
            )
            .unwrap();
            std::fs::write(
                artifacts.parts_dir().join("manifest.json"),
                serde_json::to_vec(&serde_json::json!({
                    "version": 1,
                    "download_url": format!("{}/archive/never-requested", eh_server.uri()),
                    "total_len": zip_bytes.len(),
                    "etag": null,
                    "last_modified": null,
                    "next_part_id": 1,
                    "parts": [{ "id": 0, "start": 0, "end": zip_bytes.len() }]
                }))
                .unwrap(),
            )
            .unwrap();
            let mut config = make_config();
            config.background_download_enabled = background;
            config.background_download_concurrency = 2;
            let queue = if background {
                handoff_job_to_background(&repo, &entry).await;
                Background
            } else {
                Main
            };
            EhDownloadWorker::new(
                Arc::clone(&repo),
                make_eh_client(&eh_server),
                Arc::new(config),
                temp.path().to_path_buf(),
                queue,
                None,
            )
            .tick()
            .await
            .unwrap();

            let updated = job_for_delivery(&repo, &entry).await;
            assert_eq!(updated.status, JOB_STATUS_DOWNLOADED);
            assert!(artifacts.final_zip().exists());
            assert!(!artifacts.parts_dir().exists());
            assert!(
        eh_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| { request.url.path() != "/archiver.php" }),
        "an existing final ZIP must resume without an archiver prepare/POST or GP reservation"
    );
            assert!(gp_attempts(repo.as_ref()).await.is_empty());
        }
    }
}

#[tokio::test]
async fn download_worker_replaces_terminally_rejected_persisted_manifest_in_same_tick() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    setup_chat(&repo, -100, true).await;
    let entry = seed_delivery(
        &repo,
        -100,
        (4053260, "abcdef0123", "Reprepared Gallery"),
        Default::default(),
    )
    .await;
    let job = job_for_delivery(&repo, &entry).await;
    let artifacts = archive_artifacts_for_job(temp.path(), &job);
    let zip_temp = tempfile::tempdir().unwrap();
    let source_zip = zip_temp.path().join("source.zip");
    create_test_zip(&source_zip, 2);
    let zip_bytes = std::fs::read(&source_zip).unwrap();
    let stale_url = format!("{}/archive/stale", eh_server.uri());
    std::fs::create_dir_all(artifacts.parts_dir()).unwrap();
    std::fs::write(artifacts.parts_dir().join("part-0000000000000000"), []).unwrap();
    std::fs::write(
        artifacts.parts_dir().join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "download_url": stale_url,
            "total_len": zip_bytes.len(),
            "etag": null,
            "last_modified": null,
            "next_part_id": 1,
            "parts": [{ "id": 0, "start": 0, "end": zip_bytes.len() }]
        }))
        .unwrap(),
    )
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/archive/stale"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&eh_server)
        .await;
    mock_archiver(
        &eh_server,
        4053260,
        "abcdef0123",
        ArchiverPage {
            original_cost: "Free!",
            resample_cost: "Free!",
            ..Default::default()
        },
    )
    .await;
    let fresh_url = format!("{}/archive/fresh", eh_server.uri());
    mock_eh_archiver_post(&eh_server, &fresh_url).await;
    Mock::given(method("GET"))
        .and(path("/archive/fresh"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
        .expect(1)
        .mount(&eh_server)
        .await;
    let mut config = make_config();
    config.background_download_enabled = false;
    config.archive_download_concurrency = 1;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Main,
        None,
    );

    worker.tick().await.unwrap();

    let updated = job_for_delivery(&repo, &entry).await;
    assert_eq!(
        updated.status, JOB_STATUS_DOWNLOADED,
        "error: {:?}",
        updated.error
    );
    assert!(artifacts.final_zip().exists());
    assert!(!artifacts.parts_dir().exists());
    let requests = eh_server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
            })
            .count(),
        1,
        "a stale persisted URL must prepare and POST a fresh archive in the same tick"
    );
}

#[tokio::test]
async fn download_worker_replaces_invalid_existing_final_zip_in_same_tick() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    setup_chat(&repo, -100, true).await;
    let entry = seed_delivery(
        &repo,
        -100,
        (4053263, "abcdef0123", "Invalid Final ZIP"),
        Default::default(),
    )
    .await;
    let job = job_for_delivery(&repo, &entry).await;
    let artifacts = archive_artifacts_for_job(temp.path(), &job);
    std::fs::create_dir_all(artifacts.final_zip().parent().unwrap()).unwrap();
    std::fs::write(artifacts.final_zip(), b"not a ZIP").unwrap();
    let zip_temp = tempfile::tempdir().unwrap();
    let source_zip = zip_temp.path().join("source.zip");
    create_test_zip(&source_zip, 2);
    let zip_bytes = std::fs::read(&source_zip).unwrap();
    mock_archiver(
        &eh_server,
        4053263,
        "abcdef0123",
        ArchiverPage {
            original_cost: "Free!",
            resample_cost: "Free!",
            ..Default::default()
        },
    )
    .await;
    let fresh_url = format!("{}/archive/fresh", eh_server.uri());
    mock_eh_archiver_post(&eh_server, &fresh_url).await;
    Mock::given(method("GET"))
        .and(path("/archive/fresh"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes.clone()))
        .expect(1)
        .mount(&eh_server)
        .await;
    let mut config = make_config();
    config.background_download_enabled = false;
    config.archive_download_concurrency = 1;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Main,
        None,
    );

    worker.tick().await.unwrap();

    let updated = job_for_delivery(&repo, &entry).await;
    assert_eq!(updated.status, JOB_STATUS_DOWNLOADED);
    assert_eq!(
        updated.retry_count, 0,
        "cache recovery must not consume a retry"
    );
    assert!(updated.next_retry_at.is_none());
    assert!(updated.error.is_none());
    assert_eq!(std::fs::read(artifacts.final_zip()).unwrap(), zip_bytes);
    let requests = eh_server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
            })
            .count(),
        1,
        "an invalid final ZIP must prepare and POST a fresh archive in the same tick"
    );
}

#[tokio::test]
async fn background_preflight_db_error_releases_claim_without_retrying() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    setup_chat(&repo, -100, true).await;
    let entry = seed_delivery(
        &repo,
        -100,
        (
            4053262,
            "background-preflight-failure",
            "Background Preflight Failure",
        ),
        Default::default(),
    )
    .await;
    handoff_job_to_background(&repo, &entry).await;

    // Make the activity inspection take the subscription-owner path. With
    // no subscription IDs it attempts to soft-cancel this delivery, where
    // the trigger injects a deterministic pre-download database failure.
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Source,
            Expr::value(SOURCE_SUBSCRIPTION),
        )
        .filter(eh_download_queue::Column::Id.eq(entry.id))
        .exec(repo.db())
        .await
        .unwrap();
    repo.db()
        .execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
                CREATE TRIGGER fail_background_activity_preflight
                BEFORE UPDATE OF status ON eh_download_queue
                WHEN NEW.status = 'canceled'
                BEGIN
                    SELECT RAISE(FAIL, 'injected background activity preflight failure');
                END
                "#,
        ))
        .await
        .unwrap();

    let mut config = make_config();
    config.background_download_enabled = true;
    config.background_download_concurrency = 1;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Background,
        None,
    );

    let error = worker
        .tick()
        .await
        .expect_err("preflight database failure must reach the background tick");
    assert!(format!("{error:#}").contains("injected background activity preflight failure"));

    let deferred = job_for_delivery(&repo, &entry).await;
    assert_eq!(deferred.status, STATUS_PENDING);
    assert_eq!(
        deferred.background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_PENDING)
    );
    assert!(deferred.background_download_started_at.is_none());
    assert!(deferred
        .background_download_next_retry_at
        .is_some_and(|next_retry_at| next_retry_at > Local::now().naive_local()));
    assert_eq!(deferred.background_download_attempt_count, 0);
    assert!(deferred
        .background_download_error
        .as_deref()
        .unwrap()
        .contains("injected background activity preflight failure"));
    assert!(
        deferred.zip_path.is_some(),
        "preflight recovery must preserve archive artifact ownership"
    );
    let failed_generation = deferred.started_at.unwrap();

    // Make the deferred job due without waiting so a later tick can claim
    // it. The new generation proves the old worker cannot strand the job.
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::BackgroundDownloadNextRetryAt,
            Expr::value(Some(
                Local::now().naive_local() - chrono::Duration::seconds(1),
            )),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(deferred.id))
        .exec(repo.db())
        .await
        .unwrap();
    let reclaimed = repo
        .claim_eh_download_job(Background, true)
        .await
        .unwrap()
        .expect("preflight recovery must leave the job reclaimable");
    assert!(reclaimed.started_at.unwrap() > failed_generation);
}

#[tokio::test]
async fn test_background_worker_selected_size_limit_runs_after_prepare_without_metadata() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    setup_chat(&repo, -100, true).await;
    let entry = seed_delivery(
        &repo,
        -100,
        (2284789, "7841d194d4", "Oversized background archive"),
        Default::default(),
    )
    .await;
    handoff_job_to_background(&repo, &entry).await;
    mock_archiver(
        &eh_server,
        2284789,
        "7841d194d4",
        ArchiverPage {
            sizes: Some(("400.0 MiB", "300.01 MiB")),
            ..Default::default()
        },
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("should not be called"))
        .expect(0)
        .mount(&eh_server)
        .await;

    let mut config = make_config();
    config.background_download_enabled = true;
    config.background_download_concurrency = 1;
    config.max_archive_size_mb = 300;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Background,
        None,
    );

    worker.tick().await.unwrap();

    let updated = job_for_delivery(&repo, &entry).await;
    assert_eq!(updated.status, STATUS_PENDING);
    assert_eq!(
        updated.background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_PENDING)
    );
    assert_eq!(updated.background_download_attempt_count, 1);

    let requests = eh_server.received_requests().await.unwrap();
    assert!(requests.iter().any(|request| {
        request.method.as_str() == "GET" && request.url.path() == "/g/2284789/7841d194d4/"
    }));
    assert!(requests.iter().any(|request| {
        request.method.as_str() == "GET" && request.url.path() == "/archiver.php"
    }));
    assert!(
        !requests
            .iter()
            .any(|request| request.method.as_str() == "POST" && request.url.path() == "/api.php"),
        "background selected archive-size checks must not request gallery metadata"
    );
}

#[tokio::test]
async fn background_source_deferral_and_completion_errors_use_distinct_retry_paths() {
    for fail_completion in [false, true] {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let server = MockServer::start().await;
        let cache = tempfile::tempdir().unwrap();
        setup_chat(&repo, -100, true).await;
        let entry = seed_delivery(
            &repo,
            -100,
            (4053263, "abc123def0", "Database failure"),
            Default::default(),
        )
        .await;
        handoff_job_to_background(&repo, &entry).await;
        mock_archiver(
            &server,
            4053263,
            "abc123def0",
            ArchiverPage {
                original_cost: "218 GP",
                resample_cost: "218 GP",
                ..Default::default()
            },
        )
        .await;
        if fail_completion {
            let fixture = cache.path().join("fixture.zip");
            create_test_zip(&fixture, 1);
            mock_eh_archiver_post(&server, &format!("{}/archive/gallery.zip", server.uri())).await;
            mock_eh_archive_download(
                &server,
                "/archive/gallery.zip",
                std::fs::read(fixture).unwrap(),
            )
            .await;
        } else {
            Mock::given(method("POST"))
                .and(path("/archiver.php"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
        }
        let failure_condition = if fail_completion {
            "NEW.status = 'downloaded'"
        } else {
            "NEW.background_download_error LIKE 'EH GP rate limit would be exceeded%'"
        };
        repo.db()
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                format!(
            "CREATE TRIGGER fail_background_transition BEFORE UPDATE ON eh_gallery_jobs \
             WHEN {failure_condition} BEGIN SELECT RAISE(FAIL, 'injected transition failure'); END;"
        ),
            ))
            .await
            .unwrap();
        let mut config = make_config();
        config.background_download_concurrency = 1;
        config.max_archive_gp_cost = 218;
        config.gp_rate_limit = if fail_completion { 0 } else { 1 };
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&server),
            Arc::new(config),
            cache.path().to_path_buf(),
            Background,
            None,
        );

        let result = worker.tick().await;
        if fail_completion {
            let error = result.expect_err("completion errors must reach the background tick");
            assert!(format!("{error:#}").contains("injected transition failure"));
        } else {
            result.expect("failure to persist a source quota deferral must schedule a retry");
        }
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(job.status, JOB_STATUS_PENDING);
        assert_eq!(
            job.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING)
        );
        assert!(job.background_download_started_at.is_none());
        assert!(job
            .background_download_next_retry_at
            .is_some_and(|next| next > Local::now().naive_local()));
        assert_eq!(job.retry_count, 0);
        assert_eq!(
            job.background_download_attempt_count,
            if fail_completion { 0 } else { 1 }
        );
        let error = job.background_download_error.as_deref().unwrap();
        if fail_completion {
            assert!(error.contains("injected transition failure"));
        } else {
            assert_eq!(
                error,
                "Failed to defer shared EH gallery background download"
            );
        }
        assert_eq!(
            archive_artifacts_for_job(cache.path(), &job)
                .final_zip()
                .exists(),
            fail_completion
        );
        assert_eq!(gp_attempts(&repo).await.len(), usize::from(fail_completion));
        assert_eq!(
            eh_download_completions::Entity::find()
                .count(repo.db())
                .await
                .unwrap(),
            0
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.method.as_str() == "GET" && r.url.path() == "/archiver.php")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/archiver.php")
                .count(),
            usize::from(fail_completion)
        );
    }
}
