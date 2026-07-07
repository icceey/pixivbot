use eh_client::{EhClient, EhClientBuilder, EhCookies};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zip::write::SimpleFileOptions;

fn test_zip_bytes(name: &str, content: &[u8]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        zip.start_file(name, SimpleFileOptions::default())
            .expect("start zip file");
        std::io::Write::write_all(&mut zip, content).expect("write zip file");
        zip.finish().expect("finish zip");
    }
    cursor.into_inner()
}

fn corrupt_zip_entry_payload(mut zip_bytes: Vec<u8>, original: &[u8]) -> Vec<u8> {
    let offset = zip_bytes
        .windows(original.len())
        .position(|window| window == original)
        .expect("test ZIP should contain stored payload bytes");
    zip_bytes[offset] ^= 0xff;
    zip_bytes
}

/// Build an EhClient pointing at the given mock server.
fn client_at(server: &MockServer) -> EhClient {
    EhClientBuilder::new()
        .base_url(&server.uri())
        .api_url(&format!("{}/api.php", server.uri()))
        .cookies(EhCookies {
            nw: true,
            ..Default::default()
        })
        .build()
}

#[derive(Debug)]
struct BodyContains(&'static str);

impl wiremock::Match for BodyContains {
    fn matches(&self, request: &wiremock::Request) -> bool {
        String::from_utf8_lossy(&request.body).contains(self.0)
    }
}

#[derive(Debug)]
struct HeaderAbsent(&'static str);

impl wiremock::Match for HeaderAbsent {
    fn matches(&self, request: &wiremock::Request) -> bool {
        request.headers.get(self.0).is_none()
    }
}

const SEARCH_HTML: &str = r#"
<div class="gl1t">
  <a href="https://e-hentai.org/g/123456/abcdef0123/">
    <img src="https://ehgt.org/t/abc.jpg" />
  </a>
  <div class="gl3t"><div class="glink">Gallery One</div></div>
</div>
<div class="gl1t">
  <a href="https://e-hentai.org/g/789012/987654abcd/">
    <img src="https://ehgt.org/t/def.jpg" />
  </a>
  <div class="gl3t"><div class="glink">Gallery Two</div></div>
</div>
"#;

const ARCHIVER_PAGE_HTML: &str = r#"
<html><body>
<input type="hidden" name="or" value="470592--63bbddc729b849100ec24ab920ffdb84b6542b23" />
</body></html>
"#;

const ARCHIVER_REDIRECT_HTML: &str = r#"
<script type="text/javascript">
function gotonext() {
    document.location = "http://123.45.67.89/archive/123456/abcdef0123/abcdef0123/0?autostart=1";
}
</script>
"#;

fn metadata_json() -> serde_json::Value {
    serde_json::json!({
        "gmetadata": [{
            "gid": 123456,
            "token": "abcdef0123",
            "title": "Test Gallery",
            "title_jpn": "テスト画廊",
            "category": "Doujinshi",
            "thumb": "https://ehgt.org/t/test.jpg",
            "uploader": "testuser",
            "posted": "1376143500",
            "filecount": "20",
            "filesize": 51210504,
            "expunged": false,
            "rating": "4.64",
            "tags": ["parody:touhou", "artist:test", "full color"]
        }]
    })
}

#[tokio::test]
async fn test_search_parses_results() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH_HTML))
        .mount(&server)
        .await;

    let client = client_at(&server);
    let results = client
        .search("female:elf", 0, 0)
        .await
        .expect("search should succeed");

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].gid, 123456);
    assert_eq!(results[0].token, "abcdef0123");
    assert_eq!(results[0].title, "Gallery One");
    assert_eq!(results[1].gid, 789012);
    assert_eq!(results[1].title, "Gallery Two");
}

