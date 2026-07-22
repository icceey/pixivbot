use eh_client::{ArchiveDownloadOptions, EhClient, EhClientBuilder, EhCookies};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zip::write::SimpleFileOptions;

fn test_zip_bytes(name: &str, content: &[u8]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        zip.start_file(
            name,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
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
struct BodyNotContains(&'static str);

impl wiremock::Match for BodyNotContains {
    fn matches(&self, request: &wiremock::Request) -> bool {
        !String::from_utf8_lossy(&request.body).contains(self.0)
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
async fn test_download_archive_restarts_complete_partial_on_416() {
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
    let fresh_zip = test_zip_bytes("image.jpg", b"fresh_zip_after_restart");
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .and(header("range", format!("bytes={}-", complete.len())))
        .respond_with(ResponseTemplate::new(416))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .and(HeaderAbsent("range"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fresh_zip.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let part = dest.with_extension("zip.part");
    tokio::fs::write(dest.with_extension("zip.part"), &complete)
        .await
        .unwrap();

    let bytes = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await
        .expect("complete partial should be discarded and restarted on 416");

    assert_eq!(bytes as usize, fresh_zip.len());
    assert!(!part.exists(), "stale complete partial should be removed");
    assert_eq!(std::fs::read(dest).unwrap(), fresh_zip);
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
async fn test_download_archive_returns_download_in_progress_when_fast_partial() {
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

    // Pre-seed .part so the 206 response takes the append path and validates Content-Range.
    // Content-Range claims total=100000, end=99999, so end+1==total → validate_content_range
    // returns Ok(100000). Body is 20000 bytes per attempt; after 4 attempts total=80001,
    // still < 100000 → Error::Other size-mismatch every attempt.
    // new_bytes=20000, elapsed tiny → made_progress=true → DownloadInProgress.
    let partial_body = vec![0u8; 20000];
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 1-99999/100000")
                .set_body_bytes(partial_body.clone()),
        )
        // 4 attempts (ARCHIVE_DOWNLOAD_MAX_ATTEMPTS)
        .expect(4)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let part = dest.with_extension("zip.part");
    tokio::fs::write(&part, b"x").await.unwrap();

    let err = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await
        .expect_err("fast partial download should fail after 4 attempts");

    assert!(
        matches!(err, eh_client::Error::DownloadInProgress { .. }),
        "expected DownloadInProgress, got: {:?}",
        err
    );
    // .part file should be preserved for resumption
    assert!(
        part.exists(),
        ".part file should be preserved for resumption"
    );
    assert!(!dest.exists(), "final dest should not exist on failure");
}

#[tokio::test]
async fn test_download_archive_returns_plain_error_when_no_progress() {
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

    // Pre-seed .part so the 206 response takes the append path and runs validate_content_range.
    // Content-Range: start=1, end=26, total=127, end+1=27 != 127 → validate_content_range fails
    // BEFORE any bytes are written → new_bytes=0 → made_progress=false → plain Error.
    let rest = b"zip_content";
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 1-26/127")
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
    tokio::fs::write(&part, b"x").await.unwrap();

    let err = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await
        .expect_err("no-progress download should fail");

    assert!(
        !matches!(err, eh_client::Error::DownloadInProgress { .. }),
        "should NOT be DownloadInProgress when no progress was made, got: {:?}",
        err
    );
    assert!(
        err.to_string().contains("Content-Range ended"),
        "should contain Content-Range error message, got: {}",
        err
    );
}

#[tokio::test]
async fn test_prepare_archive_download_1280x_uses_resample_form_and_cost() {
    let server = MockServer::start().await;
    let gallery_page_html = r#"
<html><body>
<a onclick="return popUp('/archiver.php?gid=4034806&amp;token=fedcba9876',480,320)">Archive Download</a>
</body></html>
"#;
    let archiver_form_html = format!(
        r#"
<html><body>
<div>Download Cost: &nbsp; <strong>8,800 GP</strong></div>
<form method="post" action="{}/org-archiver.php?form=org">
  <input type="hidden" name="dltype" value="org" />
  <input type="hidden" name="org_sentinel" value="org-only" />
  <input type="submit" name="dlcheck" value="Download Original Archive" />
</form>
<p>Estimated Size: <strong>400.0 MiB</strong></p>
<div>Download Cost: &nbsp; <strong>218 GP</strong></div>
<form method="post" action="{}/res-archiver.php?form=res">
  <input type="hidden" name="dltype" value="res" />
  <input type="hidden" name="res_sentinel" value="res-only" />
  <input type="submit" name="dlcheck" value="Download Resample Archive" />
</form>
<p>Estimated Size: <strong>5.01 MiB</strong></p>
<p>H@H Downloader</p>
<form id="hathdl_form" method="post" action="{}/archiver.php?form=hathdl">
  <input type="hidden" id="hathdl_xres" name="hathdl_xres" value="" />
</form>
<table><tr>
  <td><p>Original</p><p>400.0 MiB</p><p>8,800 GP</p></td>
  <td><p>800x</p><p>8.0 MiB</p><p>114 GP</p></td>
  <td><p>1280x</p><p>12.5 MiB</p><p>999 GP</p></td>
  <td><p>1920x</p><p>19.25 MiB</p><p>1,999 GP</p></td>
  <td><p>2560x</p><p>25.0 MiB</p><p>2,999 GP</p></td>
</tr></table>
</body></html>
"#,
        server.uri(),
        server.uri(),
        server.uri()
    );
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/4034806/fedcba9876/archive/0?autostart=1";</script>"#,
        server.uri()
    );
    let zip_bytes = test_zip_bytes("image.jpg", b"resample_form_zip");

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
        .and(path("/res-archiver.php"))
        .and(query_param("form", "res"))
        .and(BodyContains("dltype=res"))
        .and(BodyContains("dlcheck=Download+Resample+Archive"))
        .and(BodyContains("res_sentinel=res-only"))
        .and(BodyNotContains("hathdl_xres"))
        .and(BodyNotContains("org_sentinel"))
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
        .prepare_archive_download(4034806, "e13b7d119b", "1280x")
        .await
        .expect("should prepare resample form-driven archive request");
    assert_eq!(request.cost(), &eh_client::parser::DownloadCost::Gp(218));
    assert_eq!(request.estimated_size_bytes(), Some(5_253_366));

    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let bytes = client
        .download_archive_with_request(&request, &dest)
        .await
        .expect("resample form-driven download should succeed");

    assert_eq!(bytes as usize, zip_bytes.len());
    assert_eq!(std::fs::read(dest).unwrap(), zip_bytes);
}

#[tokio::test]
async fn test_prepare_archive_download_rejects_unsupported_resolution_before_network() {
    let server = MockServer::start().await;
    let client = client_at(&server);
    for resolution in ["1600x", "2400x", "bogus", ""] {
        let error = client
            .prepare_archive_download(4034806, "e13b7d119b", resolution)
            .await
            .expect_err("unsupported archive resolution should be rejected");
        assert!(matches!(error, eh_client::Error::Other(_)));
        assert!(error.to_string().contains(&format!(
            "unsupported EH archive resolution '{resolution}'; supported values: 780x, 980x, 1280x, original"
        )));
    }

    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_archive_key_downloads_reject_unsupported_resolution_before_network() {
    let server = MockServer::start().await;
    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");

    for resolution in ["1600x", "2400x", "bogus", ""] {
        let error = client
            .download_archive(
                123456,
                "abcdef0123",
                "123456--abc123def456",
                resolution,
                &dest,
            )
            .await
            .expect_err("unsupported archive resolution should be rejected");
        assert!(matches!(error, eh_client::Error::Other(_)));
        assert!(error.to_string().contains(&format!(
            "unsupported EH archive resolution '{resolution}'; supported values: 780x, 980x, 1280x, original"
        )));

        let error = client
            .download_archive_with_options(
                123456,
                "abcdef0123",
                "123456--abc123def456",
                resolution,
                &dest,
                ArchiveDownloadOptions::default(),
            )
            .await
            .expect_err("unsupported archive resolution should be rejected");
        assert!(matches!(error, eh_client::Error::Other(_)));
        assert!(error.to_string().contains(&format!(
            "unsupported EH archive resolution '{resolution}'; supported values: 780x, 980x, 1280x, original"
        )));
    }

    assert!(server.received_requests().await.unwrap().is_empty());
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

const TEST_ARCHIVE_PATH: &str = "/archive/123456/abcdef0123/abcdef0123/0";
const RANGE_TRUNCATION_BYTES: usize = 64 * 1024;
const LOW_START_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedRangeRequest {
    range: String,
    if_range: Option<String>,
    timestamp: Instant,
    artifact_presence: Option<Vec<bool>>,
}

#[derive(Clone)]
struct ArtifactProbe {
    paths: Vec<PathBuf>,
}

/// Stateful response script for the archive Range surface. Delayed bounded
/// responses are counted until their matching response delay has elapsed.
#[derive(Clone)]
struct ScriptedRangeResponder {
    bytes: Arc<Vec<u8>>,
    etag: Option<String>,
    last_modified: Option<String>,
    requests: Arc<Mutex<Vec<RecordedRangeRequest>>>,
    first_open_ended_delay: Option<Duration>,
    first_open_ended_truncated: Arc<AtomicBool>,
    first_low_bounded_delay: Option<Duration>,
    first_low_bounded_delayed: Arc<AtomicBool>,
    first_low_bounded_truncated: Arc<AtomicBool>,
    bounded_delay: Option<Duration>,
    truncate_every_bounded: bool,
    delayed_bounded_active: Arc<AtomicUsize>,
    delayed_bounded_max_active: Arc<AtomicUsize>,
    artifact_probe: Option<ArtifactProbe>,
}

impl ScriptedRangeResponder {
    fn new(bytes: Arc<Vec<u8>>) -> Self {
        Self {
            bytes,
            etag: None,
            last_modified: None,
            requests: Arc::new(Mutex::new(Vec::new())),
            first_open_ended_delay: None,
            first_open_ended_truncated: Arc::new(AtomicBool::new(false)),
            first_low_bounded_delay: None,
            first_low_bounded_delayed: Arc::new(AtomicBool::new(false)),
            first_low_bounded_truncated: Arc::new(AtomicBool::new(false)),
            bounded_delay: None,
            truncate_every_bounded: false,
            delayed_bounded_active: Arc::new(AtomicUsize::new(0)),
            delayed_bounded_max_active: Arc::new(AtomicUsize::new(0)),
            artifact_probe: None,
        }
    }

    fn with_etag(mut self, etag: &str) -> Self {
        self.etag = Some(etag.to_owned());
        self
    }

    fn with_last_modified(mut self, last_modified: &str) -> Self {
        self.last_modified = Some(last_modified.to_owned());
        self
    }

    fn delay_first_open_ended(mut self, delay: Duration, truncate: bool) -> Self {
        assert!(delay >= Duration::from_millis(1100));
        self.first_open_ended_delay = Some(delay);
        self.first_open_ended_truncated
            .store(truncate, Ordering::SeqCst);
        self
    }

    fn delay_and_truncate_first_low_bounded(mut self, delay: Duration) -> Self {
        self.first_low_bounded_delay = Some(delay);
        self.first_low_bounded_delayed.store(true, Ordering::SeqCst);
        self.first_low_bounded_truncated
            .store(true, Ordering::SeqCst);
        self
    }

    fn delay_first_low_bounded(mut self, delay: Duration) -> Self {
        self.first_low_bounded_delay = Some(delay);
        self.first_low_bounded_delayed.store(true, Ordering::SeqCst);
        self
    }

    fn delay_all_bounded(mut self, delay: Duration) -> Self {
        self.bounded_delay = Some(delay);
        self
    }

    fn truncate_every_bounded(mut self) -> Self {
        self.truncate_every_bounded = true;
        self
    }

    fn with_artifact_probe(mut self, paths: Vec<PathBuf>) -> Self {
        self.artifact_probe = Some(ArtifactProbe { paths });
        self
    }

    fn requests(&self) -> Vec<RecordedRangeRequest> {
        self.requests.lock().expect("range request lock").clone()
    }

    fn max_active(&self) -> usize {
        self.delayed_bounded_max_active.load(Ordering::SeqCst)
    }

    fn record_request(&self, range: &str, if_range: Option<String>) {
        let artifact_presence = self.artifact_probe.as_ref().map(|probe| {
            probe
                .paths
                .iter()
                .map(|path| path.exists())
                .collect::<Vec<_>>()
        });
        self.requests
            .lock()
            .expect("range request lock")
            .push(RecordedRangeRequest {
                range: range.to_owned(),
                if_range,
                timestamp: Instant::now(),
                artifact_presence,
            });
    }

    fn response_for(&self, start: u64, end: u64, open_ended: bool) -> ResponseTemplate {
        let mut body = self.bytes[start as usize..end as usize].to_vec();
        let delay = if open_ended {
            if self
                .first_open_ended_truncated
                .swap(false, Ordering::SeqCst)
            {
                body.truncate(RANGE_TRUNCATION_BYTES);
            }
            self.first_open_ended_delay
        } else {
            let first_low_bounded = start < LOW_START_LIMIT
                && self.first_low_bounded_delayed.swap(false, Ordering::SeqCst);
            if first_low_bounded {
                if self
                    .first_low_bounded_truncated
                    .swap(false, Ordering::SeqCst)
                {
                    body.truncate(RANGE_TRUNCATION_BYTES);
                }
            } else {
                if self.truncate_every_bounded {
                    body.truncate(RANGE_TRUNCATION_BYTES);
                }
            }

            let delay = if first_low_bounded {
                self.first_low_bounded_delay.or(self.bounded_delay)
            } else {
                self.bounded_delay
            };
            if let Some(delay) = delay {
                let active = self.delayed_bounded_active.fetch_add(1, Ordering::SeqCst) + 1;
                self.delayed_bounded_max_active
                    .fetch_max(active, Ordering::SeqCst);
                let delayed_bounded_active = Arc::clone(&self.delayed_bounded_active);
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    delayed_bounded_active.fetch_sub(1, Ordering::SeqCst);
                });
            }
            delay
        };

        let mut response = ResponseTemplate::new(206)
            .insert_header(
                "Content-Range",
                format!("bytes {start}-{}/{}", end - 1, self.bytes.len()),
            )
            .set_body_bytes(body);
        if let Some(etag) = &self.etag {
            response = response.insert_header("ETag", etag);
        }
        if let Some(last_modified) = &self.last_modified {
            response = response.insert_header("Last-Modified", last_modified);
        }
        if let Some(delay) = delay {
            response = response.set_delay(delay);
        }
        response
    }
}

impl wiremock::Respond for ScriptedRangeResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let range = request
            .headers
            .get("range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .expect("multipart request must include Range");
        let (start, end) = parse_test_range(&range, self.bytes.len() as u64);
        let if_range = request
            .headers
            .get("if-range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        self.record_request(&range, if_range);
        self.response_for(start, end, range.ends_with('-'))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedProtocolResetRequest {
    range: Option<String>,
    if_range: Option<String>,
    artifact_presence: Vec<bool>,
}

#[derive(Clone)]
enum ProtocolResetFault {
    Status200,
    Status416,
    InvalidContentRange,
    ChangedTotal,
    Validator(Option<(&'static str, &'static str)>),
}

#[derive(Clone)]
struct ProtocolResetResponder {
    bytes: Arc<Vec<u8>>,
    fault: ProtocolResetFault,
    requests: Arc<Mutex<Vec<RecordedProtocolResetRequest>>>,
    assembly_scratch: PathBuf,
    parts_dir: PathBuf,
}

impl ProtocolResetResponder {
    fn new(bytes: Arc<Vec<u8>>, fault: ProtocolResetFault, dest: &Path) -> Self {
        Self {
            bytes,
            fault,
            requests: Arc::new(Mutex::new(Vec::new())),
            assembly_scratch: dest.with_extension("zip.part"),
            parts_dir: dest.with_extension("zip.parts"),
        }
    }

    fn requests(&self) -> Vec<RecordedProtocolResetRequest> {
        self.requests
            .lock()
            .expect("protocol reset request lock")
            .clone()
    }

    fn exact_range_response(&self, start: u64, end: u64) -> ResponseTemplate {
        ResponseTemplate::new(206)
            .insert_header(
                "Content-Range",
                format!("bytes {start}-{}/{}", end - 1, self.bytes.len()),
            )
            .set_body_bytes(self.bytes[start as usize..end as usize].to_vec())
    }
}

impl wiremock::Respond for ProtocolResetResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let range = request
            .headers
            .get("range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let if_range = request
            .headers
            .get("if-range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        self.requests
            .lock()
            .expect("protocol reset request lock")
            .push(RecordedProtocolResetRequest {
                range: range.clone(),
                if_range,
                artifact_presence: vec![self.assembly_scratch.exists(), self.parts_dir.exists()],
            });

        let Some(range) = range else {
            return ResponseTemplate::new(200).set_body_bytes((*self.bytes).clone());
        };
        let (start, end) = parse_test_range(&range, self.bytes.len() as u64);
        if start != 0 {
            return self
                .exact_range_response(start, end)
                .set_delay(Duration::from_secs(5));
        }

        // Give a second seeded worker an opportunity to enter its in-flight
        // GET before the protocol fault asks the coordinator to cancel it.
        std::thread::sleep(Duration::from_millis(100));
        match &self.fault {
            ProtocolResetFault::Status200 => {
                ResponseTemplate::new(200).set_body_bytes((*self.bytes).clone())
            }
            ProtocolResetFault::Status416 => ResponseTemplate::new(416),
            ProtocolResetFault::InvalidContentRange => ResponseTemplate::new(206)
                .insert_header(
                    "Content-Range",
                    format!("bytes {start}-{}/{}", end - 2, self.bytes.len()),
                )
                .set_body_bytes(self.bytes[start as usize..end as usize].to_vec()),
            ProtocolResetFault::ChangedTotal => {
                self.exact_range_response(start, end).insert_header(
                    "Content-Range",
                    format!("bytes {start}-{}/{}", end - 1, self.bytes.len() + 1),
                )
            }
            ProtocolResetFault::Validator(header) => {
                let mut response = self.exact_range_response(start, end);
                if let Some((name, value)) = header {
                    response = response.insert_header(*name, *value);
                }
                response
            }
        }
    }
}

fn parse_test_range(range: &str, total: u64) -> (u64, u64) {
    let range = range
        .strip_prefix("bytes=")
        .expect("Range header must use bytes unit");
    let (start, end) = range
        .split_once('-')
        .expect("Range header must have a dash");
    let start = start
        .parse::<u64>()
        .expect("Range start must be an integer");
    let end = if end.is_empty() {
        total
    } else {
        end.parse::<u64>()
            .expect("Range end must be an integer")
            .checked_add(1)
            .expect("Range end must not overflow")
    };
    assert!(start < end, "Range must not be empty");
    assert!(end <= total, "Range must not exceed test archive");
    (start, end)
}

fn seed_multipart_manifest(
    dest: &Path,
    download_url: &str,
    total_len: u64,
    etag: Option<&str>,
    last_modified: Option<&str>,
    parts: &[(u64, u64, u64, &[u8])],
) -> PathBuf {
    let parts_dir = dest.with_extension("zip.parts");
    std::fs::create_dir_all(&parts_dir).expect("create multipart test directory");
    for (id, _, _, prefix) in parts {
        std::fs::write(parts_dir.join(format!("part-{id:016}")), prefix)
            .expect("write multipart test part");
    }
    let next_part_id = parts
        .iter()
        .map(|(id, _, _, _)| *id)
        .max()
        .map_or(1, |id| id + 1);
    let manifest = serde_json::json!({
        "version": 1,
        "download_url": download_url,
        "total_len": total_len,
        "etag": etag,
        "last_modified": last_modified,
        "next_part_id": next_part_id,
        "parts": parts.iter().map(|(id, start, end, _)| serde_json::json!({
            "id": id,
            "start": start,
            "end": end,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(
        parts_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).expect("serialize multipart test manifest"),
    )
    .expect("write multipart test manifest");
    parts_dir
}

fn archive_url(server: &MockServer) -> String {
    format!("{}{}?start=1", server.uri(), TEST_ARCHIVE_PATH)
}

async fn mount_archive_post_redirect(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<script>document.location = "{}";</script>"#,
            archive_url(server)
        )))
        .mount(server)
        .await;
}

fn stored_zip_with_payload(payload_len: usize) -> Vec<u8> {
    test_zip_bytes("payload.bin", &vec![0x5a; payload_len])
}

fn bounded_ranges(requests: &[RecordedRangeRequest]) -> Vec<(u64, u64)> {
    requests
        .iter()
        .filter(|request| !request.range.ends_with('-'))
        .map(|request| parse_test_range(&request.range, u64::MAX))
        .collect()
}

#[derive(Clone, Copy)]
enum ProtocolResetSeed {
    CurrentUrl,
    ChangedUrl,
    Fresh,
}

fn seed_protocol_reset_state(
    dest: &Path,
    download_url: &str,
    zip: &[u8],
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> PathBuf {
    let midpoint = zip.len() as u64 / 2;
    let parts_dir = seed_multipart_manifest(
        dest,
        download_url,
        zip.len() as u64,
        etag,
        last_modified,
        &[(0, 0, midpoint, &[]), (1, midpoint, zip.len() as u64, &[])],
    );
    std::fs::write(dest.with_extension("zip.part"), b"stale assembly")
        .expect("seed stale assembly scratch");
    parts_dir
}

async fn run_protocol_reset_case(
    fault: ProtocolResetFault,
    etag: Option<&str>,
    last_modified: Option<&str>,
    seed: ProtocolResetSeed,
) -> Vec<RecordedProtocolResetRequest> {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(256 * 1024));
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    let download_url = archive_url(&server);
    let expected_range_requests = match seed {
        ProtocolResetSeed::CurrentUrl => {
            seed_protocol_reset_state(&dest, &download_url, &zip, etag, last_modified);
            1
        }
        ProtocolResetSeed::ChangedUrl => {
            seed_protocol_reset_state(
                &dest,
                &format!("{}/superseded-archive?start=1", server.uri()),
                &zip,
                etag,
                last_modified,
            );
            0
        }
        ProtocolResetSeed::Fresh => 1,
    };
    let responder = ProtocolResetResponder::new(Arc::clone(&zip), fault, &dest);
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let bytes = client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 2 },
        )
        .await
        .expect("protocol mismatch must restart sequentially exactly once");

    assert_eq!(bytes as usize, zip.len());
    assert_eq!(std::fs::read(&dest).unwrap(), *zip);
    assert!(!dest.with_extension("zip.part").exists());
    assert!(!dest.with_extension("zip.parts").exists());

    let requests = responder.requests();
    let fallback_index = requests
        .iter()
        .position(|request| request.range.is_none())
        .expect("sequential fallback request must be recorded");
    let range_requests = requests
        .iter()
        .filter(|request| request.range.is_some())
        .collect::<Vec<_>>();
    match seed {
        ProtocolResetSeed::CurrentUrl => assert!(
            (expected_range_requests..=expected_range_requests + 1).contains(&range_requests.len()),
            "the protocol-fault worker and any already-started peer must finish before fallback"
        ),
        ProtocolResetSeed::ChangedUrl | ProtocolResetSeed::Fresh => {
            assert_eq!(range_requests.len(), expected_range_requests)
        }
    }
    assert!(
        requests[..fallback_index]
            .iter()
            .all(|request| request.range.is_some()),
        "every multipart worker must be joined before the no-Range fallback"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.range.is_none())
            .count(),
        1,
        "protocol recovery must restart sequentially once"
    );
    assert_eq!(
        requests[fallback_index].artifact_presence,
        vec![false, false],
        "assembly scratch and multipart state must be gone before fallback"
    );
    requests
}

#[tokio::test]
async fn test_multipart_starts_with_one_open_ended_range_and_assembles_exact_zip() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(4 * 1024 * 1024));
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip))
        .delay_first_open_ended(Duration::from_millis(1100), true);
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    let bytes = client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 2 },
        )
        .await
        .expect("multipart download should succeed");

    assert_eq!(bytes as usize, zip.len());
    let requests = responder.requests();
    assert_eq!(requests[0].range, "bytes=0-");
    let bounded = requests
        .iter()
        .filter(|request| !request.range.ends_with('-'))
        .collect::<Vec<_>>();
    assert!(
        !bounded.is_empty(),
        "the delayed initial response must be sampled before dynamic splitting"
    );
    assert!(
        bounded[0].timestamp.duration_since(requests[0].timestamp) >= Duration::from_millis(900),
        "no bounded request may precede the first delayed part sample"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), *zip);
    assert!(!dest.with_extension("zip.part").exists());
    assert!(!dest.with_extension("zip.parts").exists());
}

