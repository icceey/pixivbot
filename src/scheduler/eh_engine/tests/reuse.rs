use super::*;

#[tokio::test]
async fn chat_b_reuses_retired_gallery_result_without_source_work() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let telegram_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let gid = 9_801;
    let token = "abcd980001";
    let fingerprint = "9801|2|100|false";
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;

    let chat_a = repo
        .enqueue_eh_download(
            -100,
            gid,
            token,
            "Reusable Gallery",
            true,
            SOURCE_DIRECT,
            &variant,
            Some(fingerprint),
            true,
        )
        .await
        .unwrap()
        .expect("chat A must start the initial subscription wave");
    let source_zip = temp.path().join("initial-source.zip");
    create_test_zip(&source_zip, 2);
    let download_url = format!("{}/archive/{gid}/0", eh_server.uri());
    mock_archiver(
        &eh_server,
        gid as u64,
        token,
        ArchiverPage {
            keyed: true,
            ..Default::default()
        },
    )
    .await;
    mock_eh_archiver_post(&eh_server, &download_url).await;
    mock_eh_archive_download(
        &eh_server,
        &format!("/archive/{gid}/0"),
        std::fs::read(&source_zip).unwrap(),
    )
    .await;

    let archive_config = Arc::new(make_config());
    EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::clone(&archive_config),
        temp.path().to_path_buf(),
        Main,
        None,
    )
    .tick()
    .await
    .unwrap();
    let downloaded_a = job_for_delivery(&repo, &chat_a).await;
    assert_eq!(
        downloaded_a.status, JOB_STATUS_DOWNLOADED,
        "{downloaded_a:#?}"
    );
    assert_eq!(downloaded_a.gp_cost, 0);
    assert!(downloaded_a.telegraph_required, "{downloaded_a:#?}");
    assert_eq!(
        downloaded_a.telegraph_status, TELEGRAPH_STATUS_PENDING,
        "{downloaded_a:#?}"
    );
    let uploader = Arc::new(ZipFirstMockUploader::default());
    mock_telegraph_create_page(&telegram_server).await;
    EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&telegram_server),
        make_telegraph_client(&telegram_server),
        uploader.clone(),
        None,
        Some(IpfS3PreviewRewriteConfig {
            preview_gateway_url: "https://preview.example".to_string(),
            public_gateway_url: "https://public.example".to_string(),
            delay_sec: 60,
        }),
        Arc::clone(&archive_config),
    )
    .tick()
    .await
    .unwrap();
    let ready_a = job_for_delivery(&repo, &chat_a).await;
    assert_eq!(ready_a.telegraph_status, TELEGRAPH_STATUS_READY);
    let reusable_result = eh_gallery_results::Entity::find()
        .filter(eh_gallery_results::Column::Gid.eq(gid))
        .filter(eh_gallery_results::Column::Token.eq(token))
        .one(repo.db())
        .await
        .unwrap()
        .expect("the IPFS3 upload must persist a reusable gallery result");
    assert_eq!(reusable_result.source_fingerprint, fingerprint);
    assert!(reusable_result.media_cids.is_some());

    mock_tg_send_document(&telegram_server).await;
    mock_tg_send_message(&telegram_server).await;
    EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&telegram_server),
        make_eh_client(&eh_server),
        None,
        Arc::clone(&archive_config),
    )
    .tick()
    .await
    .unwrap();
    let completed_a = eh_download_queue::Entity::find_by_id(chat_a.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed_a.status, STATUS_DONE);
    assert!(completed_a.archive_sent_at.is_some());
    assert!(completed_a.telegraph_sent_at.is_some());
    let first_job = job_for_delivery(&repo, &completed_a).await;
    let first_zip = first_job.zip_path.clone().expect("initial archive path");
    assert_eq!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), None, 0, true)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::CleanRetired)
    );
    assert!(!std::path::Path::new(&first_zip).exists());
    assert_eq!(
        eh_gallery_jobs::Entity::find_by_id(first_job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .status,
        JOB_STATUS_RETIRED
    );
    let archive_posts_before_reuse = eh_server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
        })
        .count();
    let upload_calls_before_reuse = uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst);

    let b_task = repo
        .get_or_create_task(TaskType::Ehentai, "eh:artist:cache-reuse".to_string(), None)
        .await
        .unwrap();
    repo.upsert_eh_subscription(
        -200,
        b_task.id,
        crate::db::types::TagFilter::default(),
        None,
    )
    .await
    .unwrap();
    let b_subscription = repo
        .list_subscriptions_by_task(b_task.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let chat_b = repo
        .enqueue_eh_subscription_download(
            -200,
            b_subscription.id,
            gid,
            token,
            "Reusable Gallery",
            true,
            &variant,
            Some(fingerprint),
            false,
        )
        .await
        .unwrap()
        .expect("chat B must receive a Telegraph-only subscription wave");
    let reused_job = job_for_delivery(&repo, &chat_b).await;
    assert_eq!(reused_job.status, JOB_STATUS_DOWNLOADED);
    assert_eq!(
        reused_job.telegraph_url.as_deref(),
        Some(reusable_result.telegraph_url.as_str())
    );
    assert!(reused_job.zip_path.is_none());
    assert!(repo
        .claim_eh_download_job(Main, false)
        .await
        .unwrap()
        .is_none());
    assert!(repo
        .claim_eh_download_job(Background, false)
        .await
        .unwrap()
        .is_none());
    assert_eq!(reused_job.telegraph_status, TELEGRAPH_STATUS_READY);
    assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());

    EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&telegram_server),
        make_eh_client(&eh_server),
        None,
        Arc::new(EhentaiConfig {
            send_archive: false,
            ..make_config()
        }),
    )
    .tick()
    .await
    .unwrap();
    let completed_b = eh_download_queue::Entity::find_by_id(chat_b.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed_b.status, STATUS_DONE);
    let b_ledger = eh_gallery_push_ledger::Entity::find()
        .filter(eh_gallery_push_ledger::Column::ChatId.eq(-200))
        .filter(eh_gallery_push_ledger::Column::Gid.eq(gid))
        .one(repo.db())
        .await
        .unwrap()
        .expect("cached Telegraph publish must write chat B's ledger row");
    assert!(b_ledger.archive_sent_at.is_none());
    assert!(b_ledger.telegraph_sent_at.is_some());
    assert_eq!(
        eh_gallery_jobs::Entity::find_by_id(reused_job.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .status,
        JOB_STATUS_RETIRED
    );
    assert_eq!(
        eh_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
            })
            .count(),
        archive_posts_before_reuse,
        "cache reuse must not start another source archive request"
    );
    assert_eq!(
        uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
        upload_calls_before_reuse,
        "cache reuse must not invoke the upload provider"
    );
}

