use super::*;

#[tokio::test]
async fn download_workers_reject_gp_policy_before_spending_or_size_retry() {
    for background in [false, true] {
        for sizes in [None, Some(("400.0 MiB", "300.01 MiB"))] {
            let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
            let eh_server = MockServer::start().await;
            let temp = tempfile::tempdir().unwrap();

            setup_chat(&repo, -100, true).await;
            let entry = seed_delivery(
                &repo,
                -100,
                (2284788, "7841d194d4", "GP Required Gallery"),
                Default::default(),
            )
            .await;

            mock_archiver(
                &eh_server,
                2284788,
                "7841d194d4",
                ArchiverPage {
                    original_cost: "8,800 GP",
                    resample_cost: "N/A",
                    sizes,
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
            config.max_archive_gp_cost = 0;
            config.max_archive_size_mb = 300;
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
            assert_eq!(updated.status, STATUS_FAILED);
            assert_eq!(
                updated.error.as_deref(),
                Some("EH archive GP cost 8800 exceeds configured max_archive_gp_cost=0")
            );
            assert!(updated.completed_at.is_some());
            assert!(updated.started_at.is_some());
            assert!(updated.next_retry_at.is_none());
            assert_eq!(
                updated.retry_count, 0,
                "policy reject must not consume retries"
            );
            let delivery = eh_download_queue::Entity::find_by_id(entry.id)
                .one(repo.db())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(delivery.status, STATUS_FAILED);
            assert!(delivery.error.is_none());
            assert!(gp_attempts(repo.as_ref()).await.is_empty());
            assert!(updated.background_download_status.is_none());
            assert!(updated.background_download_started_at.is_none());
            assert!(updated.background_download_next_retry_at.is_none());
            assert!(updated.background_download_error.is_none());
            assert_eq!(updated.background_download_attempt_count, 0);
        }
    }
}

#[tokio::test]
async fn download_workers_record_gp_even_when_archive_redirect_is_malformed() {
    for background in [false, true] {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = seed_delivery(
            &repo,
            -100,
            (2284788, "7841d194d4", "Paid malformed redirect gallery"),
            Default::default(),
        )
        .await;
        mock_archiver(
            &eh_server,
            2284788,
            "7841d194d4",
            ArchiverPage {
                original_cost: "8,800 GP",
                resample_cost: "218 GP",
                ..Default::default()
            },
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>no redirect</html>"))
            .expect(1)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.max_archive_gp_cost = 218;
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
        assert_eq!(updated.status, STATUS_PENDING);
        if background {
            assert_eq!(
                updated.background_download_status.as_deref(),
                Some(BACKGROUND_STATUS_PENDING)
            );
            assert_eq!(updated.background_download_attempt_count, 1);
            assert_eq!(updated.retry_count, 0);
        } else {
            assert_eq!(updated.retry_count, 1);
        }
        let attempts = gp_attempts(repo.as_ref()).await;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].job_id, entry.job_id);
        assert_eq!(attempts[0].queue_id, None);
        assert_eq!(attempts[0].gp_cost, 218);
    }
}

#[tokio::test]
async fn test_download_worker_gp_attempt_insert_failure_retries_without_post() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, true).await;
    let entry = seed_delivery(
        &repo,
        -100,
        (2284788, "7841d194d4", "Paid trigger failure gallery"),
        Default::default(),
    )
    .await;
    mock_archiver(
        &eh_server,
        2284788,
        "7841d194d4",
        ArchiverPage {
            original_cost: "8,800 GP",
            resample_cost: "218 GP",
            ..Default::default()
        },
    )
    .await;
    repo.db()
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                "CREATE TRIGGER fail_eh_gp_spend_attempt_insert BEFORE INSERT ON eh_gp_spend_attempts BEGIN SELECT RAISE(FAIL, 'ledger insert blocked'); END;".to_owned(),
            ))
            .await
            .unwrap();
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not be called"))
        .expect(0)
        .mount(&eh_server)
        .await;

    let mut config = make_config();
    config.background_download_enabled = false;
    config.max_archive_gp_cost = 218;
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
    assert_eq!(updated.retry_count, 1);
    assert!(gp_attempts(repo.as_ref()).await.is_empty());
}