#[tokio::test]
async fn test_multipart_dynamic_split_uses_two_actual_connections_without_overlap() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(4 * 1024 * 1024));
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip))
        .delay_first_open_ended(Duration::from_millis(1100), true)
        .with_last_modified("Tue, 21 Jul 2026 12:00:00 GMT")
        .delay_all_bounded(Duration::from_millis(1100));
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 2 },
        )
        .await
        .expect("multipart split download should succeed");

    let requests = responder.requests();
    assert_eq!(requests[0].range, "bytes=0-");
    assert_eq!(
        responder.max_active(),
        2,
        "two delayed bounded GETs must overlap"
    );
    let bounded = bounded_ranges(&requests);
    let total = zip.len() as u64;
    let (low_start, low_end, high_start, high_end) = bounded
        .iter()
        .flat_map(|&(low_start, low_end)| {
            bounded.iter().filter_map(move |&(high_start, high_end)| {
                (low_start > 0
                    && low_start <= RANGE_TRUNCATION_BYTES as u64
                    && low_end == high_start
                    && high_end == total)
                    .then_some((low_start, low_end, high_start, high_end))
            })
        })
        .next()
        .unwrap_or_else(|| {
            panic!(
                "selected initial part must be joined and relaunched from its durable length: {bounded:?}"
            )
        });
    assert!(
        low_start <= RANGE_TRUNCATION_BYTES as u64,
        "relaunch offset must come from the initial response's truncated durable prefix"
    );
    assert_eq!(low_end, high_start);
    assert_eq!(high_end, total);
    assert_eq!(std::fs::read(&dest).unwrap(), *zip);
}

