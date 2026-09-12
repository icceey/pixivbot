use super::*;

#[tokio::test]
async fn publish_send_serializes_concurrent_enqueue_into_a_clean_next_wave() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("serialized-wave.zip");
    create_test_zip(&zip_path, 1);
    let (old_job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        910,
        "old-token",
        "Old wave",
        &zip_path,
        &[(-100, false, "Old wave")],
    )
    .await;
    mock_tg_send_document(&tg_server).await;

    let send_entered = Arc::new(tokio::sync::Notify::new());
    let release_send = Arc::new(tokio::sync::Notify::new());
    let done_entered = Arc::new(tokio::sync::Notify::new());
    let release_done = Arc::new(tokio::sync::Notify::new());
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    )
    .with_test_send_hook(EhPublishSendHook {
        entered: Arc::clone(&send_entered),
        release: Arc::clone(&release_send),
        after_done: Some(EhPublishCompletionHook {
            entered: Arc::clone(&done_entered),
            release: Arc::clone(&release_done),
        }),
    });
    let claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    let publish = tokio::spawn(async move { worker.process_claimed(claim).await });
    tokio::time::timeout(Duration::from_secs(5), send_entered.notified())
        .await
        .expect("publisher must enter the real document send");

    let enqueue_waiting = Arc::new(tokio::sync::Notify::new());
    let enqueue_acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let enqueue_hook = EhEnqueueChatLockHook {
        waiting: Arc::clone(&enqueue_waiting),
        acquired: Arc::clone(&enqueue_acquired),
    };
    let enqueue_repo = Arc::clone(&repo);
    let enqueue = tokio::spawn(async move {
        EH_ENQUEUE_CHAT_LOCK_HOOK
            .scope(enqueue_hook, async move {
                enqueue_repo
                    .enqueue_eh_download(
                        -100,
                        910,
                        "new-token",
                        "New wave",
                        false,
                        SOURCE_DIRECT,
                        &EhGalleryVariant::archive("original"),
                        None,
                        true,
                    )
                    .await
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), enqueue_waiting.notified())
        .await
        .expect("enqueue must attempt the chat lock after send begins");
    assert!(
        !enqueue_acquired.load(std::sync::atomic::Ordering::SeqCst),
        "enqueue must remain behind the in-flight publisher's chat lock"
    );

    let in_flight = eh_download_queue::Entity::find_by_id(deliveries[0].id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(in_flight.status, STATUS_PUBLISHING);
    assert!(in_flight.archive_sent_at.is_none());

    release_send.notify_one();
    tokio::time::timeout(Duration::from_secs(5), done_entered.notified())
        .await
        .expect("the old wave must persist completion before releasing the chat lock");
    let old_wave = eh_download_queue::Entity::find_by_id(deliveries[0].id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_wave.status, STATUS_DONE);
    assert!(old_wave.archive_sent_at.is_some());

    release_done.notify_one();
    publish.await.unwrap().unwrap();
    let new_wave = enqueue
        .await
        .unwrap()
        .expect("enqueue task should succeed")
        .expect("delivery should be enqueued");
    assert!(enqueue_acquired.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(new_wave.status, DELIVERY_STATUS_WAITING);
    assert_ne!(new_wave.job_id, Some(old_job.id));
    assert!(new_wave.archive_sent_at.is_none());
    assert!(new_wave.telegraph_sent_at.is_none());
    assert!(new_wave.started_at.is_none());
    assert!(new_wave.completed_at.is_none());
    assert_eq!(new_wave.retry_count, 0);
    assert_eq!(
        tg_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().ends_with("/SendDocument"))
            .count(),
        1,
        "the old wave must send exactly once before a new wave can begin"
    );
}

#[tokio::test]
async fn markerless_publish_claim_rebinds_to_direct_original_before_chat_lock() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("old-1280x.zip");
    create_test_zip(&zip_path, 1);
    let (old_job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        912,
        "same-token",
        "Old 1280x wave",
        &zip_path,
        &[(-100, false, "Old 1280x wave")],
    )
    .await;
    let old_claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_claim.delivery.id, deliveries[0].id);
    assert!(old_claim.delivery.archive_sent_at.is_none());
    assert!(old_claim.delivery.telegraph_sent_at.is_none());

    let rebound = repo
        .enqueue_eh_download(
            -100,
            912,
            "same-token",
            "Requested original",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("original"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let requested_job = job_for_delivery(&repo, &rebound).await;
    assert_eq!(rebound.status, DELIVERY_STATUS_WAITING);
    assert_ne!(rebound.job_id, Some(old_job.id));
    assert_eq!(requested_job.resolution, "original");
    assert_eq!(rebound.token, "same-token");
    assert_eq!(rebound.title, "Requested original");
    assert!(rebound.archive_sent_at.is_none());
    assert!(rebound.telegraph_sent_at.is_none());
    assert!(rebound.started_at.is_none());
    assert!(rebound.completed_at.is_none());
    assert!(rebound.error.is_none());
    assert_eq!(rebound.retry_count, 0);
    assert!(rebound.next_retry_at.is_none());

    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );
    worker.process_claimed(old_claim).await.unwrap();
    assert!(
        tg_server.received_requests().await.unwrap().is_empty(),
        "the stale publisher must re-read the re-bound delivery and send nothing"
    );
    assert_eq!(
        eh_download_queue::Entity::find_by_id(rebound.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .status,
        DELIVERY_STATUS_WAITING
    );

    let selected = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected.id, requested_job.id);
    assert_eq!(selected.resolution, "original");
}

#[tokio::test]
async fn marker_bearing_subscription_fingerprint_rebind_makes_stale_publish_claim_a_noop() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let variant = EhGalleryVariant::archive("1280x");
    let first = repo
        .enqueue_eh_subscription_download(
            -100,
            101,
            913,
            "same-token",
            "Old fingerprint",
            true,
            &variant,
            Some("fingerprint-a"),
            true,
        )
        .await
        .unwrap()
        .expect("old fingerprint delivery should be enqueued");
    let old_job = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        old_job.id,
        old_job.started_at.unwrap(),
        123,
        "old-fingerprint.zip",
        0,
    )
    .await
    .unwrap();
    let upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    repo.mark_eh_job_telegraph_ready(
        upload.id,
        upload.started_at.unwrap(),
        "https://telegra.ph/old-fingerprint",
        None,
        None,
        true,
    )
    .await
    .unwrap();
    let old_claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .expect("old fingerprint delivery should be claimed for publish");
    repo.mark_eh_archive_delivery_sent(old_claim.delivery.id)
        .await
        .unwrap();

    let rebound = repo
        .enqueue_eh_subscription_download(
            -100,
            202,
            913,
            "same-token",
            "New fingerprint",
            true,
            &variant,
            Some("fingerprint-b"),
            true,
        )
        .await
        .unwrap()
        .expect("new fingerprint must retain the unsent link demand");
    let requested_job_id = rebound.job_id.expect("rebound delivery should have a job");
    assert_ne!(requested_job_id, old_job.id);
    assert_eq!(rebound.status, DELIVERY_STATUS_WAITING);
    assert!(rebound.archive_sent_at.is_some());
    assert!(rebound.telegraph_sent_at.is_none());

    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );
    worker.process_claimed(old_claim).await.unwrap();
    assert!(
        tg_server.received_requests().await.unwrap().is_empty(),
        "the stale publisher must reread the rebound delivery before sending the old link"
    );
    let rebound = eh_download_queue::Entity::find_by_id(first.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rebound.job_id, Some(requested_job_id));
    assert_eq!(rebound.status, DELIVERY_STATUS_WAITING);
    assert!(rebound.archive_sent_at.is_some());
    assert!(rebound.telegraph_sent_at.is_none());
    assert_eq!(
        repo.claim_eh_download_job(Main, true)
            .await
            .unwrap()
            .unwrap()
            .id,
        requested_job_id
    );
}