#[tokio::test]
async fn test_download_worker_gp_rate_limit_defers_without_post() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, true).await;

    // An upgraded database may retain GP attempts owned by old queue rows.
    // They must still consume the shared budget before any new Job POST.
    // Queue `gp_cost` metadata must not affect the rate-limit calculation.
    let prior_entry = eh_download_queue::ActiveModel {
        chat_id: Set(-100),
        gid: Set(999999),
        token: Set("a1b2c3d4".to_string()),
        title: Set("Previous paid gallery".to_string()),
        source: Set(SOURCE_DIRECT.to_string()),
        status: Set(STATUS_DONE.to_string()),
        created_at: Set(Local::now().naive_local()),
        ..Default::default()
    }
    .insert(repo.db())
    .await
    .unwrap();
    eh_gp_spend_attempts::ActiveModel {
        queue_id: Set(Some(prior_entry.id)),
        gid: Set(prior_entry.gid),
        gp_cost: Set(1000),
        created_at: Set(Local::now().naive_local()),
        ..Default::default()
    }
    .insert(repo.db())
    .await
    .unwrap();

    let entry = seed_delivery(
        &repo,
        -100,
        (2284788, "7841d194d4", "GP Required Gallery"),
        Default::default(),
    )
    .await;

    // New download costs 218 GP. With gp_rate_limit = 1000 and 1000 already
    // spent, the new download would push total to 1218 > 1000, so it must defer.
    mock_archiver(
        &eh_server,
        2284788,
        "7841d194d4",
        ArchiverPage {
            original_cost: "8,800 GP",
            resample_cost: "218 GP",
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
    config.background_download_enabled = false;
    config.max_archive_gp_cost = 500; // per-archive allows 218
    config.gp_rate_limit = 1000; // but window budget is exhausted
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
        "GP rate limit must defer without POSTing"
    );
}

#[tokio::test]
async fn test_background_gp_rate_limit_allows_only_one_post() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    setup_chat(&repo, -100, true).await;

    let first = seed_delivery(
        &repo,
        -100,
        (1001, "a1b2c3d4", "First paid background gallery"),
        Default::default(),
    )
    .await;
    let second = seed_delivery(
        &repo,
        -100,
        (1002, "e5f6a7b8", "Second paid background gallery"),
        Default::default(),
    )
    .await;
    for entry in [&first, &second] {
        handoff_job_to_background(&repo, entry).await;
    }

    mock_archiver(
        &eh_server,
        1001,
        "a1b2c3d4",
        ArchiverPage {
            original_cost: "218 GP",
            resample_cost: "218 GP",
            ..Default::default()
        },
    )
    .await;
    mock_archiver(
        &eh_server,
        1002,
        "e5f6a7b8",
        ArchiverPage {
            original_cost: "218 GP",
            resample_cost: "218 GP",
            ..Default::default()
        },
    )
    .await;
    let download_url = format!("{}/archive/paid/0", eh_server.uri());
    mock_eh_archiver_post(&eh_server, &download_url).await;
    let zip_temp = tempfile::tempdir().unwrap();
    let zip_path = zip_temp.path().join("paid.zip");
    create_test_zip(&zip_path, 2);
    mock_eh_archive_download(
        &eh_server,
        "/archive/paid/0",
        std::fs::read(zip_path).unwrap(),
    )
    .await;

    let mut config = make_config();
    config.background_download_enabled = true;
    config.background_download_concurrency = 2;
    config.max_archive_size_mb = 0;
    config.max_archive_gp_cost = 218;
    config.gp_rate_limit = 218;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        temp.path().to_path_buf(),
        Background,
        None,
    );

    worker.tick().await.unwrap();

    let first = job_for_delivery(&repo, &first).await;
    let second = job_for_delivery(&repo, &second).await;
    assert_eq!(
        [first.status.as_str(), second.status.as_str()]
            .into_iter()
            .filter(|status| *status == STATUS_DOWNLOADED)
            .count(),
        1,
        "exactly one background job must download"
    );
    let deferred = [&first, &second]
        .into_iter()
        .find(|job| job.status == STATUS_PENDING)
        .expect("one background job must remain pending");
    assert_eq!(
        deferred.background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_PENDING),
        "deferred background entry must remain eligible for a later tick"
    );
    assert_eq!(gp_attempts(repo.as_ref()).await.len(), 1);
    assert_eq!(repo.get_eh_gp_cost_in_window(24).await.unwrap(), 218);

    let archiver_posts = eh_server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
        })
        .count();
    assert_eq!(
        archiver_posts, 1,
        "exactly one paid archive POST is allowed"
    );
}

