use super::*;

pub(super) fn make_notifier(tg_server: &MockServer) -> Notifier {
    let url = url::Url::parse(&tg_server.uri()).unwrap();
    let bot = Bot::new("fake_token").set_api_url(url);
    let throttled = bot.throttle(teloxide::adaptors::throttle::Limits::default());
    let http = Client::new();
    let cache = FileCacheManager::new("data/test_cache", 7);
    let downloader = Arc::new(Downloader::new(http, cache));
    Notifier::new(throttled, downloader)
}

pub(super) fn make_eh_client(eh_server: &MockServer) -> Arc<EhClient> {
    Arc::new(
        EhClientBuilder::new()
            .base_url(&eh_server.uri())
            .api_url(&format!("{}/api.php", eh_server.uri()))
            .cookies(EhCookies {
                ipb_member_id: Some("12345".into()),
                ipb_pass_hash: Some("abc".into()),
                igneous: None,
                nw: true,
            })
            .build(),
    )
}

pub(super) fn make_telegraph_client(tg_server: &MockServer) -> Arc<TelegraphClient> {
    Arc::new(TelegraphClient::new_with_urls(
        "test_token".to_string(),
        format!("{}/pixi/upload", tg_server.uri()),
        tg_server.uri(),
    ))
}

pub(super) fn make_image_uploader(tg_server: &MockServer) -> Arc<dyn ImageUploader> {
    Arc::new(PixiUploader::new_with_url(format!(
        "{}/pixi/upload",
        tg_server.uri()
    )))
}

pub(super) fn make_config() -> EhentaiConfig {
    EhentaiConfig {
        download_rate_limit_gb: 7,
        download_rate_window_hours: 168,
        download_poll_interval_sec: 60,
        max_push_per_tick: 3,
        max_retry_count: 3,
        send_archive: true,
        upload_telegraph: true,
        ..Default::default()
    }
}

pub(super) fn create_test_zip(path: &std::path::Path, image_count: usize) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for i in 0..image_count {
        let name = format!("page{:03}.jpg", i);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(name, options).unwrap();
        let data = format!("fake_image_data_{}", i);
        zip.write_all(data.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

pub(super) fn seed_archive_artifact_family(zip_path: &std::path::Path) -> ArchiveArtifacts {
    let artifacts = ArchiveArtifacts::new(zip_path);
    std::fs::write(artifacts.assembly_scratch(), b"partial").unwrap();
    std::fs::create_dir_all(artifacts.parts_dir().join("nested")).unwrap();
    std::fs::write(artifacts.parts_dir().join("nested/part-0001"), b"part").unwrap();
    std::fs::create_dir_all(artifacts.uploads_dir().join("nested")).unwrap();
    std::fs::write(artifacts.uploads_dir().join("archive.json"), b"archive").unwrap();
    std::fs::write(
        artifacts.uploads_dir().join("nested/image-0.json"),
        b"image",
    )
    .unwrap();
    artifacts
}

pub(super) fn create_test_zip_with_names(path: &std::path::Path, names: &[&str]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for name in names {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"fake_image_data").unwrap();
    }
    zip.finish().unwrap();
}

pub(super) fn create_unsupported_encrypted_metadata_zip(path: &std::path::Path) {
    fn push_u16(bytes: &mut Vec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    let name = b"folder/Photo.JPG";
    let mut bytes = Vec::new();
    push_u32(&mut bytes, 0x0403_4b50);
    push_u16(&mut bytes, 20);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 12);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, name.len() as u16);
    push_u16(&mut bytes, 0);
    bytes.extend_from_slice(name);

    let central_start = bytes.len() as u32;
    push_u32(&mut bytes, 0x0201_4b50);
    push_u16(&mut bytes, 20);
    push_u16(&mut bytes, 20);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 12);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, name.len() as u16);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    bytes.extend_from_slice(name);

    let central_size = bytes.len() as u32 - central_start;
    push_u32(&mut bytes, 0x0605_4b50);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 1);
    push_u32(&mut bytes, central_size);
    push_u32(&mut bytes, central_start);
    push_u16(&mut bytes, 0);
    std::fs::write(path, bytes).unwrap();
}