#[tokio::test]
async fn test_search_error_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let client = client_at(&server);
    let result = client.search("test", 0, 0).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_get_metadata_parses_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata_json()))
        .mount(&server)
        .await;

    let client = client_at(&server);
    let galleries = client
        .get_metadata(&[(123456, "abcdef0123")])
        .await
        .expect("metadata should succeed");

    assert_eq!(galleries.len(), 1);
    let g = &galleries[0];
    assert_eq!(g.gid, 123456);
    assert_eq!(g.token, "abcdef0123");
    assert_eq!(g.title, "Test Gallery");
    assert_eq!(g.category, "Doujinshi");
    assert_eq!(g.uploader, "testuser");
    assert_eq!(g.posted, 1376143500);
    assert_eq!(g.filecount, 20);
    assert_eq!(g.filesize, 51210504);
    assert!((g.rating - 4.64).abs() < 0.001);
    assert_eq!(g.tags.len(), 3);
}

#[tokio::test]
async fn test_get_metadata_skips_per_gallery_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "gmetadata": [
                {
                    "gid": 111111,
                    "error": "Key missing, or incorrect key provided."
                },
                {
                    "gid": 123456,
                    "token": "abcdef0123",
                    "title": "Valid Gallery",
                    "title_jpn": null,
                    "category": "Manga",
                    "thumb": "https://ehgt.org/t/valid.jpg",
                    "uploader": "validuser",
                    "posted": "1376143500",
                    "filecount": "20",
                    "filesize": 51210504,
                    "expunged": false,
                    "rating": "4.64",
                    "tags": ["artist:test"]
                }
            ]
        })))
        .mount(&server)
        .await;

    let client = client_at(&server);
    let galleries = client
        .get_metadata(&[(111111, "badtoken"), (123456, "abcdef0123")])
        .await
        .expect("metadata should skip per-gallery errors");

    assert_eq!(galleries.len(), 1);
    assert_eq!(galleries[0].gid, 123456);
    assert_eq!(galleries[0].title, "Valid Gallery");
}

#[tokio::test]
async fn test_get_metadata_empty_list() {
    let client = EhClientBuilder::new().build();
    let result = client
        .get_metadata(&[])
        .await
        .expect("empty list should succeed");
    assert!(result.is_empty());
}

#[tokio::test]
async fn test_get_metadata_too_many() {
    let client = EhClientBuilder::new().build();
    let list: Vec<(u64, &str)> = (0..26).map(|i| (i, "token")).collect();
    let result = client.get_metadata(&list).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_get_archiver_key() {
    let server = MockServer::start().await;
    let gallery_page_html = r#"
<html><body>
<a onclick="return popUp('https://e-hentai.org/archiver.php?gid=123456&amp;token=fedcba9876',480,320)">Archive Download</a>
</body></html>
"#;
    // Step 1: gallery page contains archiver URL in onclick
    Mock::given(method("GET"))
        .and(path("/g/123456/abcdef0123/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(gallery_page_html))
        .mount(&server)
        .await;
    // Step 2: archiver.php GET returns the archiver_key
    Mock::given(method("GET"))
        .and(path("/archiver.php"))
        .and(query_param("gid", "123456"))
        .and(query_param("token", "fedcba9876"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ARCHIVER_PAGE_HTML))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let key = client
        .get_archiver_key(123456, "abcdef0123")
        .await
        .expect("should get archiver key");
    assert_eq!(key, "470592--63bbddc729b849100ec24ab920ffdb84b6542b23");
}

#[tokio::test]
async fn test_get_archiver_key_not_found() {
    let server = MockServer::start().await;
    // Gallery page with no archiver link
    Mock::given(method("GET"))
        .and(path("/g/999/nonexistent/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>no archiver</html>"))
        .mount(&server)
        .await;

    let client = client_at(&server);
    let result = client.get_archiver_key(999, "nonexistent").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_download_archive_full_flow() {
    let server = MockServer::start().await;

    // Step 1: archiver.php returns HTML with JS redirect
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ARCHIVER_REDIRECT_HTML))
        .mount(&server)
        .await;

    // Step 2: the download URL returns ZIP bytes
    let zip_bytes = test_zip_bytes("image.jpg", b"fake_zip_content");
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes.clone()))
        .mount(&server)
        .await;

    // BUT: the redirect URL is hardcoded to http://123.45.67.89/... in ARCHIVER_REDIRECT_HTML.
    // We need the redirect URL to point to our mock server instead.
    // Re-do with custom redirect HTML:
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );

    // Reset mocks and re-mount with correct redirect
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes.to_vec()))
        .mount(&server)
        .await;

    let client = client_at(&server);
    let tmp = tempfile::NamedTempFile::new().expect("failed to create tempfile");
    let dest = tmp.path();

    let bytes = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", dest)
        .await
        .expect("download should succeed");

    assert_eq!(bytes as usize, zip_bytes.len());

    let saved = std::fs::read(dest).expect("should read saved file");
    assert_eq!(saved, zip_bytes);
}