#[tokio::test]
async fn publish_tick_drains_started_sibling_before_returning_refill_claim_error() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("claim-error-drain.zip");
    create_test_zip(&zip_path, 1);
    let (_, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        911,
        "claim-error",
        "Claim error drain",
        &zip_path,
        &[(-100, false, "First"), (-200, false, "Second")],
    )
    .await;
    mock_tg_send_document_for_chat(&tg_server, -100, 200, None).await;
    repo.db()
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "CREATE TRIGGER delete_second_publish_readback AFTER UPDATE OF status ON eh_download_queue \
                     WHEN NEW.id = {} AND NEW.status = 'publishing' BEGIN \
                         DELETE FROM eh_download_queue WHERE id = NEW.id; \
                     END;",
                    deliveries[1].id
                ),
            ))
            .await
            .unwrap();

    let send_entered = Arc::new(tokio::sync::Notify::new());
    let release_send = Arc::new(tokio::sync::Notify::new());
    let mut config = make_config();
    config.publish_concurrency = 2;
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(config),
    )
    .with_test_send_hook(EhPublishSendHook {
        entered: Arc::clone(&send_entered),
        release: Arc::clone(&release_send),
        after_done: None,
    });
    let tick = tokio::spawn(async move { worker.tick().await });
    tokio::time::timeout(Duration::from_secs(5), send_entered.notified())
        .await
        .expect("the first claimed sibling must reach its send hook");
    tokio::task::yield_now().await;
    assert!(
        !tick.is_finished(),
        "a refill claim error must drain the already-started sibling rather than drop it"
    );

    release_send.notify_one();
    let error = tick
        .await
        .unwrap()
        .expect_err("the refill readback failure must be returned after draining");
    assert!(
        format!("{:#}", error).contains("Shared EH delivery changed before publish claim readback")
    );
    let first = eh_download_queue::Entity::find_by_id(deliveries[0].id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    let second = eh_download_queue::Entity::find_by_id(deliveries[1].id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.status, STATUS_DONE);
    assert_eq!(
        second.status, DELIVERY_STATUS_WAITING,
        "the failed refill transaction must not leave a publishing claim"
    );
}

