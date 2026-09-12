use super::*;

#[test]
fn collect_uploadable_zip_entry_names_reads_unsupported_encrypted_metadata() {
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("metadata-only.zip");
    create_unsupported_encrypted_metadata_zip(&zip_path);

    let names = collect_uploadable_zip_entry_names(&zip_path).unwrap();

    assert_eq!(names, ["folder/Photo.JPG"]);
}

#[tokio::test]
async fn test_upload_worker_includes_images_larger_than_six_mib() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;

    setup_chat(&repo, -100, true).await;

    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("large_gallery.zip");
    create_test_zip_with_sizes(&zip_path, &[1024, 6 * 1024 * 1024 + 1, 2048]);
    let zip_path_str = zip_path.to_string_lossy().to_string();

    let entry = seed_delivery(
        &repo,
        -100,
        (123456, "abcdef0123", "Large Gallery"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(&zip_path_str),
            ..Default::default()
        },
    )
    .await;

    let upload_body = serde_json::json!({
        "success": true,
        "direct_url": "https://i.pixi.mg/i/large.jpg"
    });
    Mock::given(method("POST"))
        .and(path("/pixi/upload"))
        .and(MultipartFileCount(1))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload_body))
        .expect(3)
        .mount(&tg_server)
        .await;
    mock_telegraph_create_page(&tg_server).await;
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        make_image_uploader(&tg_server),
        None,
        None,
        Arc::new(make_config()),
    );
    worker.tick().await.unwrap();

    let updated = job_for_delivery(&repo, &entry).await;
    assert_eq!(updated.telegraph_status, TELEGRAPH_STATUS_READY);
    assert!(updated.telegraph_url.is_some());
}

#[tokio::test]
async fn test_upload_worker_no_images_fails() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;

    setup_chat(&repo, -100, true).await;

    // Create ZIP with only .txt files
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("no_images.zip");
    {
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("readme.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"no images").unwrap();
        zip.finish().unwrap();
    }
    let zip_path_str = zip_path.to_string_lossy().to_string();

    let entry = seed_delivery(
        &repo,
        -100,
        (123456, "abcdef0123", "Test"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(&zip_path_str),
            ..Default::default()
        },
    )
    .await;

    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        make_image_uploader(&tg_server),
        None,
        None,
        Arc::new(make_config()),
    );
    worker.tick().await.unwrap();

    let updated = job_for_delivery(&repo, &entry).await;
    assert_eq!(
        updated.status, STATUS_DOWNLOADED,
        "should be back to downloaded for retry"
    );
    assert_eq!(updated.retry_count, 1);
}

#[tokio::test]
async fn upload_failure_persistence_error_releases_claim() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    setup_chat(&repo, -100, true).await;

    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("failure-persistence.zip");
    create_test_zip(&zip_path, 1);
    let (job, _) = seed_downloaded_job_with_deliveries(
        &repo,
        123457,
        "failure-token",
        "Failure persistence",
        &zip_path,
        &[(-100, true, "Failure persistence")],
    )
    .await;
    repo.db()
        .execute_unprepared(
            "CREATE TRIGGER fail_eh_upload_failure_persistence \
                 BEFORE UPDATE ON eh_gallery_jobs \
                 WHEN OLD.telegraph_status = 'uploading' \
                      AND NEW.retry_count > OLD.retry_count \
                 BEGIN \
                     SELECT RAISE(ABORT, 'simulated upload failure persistence error'); \
                 END",
        )
        .await
        .unwrap();

    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        Arc::new(AlwaysFailUploader {
            message: "simulated upload error".to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        None,
        None,
        Arc::new(make_config()),
    );
    let error = worker
        .tick()
        .await
        .expect_err("failure persistence should still be reported");
    assert!(format!("{error:#}").contains("simulated upload failure persistence error"));

    let released = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(released.telegraph_status, TELEGRAPH_STATUS_PENDING);
    assert_eq!(released.retry_count, 0);
    assert!(released.next_retry_at.is_some());
}