pub(super) fn create_test_zip_with_unsupported_encrypted_non_image(path: &std::path::Path) {
    create_test_zip_with_names(path, &["page001.jpg", "notes.txt"]);

    let mut bytes = std::fs::read(path).unwrap();
    let mut modified_local = false;
    let mut modified_central = false;
    for offset in 0..=bytes.len() - 4 {
        if &bytes[offset..offset + 4] == b"PK\x03\x04" {
            let name_len = usize::from(u16::from_le_bytes(
                bytes[offset + 26..offset + 28].try_into().unwrap(),
            ));
            let extra_len = usize::from(u16::from_le_bytes(
                bytes[offset + 28..offset + 30].try_into().unwrap(),
            ));
            let name_start = offset + 30 + extra_len;
            let name_end = name_start + name_len;
            if &bytes[name_start..name_end] == b"notes.txt" {
                bytes[offset + 6..offset + 8].copy_from_slice(&1u16.to_le_bytes());
                bytes[offset + 8..offset + 10].copy_from_slice(&12u16.to_le_bytes());
                modified_local = true;
            }
        } else if &bytes[offset..offset + 4] == b"PK\x01\x02" {
            let name_len = usize::from(u16::from_le_bytes(
                bytes[offset + 28..offset + 30].try_into().unwrap(),
            ));
            let extra_len = usize::from(u16::from_le_bytes(
                bytes[offset + 30..offset + 32].try_into().unwrap(),
            ));
            let name_start = offset + 46 + extra_len;
            let name_end = name_start + name_len;
            if &bytes[name_start..name_end] == b"notes.txt" {
                bytes[offset + 8..offset + 10].copy_from_slice(&1u16.to_le_bytes());
                bytes[offset + 10..offset + 12].copy_from_slice(&12u16.to_le_bytes());
                modified_central = true;
            }
        }
    }
    assert!(modified_local && modified_central);
    std::fs::write(path, bytes).unwrap();
}

pub(super) fn create_test_zip_with_sizes(path: &std::path::Path, image_sizes: &[usize]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for (i, size) in image_sizes.iter().enumerate() {
        let name = format!("page{:03}.jpg", i);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(name, options).unwrap();
        zip.write_all(&vec![b'a'; *size]).unwrap();
    }
    zip.finish().unwrap();
}

#[derive(Debug)]
pub(super) struct MultipartFileCount(pub(super) usize);

impl wiremock::Match for MultipartFileCount {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let body = String::from_utf8_lossy(&request.body);
        body.matches("name=\"files[]\"").count() == self.0
    }
}