#[tokio::test]
async fn test_multipart_completion_reuses_slot_by_splitting_largest_remaining_interval() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(8 * 1024 * 1024));
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip))
        .delay_first_open_ended(Duration::from_millis(1100), true)
        .delay_first_low_bounded(Duration::from_millis(1100));
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 3 },
        )
        .await
        .expect("multipart work stealing download should succeed");

    let bounded = bounded_ranges(&responder.requests());
    let initial_restart = bounded
        .iter()
        .map(|(start, _)| *start)
        .min()
        .expect("initial truncation must launch a bounded continuation");
    assert!(initial_restart > 0 && initial_restart <= RANGE_TRUNCATION_BYTES as u64);
    assert!(
        bounded
            .iter()
            .filter(|&&(start, _)| start == initial_restart)
            .count()
            >= 2,
        "completion must release a slot and trigger a second split of the sampled low interval: {bounded:?}"
    );
    assert!(
        bounded.len() >= 3,
        "the second split must create another bounded child"
    );
    assert!(responder.max_active() <= 3);
    assert_eq!(std::fs::read(&dest).unwrap(), *zip);
}

#[tokio::test]
async fn test_multipart_small_archive_does_not_force_configured_slot_count() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(128 * 1024));
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip))
        .delay_first_open_ended(Duration::from_millis(1100), false);
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 4 },
        )
        .await
        .expect("small multipart archive should succeed");

    let requests = responder.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].range, "bytes=0-");
    assert_eq!(std::fs::read(&dest).unwrap(), *zip);
}