#[tokio::test]
async fn two_telegraph_deliveries_upload_zip_and_create_page_once() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("shared-gallery.zip");
    create_test_zip(&zip_path, 2);
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;
    let (job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        710,
        "token",
        "Shared Gallery",
        &zip_path,
        &[(-100, true, "T1"), (-200, true, "T2")],
    )
    .await;
    let uploader = Arc::new(ZipFirstMockUploader::default());
    let body = serde_json::json!({
        "ok": true,
        "result": {"url": "https://telegra.ph/Shared-Gallery-01-01"}
    });
    Mock::given(method("POST"))
        .and(path("/createPage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&tg_server)
        .await;
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        uploader.clone(),
        None,
        Some(IpfS3PreviewRewriteConfig {
            preview_gateway_url: "https://preview.example".to_string(),
            public_gateway_url: "https://public.example".to_string(),
            delay_sec: 60,
        }),
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();
    worker.tick().await.unwrap();

    assert_eq!(
        uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        uploader
            .image_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        *uploader.seen_entries.lock().unwrap(),
        ["page000.jpg", "page001.jpg"]
    );
    let ready = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready.telegraph_status, TELEGRAPH_STATUS_READY);
    assert_eq!(
        ready.telegraph_url.as_deref(),
        Some("https://telegra.ph/Shared-Gallery-01-01")
    );
    assert!(
        ready.telegraph_rewrite_data.is_some(),
        "create-page rewrite payload must be persisted once on the shared job"
    );
    for delivery in deliveries {
        let delivery = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.status, DELIVERY_STATUS_WAITING);
        assert_eq!(delivery.telegraph_url, None);
        assert_eq!(delivery.telegraph_rewrite_data, None);
    }
}