#[tokio::test]
async fn test_publish_archive_only_keeps_family_until_cleanup() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    setup_chat(&repo, -100, true).await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("abort-gated-archive-only.zip");
    create_test_zip(&zip_path, 2);
    let artifacts = seed_archive_artifact_family(&zip_path);
    let (_, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        123457,
        "abort-gated",
        "Archive Only",
        &zip_path,
        &[(-100, false, "Archive Only")],
    )
    .await;
    let entry = &deliveries[0];

    mock_tg_send_document(&tg_server).await;
    let eh_server = MockServer::start().await;
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();
    let done = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.status, STATUS_DONE);
    assert!(done.completed_at.is_some());
    assert!(done.error.is_none());
    assert!(done.archive_sent_at.is_some());
    assert!(artifacts.final_zip().exists());
    assert!(artifacts.assembly_scratch().exists());
    assert!(artifacts.parts_dir().exists());
    assert!(artifacts.uploads_dir().join("archive.json").exists());
    let document_sends = tg_server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path().ends_with("/SendDocument"))
        .count();
    assert_eq!(
        document_sends, 1,
        "delivery completion must not create a cleanup resend"
    );
    assert_eq!(
        job_for_delivery(&repo, &done).await.cleanup_status,
        CLEANUP_STATUS_PENDING
    );
}

#[tokio::test]
async fn test_publish_retry_skips_archive_after_marker() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let config = Arc::new(make_config());
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("501.zip");
    create_test_zip(&zip_path, 2);
    let (job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        501,
        "tok",
        "Title",
        &zip_path,
        &[(-100, true, "Title")],
    )
    .await;
    let entry = &deliveries[0];
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::TelegraphStatus,
            Expr::value(TELEGRAPH_STATUS_READY),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphUrl,
            Expr::value(Some("https://telegra.ph/page".to_string())),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();

    // Pre-set archive_sent_at directly (bypassing the publishing guard for test setup)
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::ArchiveSentAt,
            Expr::value(Some(Local::now().naive_local())),
        )
        .filter(eh_download_queue::Column::Id.eq(entry.id))
        .exec(repo.db())
        .await
        .unwrap();

    // Only mock SendMessage (telegraph link); do NOT mock SendDocument (archive)
    mock_tg_send_message(&tg_server).await;

    let eh_server = MockServer::start().await;
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        notifier,
        make_eh_client(&eh_server),
        None,
        config,
    );
    worker.tick().await.unwrap();

    let model = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(model.status, STATUS_DONE);
    assert!(model.telegraph_sent_at.is_some());
}