#[tokio::test]
async fn test_download_archive_resumes_existing_partial_file() {
    let server = MockServer::start().await;
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .expect(1)
        .mount(&server)
        .await;

    let zip_bytes = test_zip_bytes("image.jpg", b"zip_content");
    let split_at = 12;
    let first = &zip_bytes[..split_at];
    let rest = &zip_bytes[split_at..];
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .and(header("range", format!("bytes={}-", first.len())))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header(
                    "Content-Range",
                    format!(
                        "bytes {}-{}/{}",
                        first.len(),
                        first.len() + rest.len() - 1,
                        first.len() + rest.len()
                    ),
                )
                .insert_header("Content-Length", rest.len().to_string())
                .set_body_bytes(rest.to_vec()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    tokio::fs::write(dest.with_extension("zip.part"), first)
        .await
        .unwrap();

    let bytes = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await
        .expect("download should resume and succeed");

    assert_eq!(bytes as usize, zip_bytes.len());
    assert_eq!(std::fs::read(dest).unwrap(), zip_bytes);
}

#[tokio::test]
async fn test_download_archive_rejects_mismatched_content_range() {
    let server = MockServer::start().await;
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .expect(1)
        .mount(&server)
        .await;

    let first = b"PK\x03\x04partial_";
    let rest = b"zip_content";
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .and(header("range", format!("bytes={}-", first.len())))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header(
                    "Content-Range",
                    format!(
                        "bytes {}-{}/{}",
                        first.len() + 1,
                        first.len() + rest.len(),
                        first.len() + rest.len() + 1
                    ),
                )
                .insert_header("Content-Length", rest.len().to_string())
                .set_body_bytes(rest.to_vec()),
        )
        .expect(4)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let part = dest.with_extension("zip.part");
    tokio::fs::write(&part, first).await.unwrap();

    let err = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await
        .expect_err("mismatched Content-Range should fail");

    assert!(err.to_string().contains("Content-Range starts"));
    assert_eq!(std::fs::read(part).unwrap(), first);
    assert!(!dest.exists());
}

#[tokio::test]
async fn test_download_archive_accepts_complete_partial_on_416() {
    let server = MockServer::start().await;
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .expect(1)
        .mount(&server)
        .await;

    let complete = test_zip_bytes("image.jpg", b"complete_zip");
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .and(header("range", format!("bytes={}-", complete.len())))
        .respond_with(ResponseTemplate::new(416))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    tokio::fs::write(dest.with_extension("zip.part"), &complete)
        .await
        .unwrap();

    let bytes = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await
        .expect("complete partial should be accepted on 416");

    assert_eq!(bytes as usize, complete.len());
    assert_eq!(std::fs::read(dest).unwrap(), complete);
}