pub(super) async fn mock_tg_send_document(server: &MockServer) {
    let body = serde_json::json!({
        "ok": true,
        "result": {"message_id": 42, "date": 1700000000, "chat": {"id": -100, "type": "private"}}
    });
    Mock::given(method("POST"))
        .and(path("/botfake_token/SendDocument"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[derive(Debug)]
pub(super) struct TelegramDocumentChat(pub(super) i64);

impl wiremock::Match for TelegramDocumentChat {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let body = String::from_utf8_lossy(&request.body);
        let chat_id = self.0.to_string();
        body.contains("name=\"chat_id\"") && body.lines().any(|line| line.trim() == chat_id)
    }
}

pub(super) async fn mock_tg_send_document_for_chat(
    server: &MockServer,
    chat_id: i64,
    status: u16,
    delay: Option<Duration>,
) {
    let body = if status == 200 {
        serde_json::json!({
            "ok": true,
            "result": {"message_id": 42, "date": 1700000000, "chat": {"id": chat_id, "type": "private"}}
        })
    } else {
        serde_json::json!({"ok": false, "description": "mock send failure"})
    };
    let mut response = ResponseTemplate::new(status).set_body_json(body);
    if let Some(delay) = delay {
        response = response.set_delay(delay);
    }
    Mock::given(method("POST"))
        .and(path("/botfake_token/SendDocument"))
        .and(TelegramDocumentChat(chat_id))
        .respond_with(response)
        .mount(server)
        .await;
}

pub(super) async fn mock_tg_send_message(server: &MockServer) {
    let body = serde_json::json!({
        "ok": true,
        "result": {"message_id": 43, "date": 1700000000, "chat": {"id": -100, "type": "private"}}
    });
    Mock::given(method("POST"))
        .and(path("/botfake_token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

pub(super) struct ArchiverPage<'a> {
    pub(super) original_cost: &'a str,
    pub(super) resample_cost: &'a str,
    pub(super) sizes: Option<(&'a str, &'a str)>,
    pub(super) keyed: bool,
}

impl Default for ArchiverPage<'_> {
    fn default() -> Self {
        Self {
            original_cost: "Free!",
            resample_cost: "Free!",
            sizes: None,
            keyed: false,
        }
    }
}

pub(super) async fn mock_archiver(
    server: &MockServer,
    gid: u64,
    token: &str,
    page: ArchiverPage<'_>,
) {
    let gallery = format!(
        r#"<html><a onclick="return popUp('/archiver.php?gid={gid}&amp;token={token}',480,320)">Archive Download</a></html>"#
    );
    let mut forms = String::new();
    for (dltype, label, cost, size) in [
        (
            "org",
            "Original",
            page.original_cost,
            page.sizes.map(|sizes| sizes.0),
        ),
        (
            "res",
            "Resample",
            page.resample_cost,
            page.sizes.map(|sizes| sizes.1),
        ),
    ] {
        let key = if page.keyed && dltype == "org" {
            format!("&amp;or={gid}--abc123def456")
        } else {
            String::new()
        };
        let size = size
            .map(|size| format!("<p>Estimated Size: <strong>{size}</strong></p>"))
            .unwrap_or_default();
        forms.push_str(&format!(
            r#"<div style="width:180px; float:left">
            <div>Download Cost: &nbsp; <strong>{cost}</strong></div>
            <form action="/archiver.php?gid={gid}&amp;token={token}{key}" method="post">
                <input type="hidden" name="dltype" value="{dltype}" />
                <input type="submit" name="dlcheck" value="Download {label} Archive" />
            </form>{size}</div>"#
        ));
    }
    let gallery_mock = Mock::given(method("GET"))
        .and(path(format!("/g/{gid}/{token}/")))
        .respond_with(ResponseTemplate::new(200).set_body_string(gallery));
    let archiver_mock = Mock::given(method("GET"))
        .and(path("/archiver.php"))
        .and(query_param("gid", gid.to_string()))
        .and(query_param("token", token))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!("<html><body>{forms}</body></html>")),
        );
    // Size-gate scenarios must actually prepare the selected archive exactly once.
    if page.sizes.is_some() {
        gallery_mock.expect(1).mount(server).await;
        archiver_mock.expect(1).mount(server).await;
    } else {
        gallery_mock.mount(server).await;
        archiver_mock.mount(server).await;
    }
}

pub(super) async fn mock_eh_archiver_post(server: &MockServer, download_url: &str) {
    let html = format!(
        r#"<html><script>function gotonext() {{ document.location = "{}?autostart=1"; }}</script></html>"#,
        download_url
    );
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(server)
        .await;
}

pub(super) async fn mock_eh_archive_download(
    server: &MockServer,
    path_str: &str,
    zip_bytes: Vec<u8>,
) {
    Mock::given(method("GET"))
        .and(path(path_str))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
        .mount(server)
        .await;
}

pub(super) async fn mock_telegraph_upload(server: &MockServer, expected_requests: u64) {
    let body = serde_json::json!({"success": true, "direct_url": "https://i.pixi.mg/i/abc123.jpg"});
    Mock::given(method("POST"))
        .and(path("/pixi/upload"))
        .and(MultipartFileCount(1))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(expected_requests)
        .mount(server)
        .await;
}

pub(super) async fn mock_telegraph_create_page(server: &MockServer) {
    let body =
        serde_json::json!({"ok": true, "result": {"url": "https://telegra.ph/Test-Gallery-01-01"}});
    Mock::given(method("POST"))
        .and(path("/createPage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

pub(super) async fn setup_chat(repo: &Repo, chat_id: i64, enabled: bool) {
    repo.upsert_chat(chat_id, "private".into(), None, enabled, Default::default())
        .await
        .unwrap();
}

pub(super) struct DeliveryOptions<'a> {
    pub(super) telegraph: bool,
    pub(super) job_status: &'a str,
    pub(super) zip_path: Option<&'a str>,
    pub(super) subscription_ids: Option<&'a str>,
}

impl Default for DeliveryOptions<'_> {
    fn default() -> Self {
        Self {
            telegraph: false,
            job_status: JOB_STATUS_PENDING,
            zip_path: None,
            subscription_ids: None,
        }
    }
}

pub(super) async fn seed_delivery(
    repo: &Repo,
    chat_id: i64,
    (gid, token, title): (i64, &str, &str),
    options: DeliveryOptions<'_>,
) -> eh_download_queue::Model {
    let DeliveryOptions {
        telegraph,
        job_status,
        zip_path,
        subscription_ids,
    } = options;
    let now = Local::now().naive_local();
    let job = eh_gallery_jobs::ActiveModel {
        gid: Set(gid),
        token: Set(token.to_string()),
        download_mode: Set(DOWNLOAD_MODE_ARCHIVE.to_string()),
        resolution: Set("1280x".to_string()),
        title: Set(title.to_string()),
        status: Set(job_status.to_string()),
        telegraph_status: Set(if job_status == JOB_STATUS_DOWNLOADED && telegraph {
            TELEGRAPH_STATUS_PENDING.to_string()
        } else {
            TELEGRAPH_STATUS_NOT_REQUIRED.to_string()
        }),
        telegraph_required: Set(telegraph),
        zip_path: Set(zip_path.map(str::to_string)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(repo.db())
    .await
    .unwrap();
    eh_download_queue::ActiveModel {
        job_id: Set(Some(job.id)),
        chat_id: Set(chat_id),
        gid: Set(gid),
        token: Set(token.to_string()),
        title: Set(title.to_string()),
        telegraph: Set(telegraph),
        source: Set(if subscription_ids.is_some() {
            SOURCE_SUBSCRIPTION
        } else {
            SOURCE_DIRECT
        }
        .to_string()),
        subscription_ids: Set(subscription_ids.map(str::to_string)),
        status: Set(DELIVERY_STATUS_WAITING.to_string()),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(repo.db())
    .await
    .unwrap()
}

pub(super) async fn job_for_delivery(
    repo: &Repo,
    delivery: &eh_download_queue::Model,
) -> eh_gallery_jobs::Model {
    eh_gallery_jobs::Entity::find_by_id(delivery.job_id.unwrap())
        .one(repo.db())
        .await
        .unwrap()
        .unwrap()
}

pub(super) async fn seed_downloaded_job_with_deliveries(
    repo: &Repo,
    gid: i64,
    token: &str,
    job_title: &str,
    zip_path: &std::path::Path,
    deliveries: &[(i64, bool, &str)],
) -> (eh_gallery_jobs::Model, Vec<eh_download_queue::Model>) {
    let now = Local::now().naive_local();
    let telegraph_required = deliveries.iter().any(|(_, telegraph, _)| *telegraph);
    let job = eh_gallery_jobs::ActiveModel {
        gid: Set(gid),
        token: Set(token.to_string()),
        download_mode: Set(DOWNLOAD_MODE_ARCHIVE.to_string()),
        resolution: Set("1280x".to_string()),
        title: Set(job_title.to_string()),
        status: Set(JOB_STATUS_DOWNLOADED.to_string()),
        telegraph_status: Set(if telegraph_required {
            TELEGRAPH_STATUS_PENDING.to_string()
        } else {
            TELEGRAPH_STATUS_NOT_REQUIRED.to_string()
        }),
        telegraph_required: Set(telegraph_required),
        file_size: Set(std::fs::metadata(zip_path).unwrap().len() as i64),
        zip_path: Set(Some(zip_path.to_string_lossy().to_string())),
        cleanup_status: Set(CLEANUP_STATUS_NONE.to_string()),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(repo.db())
    .await
    .unwrap();
    let mut seeded = Vec::with_capacity(deliveries.len());
    for (chat_id, telegraph, title) in deliveries {
        seeded.push(
            eh_download_queue::ActiveModel {
                job_id: Set(Some(job.id)),
                chat_id: Set(*chat_id),
                gid: Set(gid),
                token: Set(token.to_string()),
                title: Set((*title).to_string()),
                telegraph: Set(*telegraph),
                source: Set(SOURCE_DIRECT.to_string()),
                status: Set(DELIVERY_STATUS_WAITING.to_string()),
                created_at: Set(now),
                ..Default::default()
            }
            .insert(repo.db())
            .await
            .unwrap(),
        );
    }
    (job, seeded)
}

pub(super) async fn seed_ready_telegraph_job_with_deliveries(
    repo: &Repo,
    gid: i64,
    deliveries: &[(i64, Option<i32>)],
) -> (eh_gallery_jobs::Model, Vec<eh_download_queue::Model>) {
    let variant = EhGalleryVariant::archive("1280x");
    let mut seeded = Vec::with_capacity(deliveries.len());
    for (chat_id, subscription_id) in deliveries {
        let delivery = if let Some(subscription_id) = subscription_id {
            repo.enqueue_eh_subscription_download(
                *chat_id,
                *subscription_id,
                gid,
                "token",
                "Shared Gallery",
                true,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued")
        } else {
            repo.enqueue_eh_download(
                *chat_id,
                gid,
                "token",
                "Shared Gallery",
                true,
                SOURCE_DIRECT,
                &variant,
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued")
        };
        seeded.push(delivery);
    }

    let job_id = seeded[0].job_id.unwrap();
    let rewrite_data = serde_json::json!({
        "pages": [{
            "path": "Shared-Gallery-01-01",
            "title": "Shared Gallery",
            "content": [{
                "tag": "img",
                "attrs": {"src": "https://preview.example/ipfs/cid"}
            }]
        }],
        "preview_gateway_url": "https://preview.example",
        "public_gateway_url": "https://public.example"
    })
    .to_string();
    eh_gallery_jobs::Entity::update_many()
        .col_expr(
            eh_gallery_jobs::Column::Status,
            Expr::value(JOB_STATUS_DOWNLOADED),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphStatus,
            Expr::value(TELEGRAPH_STATUS_READY),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphUrl,
            Expr::value(Some("https://telegra.ph/Shared-Gallery-01-01".to_string())),
        )
        .col_expr(
            eh_gallery_jobs::Column::TelegraphRewriteData,
            Expr::value(Some(rewrite_data)),
        )
        .col_expr(
            eh_gallery_jobs::Column::ZipPath,
            Expr::value(Some("shared-gallery.zip".to_string())),
        )
        .filter(eh_gallery_jobs::Column::Id.eq(job_id))
        .exec(repo.db())
        .await
        .unwrap();
    eh_download_queue::Entity::update_many()
        .col_expr(
            eh_download_queue::Column::Status,
            Expr::value(DELIVERY_STATUS_PUBLISHING),
        )
        .filter(eh_download_queue::Column::JobId.eq(job_id))
        .exec(repo.db())
        .await
        .unwrap();

    let job = eh_gallery_jobs::Entity::find_by_id(job_id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    let deliveries = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::JobId.eq(job_id))
        .all(repo.db())
        .await
        .unwrap();
    (job, deliveries)
}

pub(super) async fn handoff_job_to_background(repo: &Repo, delivery: &eh_download_queue::Model) {
    repo.schedule_eh_job_background_download(
        delivery
            .job_id
            .expect("background worker test delivery has a shared job"),
        JOB_STATUS_PENDING,
        "test setup",
    )
    .await
    .unwrap();
}

pub(super) async fn gp_attempts(repo: &Repo) -> Vec<eh_gp_spend_attempts::Model> {
    eh_gp_spend_attempts::Entity::find()
        .all(repo.db())
        .await
        .unwrap()
}

pub(super) async fn mock_eh_search_with_four_galleries(server: &MockServer) {
    let html = r#"
        <div class="gl1t"><a href="https://e-hentai.org/g/1001/aaaaaaaaaa/"><div class="glink">Gallery 1</div></a></div>
        <div class="gl1t"><a href="https://e-hentai.org/g/1002/bbbbbbbbbb/"><div class="glink">Gallery 2</div></a></div>
        <div class="gl1t"><a href="https://e-hentai.org/g/1003/cccccccccc/"><div class="glink">Gallery 3</div></a></div>
        <div class="gl1t"><a href="https://e-hentai.org/g/1004/dddddddddd/"><div class="glink">Gallery 4</div></a></div>
        "#;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(server)
        .await;
}

pub(super) async fn mock_eh_metadata_for_four_galleries(server: &MockServer) {
    Mock::given(method("POST"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "gmetadata": [
                    {"gid": 1001, "token": "aaaaaaaaaa", "title": "Gallery 1", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/1.jpg", "uploader": "tester", "posted": "100", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                    {"gid": 1002, "token": "bbbbbbbbbb", "title": "Gallery 2", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/2.jpg", "uploader": "tester", "posted": "200", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                    {"gid": 1003, "token": "cccccccccc", "title": "Gallery 3", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/3.jpg", "uploader": "tester", "posted": "300", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                    {"gid": 1004, "token": "dddddddddd", "title": "Gallery 4", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/4.jpg", "uploader": "tester", "posted": "400", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]}
                ]
            })))
            .mount(server)
            .await;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SeenResumeContext {
    pub(super) manifest_path: std::path::PathBuf,
    pub(super) logical_object_id: String,
}

pub(super) fn copy_resume_context(
    context: eh_client::UploadResumeContext<'_>,
) -> SeenResumeContext {
    SeenResumeContext {
        manifest_path: context.manifest_path.to_path_buf(),
        logical_object_id: context.logical_object_id.to_string(),
    }
}

/// Mock uploader that records whether the ZIP-archive path or the per-image
/// path was used, remembers the entry names it observed, and copies resume
/// contexts before their borrowed input is dropped.
#[derive(Default)]
pub(super) struct ZipFirstMockUploader {
    pub(super) zip_calls: std::sync::atomic::AtomicUsize,
    pub(super) image_calls: std::sync::atomic::AtomicUsize,
    pub(super) seen_entries: std::sync::Mutex<Vec<String>>,
    pub(super) seen_zip_resume_contexts: std::sync::Mutex<Vec<SeenResumeContext>>,
    pub(super) seen_image_resume_contexts: std::sync::Mutex<Vec<SeenResumeContext>>,
    pub(super) zip_fallback: bool,
    pub(super) emit_image_cids: bool,
    pub(super) fail_image_call: Option<usize>,
}

#[async_trait::async_trait]
impl ImageUploader for ZipFirstMockUploader {
    fn supports_zip_archive_upload(&self) -> bool {
        true
    }

    async fn upload_images(
        &self,
        images: &[ImageUploadInput<'_>],
    ) -> eh_client::Result<Vec<String>> {
        let image_call = self
            .image_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut seen_contexts = self.seen_image_resume_contexts.lock().unwrap();
        seen_contexts.extend(
            images
                .iter()
                .filter_map(|image| image.resume_context.map(copy_resume_context)),
        );
        drop(seen_contexts);
        if self.fail_image_call == Some(image_call) {
            return Err(eh_client::Error::Other(
                "mock image upload failure".to_string(),
            ));
        }
        Ok(images
            .iter()
            .map(|image| format!("https://images.example/{}", image.filename))
            .collect())
    }

    async fn upload_images_with_url_pairs(
        &self,
        images: &[ImageUploadInput<'_>],
    ) -> eh_client::Result<Vec<TelegraphImageUrlPair>> {
        let urls = self.upload_images(images).await?;
        Ok(urls
            .into_iter()
            .enumerate()
            .map(|(index, url)| TelegraphImageUrlPair {
                preview_url: url.clone(),
                public_url: url,
                cid: self
                    .emit_image_cids
                    .then(|| format!("bafy-image-{}", images[index].filename)),
            })
            .collect())
    }

    async fn upload_zip_archive_with_url_pairs(
        &self,
        archive: ZipArchiveUploadInput<'_>,
    ) -> eh_client::Result<Option<Vec<TelegraphImageUrlPair>>> {
        self.zip_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(context) = archive.resume_context {
            self.seen_zip_resume_contexts
                .lock()
                .unwrap()
                .push(copy_resume_context(context));
        }
        *self.seen_entries.lock().unwrap() = archive.entry_names.to_vec();
        if self.zip_fallback {
            return Ok(None);
        }
        Ok(Some(
            archive
                .entry_names
                .iter()
                .map(|name| TelegraphImageUrlPair {
                    preview_url: format!("https://preview.example/ipfs/root/{name}"),
                    public_url: format!("https://public.example/ipfs/root/{name}"),
                    cid: Some(format!("bafy-zip-{name}")),
                })
                .collect(),
        ))
    }
}

#[derive(Default)]
pub(super) struct TerminalCleanupMockUploader {
    pub(super) cleanup_calls: std::sync::Mutex<Vec<(std::path::PathBuf, bool)>>,
    pub(super) cleanup_attempted: tokio::sync::Notify,
    pub(super) fail_abort: bool,
}

pub(super) struct AlwaysFailUploader {
    pub(super) message: String,
    pub(super) calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ImageUploader for AlwaysFailUploader {
    async fn upload_images(
        &self,
        _images: &[ImageUploadInput<'_>],
    ) -> eh_client::Result<Vec<String>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(eh_client::Error::Other(self.message.clone()))
    }
}

#[async_trait::async_trait]
impl ImageUploader for TerminalCleanupMockUploader {
    async fn upload_images(
        &self,
        _images: &[ImageUploadInput<'_>],
    ) -> eh_client::Result<Vec<String>> {
        Err(eh_client::Error::Other(
            "mock image upload failure".to_string(),
        ))
    }

    async fn abort_upload_state(
        &self,
        uploads_dir: &std::path::Path,
    ) -> Result<(), eh_client::UploadStateError> {
        self.cleanup_calls
            .lock()
            .unwrap()
            .push((uploads_dir.to_path_buf(), uploads_dir.exists()));
        self.cleanup_attempted.notify_one();
        if self.fail_abort {
            return Err(eh_client::UploadStateError::AbortHttp(503));
        }
        Ok(())
    }
}

pub(super) fn assert_terminal_cleanup_precedes_local_removal(
    uploader: &TerminalCleanupMockUploader,
    artifacts: &ArchiveArtifacts,
) {
    assert_eq!(
        *uploader.cleanup_calls.lock().unwrap(),
        vec![(artifacts.uploads_dir().to_path_buf(), true)]
    );
}