#[tokio::test]
async fn test_multipart_restart_resumes_offsets_with_strong_etag_even_when_limit_is_one() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(256 * 1024));
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    let offset = 32 * 1024;
    let parts_dir = seed_multipart_manifest(
        &dest,
        &archive_url(&server),
        zip.len() as u64,
        Some("\"strong-v1\""),
        None,
        &[(0, 0, zip.len() as u64, &zip[..offset])],
    );
    std::fs::write(dest.with_extension("zip.part"), b"stale assembly").unwrap();
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip))
        .with_etag("\"strong-v1\"")
        .with_artifact_probe(vec![dest.with_extension("zip.part")]);
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 1 },
        )
        .await
        .expect("valid multipart state must resume even with one slot");

    let requests = responder.requests();
    assert_eq!(
        requests.len(),
        1,
        "one recovered interval needs one active task"
    );
    assert_eq!(
        requests[0].range,
        format!("bytes={offset}-{}", zip.len() - 1)
    );
    assert_eq!(requests[0].if_range.as_deref(), Some("\"strong-v1\""));
    assert_eq!(requests[0].artifact_presence, Some(vec![false]));
    assert_eq!(std::fs::read(&dest).unwrap(), *zip);
    assert!(!parts_dir.exists());
}

#[tokio::test]
async fn test_multipart_restart_without_validator_uses_url_total_and_content_range() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(256 * 1024));
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    let offset = 48 * 1024;
    seed_multipart_manifest(
        &dest,
        &archive_url(&server),
        zip.len() as u64,
        None,
        None,
        &[(0, 0, zip.len() as u64, &zip[..offset])],
    );
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip));
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 1 },
        )
        .await
        .expect("validator-free multipart state should resume from its durable offset");

    let requests = responder.requests();
    assert_eq!(
        requests[0].range,
        format!("bytes={offset}-{}", zip.len() - 1)
    );
    assert_eq!(requests[0].if_range, None);
    assert_eq!(std::fs::read(&dest).unwrap(), *zip);
}