#[tokio::test]
async fn test_main_and_background_gp_rate_limit_allows_only_one_post() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    setup_chat(&repo, -100, true).await;

    let main_entry = seed_delivery(
        &repo,
        -100,
        (2001, "c1d2e3f4", "Paid main gallery"),
        Default::default(),
    )
    .await;
    let background_entry = seed_delivery(
        &repo,
        -100,
        (2002, "a5b6c7d8", "Paid background gallery"),
        Default::default(),
    )
    .await;
    handoff_job_to_background(&repo, &background_entry).await;

    mock_archiver(
        &eh_server,
        2001,
        "c1d2e3f4",
        ArchiverPage {
            original_cost: "218 GP",
            resample_cost: "218 GP",
            ..Default::default()
        },
    )
    .await;
    mock_archiver(
        &eh_server,
        2002,
        "a5b6c7d8",
        ArchiverPage {
            original_cost: "218 GP",
            resample_cost: "218 GP",
            ..Default::default()
        },
    )
    .await;
    let download_url = format!("{}/archive/paid/0", eh_server.uri());
    mock_eh_archiver_post(&eh_server, &download_url).await;
    let zip_temp = tempfile::tempdir().unwrap();
    let zip_path = zip_temp.path().join("paid.zip");
    create_test_zip(&zip_path, 2);
    mock_eh_archive_download(
        &eh_server,
        "/archive/paid/0",
        std::fs::read(zip_path).unwrap(),
    )
    .await;

    let mut config = make_config();
    config.background_download_enabled = true;
    config.background_download_concurrency = 2;
    config.max_archive_size_mb = 0;
    config.max_archive_gp_cost = 218;
    config.gp_rate_limit = 218;
    let config = Arc::new(config);
    let client = make_eh_client(&eh_server);
    let main_worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        Arc::clone(&client),
        Arc::clone(&config),
        temp.path().to_path_buf(),
        Main,
        None,
    );
    let background_worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        Arc::clone(&client),
        Arc::clone(&config),
        temp.path().to_path_buf(),
        Background,
        None,
    );

    let (main_result, background_result) =
        tokio::join!(main_worker.tick(), background_worker.tick());
    main_result.unwrap();
    background_result.unwrap();

    let main_entry = job_for_delivery(&repo, &main_entry).await;
    let background_entry = job_for_delivery(&repo, &background_entry).await;
    assert_eq!(
        [main_entry.status.as_str(), background_entry.status.as_str()]
            .into_iter()
            .filter(|status| *status == STATUS_DOWNLOADED)
            .count(),
        1,
        "the process-wide lock must allow only one worker to spend the GP budget"
    );
    if main_entry.status == STATUS_PENDING {
        assert!(
            main_entry.next_retry_at.is_some(),
            "deferred main entry must remain processable"
        );
    } else {
        assert_eq!(background_entry.status, STATUS_PENDING);
        assert_eq!(
            background_entry.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_PENDING),
            "deferred background entry must remain processable"
        );
        assert!(background_entry.background_download_next_retry_at.is_some());
    }
    assert_eq!(gp_attempts(repo.as_ref()).await.len(), 1);
    assert_eq!(repo.get_eh_gp_cost_in_window(24).await.unwrap(), 218);

    let archiver_posts = eh_server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
        })
        .count();
    assert_eq!(
        archiver_posts, 1,
        "exactly one paid archive POST is allowed"
    );
}

