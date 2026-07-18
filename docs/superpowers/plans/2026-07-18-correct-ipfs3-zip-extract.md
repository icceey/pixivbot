# Correct ipfS3 ZIP Extract Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Correct `IpfS3Uploader` so an enabled EH ZIP upload sends one SigV4-signed `decompress-zip` PutObject, validates the returned `DecompressZipResult` XML, and returns ordered Telegraph URLs built only from each requested image entry's CID.

**Architecture:** Keep `ImageUploader::upload_zip_archive_with_url_pairs` and `zip_extract_enabled` unchanged, and keep the correction inside `eh_client::telegraph::IpfS3Uploader`. Separate pure XML/result validation and ordered key-to-CID mapping from the `rust-s3` transport; the transport clones the configured bucket, adds the extraction query only to that clone, uses rust-s3's public low-level Tokio request API to preserve the XML response body, then maps validated entry CIDs through the existing preview/public gateway behavior.

**Tech Stack:** Rust 1.94, `rust-s3` 0.37.2, `quick-xml` 0.38.4 with Serde support, Serde derive, Tokio, wiremock 0.6, Cargo workspace, GNU Make invoked from PowerShell.

**Global Constraints:**
- Use Rust 1.94 as pinned by `rust-toolchain.toml`; do not install software or change the toolchain.
- Do not read local `config.toml`; only `config.toml.example` is the public configuration reference.
- Do not change `ImageUploader`, `ZipArchiveUploadInput`, `zip_extract_enabled`, `eh_client/src/lib.rs`, or `src/scheduler/eh_engine.rs`; the existing capability/default/fallback behavior remains intact.
- Do not implement multipart ZIP upload or alter non-EH image uploads, Telegraph splitting, rewrite timing, retries, or archive fallback behavior.
- The ZIP request must use `Command::PutObject` plus `ReqwestRequest::response_data(false)` with `decompress-zip=<archive-key-without-.zip>/` on a cloned `Bucket`; do not use the high-level `put_object_with_content_type` because rust-s3 replaces that method's response body with ETag, and do not send `decompress-zip-result=false`.
- Treat the target deployment's default XML `DecompressZipResult` as authoritative; never use the archive response ETag/CID to construct an extracted image URL.
- Reject non-2xx responses, malformed or empty XML, count inconsistencies, any extraction failure, duplicate requested image names, missing requested final keys, and empty requested entry ETags/CIDs.
- Match exact final keys, let the last response entry win for duplicate response keys, preserve requested image order, and ignore successful unrequested entries.
- Every shell command below is directly valid in PowerShell; do not use POSIX environment assignment or command chaining.
- Do not execute any git write command. Each task ends with a manual checkpoint; version-control actions require separate, explicit user authorization.

---

## File Structure

- Modify `eh_client/Cargo.toml`: declare `quick-xml` as a direct dependency with its `serialize` feature so protocol XML parsing is owned by `eh_client` rather than relying on a transitive dependency.
- Modify `Cargo.lock`: record `quick-xml` in the direct dependency list for the `eh_client` package; Cargo should retain the already locked `quick-xml 0.38.4` package.
- Modify and test `eh_client/src/telegraph.rs:796-1098,2260-2406`: add private response models/parser/validator, replace the archive-CID/path implementation with signed query upload and per-entry CID URLs, and replace obsolete ZIP tests with protocol and failure coverage.
- Modify `config.toml.example:126-130`: describe the signed `decompress-zip` XML contract and entry-CID URLs accurately.
- Intentionally leave `eh_client/src/lib.rs` and `src/scheduler/eh_engine.rs` unchanged: the public ZIP capability API and EH worker call path already match the approved design.

### Task 1: Parse and validate `DecompressZipResult`

**Files:**
- Modify: `eh_client/Cargo.toml:9-23`
- Modify: `Cargo.lock:1058-1076`
- Modify: `eh_client/src/telegraph.rs:1004-1098`
- Test: `eh_client/src/telegraph.rs:2260-2406`

**Interfaces:**
- Consumes: `serde::Deserialize`, XML response bytes from `s3::request::ResponseData::bytes()`, the derived extraction prefix, and ordered `ZipArchiveUploadInput::entry_names`.
- Produces: private `parse_ipfs3_zip_extract_result(body: &[u8]) -> Result<IpfS3ZipExtractResult>` and `ipfs3_zip_entry_cids(extraction_prefix: &str, entry_names: &[String], result: IpfS3ZipExtractResult) -> Result<Vec<String>>` for Task 2.