#[tokio::test]
async fn test_multipart_recovery_removes_unreferenced_files_after_validation() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(256 * 1024));
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    let offset = 32 * 1024;
    let parts_dir = seed_multipart_manifest(
        &dest,
        &archive_url(&server),
        zip.len() as u64,
        None,
        None,
        &[(0, 0, zip.len() as u64, &zip[..offset])],
    );
    let referenced = parts_dir.join("part-0000000000000000");
    let unreferenced = parts_dir.join("part-0000000000000099");
    let abandoned_manifest = parts_dir.join("manifest.json.tmp-abandoned");
    std::fs::write(&unreferenced, b"orphan").unwrap();
    std::fs::write(&abandoned_manifest, b"orphan").unwrap();
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip)).with_artifact_probe(vec![
        referenced,
        unreferenced.clone(),
        abandoned_manifest.clone(),
    ]);
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 1 },
        )
        .await
        .expect("valid multipart state should recover and finish");

    assert_eq!(
        responder.requests()[0].artifact_presence,
        Some(vec![true, false, false])
    );
    assert!(!unreferenced.exists());
    assert!(!abandoned_manifest.exists());
    assert_eq!(std::fs::read(&dest).unwrap(), *zip);
}

#[tokio::test]
async fn test_malformed_manifest_clears_all_multipart_state_and_starts_sequentially() {
    let zip = test_zip_bytes("image.jpg", b"sequential fallback archive");
    for case in ["corrupt_json", "gap", "missing", "oversized"] {
        let server = MockServer::start().await;
        mount_archive_post_redirect(&server).await;
        Mock::given(method("GET"))
            .and(path(TEST_ARCHIVE_PATH))
            .and(HeaderAbsent("range"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join(format!("{case}.zip"));
        let total = zip.len() as u64;
        let parts_dir = match case {
            "corrupt_json" => {
                let parts_dir = dest.with_extension("zip.parts");
                std::fs::create_dir_all(&parts_dir).unwrap();
                std::fs::write(parts_dir.join("manifest.json"), b"{").unwrap();
                parts_dir
            }
            "gap" => seed_multipart_manifest(
                &dest,
                &archive_url(&server),
                total,
                None,
                None,
                &[(0, 0, total / 2, &[]), (1, total / 2 + 1, total, &[])],
            ),
            "missing" => {
                let parts_dir = seed_multipart_manifest(
                    &dest,
                    &archive_url(&server),
                    total,
                    None,
                    None,
                    &[(0, 0, total, &[])],
                );
                std::fs::remove_file(parts_dir.join("part-0000000000000000")).unwrap();
                parts_dir
            }
            "oversized" => {
                let mut oversized = zip.clone();
                oversized.push(0);
                seed_multipart_manifest(
                    &dest,
                    &archive_url(&server),
                    total,
                    None,
                    None,
                    &[(0, 0, total, &oversized)],
                )
            }
            _ => unreachable!(),
        };
        std::fs::write(dest.with_extension("zip.part"), b"stale assembly").unwrap();

        client_at(&server)
            .download_archive_with_options(
                123456,
                "abcdef0123",
                "123456--abc123def456",
                "780x",
                &dest,
                ArchiveDownloadOptions { max_concurrency: 2 },
            )
            .await
            .unwrap_or_else(|error| panic!("{case} should restart sequentially: {error}"));

        assert!(
            !parts_dir.exists(),
            "{case} invalid multipart state must be purged"
        );
        assert!(!dest.with_extension("zip.part").exists());
        assert_eq!(std::fs::read(&dest).unwrap(), zip, "{case}");
    }
}

#[tokio::test]
async fn test_archive_options_zero_is_rejected_before_archiver_post() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    let error = client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 0 },
        )
        .await
        .expect_err("zero archive concurrency must be rejected before the POST");

    assert_eq!(
        error.to_string(),
        "archive download max_concurrency must be at least 1"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_manifest_recovery_io_error_preserves_state_and_skips_archive_get() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    let parts_dir = dest.with_extension("zip.parts");
    let manifest_dir = parts_dir.join("manifest.json");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    let sentinel = parts_dir.join("part-0000000000000077");
    std::fs::write(&sentinel, b"sentinel").unwrap();
    let stale_scratch = dest.with_extension("zip.part");
    std::fs::write(&stale_scratch, b"stale assembly").unwrap();

    let error = client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 2 },
        )
        .await
        .expect_err("manifest I/O failures must propagate without a fallback GET");

    assert!(matches!(error, eh_client::Error::Io(_)));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "only the archiver POST may be issued");
    assert!(manifest_dir.is_dir());
    assert!(sentinel.is_file());
    assert!(stale_scratch.is_file());
}