/// Verify the conservative "Unknown cost => defer" rule: when the archiver
/// page contains an archiver_key but no price for the fallback original,
/// the download must defer rather than reuse the disabled resample's price.
#[tokio::test]
async fn test_download_worker_unknown_cost_defers_without_post() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, true).await;
    let entry = seed_delivery(
        &repo,
        -100,
        (123456, "abcdef0123", "Unknown Cost Gallery"),
        Default::default(),
    )
    .await;

    // The original price is missing. A lone Free! price belongs only to the
    // disabled resample, even though the page also exposes an archiver key.
    let gallery_html = r#"<html><body>
            <a onclick="return popUp('/archiver.php?gid=123456&amp;token=abcdef0123',480,320)">Archive Download</a>
            </body></html>"#;
    Mock::given(method("GET"))
        .and(path("/g/123456/abcdef0123/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(gallery_html))
        .expect(1)
        .mount(&eh_server)
        .await;
    let archiver_page_html = r#"<html><body>
        <form action="/archiver.php"><input name="dltype" value="org" />
            <input name="dlcheck" value="Download Original Archive" /></form>
        <div>Download Cost: <strong>Free!</strong></div>
        <form action="/archiver.php"><input name="dltype" value="res" />
            <input name="dlcheck" value="Download Resample Archive" disabled /></form>
        <input type="hidden" name="or" value="123456--abc123def456" />
        </body></html>"#;
    Mock::given(method("GET"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(archiver_page_html))
        .expect(1)
        .mount(&eh_server)
        .await;

    // POST must never happen.
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("should not be called"))
        .expect(0)
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
    assert_eq!(updated.status, JOB_STATUS_PENDING);
    assert!(
        updated.next_retry_at.is_some(),
        "unknown cost must schedule a deferral"
    );
    assert_eq!(updated.retry_count, 0);
    assert!(gp_attempts(repo.as_ref()).await.is_empty());
}

/// Verify the parser picks the original-archive cost when resolution is
/// "original" - the GP-required sample's original form says 8,800 GP, so
/// with default config (max_archive_gp_cost = 0) it must be rejected.
#[tokio::test]
async fn test_download_worker_original_resolution_gp_cost_rejects() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let eh_server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();

    setup_chat(&repo, -100, true).await;
    let entry = seed_delivery(
        &repo,
        -100,
        (2284788, "7841d194d4", "GP Original Gallery"),
        Default::default(),
    )
    .await;

    mock_archiver(
        &eh_server,
        2284788,
        "7841d194d4",
        ArchiverPage {
            original_cost: "8,800 GP",
            resample_cost: "218 GP",
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
    config.background_download_enabled = false;
    config.download_resolution = "original".to_string();
    eh_gallery_jobs::Entity::update_many()
        .col_expr(eh_gallery_jobs::Column::Resolution, Expr::value("original"))
        .filter(eh_gallery_jobs::Column::Id.eq(entry.job_id.unwrap()))
        .exec(repo.db())
        .await
        .unwrap();
    // max_archive_gp_cost defaults to 0 -> 8,800 GP must be rejected.
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
    assert_eq!(updated.status, STATUS_FAILED);
    assert_eq!(
        updated.error.as_deref(),
        Some("EH archive GP cost 8800 exceeds configured max_archive_gp_cost=0")
    );
    assert!(updated.completed_at.is_some());
    assert!(updated.started_at.is_some());
    assert!(updated.next_retry_at.is_none());
    assert_eq!(updated.retry_count, 0);
    assert!(gp_attempts(repo.as_ref()).await.is_empty());
}