#[tokio::test]
async fn upload_worker_defers_all_disabled_destinations_without_spending_quota_or_starving_next_job(
) {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let disabled_zip_path = temp_dir.path().join("disabled-gallery.zip");
    let enabled_zip_path = temp_dir.path().join("enabled-gallery.zip");
    create_test_zip(&disabled_zip_path, 1);
    create_test_zip(&enabled_zip_path, 1);
    let disabled_artifacts = ArchiveArtifacts::new(&disabled_zip_path);
    std::fs::create_dir_all(disabled_artifacts.uploads_dir()).unwrap();
    let disabled_manifest = disabled_artifacts.uploads_dir().join("archive.json");
    std::fs::write(&disabled_manifest, b"resume state").unwrap();

    setup_chat(&repo, -100, false).await;
    setup_chat(&repo, -101, true).await;
    setup_chat(&repo, -200, true).await;
    let (disabled_job, disabled_deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        711,
        "disabled",
        "Disabled Gallery",
        &disabled_zip_path,
        &[
            (-100, true, "Disabled Telegraph"),
            (-101, false, "Archive Only"),
        ],
    )
    .await;
    let (enabled_job, _) = seed_downloaded_job_with_deliveries(
        &repo,
        712,
        "enabled",
        "Enabled Gallery",
        &enabled_zip_path,
        &[(-200, true, "Enabled Telegraph")],
    )
    .await;
    let body = serde_json::json!({
        "ok": true,
        "result": {"url": "https://telegra.ph/Enabled-Gallery-01-01"}
    });
    Mock::given(method("POST"))
        .and(path("/createPage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&tg_server)
        .await;
    let uploader = Arc::new(ZipFirstMockUploader::default());
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        uploader.clone(),
        None,
        None,
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();

    assert_eq!(
        uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "disabled Telegraph destinations must not invoke the ZIP uploader"
    );
    assert_eq!(
        uploader
            .image_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "disabled Telegraph destinations must not invoke the image uploader"
    );
    assert_eq!(
        tg_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/createPage")
            .count(),
        0,
        "disabled Telegraph destinations must not create a Telegraph page"
    );
    let deferred = eh_gallery_jobs::Entity::find_by_id(disabled_job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deferred.telegraph_status, TELEGRAPH_STATUS_PENDING);
    assert!(deferred.next_retry_at.is_some());
    assert_eq!(deferred.retry_count, disabled_job.retry_count);
    assert_eq!(deferred.error, disabled_job.error);
    assert_eq!(deferred.zip_path, disabled_job.zip_path);
    assert_eq!(deferred.file_size, disabled_job.file_size);
    assert!(disabled_zip_path.exists());
    assert_eq!(std::fs::read(&disabled_manifest).unwrap(), b"resume state");
    for delivery in disabled_deliveries {
        let delivery = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.status, DELIVERY_STATUS_WAITING);
        assert!(delivery.telegraph_sent_at.is_none());
    }

    worker.tick().await.unwrap();

    assert_eq!(
        uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the next due, notifiable job must not be starved"
    );
    let ready = eh_gallery_jobs::Entity::find_by_id(enabled_job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready.telegraph_status, TELEGRAPH_STATUS_READY);
}

#[tokio::test]
async fn terminal_upload_notifies_each_telegraph_chat_once_and_never_archive_only() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("terminal-shared-gallery.zip");
    create_test_zip(&zip_path, 1);
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;
    let (job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        711,
        "token",
        "Shared Gallery",
        &zip_path,
        &[
            (-100, true, "T1"),
            (-200, true, "T2"),
            (-300, false, "Archive"),
        ],
    )
    .await;
    let response = serde_json::json!({
        "ok": true,
        "result": {
            "message_id": 43,
            "date": 1700000000,
            "chat": {"id": -100, "type": "private"}
        }
    });
    Mock::given(method("POST"))
        .and(path("/botfake_token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(2)
        .mount(&tg_server)
        .await;
    let mut config = make_config();
    config.max_retry_count = 0;
    config.send_archive = true;
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        Arc::new(AlwaysFailUploader {
            message: "sqlite secret; /private/path; multipart upload id=abc".to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        None,
        None,
        Arc::new(config),
    );

    worker.tick().await.unwrap();
    worker.tick().await.unwrap();

    let requests = tg_server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/botfake_token/SendMessage")
        .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        requests
            .iter()
            .map(|body| body["chat_id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![-100, -200]
    );
    assert_eq!(
        requests
            .iter()
            .map(|body| body["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "⚠️ Telegraph 上传失败，请稍后重试\n\n📦 T1",
            "⚠️ Telegraph 上传失败，请稍后重试\n\n📦 T2",
        ]
    );
    assert!(requests.iter().all(|body| {
        let text = body["text"].as_str().unwrap();
        !text.contains("sqlite secret")
            && !text.contains("/private/path")
            && !text.contains("upload id")
    }));

    let failed_job = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed_job.telegraph_status, TELEGRAPH_STATUS_FAILED);
    assert!(failed_job.error.unwrap().contains("sqlite secret"));
    for (delivery, (expected_status, expected_telegraph)) in deliveries.iter().zip([
        (DELIVERY_STATUS_WAITING, false),
        (DELIVERY_STATUS_WAITING, false),
        (DELIVERY_STATUS_WAITING, false),
    ]) {
        let delivery = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.status, expected_status);
        assert_eq!(delivery.telegraph, expected_telegraph);
        assert_eq!(delivery.error, None);
    }
    let fallback_claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fallback_claim.delivery.id, deliveries[0].id);
    assert!(!fallback_claim.delivery.telegraph);
}

#[tokio::test]
async fn terminal_upload_failure_skips_disabled_shared_destination_notification() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("terminal-shared-gallery.zip");
    create_test_zip(&zip_path, 1);
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, false).await;
    let (job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        712,
        "token",
        "Shared Gallery",
        &zip_path,
        &[(-100, true, "Enabled"), (-200, true, "Disabled")],
    )
    .await;
    let response = serde_json::json!({
        "ok": true,
        "result": {
            "message_id": 43,
            "date": 1700000000,
            "chat": {"id": -100, "type": "private"}
        }
    });
    Mock::given(method("POST"))
        .and(path("/botfake_token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(&tg_server)
        .await;
    let uploader = Arc::new(AlwaysFailUploader {
        message: "terminal provider failure".to_string(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut config = make_config();
    config.max_retry_count = 0;
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        uploader.clone(),
        None,
        None,
        Arc::new(config),
    );

    worker.tick().await.unwrap();
    worker.tick().await.unwrap();

    assert_eq!(
        uploader.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the enabled Telegraph delivery must authorize provider work"
    );
    let requests = tg_server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/botfake_token/SendMessage")
        .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        requests
            .iter()
            .map(|body| body["chat_id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![-100],
        "terminal notification must skip the disabled sibling and remain exact-once"
    );
    assert_eq!(
        requests[0]["text"].as_str(),
        Some("⚠️ Telegraph 上传失败，请稍后重试\n\n📦 Enabled")
    );
    for delivery in deliveries {
        let delivery = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.status, DELIVERY_STATUS_WAITING);
        assert!(!delivery.telegraph);
        assert_eq!(delivery.error, None);
    }
    let failed_job = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed_job.telegraph_status, TELEGRAPH_STATUS_FAILED);
}

#[tokio::test]
async fn upload_worker_uses_original_uploadable_order_for_image_contexts() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    mock_telegraph_create_page(&tg_server).await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("mixed.zip");
    create_test_zip_with_names(
        &zip_path,
        &[
            "notes.txt",
            "directory/",
            "first.jpg",
            "metadata.json",
            "second.png",
        ],
    );
    let artifacts = ArchiveArtifacts::new(&zip_path);
    std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
    std::fs::write(
        artifacts.uploads_dir().join("archive.json"),
        b"upload state",
    )
    .unwrap();
    let zip_path_str = zip_path.to_string_lossy().to_string();
    let entry = seed_delivery(
        &repo,
        -100,
        (704, "tok", "Title"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(&zip_path_str),
            ..Default::default()
        },
    )
    .await;
    let uploader = Arc::new(ZipFirstMockUploader {
        zip_fallback: true,
        fail_image_call: Some(1),
        ..Default::default()
    });
    let abort_uploader = Arc::new(TerminalCleanupMockUploader::default());
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        uploader.clone(),
        Some(abort_uploader),
        None,
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();
    let retried = job_for_delivery(&repo, &entry).await;
    assert_eq!(retried.telegraph_status, TELEGRAPH_STATUS_PENDING);
    assert!(
        artifacts.uploads_dir().exists(),
        "retryable upload failure should retain upload state"
    );
    let mut retry_active: eh_gallery_jobs::ActiveModel = retried.into();
    retry_active.next_retry_at = Set(None);
    retry_active.update(repo.db()).await.unwrap();
    worker.tick().await.unwrap();

    let image_0 = SeenResumeContext {
        manifest_path: artifacts.uploads_dir().join("image-0.json"),
        logical_object_id: "image-0".to_string(),
    };
    let image_1 = SeenResumeContext {
        manifest_path: artifacts.uploads_dir().join("image-1.json"),
        logical_object_id: "image-1".to_string(),
    };
    assert_eq!(
        *uploader.seen_image_resume_contexts.lock().unwrap(),
        vec![image_0.clone(), image_1.clone(), image_0, image_1]
    );
    let job = job_for_delivery(&repo, &entry).await;
    assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_READY);
}

#[tokio::test]
async fn successful_upload_persists_result_record_for_ipfs3() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("ipfs3.zip");
    create_test_zip_with_names(&zip_path, &["001.jpg", "nested/002.png"]);
    let artifacts = ArchiveArtifacts::new(&zip_path);
    std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
    std::fs::write(
        artifacts.uploads_dir().join("archive.json"),
        b"upload state",
    )
    .unwrap();
    let zip_path_str = zip_path.to_string_lossy().to_string();
    let entry = seed_delivery(
        &repo,
        -100,
        (970, "ipfs3-token", "IPFS3 Gallery"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(&zip_path_str),
            ..Default::default()
        },
    )
    .await;
    let job = job_for_delivery(&repo, &entry).await;
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::SourceFingerprint,
            Expr::value(Some("fingerprint-ipfs3".to_string())),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    mock_telegraph_create_page(&tg_server).await;

    let uploader = Arc::new(ZipFirstMockUploader::default());
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        uploader.clone(),
        Some(Arc::new(TerminalCleanupMockUploader::default())),
        Some(IpfS3PreviewRewriteConfig {
            preview_gateway_url: "https://preview.example".to_string(),
            public_gateway_url: "https://public.example".to_string(),
            delay_sec: 60,
        }),
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();

    assert_eq!(
        *uploader.seen_zip_resume_contexts.lock().unwrap(),
        vec![SeenResumeContext {
            manifest_path: artifacts.uploads_dir().join("archive.json"),
            logical_object_id: "archive".to_string(),
        }]
    );
    assert!(!artifacts.uploads_dir().exists());
    assert!(artifacts.final_zip().exists());
    let ready = job_for_delivery(&repo, &entry).await;
    assert_eq!(ready.telegraph_status, TELEGRAPH_STATUS_READY);
    let result = eh_gallery_results::Entity::find()
        .filter(eh_gallery_results::Column::Gid.eq(970_i64))
        .filter(eh_gallery_results::Column::Token.eq("ipfs3-token"))
        .one(repo.db())
        .await
        .unwrap()
        .expect("IPFS3 upload should persist a reusable result");
    assert_eq!(result.source_fingerprint, "fingerprint-ipfs3");
    assert_eq!(result.telegraph_url, ready.telegraph_url.unwrap());
    assert_eq!(result.telegraph_rewrite_data, ready.telegraph_rewrite_data);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.media_cids.as_deref().unwrap()).unwrap(),
        serde_json::json!([
            {"name": "001.jpg", "cid": "bafy-zip-001.jpg"},
            {"name": "nested/002.png", "cid": "bafy-zip-nested/002.png"},
        ])
    );
}