#[tokio::test]
async fn test_download_archive_restarts_after_invalid_partial_on_416() {
    let server = MockServer::start().await;
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .expect(1)
        .mount(&server)
        .await;

    let incomplete = b"PK\x03\x04incomplete_zip";
    let full_zip = test_zip_bytes("image.jpg", b"fresh_zip_after_restart");
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .and(header("range", format!("bytes={}-", incomplete.len())))
        .respond_with(ResponseTemplate::new(416))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .and(HeaderAbsent("range"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(full_zip.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let part = dest.with_extension("zip.part");
    tokio::fs::write(&part, incomplete).await.unwrap();

    let bytes = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await
        .expect("invalid partial should be discarded and restarted after 416");

    assert_eq!(bytes as usize, full_zip.len());
    assert!(!part.exists(), "invalid partial should be removed");
    assert_eq!(std::fs::read(dest).unwrap(), full_zip);
}

#[tokio::test]
async fn test_download_archive_rejects_incomplete_content_range() {
    let server = MockServer::start().await;
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .expect(1)
        .mount(&server)
        .await;

    let first = b"PK\x03\x04partial_";
    let rest = b"zip_content";
    let total = first.len() + rest.len() + 100;
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .and(header("range", format!("bytes={}-", first.len())))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header(
                    "Content-Range",
                    format!(
                        "bytes {}-{}/{}",
                        first.len(),
                        first.len() + rest.len() - 1,
                        total
                    ),
                )
                .insert_header("Content-Length", rest.len().to_string())
                .set_body_bytes(rest.to_vec()),
        )
        .expect(4)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let part = dest.with_extension("zip.part");
    tokio::fs::write(&part, first).await.unwrap();

    let err = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await
        .expect_err("incomplete Content-Range should fail");

    assert!(err.to_string().contains("Content-Range ended"));
    assert_eq!(std::fs::read(part).unwrap(), first);
    assert!(!dest.exists());
}

#[tokio::test]
async fn test_prepare_and_download_archive_form_flow() {
    let server = MockServer::start().await;
    let gallery_page_html = r#"
<html><body>
<a onclick="return popUp('/archiver.php?gid=4034806&amp;token=fedcba9876',480,320)">Archive Download</a>
</body></html>
"#;
    let archiver_form_html = format!(
        r#"
<html><body>
<form id="hathdl_form" method="post" action="{}/archiver.php?gid=4034806&amp;token=fedcba9876">
  <input type="hidden" name="dltype" value="org" />
  <input type="submit" name="dlcheck" value="Download Original Archive" />
</form>
</body></html>
"#,
        server.uri()
    );
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/4034806/fedcba9876/archive/0?autostart=1";</script>"#,
        server.uri()
    );
    let zip_bytes = test_zip_bytes("image.jpg", b"live_form_zip");

    Mock::given(method("GET"))
        .and(path("/g/4034806/e13b7d119b/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(gallery_page_html))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archiver.php"))
        .and(query_param("gid", "4034806"))
        .and(query_param("token", "fedcba9876"))
        .respond_with(ResponseTemplate::new(200).set_body_string(archiver_form_html))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .and(query_param("gid", "4034806"))
        .and(query_param("token", "fedcba9876"))
        .and(BodyContains("dltype=org"))
        .and(BodyContains("dlcheck=Download+Original+Archive"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archive/4034806/fedcba9876/archive/0"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let request = client
        .prepare_archive_download(4034806, "e13b7d119b", "original")
        .await
        .expect("should prepare form-driven archive request");
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let bytes = client
        .download_archive_with_request(&request, &dest)
        .await
        .expect("form-driven download should succeed");

    assert_eq!(bytes as usize, zip_bytes.len());
    assert_eq!(std::fs::read(dest).unwrap(), zip_bytes);
}

#[tokio::test]
async fn test_download_archive_no_redirect() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>no redirect</html>"))
        .mount(&server)
        .await;

    let client = client_at(&server);
    let tmp = tempfile::NamedTempFile::new().expect("failed to create tempfile");
    let result = client
        .download_archive(123, "tok", "123--abc123def456", "780x", tmp.path())
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_download_archive_archiver_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let client = client_at(&server);
    let tmp = tempfile::NamedTempFile::new().expect("failed to create tempfile");
    let result = client
        .download_archive(123, "tok", "123--abc123def456", "780x", tmp.path())
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_download_archive_invalid_zip_response_cleans_up() {
    let server = MockServer::start().await;
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .mount(&server)
        .await;

    // Return an HTML page instead of a valid ZIP — zip_magic validation will fail
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>error page</html>"))
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let result = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await;
    assert!(result.is_err());
    assert!(!dest.exists(), "dest should not exist after invalid ZIP");
    assert!(
        !dest.with_extension("zip.part").exists(),
        "temp zip should not exist after invalid ZIP"
    );
}

#[tokio::test]
async fn test_download_archive_rejects_corrupt_pk_prefixed_zip() {
    let server = MockServer::start().await;
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(b"PK\x03\x04not_a_complete_zip".to_vec()),
        )
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let result = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await;

    assert!(result.is_err());
    assert!(!dest.exists(), "dest should not exist after corrupt ZIP");
    assert!(
        !dest.with_extension("zip.part").exists(),
        "temp zip should be removed after corrupt ZIP"
    );
}

#[tokio::test]
async fn test_download_archive_rejects_zip_with_corrupt_entry_data() {
    let server = MockServer::start().await;
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );
    let payload = b"valid archive image bytes";
    let corrupt_zip_bytes = corrupt_zip_entry_payload(test_zip_bytes("001.jpg", payload), payload);

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(corrupt_zip_bytes))
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let result = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await;

    let err = result.expect_err("corrupt ZIP entry data should be rejected");
    assert!(
        err.to_string().contains("invalid") || err.to_string().contains("CRC"),
        "unexpected error: {err}"
    );
    assert!(
        !dest.exists(),
        "dest should not exist after corrupt ZIP entry"
    );
    assert!(
        !dest.with_extension("zip.part").exists(),
        "temp zip should be removed after corrupt ZIP entry"
    );
}