#[tokio::test]
async fn test_publish_skips_entry_canceled_after_claim() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let config = Arc::new(make_config());
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("509.zip");
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
        (509, "tok", "Title"),
        DeliveryOptions {
            job_status: JOB_STATUS_DOWNLOADED,
            zip_path: Some(zip_path.to_str().unwrap()),
            subscription_ids: Some("123"),
            ..Default::default()
        },
    )
    .await;
    let claimed = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.delivery.id, entry.id);
    repo.cancel_eh_subscription_queue_entries(123, true)
        .await
        .unwrap();

    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        notifier,
        make_eh_client(&MockServer::start().await),
        None,
        config,
    );
    worker.process_claimed(claimed).await.unwrap();

    let model = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(model.status, STATUS_CANCELED);
    assert!(model.archive_sent_at.is_none());
    assert!(
        zip_path.exists(),
        "canceled publish must not clean shared ZIP"
    );
    assert!(
        artifacts.uploads_dir().exists(),
        "shared upload state must remain available for job cleanup"
    );
    assert!(tg_server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_publish_terminal_send_failure_is_delivery_local_and_keeps_shared_family() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let mut cfg = make_config();
    cfg.send_archive = true;
    cfg.max_retry_count = 0;
    let config = Arc::new(cfg);
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("503.zip");
    create_test_zip(&zip_path, 2);
    let artifacts = seed_archive_artifact_family(&zip_path);
    let (job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        503,
        "tok",
        "Title",
        &zip_path,
        &[(-100, false, "Title")],
    )
    .await;
    let entry = &deliveries[0];
    mock_tg_send_document_for_chat(&tg_server, -100, 400, None).await;
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&MockServer::start().await),
        None,
        config,
    );

    worker.tick().await.unwrap();
    let model = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(model.status, STATUS_FAILED);
    assert!(model
        .error
        .as_deref()
        .unwrap()
        .contains("Failed to send archive document"));
    assert_eq!(model.retry_count, 1);
    let retired = job_for_delivery(&repo, &model).await;
    assert_eq!(retired.id, job.id);
    assert_eq!(retired.status, JOB_STATUS_RETIRED);
    assert_eq!(retired.cleanup_status, CLEANUP_STATUS_PENDING);
    assert!(artifacts.final_zip().exists());
    assert!(artifacts.assembly_scratch().exists());
    assert!(artifacts.parts_dir().exists());
    assert!(artifacts.uploads_dir().exists());
}