#[tokio::test]
async fn fingerprint_change_forces_full_refetch() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let telegram_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let gid = 9_802;
    let token = "abcd980002";
    let old_fingerprint = "9802|2|100|false";
    let new_fingerprint = "9802|3|100|false";
    setup_chat(&repo, -100, true).await;
    let old_result = eh_gallery_results::ActiveModel {
        gid: Set(gid),
        token: Set(token.to_string()),
        download_mode: Set(variant.download_mode.clone()),
        resolution: Set(variant.resolution.clone()),
        source_fingerprint: Set(old_fingerprint.to_string()),
        telegraph_url: Set("https://telegra.ph/stale-result".to_string()),
        media_cids: Set(Some(
            "[{\"name\":\"001.jpg\",\"cid\":\"bafk-old\"}]".to_string(),
        )),
        created_at: Set(Local::now().naive_local()),
        updated_at: Set(Local::now().naive_local()),
        ..Default::default()
    }
    .insert(repo.db())
    .await
    .unwrap();
    let delivery = repo
        .enqueue_eh_download(
            -100,
            gid,
            token,
            "Changed Gallery",
            true,
            SOURCE_DIRECT,
            &variant,
            Some(new_fingerprint),
            false,
        )
        .await
        .unwrap()
        .expect("a changed filecount must start a new source wave");
    assert_eq!(
        job_for_delivery(&repo, &delivery).await.status,
        STATUS_PENDING
    );

    let source_zip = temp.path().join("changed-source.zip");
    create_test_zip(&source_zip, 3);
    let download_url = format!("{}/archive/{gid}/0", eh_server.uri());
    mock_archiver(
        &eh_server,
        gid as u64,
        token,
        ArchiverPage {
            keyed: true,
            ..Default::default()
        },
    )
    .await;
    mock_eh_archiver_post(&eh_server, &download_url).await;
    mock_eh_archive_download(
        &eh_server,
        &format!("/archive/{gid}/0"),
        std::fs::read(&source_zip).unwrap(),
    )
    .await;
    let no_archive_config = Arc::new(EhentaiConfig {
        send_archive: false,
        ..make_config()
    });
    EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::clone(&no_archive_config),
        temp.path().to_path_buf(),
        Main,
        None,
    )
    .tick()
    .await
    .unwrap();
    let uploader = Arc::new(ZipFirstMockUploader::default());
    mock_telegraph_create_page(&telegram_server).await;
    EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&telegram_server),
        make_telegraph_client(&telegram_server),
        uploader.clone(),
        None,
        Some(IpfS3PreviewRewriteConfig {
            preview_gateway_url: "https://preview.example".to_string(),
            public_gateway_url: "https://public.example".to_string(),
            delay_sec: 60,
        }),
        Arc::clone(&no_archive_config),
    )
    .tick()
    .await
    .unwrap();

    let refetched_job = job_for_delivery(&repo, &delivery).await;
    assert_eq!(refetched_job.status, JOB_STATUS_DOWNLOADED);
    assert_eq!(refetched_job.telegraph_status, TELEGRAPH_STATUS_READY);
    assert_eq!(
        refetched_job.source_fingerprint.as_deref(),
        Some(new_fingerprint)
    );
    assert_eq!(
        eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .status,
        DELIVERY_STATUS_WAITING
    );
    let updated_result = eh_gallery_results::Entity::find()
        .filter(eh_gallery_results::Column::Gid.eq(gid))
        .filter(eh_gallery_results::Column::Token.eq(token))
        .one(repo.db())
        .await
        .unwrap()
        .expect("the refreshed upload must replace the stale result record");
    assert_eq!(updated_result.id, old_result.id);
    assert_eq!(updated_result.source_fingerprint, new_fingerprint);
    assert_ne!(updated_result.telegraph_url, old_result.telegraph_url);
    assert_eq!(
        eh_gallery_results::Entity::find()
            .filter(eh_gallery_results::Column::Gid.eq(gid))
            .filter(eh_gallery_results::Column::Token.eq(token))
            .count(repo.db())
            .await
            .unwrap(),
        1
    );
    let requests = eh_server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
            })
            .count(),
        1,
        "a fingerprint change must issue one new source archive POST"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.method.as_str() == "GET"
                    && request.url.path() == format!("/archive/{gid}/0")
            })
            .count(),
        1,
        "a fingerprint change must download the new archive once"
    );
    assert_eq!(
        uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a fingerprint change must upload the refreshed archive once"
    );
}