- [ ] **Step 1: Add focused failing parser and mapping tests**

Inside `eh_client/src/telegraph.rs`'s existing `mod tests`, insert the following helper and seven tests immediately after `ipfs3_zip_extract_disabled_by_default`. Keep the existing default-capability and disabled-upload tests in place.

```rust
fn ipfs3_zip_extract_xml(
    extracted_count: usize,
    failed_count: usize,
    entries: &[(&str, &str, u64)],
    failures: &[(&str, &str, &str)],
) -> String {
    let entries_xml = entries
        .iter()
        .map(|(key, etag, size)| {
            format!(
                "<Entry><Key>{key}</Key><ETag>{etag}</ETag><Size>{size}</Size></Entry>"
            )
        })
        .collect::<String>();
    let failures_xml = failures
        .iter()
        .map(|(entry_name, code, message)| {
            format!(
                "<Failure><EntryName>{entry_name}</EntryName><Code>{code}</Code><Message>{message}</Message></Failure>"
            )
        })
        .collect::<String>();

    format!(
        "<DecompressZipResult><ArchiveKey>eh/archive.zip</ArchiveKey><ArchiveETag>\"bafyArchiveCidMustNotBeUsed\"</ArchiveETag><ArchiveSize>1024</ArchiveSize><ExtractedCount>{extracted_count}</ExtractedCount><FailedCount>{failed_count}</FailedCount><Entries>{entries_xml}</Entries><Failures>{failures_xml}</Failures></DecompressZipResult>"
    )
}

#[test]
fn ipfs3_zip_extract_result_maps_exact_keys_in_requested_order_and_last_key_wins() {
    let prefix = "eh/20260718120000-archive-deadbeef/";
    let requested = vec!["page001.jpg".to_string(), "dir/page002.png".to_string()];
    let xml = ipfs3_zip_extract_xml(
        5,
        0,
        &[
            ("eh/20260718120000-archive-deadbeef/notes.txt", "\"bafyExtra\"", 4),
            ("eh/20260718120000-archive-deadbeef/page001.jpg", "\"bafyOld\"", 10),
            ("eh/20260718120000-archive-deadbeef/dir/page002.png", "bafySecond", 20),
            ("eh/20260718120000-archive-deadbeef/page001.jpg.bak", "bafyNearMatch", 30),
            ("eh/20260718120000-archive-deadbeef/page001.jpg", "  \"bafyFirstFinal\"  ", 40),
        ],
        &[],
    );

    let result = parse_ipfs3_zip_extract_result(xml.as_bytes()).unwrap();
    let cids = ipfs3_zip_entry_cids(prefix, &requested, result).unwrap();

    assert_eq!(cids, vec!["bafyFirstFinal", "bafySecond"]);
}

#[test]
fn ipfs3_zip_extract_result_rejects_empty_and_malformed_xml() {
    for body in [b"".as_slice(), b"<DecompressZipResult>".as_slice()] {
        let err = parse_ipfs3_zip_extract_result(body).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid DecompressZipResult XML"),
            "unexpected error: {err}"
        );
    }
}

#[test]
fn ipfs3_zip_extract_result_rejects_declared_count_inconsistencies() {
    let entry = [("prefix/page.jpg", "bafyPage", 10)];
    let failure = [("bad.jpg", "ExtractFailed", "invalid ZIP entry")];
    let cases = [
        (
            ipfs3_zip_extract_xml(2, 0, &entry, &[]),
            "ExtractedCount 2 does not match Entries length 1",
        ),
        (
            ipfs3_zip_extract_xml(1, 0, &entry, &failure),
            "FailedCount 0 does not match Failures length 1",
        ),
    ];

    for (xml, expected) in cases {
        let result = parse_ipfs3_zip_extract_result(xml.as_bytes()).unwrap();
        let err = ipfs3_zip_entry_cids("prefix/", &["page.jpg".into()], result)
            .unwrap_err();
        assert!(err.to_string().contains(expected), "unexpected error: {err}");
    }
}

#[test]
fn ipfs3_zip_extract_result_rejects_reported_failures() {
    let xml = ipfs3_zip_extract_xml(
        1,
        1,
        &[("prefix/page.jpg", "bafyPage", 10)],
        &[("bad.jpg", "ExtractFailed", "invalid ZIP entry")],
    );

    let result = parse_ipfs3_zip_extract_result(xml.as_bytes()).unwrap();
    let err = ipfs3_zip_entry_cids("prefix/", &["page.jpg".into()], result).unwrap_err();

    assert!(
        err.to_string().contains("reported 1 failed entries"),
        "unexpected error: {err}"
    );
    assert!(err.to_string().contains("bad.jpg"));
}

#[test]
fn ipfs3_zip_extract_result_rejects_missing_requested_key() {
    let xml = ipfs3_zip_extract_xml(
        1,
        0,
        &[("other-prefix/page.jpg", "bafyWrongPrefix", 10)],
        &[],
    );

    let result = parse_ipfs3_zip_extract_result(xml.as_bytes()).unwrap();
    let err = ipfs3_zip_entry_cids("prefix/", &["page.jpg".into()], result).unwrap_err();

    assert!(
        err.to_string()
            .contains("missing extracted entry for key prefix/page.jpg"),
        "unexpected error: {err}"
    );
}

#[test]
fn ipfs3_zip_extract_result_rejects_duplicate_requested_names() {
    let xml = ipfs3_zip_extract_xml(
        1,
        0,
        &[("prefix/page.jpg", "bafyPage", 10)],
        &[],
    );
    let requested = vec!["page.jpg".to_string(), "page.jpg".to_string()];

    let result = parse_ipfs3_zip_extract_result(xml.as_bytes()).unwrap();
    let err = ipfs3_zip_entry_cids("prefix/", &requested, result).unwrap_err();

    assert!(
        err.to_string()
            .contains("duplicate requested ZIP entry name page.jpg"),
        "unexpected error: {err}"
    );
}

#[test]
fn ipfs3_zip_extract_result_rejects_empty_requested_entry_cid() {
    let xml = ipfs3_zip_extract_xml(
        2,
        0,
        &[
            ("prefix/notes.txt", "", 1),
            ("prefix/page.jpg", "  \"\"  ", 10),
        ],
        &[],
    );

    let result = parse_ipfs3_zip_extract_result(xml.as_bytes()).unwrap();
    let err = ipfs3_zip_entry_cids("prefix/", &["page.jpg".into()], result).unwrap_err();

    assert!(
        err.to_string()
            .contains("extracted entry prefix/page.jpg returned an empty ETag (CID)"),
        "unexpected error: {err}"
    );
}
```