#[tokio::test]
async fn per_image_upload_persists_cids_in_filename_order() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("per-image-ipfs3.zip");
    create_test_zip_with_names(&zip_path, &["dir\\001.jpg", "002.png"]);
    let zip_path_str = zip_path.to_string_lossy().to_string();
    let entry = seed_delivery(
        &repo,
        -100,
        (971, "per-image-token", "Per Image IPFS3 Gallery"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(&zip_path_str),
            ..Default::default()
        },
    )
    .await;
    let job = job_for_delivery(&repo, &entry).await;
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::SourceFingerprint,
            Expr::value(Some("fingerprint-per-image".to_string())),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    mock_telegraph_create_page(&tg_server).await;
    let uploader = Arc::new(ZipFirstMockUploader {
        zip_fallback: true,
        emit_image_cids: true,
        ..Default::default()
    });
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        uploader.clone(),
        None,
        None,
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();

    assert_eq!(
        uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        uploader
            .image_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(
        *uploader.seen_entries.lock().unwrap(),
        vec!["dir\\001.jpg", "002.png"]
    );
    let ready = job_for_delivery(&repo, &entry).await;
    assert_eq!(ready.telegraph_status, TELEGRAPH_STATUS_READY);
    assert!(ready.telegraph_url.is_some());
    let result = eh_gallery_results::Entity::find()
        .filter(eh_gallery_results::Column::Gid.eq(971_i64))
        .filter(eh_gallery_results::Column::Token.eq("per-image-token"))
        .one(repo.db())
        .await
        .unwrap()
        .expect("per-image IPFS3 upload should persist a reusable result");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.media_cids.as_deref().unwrap()).unwrap(),
        serde_json::json!([
            {"name": "dir\\001.jpg", "cid": "bafy-image-dir\\001.jpg"},
            {"name": "002.png", "cid": "bafy-image-002.png"},
        ])
    );
}

