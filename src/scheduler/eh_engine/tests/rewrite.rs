use super::*;

#[tokio::test]
async fn first_telegraph_delivery_schedules_one_job_rewrite() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let (job, deliveries) =
        seed_ready_telegraph_job_with_deliveries(&repo, 712, &[(-100, None), (-200, None)]).await;

    repo.mark_eh_telegraph_delivery_sent(deliveries[0].id, job.id, Some(0))
        .await
        .unwrap();
    let after_first = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    let scheduled_after = after_first.telegraph_rewrite_after.unwrap();
    assert_eq!(
        after_first.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_PENDING)
    );
    let marked_first = eh_download_queue::Entity::find_by_id(deliveries[0].id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    let untouched_second = eh_download_queue::Entity::find_by_id(deliveries[1].id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert!(marked_first.telegraph_sent_at.is_some());
    assert!(untouched_second.telegraph_sent_at.is_none());

    repo.mark_eh_telegraph_delivery_sent(deliveries[0].id, job.id, Some(7200))
        .await
        .unwrap();

    repo.mark_eh_telegraph_delivery_sent(deliveries[1].id, job.id, Some(3600))
        .await
        .unwrap();
    let after_second = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_second.telegraph_rewrite_after, Some(scheduled_after));

    Mock::given(method("POST"))
        .and(path("/editPage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {"url": "https://telegra.ph/Shared-Gallery-01-01"}
        })))
        .expect(1)
        .mount(&tg_server)
        .await;
    let worker = EhTelegraphRewriteWorker::new(
        Arc::clone(&repo),
        make_telegraph_client(&tg_server),
        true,
        Arc::new(make_config()),
    );
    worker.tick().await.unwrap();

    let requests = tg_server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/editPage")
            .count(),
        1
    );
    let rewritten = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert!(rewritten.telegraph_rewritten_at.is_some());
    assert!(repo
        .get_next_eh_job_for_telegraph_rewrite()
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn job_telegraph_rewrite_worker_attempts_each_migrated_payload_independently() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let (job, deliveries) =
        seed_ready_telegraph_job_with_deliveries(&repo, 714, &[(-100, None)]).await;
    let rewrite_data = serde_json::json!([
        {
            "pages": [{
                "path": "Shared-Gallery-01-01",
                "title": "Shared Gallery 1",
                "content": [{
                    "tag": "img",
                    "attrs": {"src": "https://preview.example/ipfs/first"}
                }]
            }],
            "preview_gateway_url": "https://preview.example",
            "public_gateway_url": "https://public.example"
        },
        {
            "pages": [{
                "path": "Shared-Gallery-01-02",
                "title": "Shared Gallery 2",
                "content": [{
                    "tag": "img",
                    "attrs": {"src": "https://preview.example/ipfs/second"}
                }]
            }],
            "preview_gateway_url": "https://preview.example",
            "public_gateway_url": "https://public.example"
        }
    ])
    .to_string();
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteData,
            Expr::value(Some(rewrite_data)),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    repo.mark_eh_telegraph_delivery_sent(deliveries[0].id, job.id, Some(0))
        .await
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/editPage"))
        .and(body_string_contains("path=Shared-Gallery-01-01"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&tg_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/editPage"))
        .and(body_string_contains("path=Shared-Gallery-01-02"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {"url": "https://telegra.ph/Shared-Gallery"}
        })))
        .expect(1)
        .mount(&tg_server)
        .await;
    let mut config = make_config();
    config.max_retry_count = 0;
    let worker = EhTelegraphRewriteWorker::new(
        Arc::clone(&repo),
        make_telegraph_client(&tg_server),
        true,
        Arc::new(config),
    );
    worker.tick().await.unwrap();

    let requests = tg_server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/editPage")
            .count(),
        2
    );
    for request in requests
        .iter()
        .filter(|request| request.url.path() == "/editPage")
    {
        let content = url::form_urlencoded::parse(&request.body)
            .find(|(key, _)| key == "content")
            .unwrap()
            .1
            .into_owned();
        assert!(content.contains("https://public.example/ipfs/"));
        assert!(!content.contains("https://preview.example/ipfs/"));
    }
    let terminal = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        terminal.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_FAILED)
    );
}

#[tokio::test]
async fn final_delivery_with_delayed_rewrite_keeps_payload_until_rewrite_is_terminal() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let tg_server = MockServer::start().await;
    let (job, deliveries) =
        seed_ready_telegraph_job_with_deliveries(&repo, 713, &[(-100, Some(812))]).await;

    repo.mark_eh_telegraph_delivery_sent(deliveries[0].id, job.id, Some(60))
        .await
        .unwrap();
    repo.cancel_eh_subscription_queue_entries(812, true)
        .await
        .unwrap();
    let interleaved = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(interleaved.status, JOB_STATUS_RETIRED);
    assert_eq!(
        interleaved.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_PENDING)
    );
    assert!(interleaved.telegraph_rewrite_data.is_some());

    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteAfter,
            Expr::value(Local::now().naive_local() - chrono::Duration::seconds(1)),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job.id))
        .exec(repo.db())
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/editPage"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&tg_server)
        .await;
    let mut config = make_config();
    config.max_retry_count = 0;
    let worker = EhTelegraphRewriteWorker::new(
        Arc::clone(&repo),
        make_telegraph_client(&tg_server),
        true,
        Arc::new(config),
    );
    worker.tick().await.unwrap();

    let terminal = eh_gallery_jobs::Entity::find_by_id(job.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        terminal.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_FAILED)
    );
    assert_eq!(terminal.status, JOB_STATUS_RETIRED);
    assert!(terminal.telegraph_rewrite_data.is_none());
    assert_eq!(terminal.cleanup_status, CLEANUP_STATUS_PENDING);
}