- [ ] **Step 2: Run the focused tests and confirm RED**

Run:

```powershell
cargo test -p eh_client ipfs3_zip_extract_result -- --nocapture
```

Expected: compilation fails because `parse_ipfs3_zip_extract_result` and `ipfs3_zip_entry_cids` are not defined. This is the required RED state; do not weaken or remove the assertions.

- [ ] **Step 3: Add the direct XML dependency and minimal protocol implementation**

In `eh_client/Cargo.toml`, add the direct dependency between `futures-util` and `regex`:

```toml
futures-util = "0.3"
quick-xml = { version = "0.38.4", features = ["serialize"] }
regex = "1.11.1"
```

In `eh_client/src/telegraph.rs`, insert these private protocol types and helpers immediately before `extension_for_content_type`. Explicit XML renames make the wire contract independent of Rust field naming.

```rust
#[derive(Debug, Deserialize)]
#[serde(rename = "DecompressZipResult")]
struct IpfS3ZipExtractResult {
    #[serde(rename = "ArchiveKey")]
    _archive_key: String,
    #[serde(rename = "ArchiveETag")]
    _archive_e_tag: String,
    #[serde(rename = "ArchiveSize")]
    _archive_size: u64,
    #[serde(rename = "ExtractedCount")]
    extracted_count: usize,
    #[serde(rename = "FailedCount")]
    failed_count: usize,
    #[serde(rename = "Entries", default)]
    entries: IpfS3ZipExtractEntries,
    #[serde(rename = "Failures", default)]
    failures: IpfS3ZipExtractFailures,
}

#[derive(Debug, Default, Deserialize)]
struct IpfS3ZipExtractEntries {
    #[serde(rename = "Entry", default)]
    entries: Vec<IpfS3ZipExtractEntry>,
}

#[derive(Debug, Deserialize)]
struct IpfS3ZipExtractEntry {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "ETag")]
    e_tag: String,
    #[serde(rename = "Size")]
    _size: u64,
}

#[derive(Debug, Default, Deserialize)]
struct IpfS3ZipExtractFailures {
    #[serde(rename = "Failure", default)]
    failures: Vec<IpfS3ZipExtractFailure>,
}

#[derive(Debug, Deserialize)]
struct IpfS3ZipExtractFailure {
    #[serde(rename = "EntryName")]
    entry_name: String,
    #[serde(rename = "Code")]
    code: String,
    #[serde(rename = "Message")]
    message: String,
}

fn parse_ipfs3_zip_extract_result(body: &[u8]) -> Result<IpfS3ZipExtractResult> {
    if body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(Error::Other(
            "invalid DecompressZipResult XML: empty response body".into(),
        ));
    }

    quick_xml::de::from_reader(body).map_err(|error| {
        Error::Other(format!(
            "invalid DecompressZipResult XML: {error}"
        ))
    })
}

fn ipfs3_zip_entry_cids(
    extraction_prefix: &str,
    entry_names: &[String],
    result: IpfS3ZipExtractResult,
) -> Result<Vec<String>> {
    let actual_entry_count = result.entries.entries.len();
    if result.extracted_count != actual_entry_count {
        return Err(Error::Other(format!(
            "ipfS3 ZIP ExtractedCount {} does not match Entries length {}",
            result.extracted_count, actual_entry_count
        )));
    }

    let actual_failure_count = result.failures.failures.len();
    if result.failed_count != actual_failure_count {
        return Err(Error::Other(format!(
            "ipfS3 ZIP FailedCount {} does not match Failures length {}",
            result.failed_count, actual_failure_count
        )));
    }

    if result.failed_count != 0 || !result.failures.failures.is_empty() {
        let first_failure = result
            .failures
            .failures
            .first()
            .map(|failure| {
                format!(
                    "; first failure {}: {} ({})",
                    failure.entry_name, failure.message, failure.code
                )
            })
            .unwrap_or_default();
        return Err(Error::Other(format!(
            "ipfS3 ZIP extraction reported {} failed entries{}",
            result.failed_count, first_failure
        )));
    }

    let mut etags_by_key =
        std::collections::HashMap::with_capacity(result.entries.entries.len());
    for entry in result.entries.entries {
        etags_by_key.insert(entry.key, entry.e_tag);
    }

    let mut seen_requested_names = std::collections::HashSet::new();
    let mut cids = Vec::with_capacity(entry_names.len());
    for entry_name in entry_names {
        if !seen_requested_names.insert(entry_name.as_str()) {
            return Err(Error::Other(format!(
                "duplicate requested ZIP entry name {entry_name}"
            )));
        }

        let final_key = format!("{extraction_prefix}{entry_name}");
        let e_tag = etags_by_key.get(&final_key).ok_or_else(|| {
            Error::Other(format!(
                "ipfS3 ZIP response is missing extracted entry for key {final_key}"
            ))
        })?;
        let cid = e_tag.trim().trim_matches('"').trim();
        if cid.is_empty() {
            return Err(Error::Other(format!(
                "ipfS3 ZIP extracted entry {final_key} returned an empty ETag (CID)"
            )));
        }
        cids.push(cid.to_string());
    }

    Ok(cids)
}
```