#[tokio::test]
async fn terminal_upload_failure_fallback_then_later_telegraph_subscription() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let telegram_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let variant = EhGalleryVariant::archive("1280x");
    let gid = 9_803;
    let token = "fallback-token";
    setup_chat(&repo, -100, true).await;

    let first = repo
        .enqueue_eh_download(
            -100,
            gid,
            token,
            "Fallback Gallery",
            true,
            SOURCE_DIRECT,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("initial mixed subscription delivery");
    let first_source = temp.path().join("fallback-first.zip");
    create_test_zip(&first_source, 2);
    let first_claim = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        first_claim.id,
        first_claim.started_at.unwrap(),
        std::fs::metadata(&first_source).unwrap().len() as i64,
        first_source.to_str().unwrap(),
        0,
    )
    .await
    .unwrap();
    mock_tg_send_message(&telegram_server).await;
    let mut failing_config = make_config();
    failing_config.max_retry_count = 0;
    let failing_uploader = Arc::new(AlwaysFailUploader {
        message: "terminal provider failure".to_string(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&telegram_server),
        make_telegraph_client(&telegram_server),
        failing_uploader.clone(),
        None,
        None,
        Arc::new(failing_config),
    )
    .tick()
    .await
    .unwrap();
    assert_eq!(
        failing_uploader
            .calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let failed_job = job_for_delivery(&repo, &first).await;
    assert_eq!(failed_job.status, JOB_STATUS_DOWNLOADED);
    assert_eq!(failed_job.telegraph_status, TELEGRAPH_STATUS_FAILED);
    let fallback = eh_download_queue::Entity::find_by_id(first.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fallback.status, DELIVERY_STATUS_WAITING);
    assert!(!fallback.telegraph);

    mock_tg_send_document(&telegram_server).await;
    let archive_config = Arc::new(make_config());
    EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&telegram_server),
        make_eh_client(&eh_server),
        None,
        Arc::clone(&archive_config),
    )
    .tick()
    .await
    .unwrap();
    let archive_ledger = eh_gallery_push_ledger::Entity::find()
        .filter(eh_gallery_push_ledger::Column::ChatId.eq(-100))
        .filter(eh_gallery_push_ledger::Column::Gid.eq(gid))
        .one(repo.db())
        .await
        .unwrap()
        .expect("archive fallback must persist its ledger marker");
    assert!(archive_ledger.archive_sent_at.is_some());
    assert!(archive_ledger.telegraph_sent_at.is_none());
    assert_eq!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), None, 0, true)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::CleanRetired)
    );
    let document_sends_before_later_subscription = telegram_server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path().ends_with("/SendDocument"))
        .count();

    let later_task = repo
        .get_or_create_task(
            TaskType::Ehentai,
            "eh:artist:fallback-later".to_string(),
            None,
        )
        .await
        .unwrap();
    repo.upsert_eh_subscription(
        -100,
        later_task.id,
        crate::db::types::TagFilter::default(),
        None,
    )
    .await
    .unwrap();
    let later_subscription = repo
        .list_subscriptions_by_task(later_task.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let later = repo
        .enqueue_eh_subscription_download(
            -100,
            later_subscription.id,
            gid,
            token,
            "Fallback Gallery",
            true,
            &variant,
            None,
            true,
        )
        .await
        .unwrap()
        .expect("the missing Telegraph surface must start a later subscription wave");
    assert_eq!(later.archive_sent_at, archive_ledger.archive_sent_at);
    assert!(later.telegraph_sent_at.is_none());
    let second_source = temp.path().join("fallback-second.zip");
    create_test_zip(&second_source, 2);
    let later_claim = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    repo.mark_eh_job_downloaded(
        Main,
        later_claim.id,
        later_claim.started_at.unwrap(),
        std::fs::metadata(&second_source).unwrap().len() as i64,
        second_source.to_str().unwrap(),
        0,
    )
    .await
    .unwrap();
    let succeeding_uploader = Arc::new(ZipFirstMockUploader::default());
    mock_telegraph_create_page(&telegram_server).await;
    EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&telegram_server),
        make_telegraph_client(&telegram_server),
        succeeding_uploader.clone(),
        None,
        Some(IpfS3PreviewRewriteConfig {
            preview_gateway_url: "https://preview.example".to_string(),
            public_gateway_url: "https://public.example".to_string(),
            delay_sec: 60,
        }),
        Arc::clone(&archive_config),
    )
    .tick()
    .await
    .unwrap();
    let ready_later = job_for_delivery(&repo, &later).await;
    assert_eq!(ready_later.status, JOB_STATUS_DOWNLOADED);
    assert_eq!(ready_later.telegraph_status, TELEGRAPH_STATUS_READY);
    assert_eq!(
        ready_later.cleanup_status, CLEANUP_STATUS_PENDING,
        "the archive is no longer an active surface after its persisted marker"
    );
    assert_eq!(
        run_eh_job_cleanup_maintenance_once(repo.as_ref(), None, 0, true)
            .await
            .unwrap(),
        Some(EhCleanupFinalizeOutcome::FinalizedWithoutSourceWork)
    );
    EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&telegram_server),
        make_eh_client(&eh_server),
        None,
        Arc::clone(&archive_config),
    )
    .tick()
    .await
    .unwrap();
    assert_eq!(
        succeeding_uploader
            .zip_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let final_delivery = eh_download_queue::Entity::find_by_id(later.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_delivery.status, STATUS_DONE);
    assert_eq!(
        final_delivery.archive_sent_at,
        archive_ledger.archive_sent_at
    );
    assert!(final_delivery.telegraph_sent_at.is_some());
    let final_ledger = eh_gallery_push_ledger::Entity::find()
        .filter(eh_gallery_push_ledger::Column::ChatId.eq(-100))
        .filter(eh_gallery_push_ledger::Column::Gid.eq(gid))
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_ledger.archive_sent_at, archive_ledger.archive_sent_at);
    assert!(final_ledger.telegraph_sent_at.is_some());
    assert_eq!(
        telegram_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().ends_with("/SendDocument"))
            .count(),
        document_sends_before_later_subscription,
        "the later Telegraph-only wave must not resend the archive"
    );
}