#[tokio::test]
async fn non_ipfs3_upload_writes_no_result_record() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("pixi.zip");
    create_test_zip(&zip_path, 2);
    let zip_path_str = zip_path.to_string_lossy().to_string();
    let entry = seed_delivery(
        &repo,
        -100,
        (972, "pixi-token", "Pixi Gallery"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(&zip_path_str),
            ..Default::default()
        },
    )
    .await;
    let job = job_for_delivery(&repo, &entry).await;
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::SourceFingerprint,
            Expr::value(Some("fingerprint-pixi".to_string())),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    mock_telegraph_upload(&tg_server, 2).await;
    mock_telegraph_create_page(&tg_server).await;
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        make_image_uploader(&tg_server),
        None,
        None,
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();

    let ready = job_for_delivery(&repo, &entry).await;
    assert_eq!(ready.telegraph_status, TELEGRAPH_STATUS_READY);
    assert!(eh_gallery_results::Entity::find()
        .filter(eh_gallery_results::Column::Gid.eq(972_i64))
        .filter(eh_gallery_results::Column::Token.eq("pixi-token"))
        .one(repo.db())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_upload_worker_fallback_skips_unsupported_non_image_zip_entry() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    mock_telegraph_create_page(&tg_server).await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("zip_fallback_with_metadata.zip");
    create_test_zip_with_unsupported_encrypted_non_image(&zip_path);
    let zip_path_str = zip_path.to_string_lossy().to_string();
    let entry = seed_delivery(
        &repo,
        -100,
        (702, "tok", "Title"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(&zip_path_str),
            ..Default::default()
        },
    )
    .await;
    let uploader = Arc::new(ZipFirstMockUploader {
        zip_fallback: true,
        ..Default::default()
    });
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        uploader.clone(),
        None,
        None,
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();

    assert_eq!(
        uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        uploader
            .image_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let job = job_for_delivery(&repo, &entry).await;
    assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_READY);
}