The `HashMap::insert` overwrite is intentional: duplicate response keys resolve to the last response entry. Validation only rejects an empty CID when that final key was requested, so an unrelated successful non-image entry is ignored.

- [ ] **Step 4: Run the parser tests and confirm GREEN plus the lockfile change**

Run:

```powershell
cargo test -p eh_client ipfs3_zip_extract_result -- --nocapture
git diff -- eh_client/Cargo.toml Cargo.lock
```

Expected: the focused test binary reports `7 passed; 0 failed`. The diff shows `quick-xml` added to `eh_client/Cargo.toml` and to the `eh_client` dependency array in `Cargo.lock`; the locked package remains `quick-xml 0.38.4` with no unrelated dependency upgrade.

- [ ] **Step 5: Manual checkpoint requiring explicit user authorization**

Stop and report that Task 1 changes are limited to `eh_client/Cargo.toml`, `Cargo.lock`, and parser/mapping code plus tests in `eh_client/src/telegraph.rs`. Do not run `git add`, `git commit`, or another git write command; the user must explicitly authorize or perform any version-control checkpoint.

### Task 2: Send the signed extraction query and use entry CIDs

**Files:**
- Modify: `eh_client/src/telegraph.rs:917-972`
- Delete obsolete helpers from: `eh_client/src/telegraph.rs:1070-1098`
- Replace obsolete tests in: `eh_client/src/telegraph.rs:2266-2305,2370-2406`

