use super::*;
use migration::{Migrator, MigratorTrait};
use sea_orm::Database;

#[tokio::test]
async fn old_queue_upgrade_downloads_once_and_preserves_gp_and_delivery_progress() {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    db.execute_unprepared("PRAGMA foreign_keys = ON")
        .await
        .unwrap();
    let before_shared_jobs = Migrator::migrations()
        .iter()
        .position(|migration| migration.name() == "m20260824_000000_eh_shared_gallery_jobs")
        .unwrap();
    Migrator::up(&db, Some(before_shared_jobs as u32))
        .await
        .unwrap();
    let sent_at = Local::now().naive_local();
    // Persist the old schema, including one already-sent archive and a charge
    // owned by the old queue. No current entities or enqueue APIs create it.
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO eh_download_queue \
            (id, chat_id, gid, token, title, source, status, telegraph, archive_sent_at) \
         VALUES (1, -100, 4053300, 'abc123def0', 'Old gallery', 'direct', 'downloading', 1, ?), \
                (2, -200, 4053300, 'abc123def0', 'Old gallery', 'direct', 'pending', 0, NULL)",
        [sent_at.into()],
    ))
    .await
    .unwrap();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO eh_gp_spend_attempts (queue_id, gid, gp_cost, created_at) VALUES (1, 4053300, 100, ?)",
        [sent_at.into()],
    ))
    .await
    .unwrap();

    Migrator::up(&db, None).await.unwrap();
    // A second startup must not repeat data migration or manufacture charges.
    Migrator::up(&db, None).await.unwrap();
    let repo = Arc::new(Repo::new(db));
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;
    let cache = tempfile::tempdir().unwrap();
    let cache_dir = cache.path().join("eh_cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    repo.reset_stale_eh_shared_work(60, 60).await.unwrap();
    repo.reconcile_eh_shared_job_liveness(true).await.unwrap();
    repo.handoff_legacy_eh_archive_artifacts(&cache_dir)
        .await
        .unwrap();
    repo.cleanup_eh_cache_orphans(&cache_dir, None)
        .await
        .unwrap();
    drain_eh_job_cleanup_maintenance(&repo, None, 1, true)
        .await
        .unwrap();

    let server = MockServer::start().await;
    mock_archiver(
        &server,
        4053300,
        "abc123def0",
        ArchiverPage {
            original_cost: "8800 GP",
            resample_cost: "218 GP",
            ..Default::default()
        },
    )
    .await;
    mock_eh_archiver_post(&server, &format!("{}/archive/gallery.zip", server.uri())).await;
    let fixture = cache.path().join("fixture.zip");
    create_test_zip(&fixture, 2);
    let zip_bytes = std::fs::read(&fixture).unwrap();
    mock_eh_archive_download(&server, "/archive/gallery.zip", zip_bytes.clone()).await;
    let mut config = make_config();
    config.background_download_enabled = false;
    config.archive_download_concurrency = 1;
    config.max_archive_gp_cost = 218;
    config.gp_rate_limit = 318;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&server),
        Arc::new(config),
        cache.path().to_path_buf(),
        Main,
        None,
    );
    worker.tick().await.unwrap();
    worker.tick().await.unwrap();

    let deliveries = eh_download_queue::Entity::find()
        .all(repo.db())
        .await
        .unwrap();
    let first = deliveries.iter().find(|delivery| delivery.id == 1).unwrap();
    let second = deliveries.iter().find(|delivery| delivery.id == 2).unwrap();
    assert_eq!(first.job_id, second.job_id);
    assert_eq!(first.archive_sent_at, Some(sent_at));
    assert!(second.archive_sent_at.is_none());
    let job = job_for_delivery(&repo, second).await;
    assert_eq!(job.status, JOB_STATUS_DOWNLOADED, "upgraded job: {job:?}");
    assert_eq!(std::fs::read(job.zip_path.unwrap()).unwrap(), zip_bytes);
    let attempts = gp_attempts(&repo).await;
    assert_eq!(attempts.len(), 2);
    assert!(attempts
        .iter()
        .any(|attempt| attempt.queue_id == Some(1) && attempt.gp_cost == 100));
    assert!(attempts.iter().any(|attempt| attempt.job_id == Some(job.id)
        && attempt.queue_id.is_none()
        && attempt.gp_cost == 218));
    assert_eq!(repo.get_eh_gp_cost_in_window(24).await.unwrap(), 318);
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/archiver.php"
            })
            .count(),
        1,
        "both old queue entries must share one paid source download after upgrade"
    );
}