#[tokio::test]
async fn test_multipart_part_retries_from_durable_offset_after_truncated_body() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(1024 * 1024));
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    seed_multipart_manifest(
        &dest,
        &archive_url(&server),
        zip.len() as u64,
        None,
        None,
        &[(0, 0, zip.len() as u64, &[])],
    );
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip))
        .delay_and_truncate_first_low_bounded(Duration::from_millis(1));
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 1 },
        )
        .await
        .expect("truncated part must retry and complete");

    let requests = responder.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].range, format!("bytes=0-{}", zip.len() - 1));
    assert_eq!(
        requests[1].range,
        format!("bytes={RANGE_TRUNCATION_BYTES}-{}", zip.len() - 1)
    );
    assert_eq!(std::fs::read(&dest).unwrap(), *zip);
}

#[tokio::test]
async fn test_multipart_part_exhausts_four_attempts_and_preserves_state() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(1024 * 1024));
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    let parts_dir = seed_multipart_manifest(
        &dest,
        &archive_url(&server),
        zip.len() as u64,
        None,
        None,
        &[(0, 0, zip.len() as u64, &[])],
    );
    let manifest = parts_dir.join("manifest.json");
    let part = parts_dir.join("part-0000000000000000");
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip)).truncate_every_bounded();
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let error = client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 1 },
        )
        .await
        .expect_err("four truncated part attempts must leave recoverable state");

    let requests = responder.requests();
    assert_eq!(requests.len(), 4, "a part gets exactly four attempts");
    for pair in requests.windows(2) {
        assert!(
            pair[1].timestamp.duration_since(pair[0].timestamp) >= Duration::from_millis(900),
            "retry requests must be separated by the one-second backoff"
        );
    }
    assert!(
        error.to_string().contains("archive part")
            || matches!(error, eh_client::Error::DownloadInProgress { .. }),
        "retry exhaustion must return the part failure or aggregate progress classification"
    );
    assert!(manifest.is_file());
    assert!(part.is_file());
    assert!(part.metadata().unwrap().len() < zip.len() as u64);
    assert!(!dest.exists());
}

