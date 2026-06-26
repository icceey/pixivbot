use eh_client::{EhClient, EhClientBuilder, EhCookies};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

const GALLERY_PAGE_HTML: &str = r#"
<html><body>
<a href="https://e-hentai.org/archiver.php?gid=123456&token=abcdef0123&or=470592--63bbddc729b849100ec24ab920ffdb84b6542b23">Archive Download</a>
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
    // The gallery page URL is /g/{gid}/{token}/
    Mock::given(method("GET"))
        .and(path("/g/123456/abcdef0123/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(GALLERY_PAGE_HTML))
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
    let zip_bytes = b"PK\x03\x04fake_zip_content";
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes.to_vec()))
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
        .download_archive(123456, "abcdef0123", "780x", dest)
        .await
        .expect("download should succeed");

    assert_eq!(bytes as usize, zip_bytes.len());

    let saved = std::fs::read(dest).expect("should read saved file");
    assert_eq!(saved, zip_bytes);
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
        .download_archive(123, "tok", "780x", tmp.path())
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
        .download_archive(123, "tok", "780x", tmp.path())
        .await;
    assert!(result.is_err());
}