#[tokio::test]
async fn overlapping_subscriptions_deliver_gallery_once() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let telegram_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let gid = 9_804_i64;
    let token = "abcd980004";
    setup_chat(&repo, -100, true).await;
    let task_a = repo
        .get_or_create_task(TaskType::Ehentai, "eh:artist:overlap-a".to_string(), None)
        .await
        .unwrap();
    let task_b = repo
        .get_or_create_task(TaskType::Ehentai, "eh:artist:overlap-b".to_string(), None)
        .await
        .unwrap();
    let telegraph_filter = Some(EhFilter {
        telegraph: true,
        ..Default::default()
    });
    repo.upsert_eh_subscription(
        -100,
        task_a.id,
        crate::db::types::TagFilter::default(),
        telegraph_filter.clone(),
    )
    .await
    .unwrap();
    repo.upsert_eh_subscription(
        -100,
        task_b.id,
        crate::db::types::TagFilter::default(),
        telegraph_filter,
    )
    .await
    .unwrap();
    let subscription_a = repo
        .list_subscriptions_by_task(task_a.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let subscription_b = repo
        .list_subscriptions_by_task(task_b.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let gallery = EhGallery {
        gid: gid as u64,
        token: token.to_string(),
        title: "Overlapping Gallery".to_string(),
        title_jpn: None,
        category: "Doujinshi".to_string(),
        thumb: "https://ehgt.org/t/9804.jpg".to_string(),
        uploader: "tester".to_string(),
        posted: gid,
        filecount: 2,
        filesize: 100,
        expunged: false,
        rating: 4.0,
        tags: vec![
            "artist:overlap-a".to_string(),
            "artist:overlap-b".to_string(),
        ],
    };
    let config = Arc::new(make_config());
    let engine = EhEngine::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::clone(&config),
        true,
        60,
    );
    engine
        .process_eh_sub_with_slots(&subscription_a, std::slice::from_ref(&gallery), 1)
        .await
        .unwrap();
    let first_delivery = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::ChatId.eq(-100))
        .filter(eh_download_queue::Column::Gid.eq(gid))
        .one(repo.db())
        .await
        .unwrap()
        .expect("the first subscription should enqueue the gallery");
    assert_eq!(first_delivery.subscription_ids.as_deref(), Some("1"));

    let source_zip = temp.path().join("overlap-source.zip");
    create_test_zip(&source_zip, 2);
    let download_url = format!("{}/archive/{gid}/0", eh_server.uri());
    mock_archiver(
        &eh_server,
        gid as u64,
        token,
        ArchiverPage {
            keyed: true,
            ..Default::default()
        },
    )
    .await;
    mock_eh_archiver_post(&eh_server, &download_url).await;
    mock_eh_archive_download(
        &eh_server,
        &format!("/archive/{gid}/0"),
        std::fs::read(&source_zip).unwrap(),
    )
    .await;
    EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::clone(&config),
        temp.path().to_path_buf(),
        Main,
        None,
    )
    .tick()
    .await
    .unwrap();
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
        Arc::clone(&config),
    )
    .tick()
    .await
    .unwrap();
    let ready_first = job_for_delivery(&repo, &first_delivery).await;
    assert_eq!(ready_first.status, JOB_STATUS_DOWNLOADED);
    assert_eq!(ready_first.telegraph_status, TELEGRAPH_STATUS_READY);
    mock_tg_send_document(&telegram_server).await;
    mock_tg_send_message(&telegram_server).await;
    EhPublishWorker::new(
        Arc::clone(&repo),
        make_notifier(&telegram_server),
        make_eh_client(&eh_server),
        None,
        Arc::clone(&config),
    )
    .tick()
    .await
    .unwrap();
    let ledger = eh_gallery_push_ledger::Entity::find()
        .filter(eh_gallery_push_ledger::Column::ChatId.eq(-100))
        .filter(eh_gallery_push_ledger::Column::Gid.eq(gid))
        .one(repo.db())
        .await
        .unwrap()
        .expect("the first completed subscription must mark both surfaces");
    assert!(ledger.archive_sent_at.is_some());
    assert!(ledger.telegraph_sent_at.is_some());

    engine
        .process_eh_sub_with_slots(&subscription_b, std::slice::from_ref(&gallery), 1)
        .await
        .unwrap();
    assert_eq!(
        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(-100))
            .filter(eh_download_queue::Column::Gid.eq(gid))
            .count(repo.db())
            .await
            .unwrap(),
        1,
        "the fully ledgered second subscription must enqueue no delivery wave"
    );
    let updated_b = repo
        .list_subscriptions_by_task(task_b.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let state_b = eh_tag_subscription_state(&updated_b).unwrap();
    assert_eq!(state_b.pushed_gids, vec![gid as u64]);
    assert!(state_b.pending_galleries.is_empty());
    let requests = telegram_server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/SendDocument"))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/SendMessage"))
            .count(),
        1,
        "overlapping subscriptions must publish one gallery link"
    );
}