#[tokio::test]
async fn test_multipart_200_if_range_fallback_resets_to_sequential() {
    let requests = run_protocol_reset_case(
        ProtocolResetFault::Status200,
        Some("\"strong-v1\""),
        None,
        ProtocolResetSeed::CurrentUrl,
    )
    .await;
    let initial = requests
        .iter()
        .find(|request| {
            request
                .range
                .as_deref()
                .is_some_and(|range| range.starts_with("bytes=0-"))
        })
        .expect("initial multipart request");
    assert_eq!(initial.if_range.as_deref(), Some("\"strong-v1\""));
}

#[tokio::test]
async fn test_multipart_416_resets_to_sequential() {
    run_protocol_reset_case(
        ProtocolResetFault::Status416,
        None,
        None,
        ProtocolResetSeed::CurrentUrl,
    )
    .await;
}

#[tokio::test]
async fn test_multipart_invalid_content_range_resets_to_sequential() {
    run_protocol_reset_case(
        ProtocolResetFault::InvalidContentRange,
        None,
        None,
        ProtocolResetSeed::CurrentUrl,
    )
    .await;
}

#[tokio::test]
async fn test_multipart_url_change_resets_before_range_request() {
    run_protocol_reset_case(
        ProtocolResetFault::Status200,
        None,
        None,
        ProtocolResetSeed::ChangedUrl,
    )
    .await;
}