#[tokio::test]
async fn terminal_upload_failure_preserves_archive_fallback_with_any_abort_support() {
    for abort_support in [Some(false), Some(true), None] {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let mut cfg = make_config();
        cfg.max_retry_count = 0;
        cfg.send_archive = true;
        let config = Arc::new(cfg);
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("505.zip");
        create_test_zip(&zip_path, 2);
        let artifacts = seed_archive_artifact_family(&zip_path);
        let entry = seed_delivery(
            &repo,
            -100,
            (505, "tok", "Title"),
            DeliveryOptions {
                telegraph: true,
                job_status: JOB_STATUS_DOWNLOADED,
                zip_path: Some(zip_path.to_str().unwrap()),
                ..Default::default()
            },
        )
        .await;
        let uploader = Arc::new(TerminalCleanupMockUploader {
            fail_abort: abort_support == Some(true),
            ..Default::default()
        });

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            uploader.clone(),
            abort_support.map(|_| uploader.clone() as Arc<dyn ImageUploader>),
            None,
            config,
        );
        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, DELIVERY_STATUS_WAITING);
        assert!(!model.telegraph);
        assert_eq!(model.error, None);
        assert!(model.started_at.is_none());
        assert!(model.completed_at.is_none());
        assert!(model.next_retry_at.is_none());
        let job = job_for_delivery(&repo, &entry).await;
        assert_eq!(job.status, JOB_STATUS_DOWNLOADED);
        assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_FAILED);
        assert_eq!(job.retry_count, 1);
        assert!(job.next_retry_at.is_none());
        assert_eq!(job.cleanup_status, CLEANUP_STATUS_NONE);
        assert!(zip_path.exists());
        assert!(artifacts.uploads_dir().join("archive.json").exists());
        assert!(artifacts.assembly_scratch().exists());
        assert!(artifacts.parts_dir().exists());
        let fallback_claim = repo
            .get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fallback_claim.delivery.id, entry.id);
        assert!(!fallback_claim.delivery.telegraph);
        assert!(uploader.cleanup_calls.lock().unwrap().is_empty());

        repo.mark_eh_archive_delivery_sent(fallback_claim.delivery.id)
            .await
            .unwrap();
        let ledger = eh_gallery_push_ledger::Entity::find()
            .filter(eh_gallery_push_ledger::Column::ChatId.eq(entry.chat_id))
            .filter(eh_gallery_push_ledger::Column::Gid.eq(entry.gid))
            .one(repo.db())
            .await
            .unwrap()
            .expect("archive fallback's standard publish marker must write the push ledger");
        assert!(ledger.archive_sent_at.is_some());
        assert!(ledger.telegraph_sent_at.is_none());
    }
}

#[tokio::test]
async fn test_upload_permanent_failure_without_fallback_removes_whole_archive_family() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let mut cfg = make_config();
    cfg.max_retry_count = 0;
    cfg.send_archive = false;
    let config = Arc::new(cfg);
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("506.zip");
    create_test_zip(&zip_path, 2);
    let artifacts = seed_archive_artifact_family(&zip_path);
    let entry = seed_delivery(
        &repo,
        -100,
        (506, "tok", "Title"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(zip_path.to_str().unwrap()),
            ..Default::default()
        },
    )
    .await;
    let uploader = Arc::new(TerminalCleanupMockUploader::default());

    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        notifier,
        make_telegraph_client(&tg_server),
        uploader.clone(),
        Some(uploader.clone()),
        None,
        config,
    );
    worker.tick().await.unwrap();

    let delivery = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery.status, STATUS_FAILED);
    assert_eq!(delivery.error, None);
    let job = job_for_delivery(&repo, &entry).await;
    assert_eq!(job.status, JOB_STATUS_RETIRED);
    assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_FAILED);
    assert_eq!(job.retry_count, 1);
    assert!(job.next_retry_at.is_none());
    assert_eq!(job.cleanup_status, CLEANUP_STATUS_PENDING);
    assert_eq!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(uploader.as_ref()), 1, true)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::CleanRetired)
    );
    assert!(!artifacts.final_zip().exists());
    assert!(!artifacts.assembly_scratch().exists());
    assert!(!artifacts.parts_dir().exists());
    assert!(!artifacts.uploads_dir().exists());
    assert_terminal_cleanup_precedes_local_removal(&uploader, &artifacts);
}