#[tokio::test]
async fn test_publish_both_markers_already_set_skips_to_done() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let config = Arc::new(make_config());
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("508.zip");
    create_test_zip(&zip_path, 2);
    let (_, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        508,
        "tok",
        "Title",
        &zip_path,
        &[(-100, true, "Title")],
    )
    .await;
    let entry = &deliveries[0];

    // Pre-set both markers to simulate a completed-but-not-done entry.
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::ArchiveSentAt,
            Expr::value(Some(Local::now().naive_local())),
        )
        .col_expr(
            eh_download_queue::Column::TelegraphSentAt,
            Expr::value(Some(Local::now().naive_local())),
        )
        .filter(eh_download_queue::Column::Id.eq(entry.id))
        .exec(repo.db())
        .await
        .unwrap();

    let eh_server = MockServer::start().await;
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        notifier,
        make_eh_client(&eh_server),
        None,
        config,
    );
    worker.tick().await.unwrap();

    let model = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(model.status, STATUS_DONE);
    assert!(tg_server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn direct_archive_only_variant_after_partial_publish_starts_a_clean_delivery_wave() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("partial-publish.zip");
    create_test_zip(&zip_path, 1);
    setup_chat(&repo, -100, true).await;

    let initial = repo
        .enqueue_eh_download(
            -100,
            914,
            "token",
            "Original 1280x request",
            true,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let old_download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_download.id, initial.job_id.unwrap());
    repo.mark_eh_job_downloaded(
        Main,
        old_download.id,
        old_download.started_at.unwrap(),
        1,
        zip_path.to_str().unwrap(),
        0,
    )
    .await
    .unwrap();
    let old_upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    repo.mark_eh_job_telegraph_ready(
        old_upload.id,
        old_upload.started_at.unwrap(),
        "https://telegra.ph/old-1280",
        None,
        None,
        true,
    )
    .await
    .unwrap();

    mock_tg_send_document(&tg_server).await;
    Mock::given(method("POST"))
        .and(path("/botfake_token/SendMessage"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "ok": false,
            "description": "mock Telegraph send failure"
        })))
        .expect(1)
        .mount(&tg_server)
        .await;
    let initial_worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );
    initial_worker.tick().await.unwrap();

    let partial = eh_download_queue::Entity::find_by_id(initial.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(partial.status, DELIVERY_STATUS_WAITING);
    assert!(partial.archive_sent_at.is_some());
    assert!(partial.telegraph_sent_at.is_none());
    assert!(partial.next_retry_at.is_some());

    let same_job = repo
        .enqueue_eh_download(
            -100,
            914,
            "token",
            "Same 1280x request",
            true,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(same_job.job_id, initial.job_id);
    assert_eq!(same_job.status, partial.status);
    assert_eq!(same_job.archive_sent_at, partial.archive_sent_at);
    assert_eq!(same_job.telegraph_sent_at, partial.telegraph_sent_at);
    assert_eq!(same_job.next_retry_at, partial.next_retry_at);

    let subscription_merge = repo
        .enqueue_eh_subscription_download(
            -100,
            914,
            914,
            "token",
            "Subscription original request",
            true,
            &EhGalleryVariant::archive("original"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(subscription_merge.job_id, initial.job_id);
    assert_eq!(subscription_merge.archive_sent_at, partial.archive_sent_at);
    assert_eq!(
        subscription_merge.telegraph_sent_at,
        partial.telegraph_sent_at
    );

    let forced = repo
        .enqueue_eh_download(
            -100,
            914,
            "token",
            "Direct original request",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("original"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(forced.id, initial.id);
    assert_eq!(forced.status, DELIVERY_STATUS_WAITING);
    assert_ne!(forced.job_id, initial.job_id);
    assert!(forced.archive_sent_at.is_none());
    assert!(forced.telegraph_sent_at.is_none());
    assert_eq!(forced.retry_count, 0);
    assert!(forced.started_at.is_none());
    assert!(forced.completed_at.is_none());
    assert!(forced.next_retry_at.is_none());
    assert_eq!(forced.source, SOURCE_DIRECT);
    assert!(!forced.telegraph);
    assert!(forced.subscription_ids.is_none());
    assert!(forced.telegraph_subscription_ids.is_none());
    assert_eq!(
        job_for_delivery(&repo, &initial).await.status,
        JOB_STATUS_RETIRED
    );

    let new_download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new_download.id, forced.job_id.unwrap());
    assert_eq!(new_download.resolution, "original");
    assert!(!new_download.telegraph_required);
    repo.mark_eh_job_downloaded(
        Main,
        new_download.id,
        new_download.started_at.unwrap(),
        1,
        zip_path.to_str().unwrap(),
        0,
    )
    .await
    .unwrap();

    tg_server.reset().await;
    mock_tg_send_document(&tg_server).await;
    let replacement_worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );
    replacement_worker.tick().await.unwrap();

    let requests = tg_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(body.contains("Direct original request.zip"));
    assert!(!body.contains("old-1280"));
}

#[tokio::test]
async fn publish_worker_claims_at_most_two_deliveries_and_refills() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("shared.zip");
    create_test_zip(&zip_path, 1);
    for chat_id in [-100, -200, -300] {
        setup_chat(&repo, chat_id, true).await;
        mock_tg_send_document_for_chat(&tg_server, chat_id, 200, None).await;
    }
    let (job, _) = seed_downloaded_job_with_deliveries(
        &repo,
        900,
        "publish-two",
        "Shared Publish",
        &zip_path,
        &[
            (-100, false, "One"),
            (-200, false, "Two"),
            (-300, false, "Three"),
        ],
    )
    .await;
    let mut config = make_config();
    config.publish_concurrency = 2;
    let send_entered = Arc::new(tokio::sync::Notify::new());
    let release_send = Arc::new(tokio::sync::Notify::new());
    let first_send = send_entered.notified();
    let second_send = send_entered.notified();
    tokio::pin!(first_send, second_send);
    // Register both waiters before starting the worker so notifications cannot coalesce.
    first_send.as_mut().enable();
    second_send.as_mut().enable();
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(config),
    )
    .with_test_send_hook(EhPublishSendHook {
        entered: Arc::clone(&send_entered),
        release: Arc::clone(&release_send),
        after_done: None,
    });

    let running = tokio::spawn(async move { worker.tick().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        first_send.await;
        second_send.await;
    })
    .await
    .expect("both publish slots must reach the send hook");
    let in_flight = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::JobId.eq(job.id))
        .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
        .all(repo.db())
        .await
        .unwrap();
    let waiting = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::JobId.eq(job.id))
        .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_WAITING))
        .all(repo.db())
        .await
        .unwrap();
    assert_eq!(in_flight.len(), 2);
    assert_eq!(waiting.len(), 1);

    release_send.notify_waiters();
    release_send.notify_one(); // Permit the third delivery when a slot is refilled.
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the third delivery must refill a completed slot")
        .unwrap()
        .unwrap();
    let done = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::JobId.eq(job.id))
        .filter(eh_download_queue::Column::Status.eq(STATUS_DONE))
        .all(repo.db())
        .await
        .unwrap();
    assert_eq!(done.len(), 3);
}