**Interfaces:**
- Consumes: `IpfS3Uploader::archive_object_key`, cloneable `Box<Bucket>`, `Bucket::add_query(&mut self, key: &str, value: &str)`, public `s3::command::Command::PutObject`, public `s3::request::tokio_backend::ReqwestRequest::new`, the `s3::request::Request` trait's `response_data(false)`, `ResponseData::{status_code,bytes}`, and Task 1's parser/mapping helpers.
- Produces: unchanged `IpfS3Uploader::upload_zip_archive_with_url_pairs(&self, archive: ZipArchiveUploadInput<'_>) -> Result<Option<Vec<TelegraphImageUrlPair>>>`, now implementing the signed `decompress-zip` protocol and returning CID-only gateway URLs.

- [ ] **Step 1: Replace archive-root/path tests with failing request/response protocol tests**

Delete `ipfs3_zip_extract_entry_url_pairs_encode_each_path_segment` and `ipfs3_zip_archive_upload_rejects_empty_cid`. Insert the following responder and tests in their place. Preserve `ipfs3_zip_extract_disabled_by_default`, `default_zip_archive_upload_capability_returns_none`, and `ipfs3_zip_archive_upload_disabled_returns_none_without_put` unchanged.

```rust
#[derive(Debug)]
struct IpfS3ZipExtractResponder;

impl wiremock::Respond for IpfS3ZipExtractResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let archive_key = request
            .url
            .path()
            .strip_prefix("/bucket/")
            .expect("path-style request must contain the bucket prefix");
        let archive_stem = archive_key
            .strip_suffix(".zip")
            .expect("ZIP request key must end in .zip");
        let extraction_prefix = format!("{archive_stem}/");
        let notes_key = format!("{extraction_prefix}notes.txt");
        let second_key = format!("{extraction_prefix}dir/page002.png");
        let first_key = format!("{extraction_prefix}page001.jpg");
        let body = ipfs3_zip_extract_xml(
            3,
            0,
            &[
                (notes_key.as_str(), "bafyExtra", 4),
                (second_key.as_str(), "\"bafySecond\"", 20),
                (first_key.as_str(), "\"bafyFirst\"", 10),
            ],
            &[],
        );

        wiremock::ResponseTemplate::new(200)
            .insert_header("etag", "\"bafyHttpArchiveCidMustNotAppear\"")
            .set_body_string(body)
    }
}

#[tokio::test]
async fn ipfs3_zip_archive_upload_signs_query_and_uses_ordered_entry_cids() {
    use wiremock::matchers::{body_bytes, method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/bucket/eh/\d{14}-archive-[0-9a-f]{8}\.zip$",
        ))
        .and(body_bytes(b"zip bytes".to_vec()))
        .respond_with(IpfS3ZipExtractResponder)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/bucket/eh/\d{14}-0001-[0-9a-f]{8}\.png$"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("etag", "\"bafyNormalImage\""),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
    config.preview_gateway_url = Some("https://preview.example/ipfs/".to_string());
    config.zip_extract_enabled = true;
    let uploader = IpfS3Uploader::from_config(&config).unwrap();
    let requested = vec!["page001.jpg".to_string(), "dir/page002.png".to_string()];

    let pairs = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            filename: "gallery.zip",
            bytes: b"zip bytes",
            entry_names: &requested,
        })
        .await
        .unwrap()
        .unwrap();
    let normal_urls = uploader
        .upload_images(&[ImageUploadInput {
            filename: "normal.png",
            bytes: b"\x89PNG\r\n\x1a\n",
        }])
        .await
        .unwrap();

    assert_eq!(
        pairs,
        vec![
            TelegraphImageUrlPair {
                preview_url: "https://preview.example/ipfs/bafyFirst".to_string(),
                public_url: "https://public.example/ipfs/bafyFirst".to_string(),
            },
            TelegraphImageUrlPair {
                preview_url: "https://preview.example/ipfs/bafySecond".to_string(),
                public_url: "https://public.example/ipfs/bafySecond".to_string(),
            },
        ]
    );
    assert_eq!(
        normal_urls,
        vec!["https://preview.example/ipfs/bafyNormalImage"]
    );
    assert!(pairs.iter().all(|pair| {
        !pair.preview_url.contains("ArchiveCid")
            && !pair.public_url.contains("ArchiveCid")
            && !pair.preview_url.contains("page001.jpg")
            && !pair.public_url.contains("page002.png")
    }));

    let requests = server.received_requests().await.unwrap();
    let zip_request = requests
        .iter()
        .find(|request| request.url.path().ends_with(".zip"))
        .expect("ZIP PUT request");
    let archive_key = zip_request
        .url
        .path()
        .strip_prefix("/bucket/")
        .unwrap();
    let expected_prefix = format!("{}/", archive_key.strip_suffix(".zip").unwrap());
    let query = zip_request
        .url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        query.get("decompress-zip").map(|value| value.as_ref()),
        Some(expected_prefix.as_str())
    );
    assert!(!query.contains_key("decompress-zip-result"));
    assert_eq!(
        zip_request
            .headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/zip"
    );
    assert!(
        zip_request
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("AWS4-HMAC-SHA256 ")
    );

    let normal_request = requests
        .iter()
        .find(|request| request.url.path().ends_with(".png"))
        .expect("ordinary image PUT request");
    assert!(!normal_request
        .url
        .query_pairs()
        .any(|(key, _)| key == "decompress-zip"));
}

#[tokio::test]
async fn ipfs3_zip_archive_upload_rejects_malformed_xml() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<DecompressZipResult>"))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs");
    config.zip_extract_enabled = true;
    let uploader = IpfS3Uploader::from_config(&config).unwrap();
    let requested = vec!["page001.jpg".to_string()];

    let err = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            filename: "gallery.zip",
            bytes: b"zip bytes",
            entry_names: &requested,
        })
        .await
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("invalid DecompressZipResult XML"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn ipfs3_zip_archive_upload_rejects_non_success_status() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs");
    config.zip_extract_enabled = true;
    let uploader = IpfS3Uploader::from_config(&config).unwrap();
    let requested = vec!["page001.jpg".to_string()];

    let err = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            filename: "gallery.zip",
            bytes: b"zip bytes",
            entry_names: &requested,
        })
        .await
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("ipfS3 ZIP put_object returned 503"),
        "unexpected error: {err}"
    );
}
```