#[tokio::test]
async fn test_download_gallery_images_fails_when_one_page_fetch_fails() {
    let server = MockServer::start().await;
    let client = client_at(&server);
    Mock::given(method("GET"))
        .and(path("/g/123/abc/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<a href="/s/1/123-1">1</a><a href="/s/2/123-2">2</a>"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/s/1/123-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<img id="img" src="{}/img/1.jpg">"#,
            server.uri()
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/s/2/123-2"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/img/1.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
        .mount(&server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("gallery.zip");
    let err = client
        .download_gallery_images(123, "abc", &dest)
        .await
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("failed to download all gallery images"));
    assert!(!dest.exists(), "dest should not exist after error");
    assert!(
        !dest.with_extension("zip.part").exists(),
        "temp zip should not exist after error"
    );
}

#[tokio::test]
async fn test_download_gallery_images_fails_when_one_image_fetch_fails() {
    let server = MockServer::start().await;
    let client = client_at(&server);
    Mock::given(method("GET"))
        .and(path("/g/123/abc/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<a href="/s/1/123-1">1</a><a href="/s/2/123-2">2</a>"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/s/1/123-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<img id="img" src="{}/img/1.jpg">"#,
            server.uri()
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/s/2/123-2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<img id="img" src="{}/img/2.jpg">"#,
            server.uri()
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/img/1.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/img/2.jpg"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("gallery.zip");
    let err = client
        .download_gallery_images(123, "abc", &dest)
        .await
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("failed to download all gallery images"));
    assert!(!dest.exists(), "dest should not exist after error");
    assert!(
        !dest.with_extension("zip.part").exists(),
        "temp zip should not exist after error"
    );
}

#[tokio::test]
async fn test_download_gallery_images_fails_when_image_src_missing() {
    let server = MockServer::start().await;
    let client = client_at(&server);
    Mock::given(method("GET"))
        .and(path("/g/123/abc/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"<a href="/s/1/123-1">1</a>"#))
        .mount(&server)
        .await;
    // Image page returns HTML without <img id="img">
    Mock::given(method("GET"))
        .and(path("/s/1/123-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>no image here</html>"))
        .mount(&server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("gallery.zip");
    let err = client
        .download_gallery_images(123, "abc", &dest)
        .await
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("failed to download all gallery images"));
    assert!(!dest.exists(), "dest should not exist after error");
    assert!(
        !dest.with_extension("zip.part").exists(),
        "temp zip should not exist after error"
    );
}

#[tokio::test]
async fn test_download_gallery_images_fails_when_later_gallery_page_fetch_fails() {
    let server = MockServer::start().await;
    let client = client_at(&server);
    Mock::given(method("GET"))
        .and(path("/g/123/abc/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            <table class="ptt"><tr><td>1</td><td><a href="?p=1">2</a></td></tr></table>
            <a href="/s/1/123-1">1</a>
            "#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/g/123/abc/"))
        .and(query_param("p", "1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("gallery.zip");
    let err = client
        .download_gallery_images(123, "abc", &dest)
        .await
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("failed to download all gallery images"));
    assert!(!dest.exists(), "dest should not exist after error");
    assert!(
        !dest.with_extension("zip.part").exists(),
        "temp zip should not exist after error"
    );
}