#[tokio::test]
async fn telegram_failure_retries_only_one_delivery_without_repeating_shared_work() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("shared.zip");
    create_test_zip(&zip_path, 1);
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;
    mock_tg_send_document_for_chat(&tg_server, -100, 400, None).await;
    mock_tg_send_document_for_chat(&tg_server, -200, 200, None).await;
    let (job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        901,
        "publish-retry",
        "Shared Retry",
        &zip_path,
        &[(-100, false, "Failing"), (-200, false, "Working")],
    )
    .await;
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();

    let failed = eh_download_queue::Entity::find_by_id(deliveries[0].id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    let succeeded = eh_download_queue::Entity::find_by_id(deliveries[1].id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, DELIVERY_STATUS_WAITING);
    assert_eq!(failed.retry_count, 1);
    assert_eq!(succeeded.status, STATUS_DONE);
    assert_eq!(
        job_for_delivery(&repo, &succeeded).await.status,
        JOB_STATUS_DOWNLOADED
    );
    assert!(eh_server.received_requests().await.unwrap().is_empty());
    assert_eq!(job.id, succeeded.job_id.unwrap());
}

#[tokio::test]
async fn archive_only_delivery_bypasses_upload_wait_and_disabled_chat_does_not_block_sibling() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("archive-only.zip");
    create_test_zip(&zip_path, 1);
    setup_chat(&repo, -100, false).await;
    setup_chat(&repo, -200, true).await;
    mock_tg_send_document_for_chat(&tg_server, -200, 200, None).await;
    let (job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        902,
        "archive-only",
        "Archive Only",
        &zip_path,
        &[(-100, false, "Disabled"), (-200, false, "Enabled")],
    )
    .await;
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRequired,
            Expr::value(true),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphStatus,
            Expr::value(TELEGRAPH_STATUS_UPLOADING),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();

    let deferred = eh_download_queue::Entity::find_by_id(deliveries[0].id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    let done = eh_download_queue::Entity::find_by_id(deliveries[1].id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deferred.status, DELIVERY_STATUS_WAITING);
    assert_eq!(deferred.retry_count, 0);
    assert!(deferred.next_retry_at.is_some());
    assert_eq!(done.status, STATUS_DONE);
    assert_eq!(
        job_for_delivery(&repo, &done).await.telegraph_status,
        TELEGRAPH_STATUS_UPLOADING
    );
}