#[tokio::test]
async fn test_upload_permanent_failure_without_fallback_preserves_family_when_abort_fails() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let mut cfg = make_config();
    cfg.max_retry_count = 0;
    cfg.send_archive = false;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("506-abort-fails.zip");
    create_test_zip(&zip_path, 2);
    let artifacts = seed_archive_artifact_family(&zip_path);
    let entry = seed_delivery(
        &repo,
        -100,
        (506, "tok", "Title"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(zip_path.to_str().unwrap()),
            ..Default::default()
        },
    )
    .await;
    let uploader = Arc::new(TerminalCleanupMockUploader {
        fail_abort: true,
        ..Default::default()
    });
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        notifier,
        make_telegraph_client(&tg_server),
        uploader.clone(),
        Some(uploader.clone()),
        None,
        Arc::new(cfg),
    );

    worker.tick().await.unwrap();
    let delivery = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery.status, DELIVERY_STATUS_FAILED);
    assert_eq!(delivery.error, None);
    let job = job_for_delivery(&repo, &entry).await;
    assert_eq!(job.status, JOB_STATUS_RETIRED);
    assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_FAILED);
    assert_eq!(job.retry_count, 1);
    assert_eq!(job.cleanup_status, CLEANUP_STATUS_PENDING);
    assert!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), Some(uploader.as_ref()), 1, true)
            .await
            .is_err()
    );
    let failed_cleanup = job_for_delivery(&repo, &entry).await;
    assert_eq!(failed_cleanup.cleanup_status, CLEANUP_STATUS_FAILED);
    assert!(artifacts.final_zip().exists());
    assert!(artifacts.assembly_scratch().exists());
    assert!(artifacts.parts_dir().exists());
    assert!(artifacts.uploads_dir().exists());
    assert!(artifacts.uploads_dir().join("archive.json").exists());
    assert_terminal_cleanup_precedes_local_removal(&uploader, &artifacts);
}

#[tokio::test]
async fn test_upload_canceled_after_claim_removes_upload_state_without_sending() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("507-canceled.zip");
    create_test_zip(&zip_path, 2);
    let artifacts = ArchiveArtifacts::new(&zip_path);
    std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
    std::fs::write(
        artifacts.uploads_dir().join("archive.json"),
        b"upload state",
    )
    .unwrap();
    let entry = seed_delivery(
        &repo,
        -100,
        (507, "tok", "Title"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(zip_path.to_str().unwrap()),
            subscription_ids: Some("123"),
        },
    )
    .await;
    mock_telegraph_create_page(&tg_server).await;
    let claimed = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    assert_eq!(claimed.id, entry.job_id.unwrap());
    repo.cancel_eh_subscription_queue_entries(123, true)
        .await
        .unwrap();
    let uploader = Arc::new(ZipFirstMockUploader::default());
    let abort_uploader = Arc::new(TerminalCleanupMockUploader::default());

    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        uploader.clone(),
        Some(abort_uploader.clone()),
        None,
        Arc::new(make_config()),
    );
    worker.process(&claimed).await.unwrap();

    let model = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(model.status, STATUS_CANCELED);
    assert!(zip_path.exists(), "canceled upload should not clean ZIP");
    assert!(!artifacts.uploads_dir().exists());
    let job = job_for_delivery(&repo, &entry).await;
    assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_READY);
    assert_eq!(
        uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "cancellation after claim must not interrupt the shared upload"
    );
    assert_terminal_cleanup_precedes_local_removal(&abort_uploader, &artifacts);
}