The first test proves all transport invariants together: the request has the derived query, `application/zip`, and SigV4 authorization; no result-suppression query is present; the XML response body survives transport even though the response also has an archive ETag header; a later ordinary image PUT has no leaked extraction query; XML result order is ignored; requested order is restored; and neither the HTTP/archive XML ETag nor entry paths appear in URLs.

- [ ] **Step 2: Run the upload tests and confirm RED**

Run:

```powershell
cargo test -p eh_client ipfs3_zip_archive_upload -- --nocapture
```

Expected: the new signed-query/entry-CID test fails because the current implementation sends no `decompress-zip` query and its high-level PutObject path replaces the XML response body with the archive ETag; the malformed-XML test also fails because the current code accepts the returned ETag as a CID. The existing disabled-capability and non-2xx assertions remain passing.

- [ ] **Step 3: Replace the ZIP transport implementation**

Add these imports beside the existing `s3` imports at the top of `eh_client/src/telegraph.rs`:

```rust
use s3::command::Command;
use s3::request::{Request, tokio_backend::ReqwestRequest};
```

Replace the complete inherent `IpfS3Uploader::upload_zip_archive_with_url_pairs` method with:

```rust
pub async fn upload_zip_archive_with_url_pairs(
    &self,
    archive: ZipArchiveUploadInput<'_>,
) -> Result<Option<Vec<TelegraphImageUrlPair>>> {
    if !self.config.zip_extract_enabled {
        return Ok(None);
    }
    if archive.entry_names.is_empty() {
        return Ok(Some(Vec::new()));
    }

    let key = self.archive_object_key(&archive);
    let archive_stem = key.strip_suffix(".zip").ok_or_else(|| {
        Error::Other(format!(
            "ipfS3 ZIP object key {key} does not end in .zip"
        ))
    })?;
    let extraction_prefix = format!("{archive_stem}/");
    let mut upload_bucket = self.bucket.clone();
    upload_bucket.add_query("decompress-zip", &extraction_prefix);

    let command = Command::PutObject {
        content: archive.bytes,
        content_type: "application/zip",
        custom_headers: None,
        multipart: None,
    };
    let request = ReqwestRequest::new(upload_bucket.as_ref(), &key, command)
        .await
        .map_err(|error| {
            Error::Other(format!(
                "failed to build ipfS3 ZIP put_object request for key {key}: {error}"
            ))
        })?;
    let response = request.response_data(false).await.map_err(|error| {
        Error::Other(format!(
            "ipfS3 ZIP put_object failed for key {key}: {error}"
        ))
    })?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        return Err(Error::Api {
            message: format!("ipfS3 ZIP put_object returned {status} for key {key}"),
            status,
        });
    }

    let extract_result = parse_ipfs3_zip_extract_result(response.bytes()).map_err(|error| {
        Error::Other(format!(
            "ipfS3 ZIP put_object for key {key} returned {error}"
        ))
    })?;
    let cids = ipfs3_zip_entry_cids(
        &extraction_prefix,
        archive.entry_names,
        extract_result,
    )?;
    let pairs = cids
        .into_iter()
        .map(|cid| {
            self.warm_public_gateway(&cid);
            self.url_pair_for_cid(&cid)
        })
        .collect();

    Ok(Some(pairs))
}
```