#[tokio::test]
async fn missing_ready_zip_resets_one_job_generation_for_all_archive_consumers() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("gone.zip");
    create_test_zip(&zip_path, 1);
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;
    let (job, _) = seed_downloaded_job_with_deliveries(
        &repo,
        903,
        "missing-zip",
        "Missing ZIP",
        &zip_path,
        &[(-100, false, "First"), (-200, false, "Second")],
    )
    .await;
    let expected_started_at = Local::now().naive_local();
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::StartedAt,
            Expr::value(Some(expected_started_at)),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    std::fs::remove_file(&zip_path).unwrap();
    let claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    let claimed_delivery_id = claim.delivery.id;
    let expected_zip_path = claim.job.zip_path.clone().unwrap();
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );

    worker.process_claimed(claim).await.unwrap();

    assert!(
        repo.get_next_eh_delivery_for_publish(true)
            .await
            .unwrap()
            .is_none(),
        "the reset job is not publishable until its shared redownload is due"
    );
    let reset = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reset.status, JOB_STATUS_PENDING);
    assert!(reset.zip_path.is_none());
    assert_eq!(reset.retry_count, 1);
    let waiting = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::JobId.eq(job.id))
        .filter(eh_download_queue::Column::Status.eq(DELIVERY_STATUS_WAITING))
        .all(repo.db())
        .await
        .unwrap();
    assert_eq!(waiting.len(), 2);
    assert!(waiting.iter().all(|delivery| delivery.retry_count == 0));
    assert!(waiting
        .iter()
        .find(|delivery| delivery.id == claimed_delivery_id)
        .unwrap()
        .next_retry_at
        .is_some());
    assert_eq!(
        repo.reset_eh_job_for_missing_zip(
            job.id,
            expected_started_at,
            &expected_zip_path,
            make_config().max_retry_count,
        )
        .await
        .unwrap(),
        EhMissingZipResetOutcome::Stale,
        "the second concurrent-style reset must not consume another shared retry"
    );
}

#[tokio::test]
async fn missing_zip_reset_at_retry_limit_fails_job_and_deliveries_once() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("at-limit-gone.zip");
    create_test_zip(&zip_path, 1);
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;
    let (job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        907,
        "at-limit",
        "At Limit",
        &zip_path,
        &[(-100, false, "First"), (-200, false, "Second")],
    )
    .await;
    let expected_started_at = Local::now().naive_local();
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::StartedAt,
            Expr::value(Some(expected_started_at)),
        )
        .col_expr(
            eh_gallery_jobs::Column::RetryCount,
            Expr::value(make_config().max_retry_count as i32),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    std::fs::remove_file(&zip_path).unwrap();
    let claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    let expected_zip_path = claim.job.zip_path.clone().unwrap();
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );

    worker
        .handle_missing_zip(&claim.delivery, &claim.job)
        .await
        .unwrap();

    // The terminal download failure stays log + /estatus: no Telegram
    // notification and no provider request is allowed.
    assert!(tg_server.received_requests().await.unwrap().is_empty());
    assert!(eh_server.received_requests().await.unwrap().is_empty());

    let failed_job = job_for_delivery(&repo, &claim.delivery).await;
    assert_eq!(failed_job.status, JOB_STATUS_FAILED);
    assert!(failed_job.next_retry_at.is_none());
    assert_eq!(
        failed_job.zip_path.as_deref(),
        Some(expected_zip_path.as_str()),
        "durable cleanup must keep owning the leftover archive family"
    );
    for delivery in &deliveries {
        let failed_delivery = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed_delivery.status, DELIVERY_STATUS_FAILED);
    }

    // A repeated publish claim can never resurrect the failed job.
    assert!(repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .is_none());
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
}