#[tokio::test]
async fn test_multipart_total_change_resets_to_sequential() {
    run_protocol_reset_case(
        ProtocolResetFault::ChangedTotal,
        None,
        None,
        ProtocolResetSeed::CurrentUrl,
    )
    .await;
}

#[tokio::test]
async fn test_multipart_validator_change_or_missing_validator_resets() {
    for (etag, last_modified, response_validator, expected_if_range) in [
        (
            Some("\"strong-v1\""),
            None,
            Some(("ETag", "\"strong-v2\"")),
            "\"strong-v1\"",
        ),
        (Some("\"strong-v1\""), None, None, "\"strong-v1\""),
        (
            None,
            Some("Tue, 21 Jul 2026 12:00:00 GMT"),
            Some(("Last-Modified", "Wed, 22 Jul 2026 12:00:00 GMT")),
            "Tue, 21 Jul 2026 12:00:00 GMT",
        ),
        (
            None,
            Some("Tue, 21 Jul 2026 12:00:00 GMT"),
            None,
            "Tue, 21 Jul 2026 12:00:00 GMT",
        ),
    ] {
        let requests = run_protocol_reset_case(
            ProtocolResetFault::Validator(response_validator),
            etag,
            last_modified,
            ProtocolResetSeed::CurrentUrl,
        )
        .await;
        let initial = requests
            .iter()
            .find(|request| {
                request
                    .range
                    .as_deref()
                    .is_some_and(|range| range.starts_with("bytes=0-"))
            })
            .expect("initial multipart request");
        assert_eq!(initial.if_range.as_deref(), Some(expected_if_range));
    }
}

#[tokio::test]
async fn test_multipart_initial_416_resets_to_sequential() {
    run_protocol_reset_case(
        ProtocolResetFault::Status416,
        None,
        None,
        ProtocolResetSeed::Fresh,
    )
    .await;
}

#[tokio::test]
async fn test_multipart_initial_invalid_content_range_resets_to_sequential() {
    run_protocol_reset_case(
        ProtocolResetFault::InvalidContentRange,
        None,
        None,
        ProtocolResetSeed::Fresh,
    )
    .await;
}

#[tokio::test]
async fn test_multipart_truncated_parts_preserve_aggregate_progress() {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(stored_zip_with_payload(1024 * 1024));
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    let midpoint = zip.len() as u64 / 2;
    let first_prefix = &zip[..17];
    let second_prefix = &zip[midpoint as usize..midpoint as usize + 23];
    let parts_dir = seed_multipart_manifest(
        &dest,
        &archive_url(&server),
        zip.len() as u64,
        None,
        None,
        &[
            (0, 0, midpoint, first_prefix),
            (1, midpoint, zip.len() as u64, second_prefix),
        ],
    );
    let initial_downloaded = (first_prefix.len() + second_prefix.len()) as u64;
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip)).truncate_every_bounded();
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let error = client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 2 },
        )
        .await
        .expect_err("truncated multipart parts must exhaust retry attempts");

    let bytes_delta = match error {
        eh_client::Error::DownloadInProgress { bytes_delta, .. } => bytes_delta,
        other => panic!("expected aggregate DownloadInProgress, got {other:?}"),
    };
    let final_downloaded = [0_u64, 1]
        .into_iter()
        .map(|id| {
            parts_dir
                .join(format!("part-{id:016}"))
                .metadata()
                .unwrap()
                .len()
        })
        .sum::<u64>();
    assert_eq!(bytes_delta, final_downloaded - initial_downloaded);
    assert!(
        responder.requests().len() >= 4,
        "one part must use all four attempts before coordinator shutdown"
    );
    assert!(
        [0_u64, 1].into_iter().all(|id| {
            parts_dir
                .join(format!("part-{id:016}"))
                .metadata()
                .unwrap()
                .len()
                > 23
        }),
        "both persisted intervals must contribute durable bytes"
    );
    assert!(parts_dir.exists());
    assert!(!dest.exists());
}

async fn assert_multipart_validation_removes_artifacts(corrupt_zip: Vec<u8>) {
    let server = MockServer::start().await;
    mount_archive_post_redirect(&server).await;
    let zip = Arc::new(corrupt_zip);
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("archive.zip");
    seed_protocol_reset_state(&dest, &archive_url(&server), &zip, None, None);
    let responder = ScriptedRangeResponder::new(Arc::clone(&zip));
    Mock::given(method("GET"))
        .and(path(TEST_ARCHIVE_PATH))
        .respond_with(responder)
        .mount(&server)
        .await;

    let error = client_at(&server)
        .download_archive_with_options(
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "780x",
            &dest,
            ArchiveDownloadOptions { max_concurrency: 2 },
        )
        .await
        .expect_err("corrupt assembled ZIP must be rejected");

    assert!(
        error.to_string().contains("downloaded") || error.to_string().contains("CRC"),
        "ZIP validation error must be preserved: {error}"
    );
    assert!(!dest.exists());
    assert!(!dest.with_extension("zip.part").exists());
    assert!(!dest.with_extension("zip.parts").exists());
}

#[tokio::test]
async fn test_multipart_final_zip_corruption_removes_assembly_and_parts() {
    assert_multipart_validation_removes_artifacts(b"PK\x03\x04not_a_complete_zip".to_vec()).await;
}

#[tokio::test]
async fn test_multipart_corrupt_entry_removes_assembly_and_parts() {
    let payload = b"stored ZIP entry payload";
    assert_multipart_validation_removes_artifacts(corrupt_zip_entry_payload(
        test_zip_bytes("payload.bin", payload),
        payload,
    ))
    .await;
}