#[tokio::test]
async fn test_upload_canceled_after_claim_preserves_upload_state_when_abort_fails() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("507-canceled-abort-fails.zip");
    create_test_zip(&zip_path, 2);
    let artifacts = ArchiveArtifacts::new(&zip_path);
    std::fs::create_dir_all(artifacts.uploads_dir()).unwrap();
    let manifest = artifacts.uploads_dir().join("archive.json");
    std::fs::write(&manifest, b"upload state").unwrap();
    let entry = seed_delivery(
        &repo,
        -100,
        (507, "tok", "Title"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(zip_path.to_str().unwrap()),
            subscription_ids: Some("123"),
        },
    )
    .await;
    mock_telegraph_create_page(&tg_server).await;
    let claimed = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    assert_eq!(claimed.id, entry.job_id.unwrap());
    repo.cancel_eh_subscription_queue_entries(123, true)
        .await
        .unwrap();
    let uploader = Arc::new(ZipFirstMockUploader::default());
    let abort_uploader = Arc::new(TerminalCleanupMockUploader {
        fail_abort: true,
        ..Default::default()
    });
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        uploader.clone(),
        Some(abort_uploader.clone()),
        None,
        Arc::new(make_config()),
    );

    worker.process(&claimed).await.unwrap();
    let model = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(model.status, STATUS_CANCELED);
    assert!(zip_path.exists(), "canceled upload must not clean ZIP");
    assert!(artifacts.uploads_dir().exists());
    assert!(
        manifest.exists(),
        "failed Abort must retain upload manifest"
    );
    let job = job_for_delivery(&repo, &entry).await;
    assert_eq!(job.telegraph_status, TELEGRAPH_STATUS_READY);
    assert_eq!(
        uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "cancellation after claim must not suppress page creation"
    );
    assert_terminal_cleanup_precedes_local_removal(&abort_uploader, &artifacts);
}

#[tokio::test]
async fn test_upload_permanent_failure_with_missing_zip_enters_archive_fallback() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let mut cfg = make_config();
    cfg.max_retry_count = 0;
    cfg.send_archive = true;
    let config = Arc::new(cfg);
    let entry = seed_delivery(
        &repo,
        -100,
        (506, "tok", "Title"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some("data/test_cache/missing_506.zip"),
            ..Default::default()
        },
    )
    .await;

    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        notifier,
        make_telegraph_client(&tg_server),
        make_image_uploader(&tg_server),
        None,
        None,
        config,
    );
    worker.tick().await.unwrap();
    let model = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    // The terminal upload failure preserves the DB-visible archive surface;
    // the publish worker owns filesystem validation and missing-ZIP recovery.
    assert_eq!(model.status, DELIVERY_STATUS_WAITING);
    assert!(!model.telegraph);
    assert_eq!(model.error, None, "provider errors stay on the shared job");
    assert!(job_for_delivery(&repo, &entry).await.error.is_some());
}

#[tokio::test]
async fn ipfs3_upload_without_fingerprint_writes_no_result_record() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let telegram_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let gid = 9_805;
    setup_chat(&repo, -100, true).await;
    let zip_path = temp.path().join("missing-fingerprint.zip");
    create_test_zip(&zip_path, 2);
    let delivery = seed_delivery(
        &repo,
        -100,
        (gid, "abcd980005", "Missing Fingerprint Gallery"),
        DeliveryOptions {
            telegraph: true,
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(zip_path.to_str().unwrap()),
            ..Default::default()
        },
    )
    .await;
    mock_telegraph_create_page(&telegram_server).await;
    EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&telegram_server),
        make_telegraph_client(&telegram_server),
        Arc::new(ZipFirstMockUploader::default()),
        None,
        Some(IpfS3PreviewRewriteConfig {
            preview_gateway_url: "https://preview.example".to_string(),
            public_gateway_url: "https://public.example".to_string(),
            delay_sec: 60,
        }),
        Arc::new(make_config()),
    )
    .tick()
    .await
    .unwrap();
    assert_eq!(
        job_for_delivery(&repo, &delivery).await.telegraph_status,
        TELEGRAPH_STATUS_READY
    );
    assert!(eh_gallery_results::Entity::find()
        .filter(eh_gallery_results::Column::Gid.eq(gid))
        .filter(eh_gallery_results::Column::Token.eq("abcd980005"))
        .one(repo.db())
        .await
        .unwrap()
        .is_none());
}