#[tokio::test]
async fn late_telegraph_demand_missing_zip_defers_archive_and_advances_upload_generation() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("late-demand-gone.zip");
    create_test_zip(&zip_path, 1);
    let variant = EhGalleryVariant::archive("1280x");
    let archive = repo
        .enqueue_eh_download(
            -100,
            905,
            "late-demand",
            "Late demand",
            false,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    repo.enqueue_eh_download(
        -200,
        905,
        "late-demand",
        "Late demand",
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
    let downloaded_generation = download.started_at.unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        download.id,
        downloaded_generation,
        10,
        zip_path.to_str().unwrap(),
        0,
    )
    .await
    .unwrap();
    let failed_upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    let failed_upload_generation = failed_upload.started_at.unwrap();
    assert!(matches!(
        repo.record_eh_job_upload_failure(
            failed_upload.id,
            failed_upload_generation,
            "terminal provider failure",
            0,
            true,
        )
        .await
        .unwrap(),
        EhJobUploadFailureOutcome::Terminal { .. }
    ));

    let late = repo
        .enqueue_eh_download(
            -300,
            905,
            "late-demand",
            "Late demand",
            true,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    assert_eq!(late.job_id, Some(download.id));
    let after_late_demand = job_for_delivery(&repo, &late).await;
    assert_eq!(
        after_late_demand.started_at,
        Some(failed_upload_generation),
        "late upload demand must retain the prior generation fence"
    );
    assert_eq!(after_late_demand.telegraph_status, TELEGRAPH_STATUS_PENDING);

    std::fs::remove_file(&zip_path).unwrap();
    let archive_claim = repo
        .get_next_eh_delivery_for_publish(true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(archive_claim.delivery.id, archive.id);
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );
    worker
        .handle_missing_zip(&archive_claim.delivery, &archive_claim.job)
        .await
        .unwrap();

    let reset = job_for_delivery(&repo, &archive).await;
    assert_eq!(reset.status, JOB_STATUS_PENDING);
    assert!(reset.zip_path.is_none());
    assert_eq!(reset.started_at, Some(failed_upload_generation));
    assert_eq!(
        eh_download_queue::Entity::find_by_id(archive.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .status,
        DELIVERY_STATUS_WAITING
    );

    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::NextRetryAt,
            Expr::value(None::<chrono::NaiveDateTime>),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(download.id))
        .exec(repo.db())
        .await
        .unwrap();
    let replacement_download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    let replacement_generation = replacement_download.started_at.unwrap();
    assert!(replacement_generation > failed_upload_generation);
    create_test_zip(&zip_path, 1);
    repo.mark_eh_job_downloaded(
        Main,
        replacement_download.id,
        replacement_generation,
        10,
        zip_path.to_str().unwrap(),
        0,
    )
    .await
    .unwrap();
    let replacement_upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    assert!(
        replacement_upload.started_at.unwrap() > replacement_generation,
        "the replacement upload must own a strictly newer generation"
    );
}

#[tokio::test]
async fn malformed_missing_zip_state_defers_publishing_delivery_before_error() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("malformed.zip");
    create_test_zip(&zip_path, 1);
    let (job, deliveries) = seed_downloaded_job_with_deliveries(
        &repo,
        906,
        "malformed",
        "Malformed",
        &zip_path,
        &[(-100, false, "Archive")],
    )
    .await;
    let delivery = deliveries.into_iter().next().unwrap();
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(crate::db::repo::eh_download_queue::STATUS_PUBLISHING),
        )
        .filter(eh_download_queue::Column::Id.eq(delivery.id))
        .exec(repo.db())
        .await
        .unwrap();
    let publishing = eh_download_queue::Entity::find_by_id(delivery.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    let malformed_job = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert!(malformed_job.started_at.is_none());
    let worker = EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(make_config()),
    );

    let error = worker
        .handle_missing_zip(&publishing, &malformed_job)
        .await
        .expect_err("missing generation must still release the delivery first");
    assert!(error
        .to_string()
        .contains("Missing shared EH ZIP reset requires a persisted generation"));
    let deferred = eh_download_queue::Entity::find_by_id(delivery.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deferred.status, DELIVERY_STATUS_WAITING);
    assert!(deferred.next_retry_at.is_some());
}

#[tokio::test]
async fn old_publish_claim_cannot_reset_a_newer_same_path_job_generation() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("same-path.zip");
    create_test_zip(&zip_path, 1);
    let delivery = repo
        .enqueue_eh_download(
            -100,
            904,
            "same-path",
            "Generation fence",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let claim = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        claim.id,
        claim.started_at.unwrap(),
        10,
        zip_path.to_str().unwrap(),
        0,
    )
    .await
    .unwrap();
    let job = job_for_delivery(&repo, &delivery).await;
    let old_generation = job.started_at.unwrap();
    let new_generation = old_generation + chrono::Duration::seconds(1);
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::StartedAt,
            Expr::value(Some(new_generation)),
        )
        .col_expr(eh_gallery_jobs::Column::RetryCount, Expr::value(7_i32))
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    let before = job_for_delivery(&repo, &delivery).await;

    assert_eq!(
        repo.reset_eh_job_for_missing_zip(
            job.id,
            old_generation,
            zip_path.to_str().unwrap(),
            make_config().max_retry_count,
        )
        .await
        .unwrap(),
        EhMissingZipResetOutcome::Stale,
        "an old publishing claim must not reset a newer generation sharing the same path"
    );

    let after = job_for_delivery(&repo, &delivery).await;
    assert_eq!(after.status, before.status);
    assert_eq!(after.started_at, Some(new_generation));
    assert_eq!(after.zip_path, before.zip_path);
    assert_eq!(after.retry_count, before.retry_count);
    assert_eq!(
        eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .status,
        DELIVERY_STATUS_WAITING
    );
}