Delete both obsolete free functions completely:

```rust
fn gateway_url_for_zip_entry(gateway_url: &str, cid: &str, entry_name: &str) -> String
```

```rust
fn ipfs3_zip_entry_url_pairs(
    config: &ResolvedIpfS3UploaderConfig,
    cid: &str,
    entry_names: &[String],
) -> Vec<TelegraphImageUrlPair>
```

Do not add `decompress-zip-result`: omitting it requests the default XML result required by the parser. Cloning `self.bucket` before `add_query` is mandatory because bucket-level extra query parameters apply to every request made through that bucket. Calling `response_data(false)` is also mandatory: rust-s3's high-level PutObject methods call `response_data(true)`, whose Tokio backend intentionally substitutes the ETag header for the response body and would discard `DecompressZipResult`.

- [ ] **Step 4: Format and confirm upload GREEN**

Run:

```powershell
cargo fmt --all
cargo test -p eh_client ipfs3_zip_archive_upload -- --nocapture
```

Expected: the focused test binary reports `4 passed; 0 failed`: enabled signed-query upload, malformed XML, non-success status, and disabled capability. The request assertion confirms the query is percent-decoded back to the exact trailing-slash extraction prefix, and the ordinary image request confirms the original bucket was not mutated.

- [ ] **Step 5: Run the complete ZIP-extract behavior slice**

Run:

```powershell
cargo test -p eh_client ipfs3_zip_extract -- --nocapture
cargo test -p eh_client ipfs3_zip_archive_upload -- --nocapture
cargo test -p eh_client default_zip_archive_upload_capability_returns_none -- --nocapture
```

Expected: the first command reports `8 passed; 0 failed`, covering the default flag and seven pure result tests. The second reports `4 passed; 0 failed`, covering disabled upload plus three enabled transport/error tests. The third reports `1 passed; 0 failed`, confirming the unchanged trait default still returns `None`.

- [ ] **Step 6: Manual checkpoint requiring explicit user authorization**

Stop and report that Task 2 changes are confined to the private ipfS3 ZIP transport, private obsolete helper removal, and colocated tests in `eh_client/src/telegraph.rs`. Do not perform a git write; any version-control checkpoint must be explicitly authorized or performed by the user.

### Task 3: Correct public configuration documentation and run repository gates

**Files:**
- Modify: `config.toml.example:126-130`
- Verify: `eh_client/Cargo.toml`
- Verify: `Cargo.lock`
- Verify: `eh_client/src/telegraph.rs`
- Verify unchanged: `eh_client/src/lib.rs`
- Verify unchanged: `src/scheduler/eh_engine.rs`

**Interfaces:**
- Consumes: the final Task 1/2 protocol behavior and existing `image_upload.ipfs3.zip_extract_enabled = false` configuration key.
- Produces: accurate operator-facing configuration comments plus focused-test, full-CI, whitespace, and scope evidence for handoff.

- [ ] **Step 1: Replace the obsolete root-CID/path configuration comments**

Replace `config.toml.example:126-130` with exactly:

```toml
# # If your ipfS3 deployment supports the signed `decompress-zip` PutObject
# # extension, EH Telegraph uploads can upload one ZIP and use each extracted
# # entry's returned CID as {gateway}/{entry-CID}. The client derives an isolated
# # extraction prefix and requires the default DecompressZipResult XML response.
# # Leave disabled unless your provider implements this protocol.
# zip_extract_enabled = false
```

This text must not claim that the archive ETag is a root CID or that ZIP entry paths are appended to gateway URLs.

- [ ] **Step 2: Re-run formatting and the focused final tests**

Run:

```powershell
cargo fmt --all -- --check
cargo test -p eh_client ipfs3_zip_extract -- --nocapture
cargo test -p eh_client ipfs3_zip_archive_upload -- --nocapture
cargo test -p eh_client default_zip_archive_upload_capability_returns_none -- --nocapture
```

Expected: formatting exits with code 0 and no diff; the pure ZIP-result slice reports `8 passed; 0 failed`; the ZIP-upload transport slice reports `4 passed; 0 failed`; the trait-default test reports `1 passed; 0 failed`.

- [ ] **Step 3: Run the full repository CI gate**

Run from the repository root:

```powershell
make ci
```

Expected: `fmt-check`, Clippy with warnings denied, workspace check, all workspace tests, and the release build with `ffmpeg-codec` all exit with code 0; Make ends with `All CI checks passed!`. If the pre-documented local FFmpeg development libraries or H.264 encoder are unavailable, report the exact failing subcommand and environment error without installing software or weakening the gate.

- [ ] **Step 4: Check whitespace and verify scope**

Run:

```powershell
git diff --check
git status --short
git diff -- eh_client/Cargo.toml Cargo.lock eh_client/src/telegraph.rs config.toml.example
```

Expected: `git diff --check` prints nothing and exits 0. `git status --short` lists only the pre-existing corrected spec plus the intended implementation files and this plan; the implementation diff contains one direct dependency addition, the parser/mapping and transport correction, focused tests, and corrected config comments. There must be no implementation diff in `eh_client/src/lib.rs` or `src/scheduler/eh_engine.rs`, and local `config.toml` must not appear.

- [ ] **Step 5: Real-surface protocol QA from captured wiremock evidence**

Review the successful output of `ipfs3_zip_archive_upload_signs_query_and_uses_ordered_entry_cids` together with its request assertions and record these observable facts in the handoff:

```text
PUT path: /bucket/eh/<14-digit-timestamp>-archive-<8-hex-hash>.zip
Decoded query: decompress-zip=eh/<14-digit-timestamp>-archive-<8-hex-hash>/
Content-Type: application/zip
Authorization scheme: AWS4-HMAC-SHA256
decompress-zip-result query: absent
Returned Telegraph order: page001 CID, then dir/page002 CID
Subsequent ordinary image PUT decompress-zip query: absent
Archive CID and ZIP entry paths in gateway URLs: absent
```

Expected: all eight observations are enforced by executable assertions, not manual access to a live provider or secret credentials.

- [ ] **Step 6: Manual checkpoint requiring explicit user authorization**

Report the changed-file scope, focused-test results, `make ci` result, and `git diff --check` result. Do not execute a git write command. If the user later authorizes a repository checkpoint, present the exact intended files and wait for that authorization before any staging or commit action.

## Plan Self-Review

- [x] **Spec coverage:** Task 1 covers XML parsing with the exact `ArchiveKey`/`ArchiveETag`/`ArchiveSize` wire names, entry/failure fields, count consistency, exact-key matching, last-response-key wins, requested order, ignored extra entries, duplicate requested names, missing keys, and empty requested CIDs. Task 2 covers cloned-bucket query isolation, SigV4 PUT evidence, trailing-slash prefix derivation, `application/zip`, default result mode, status handling, response-byte parsing, entry-CID-only URLs, preview/public gateways, archive-CID regression, and unchanged capability behavior. Task 3 covers public docs, focused tests, `make ci`, diff validation, and real-surface request evidence.
- [x] **Placeholder scan:** Every task names exact files, interfaces, code, commands, RED/GREEN outcomes, and manual checkpoints; no deferred implementation markers or shorthand references remain.
- [x] **Type consistency:** `parse_ipfs3_zip_extract_result` returns `IpfS3ZipExtractResult`; `ipfs3_zip_entry_cids` consumes that exact type and returns ordered `Vec<String>`; Task 2 builds `Command::PutObject`, constructs `ReqwestRequest`, imports `Request` for `response_data(false)`, passes `ResponseData::bytes()` and `archive.entry_names` to the parser/mapping signatures, then converts each CID through `IpfS3Uploader::url_pair_for_cid` into the unchanged `Result<Option<Vec<TelegraphImageUrlPair>>>` API.
- [x] **Scope consistency:** Only `eh_client/Cargo.toml`, `Cargo.lock`, `eh_client/src/telegraph.rs`, and `config.toml.example` are implementation targets. The EH worker and public trait/export surfaces remain unchanged, no secret config is read, and no git write is part of execution.

**Execution order:** Task 1 must finish before Task 2 because transport wiring consumes the parser/mapping interfaces; Task 3 follows Task 2 and supplies documentation plus final acceptance evidence.

**Plan review receipt:** `waiting for receipt` — the planner role cannot dispatch the orchestrator-owned `plan-critic` loop.
