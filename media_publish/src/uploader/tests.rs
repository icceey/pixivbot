use super::ipfs3::{
    default_ipfs_preview_rewrite_delay_sec, ipfs3_zip_archive_is_compatible, ipfs3_zip_entry_cids,
    parse_ipfs3_zip_extract_result,
};
use super::s3::default_s3_request_timeout_sec;
use super::s3_multipart::{fingerprint_fields, sha256_hex, CapabilityState};
use super::*;
use crate::telegraph::{split_for_pages, TelegraphClient, MAX_PAGE_CONTENT_BYTES};
use std::sync::Arc;
const PART_SIZE: usize = 8 * 1024 * 1024;
use wiremock::Match;

fn complete_s3_config(endpoint: &str, public_base_url: &str) -> S3UploaderConfig {
    S3UploaderConfig {
        endpoint_url: Some(endpoint.to_string()),
        bucket: Some("bucket".to_string()),
        region: Some("auto".to_string()),
        access_key_id: Some("key".to_string()),
        secret_access_key: Some("secret".to_string()),
        public_base_url: Some(public_base_url.to_string()),
        key_prefix: "eh".to_string(),
        path_style: true,
        request_timeout_sec: default_s3_request_timeout_sec(),
        multipart_image_threshold_mb: 8,
        multipart_part_size_mb: 8,
    }
}

fn complete_ipfs3_config(endpoint: &str, gateway_url: &str) -> IpfS3UploaderConfig {
    IpfS3UploaderConfig {
        endpoint_url: Some(endpoint.to_string()),
        bucket: Some("bucket".to_string()),
        region: Some("auto".to_string()),
        access_key_id: Some("key".to_string()),
        secret_access_key: Some("secret".to_string()),
        gateway_url: Some(gateway_url.to_string()),
        preview_gateway_url: None,
        preview_rewrite_delay_sec: default_ipfs_preview_rewrite_delay_sec(),
        key_prefix: "eh".to_string(),
        path_style: true,
        request_timeout_sec: default_s3_request_timeout_sec(),
        multipart_image_threshold_mb: 8,
        multipart_part_size_mb: 8,
        warm_public_gateway_after_upload: false,
        zip_extract_enabled: true,
    }
}

#[tokio::test]
async fn strict_gallery_rejects_intermediate_page_failure_while_default_returns_partial() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let segment = "a".repeat(MAX_PAGE_CONTENT_BYTES / 2);
    let image_urls: Vec<_> = (1..=3)
        .map(|index| format!("https://images.example/{index}/{segment}"))
        .collect();
    assert_eq!(
        split_for_pages(&image_urls, MAX_PAGE_CONTENT_BYTES).len(),
        3
    );

    for strict in [false, true] {
        let server = MockServer::start().await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let responder_calls = Arc::clone(&calls);
        Mock::given(method("POST"))
            .and(path("/createPage"))
            .respond_with(move |_: &wiremock::Request| {
                if responder_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "ok": true,
                        "result": { "url": "https://telegra.ph/tail-page" }
                    }))
                } else {
                    ResponseTemplate::new(500)
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let client = TelegraphClient::new_with_urls(
            "token".to_string(),
            format!("{}/upload", server.uri()),
            server.uri(),
        );

        let result = if strict {
            client
                .create_gallery_page_strict("Gallery", &image_urls)
                .await
        } else {
            client.create_gallery_page("Gallery", &image_urls).await
        };

        if strict {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), "https://telegra.ph/tail-page");
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        server.verify().await;
    }
}

#[tokio::test]
async fn ipfs3_uploader_warms_public_gateway_after_upload() {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let cid = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/bucket/eh/\d{14}-0001-[0-9a-f]{8}\.png$"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", format!("\"{cid}\"")))
        .expect(1)
        .mount(&server)
        .await;
    let warmed = std::sync::Arc::new(tokio::sync::Notify::new());
    let observed = std::sync::Arc::clone(&warmed);
    Mock::given(method("HEAD"))
        .and(path(format!("/ipfs/{cid}")))
        .respond_with(move |_: &wiremock::Request| {
            observed.notify_one();
            ResponseTemplate::new(503)
        })
        .expect(1)
        .mount(&server)
        .await;

    let mut config = complete_ipfs3_config(&server.uri(), &format!("{}/ipfs", server.uri()));
    config.warm_public_gateway_after_upload = true;
    let uploader = IpfS3Uploader::from_config(&config).unwrap();
    let urls = uploader
        .upload_images(&[ImageUploadInput {
            filename: "image.png",
            bytes: b"\x89PNG\r\n\x1a\n",
            resume_context: None,
        }])
        .await
        .unwrap();

    assert_eq!(urls, vec![format!("{}/ipfs/{cid}", server.uri())]);
    tokio::time::timeout(std::time::Duration::from_secs(2), warmed.notified())
        .await
        .expect("expected IPFS gateway warmup HEAD request");
}

#[tokio::test]
async fn ipfs3_uploader_returns_error_when_etag_missing() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/bucket/eh/.*\.png$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let uploader = IpfS3Uploader::from_config(&complete_ipfs3_config(
        &server.uri(),
        "https://ipfs.io/ipfs",
    ))
    .unwrap();
    let err = uploader
        .upload_images(&[ImageUploadInput {
            filename: "image.png",
            bytes: b"\x89PNG\r\n\x1a\n",
            resume_context: None,
        }])
        .await
        .unwrap_err();

    assert!(err.to_string().contains("no ETag (CID)"));
}

#[tokio::test]
async fn catbox_uploader_rejects_non_url_response() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/user/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("error: file too large"))
        .expect(1)
        .mount(&server)
        .await;

    let uploader = CatboxUploader::from_config(&CatboxUploaderConfig {
        api_url: format!("{}/user/api.php", server.uri()),
        userhash: None,
    })
    .unwrap();
    let err = uploader
        .upload_images(&[ImageUploadInput {
            filename: "image.png",
            bytes: b"\x89PNG\r\n\x1a\n",
            resume_context: None,
        }])
        .await
        .unwrap_err();

    assert!(err.to_string().contains("non-url response"));
}

const ZIP_FIXTURE_DATA: &[u8] = b"hello";
const ZIP_FIXTURE_DEFLATED_DATA: &[u8] = &[0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00];

#[derive(Clone, Copy)]
struct ZipEntryFixture<'a> {
    local_name: &'a [u8],
    central_name: &'a [u8],
    local_extra: &'a [u8],
    central_extra: &'a [u8],
    data: &'a [u8],
    local_method: u16,
    central_method: u16,
    local_flags: u16,
    central_flags: u16,
    data_descriptor_has_signature: bool,
}

impl<'a> ZipEntryFixture<'a> {
    fn stored(name: &'a [u8]) -> Self {
        Self {
            local_name: name,
            central_name: name,
            local_extra: &[],
            central_extra: &[],
            data: ZIP_FIXTURE_DATA,
            local_method: 0,
            central_method: 0,
            local_flags: 0,
            central_flags: 0,
            data_descriptor_has_signature: true,
        }
    }

    fn deflated(name: &'a [u8]) -> Self {
        Self {
            local_name: name,
            central_name: name,
            local_extra: &[],
            central_extra: &[],
            data: ZIP_FIXTURE_DATA,
            local_method: 8,
            central_method: 8,
            local_flags: 0,
            central_flags: 0,
            data_descriptor_has_signature: true,
        }
    }
}

fn push_zip_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_zip_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn zip_fixture_crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn zip_fixture_encoded_data<'a>(entry: ZipEntryFixture<'a>) -> &'a [u8] {
    if entry.local_method == 8 && entry.data == ZIP_FIXTURE_DATA {
        ZIP_FIXTURE_DEFLATED_DATA
    } else {
        entry.data
    }
}

fn zip_fixture(entries: &[ZipEntryFixture<'_>]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut local_offsets = Vec::with_capacity(entries.len());

    for entry in entries {
        let encoded = zip_fixture_encoded_data(*entry);
        let crc = zip_fixture_crc32(entry.data);
        let uses_descriptor = entry.local_flags & (1 << 3) != 0;
        local_offsets.push(output.len() as u32);

        push_zip_u32(&mut output, 0x0403_4b50);
        push_zip_u16(&mut output, 20);
        push_zip_u16(&mut output, entry.local_flags);
        push_zip_u16(&mut output, entry.local_method);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, 0);
        push_zip_u32(&mut output, if uses_descriptor { 0 } else { crc });
        push_zip_u32(
            &mut output,
            if uses_descriptor {
                0
            } else {
                encoded.len() as u32
            },
        );
        push_zip_u32(
            &mut output,
            if uses_descriptor {
                0
            } else {
                entry.data.len() as u32
            },
        );
        push_zip_u16(&mut output, entry.local_name.len() as u16);
        push_zip_u16(&mut output, entry.local_extra.len() as u16);
        output.extend_from_slice(entry.local_name);
        output.extend_from_slice(entry.local_extra);
        output.extend_from_slice(encoded);

        if uses_descriptor {
            if entry.data_descriptor_has_signature {
                push_zip_u32(&mut output, 0x0807_4b50);
            }
            push_zip_u32(&mut output, crc);
            push_zip_u32(&mut output, encoded.len() as u32);
            push_zip_u32(&mut output, entry.data.len() as u32);
        }
    }

    let central_offset = output.len() as u32;
    for (entry, local_offset) in entries.iter().zip(local_offsets) {
        let encoded = zip_fixture_encoded_data(*entry);
        push_zip_u32(&mut output, 0x0201_4b50);
        push_zip_u16(&mut output, 20);
        push_zip_u16(&mut output, 20);
        push_zip_u16(&mut output, entry.central_flags);
        push_zip_u16(&mut output, entry.central_method);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, 0);
        push_zip_u32(&mut output, zip_fixture_crc32(entry.data));
        push_zip_u32(&mut output, encoded.len() as u32);
        push_zip_u32(&mut output, entry.data.len() as u32);
        push_zip_u16(&mut output, entry.central_name.len() as u16);
        push_zip_u16(&mut output, entry.central_extra.len() as u16);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, 0);
        push_zip_u32(&mut output, 0);
        push_zip_u32(&mut output, local_offset);
        output.extend_from_slice(entry.central_name);
        output.extend_from_slice(entry.central_extra);
    }

    let central_size = output.len() as u32 - central_offset;
    push_zip_u32(&mut output, 0x0605_4b50);
    push_zip_u16(&mut output, 0);
    push_zip_u16(&mut output, 0);
    push_zip_u16(&mut output, entries.len() as u16);
    push_zip_u16(&mut output, entries.len() as u16);
    push_zip_u32(&mut output, central_size);
    push_zip_u32(&mut output, central_offset);
    push_zip_u16(&mut output, 0);
    output
}

#[test]
fn ipfs3_zip_preflight_accepts_deflate_with_data_descriptor() {
    let signed_descriptor = zip_fixture(&[ZipEntryFixture {
        local_flags: 1 << 3,
        central_flags: 1 << 3,
        ..ZipEntryFixture::deflated(b"page.jpg")
    }]);
    let unsigned_descriptor = zip_fixture(&[ZipEntryFixture {
        local_flags: 1 << 3,
        central_flags: 1 << 3,
        data_descriptor_has_signature: false,
        ..ZipEntryFixture::deflated(b"page.jpg")
    }]);

    for bytes in [signed_descriptor, unsigned_descriptor] {
        assert!(ipfs3_zip_archive_is_compatible(
            &mut std::io::Cursor::new(&bytes),
            &["page.jpg".to_string()],
            "eh/archive/"
        ));
    }
}

#[test]
fn ipfs3_zip_preflight_rejects_incompatible_entries_and_prefixes() {
    let unsafe_names: &[&[u8]] = &[
        b"",
        b"/",
        b"/absolute.jpg",
        b"C:/drive.jpg",
        b"dir\\page.jpg",
        b"a/./page.jpg",
        b"a/../page.jpg",
        b"\xff.jpg",
    ];
    for name in unsafe_names {
        let bytes = zip_fixture(&[ZipEntryFixture::stored(name)]);
        assert!(!ipfs3_zip_archive_is_compatible(
            &mut std::io::Cursor::new(&bytes),
            &["page.jpg".to_string()],
            "eh/archive/"
        ));
    }

    let requested = vec!["page.jpg".to_string()];
    let encrypted = zip_fixture(&[ZipEntryFixture {
        central_flags: 1,
        ..ZipEntryFixture::stored(b"page.jpg")
    }]);
    let unsupported = zip_fixture(&[ZipEntryFixture {
        local_method: 12,
        central_method: 12,
        ..ZipEntryFixture::stored(b"page.jpg")
    }]);
    let mismatched = zip_fixture(&[ZipEntryFixture {
        local_method: 8,
        central_method: 0,
        ..ZipEntryFixture::stored(b"page.jpg")
    }]);
    let stored_descriptor = zip_fixture(&[ZipEntryFixture {
        local_flags: 1 << 3,
        central_flags: 1 << 3,
        ..ZipEntryFixture::stored(b"page.jpg")
    }]);
    let descriptor_flag_mismatch = zip_fixture(&[ZipEntryFixture {
        local_flags: 1 << 3,
        ..ZipEntryFixture::deflated(b"page.jpg")
    }]);
    let unsafe_unrequested = zip_fixture(&[
        ZipEntryFixture::stored(b"page.jpg"),
        ZipEntryFixture::stored(b"../notes.txt"),
    ]);

    for bytes in [
        encrypted,
        unsupported,
        mismatched,
        stored_descriptor,
        descriptor_flag_mismatch,
        unsafe_unrequested,
    ] {
        assert!(!ipfs3_zip_archive_is_compatible(
            &mut std::io::Cursor::new(&bytes),
            &requested,
            "eh/archive/"
        ));
    }

    let valid = zip_fixture(&[ZipEntryFixture::stored(b"page.jpg")]);
    for prefix in [
        "/absolute/",
        "C:/drive/",
        "dir\\prefix/",
        "a/./",
        "a/../",
        "/",
    ] {
        assert!(!ipfs3_zip_archive_is_compatible(
            &mut std::io::Cursor::new(&valid),
            &requested,
            prefix
        ));
    }
    assert!(!ipfs3_zip_archive_is_compatible(
        &mut std::io::Cursor::new(&valid),
        &["page.jpg".to_string(), "page.jpg".to_string()],
        "eh/archive/"
    ));
}

#[tokio::test]
async fn ipfs3_zip_archive_upload_preflight_fallback_sends_no_put() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let config = IpfS3UploaderConfig {
        endpoint_url: Some(server.uri()),
        bucket: Some("bucket".into()),
        region: Some("auto".into()),
        access_key_id: Some("key".into()),
        secret_access_key: Some("secret".into()),
        gateway_url: Some("https://public.example/ipfs/".into()),
        key_prefix: "eh".into(),
        ..Default::default()
    };
    let uploader = IpfS3Uploader::from_config(&config).unwrap();
    let zip_bytes = zip_fixture(&[ZipEntryFixture::stored(b"dir\\page.jpg")]);
    let entries = vec!["dir\\page.jpg".to_string()];

    let result = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            path: zip_fixture_file(&zip_bytes).path(),
            entry_names: &entries,
            resume_context: None,
        })
        .await
        .unwrap();

    assert!(result.is_none());
}

fn zip_fixture_file(bytes: &[u8]) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file
}

fn ipfs3_zip_extract_result_xml(
    entries: &[(&str, &str)],
    failures: &[(&str, &str, &str)],
    extracted_count: usize,
    failed_count: usize,
) -> Vec<u8> {
    let entries = entries
        .iter()
        .map(|(key, etag)| {
            format!("<Entry><Key>{key}</Key><ETag>{etag}</ETag><Size>123</Size></Entry>")
        })
        .collect::<String>();
    let failures = failures
        .iter()
        .map(|(entry_name, code, message)| {
            format!(
                "<Failure><EntryName>{entry_name}</EntryName><Code>{code}</Code><Message>{message}</Message></Failure>"
            )
        })
        .collect::<String>();

    format!(
        "<DecompressZipResult><ArchiveKey>archive.zip</ArchiveKey><ArchiveETag>archive-cid</ArchiveETag><ArchiveSize>456</ArchiveSize><ExtractedCount>{extracted_count}</ExtractedCount><FailedCount>{failed_count}</FailedCount><Entries>{entries}</Entries><Failures>{failures}</Failures></DecompressZipResult>"
    )
    .into_bytes()
}

struct IpfS3ZipExtractResponder;

impl wiremock::Respond for IpfS3ZipExtractResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let archive_key = request
            .url
            .path()
            .strip_prefix("/bucket/")
            .expect("ZIP extraction request must target the configured bucket");
        let archive_stem = archive_key
            .strip_suffix(".zip")
            .expect("ZIP extraction request must target a ZIP object");
        let entries = [
            (format!("{archive_stem}/notes.txt"), "bafyExtra"),
            (format!("{archive_stem}/dir/page002.png"), "bafySecond"),
            (format!("{archive_stem}/page001.jpg"), "bafyFirst"),
        ];
        let entries = entries
            .iter()
            .map(|(key, cid)| (key.as_str(), *cid))
            .collect::<Vec<_>>();

        wiremock::ResponseTemplate::new(200)
            .insert_header("etag", "\"bafyHttpArchiveCidMustNotAppear\"")
            .set_body_bytes(ipfs3_zip_extract_result_xml(&entries, &[], 3, 0))
    }
}

#[test]
fn ipfs3_zip_extract_result_maps_exact_keys_in_requested_order_and_last_key_wins() {
    let xml = ipfs3_zip_extract_result_xml(
        &[
            ("extract/page-001.jpg", "cid-first"),
            ("extract/page-002.jpg", "cid-two"),
            ("extract/page-001.jpg", "cid-last"),
            ("extract/unrequested.jpg", "cid-extra"),
        ],
        &[],
        4,
        0,
    );
    let xml = [
        b"<?xml version=\"1.0\"?>\n ".as_slice(),
        xml.as_slice(),
        b"\n\t",
    ]
    .concat();
    let result = parse_ipfs3_zip_extract_result(&xml).unwrap();

    let cids = ipfs3_zip_entry_cids(
        "extract/",
        &["page-002.jpg".to_string(), "page-001.jpg".to_string()],
        result,
    )
    .unwrap()
    .unwrap();

    assert_eq!(cids, ["cid-two", "cid-last"]);
}

#[tokio::test]
async fn ipfs3_zip_archive_replays_signed_put_and_uses_ordered_entry_cids() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use wiremock::matchers::{body_bytes, method, path_regex};
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

    let server = MockServer::start().await;
    let page = vec![0x5a; 128 * 1024];
    let zip_bytes = zip_fixture(&[
        ZipEntryFixture {
            data: &page,
            ..ZipEntryFixture::stored(b"page001.jpg")
        },
        ZipEntryFixture::deflated(b"dir/page002.png"),
        ZipEntryFixture::stored(b"notes.txt"),
    ]);
    let first_attempt = AtomicBool::new(true);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/bucket/eh/\d{14}-archive-[0-9a-f]{8}\.zip$"))
        .and(body_bytes(zip_bytes.clone()))
        .respond_with(move |request: &wiremock::Request| {
            if first_attempt.swap(false, Ordering::Relaxed) {
                ResponseTemplate::new(503)
            } else {
                IpfS3ZipExtractResponder.respond(request)
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/bucket/eh/\d{14}-0001-[0-9a-f]{8}\.png$"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"bafyNormalImage\""))
        .expect(1)
        .mount(&server)
        .await;

    let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
    config.preview_gateway_url = Some("https://preview.example/ipfs/".to_string());
    let uploader = IpfS3Uploader::from_config(&config).unwrap();
    uploader.multipart_zip_extract.mark_unsupported();
    let entries = vec!["page001.jpg".to_string(), "dir/page002.png".to_string()];

    let pairs = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            path: zip_fixture_file(&zip_bytes).path(),
            entry_names: &entries,
            resume_context: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        pairs,
        vec![
            TelegraphImageUrlPair {
                preview_url: "https://preview.example/ipfs/bafyFirst".to_string(),
                public_url: "https://public.example/ipfs/bafyFirst".to_string(),
                cid: Some("bafyFirst".to_string()),
            },
            TelegraphImageUrlPair {
                preview_url: "https://preview.example/ipfs/bafySecond".to_string(),
                public_url: "https://public.example/ipfs/bafySecond".to_string(),
                cid: Some("bafySecond".to_string()),
            },
        ]
    );
    assert_eq!(
        uploader.single_put_zip_extract.state(),
        CapabilityState::Supported
    );

    let normal_pairs = uploader
        .upload_images_with_url_pairs(&[ImageUploadInput {
            filename: "normal.png",
            bytes: b"\x89PNG\r\n\x1a\n",
            resume_context: None,
        }])
        .await
        .unwrap();
    assert_eq!(
        normal_pairs,
        vec![TelegraphImageUrlPair {
            preview_url: "https://preview.example/ipfs/bafyNormalImage".to_string(),
            public_url: "https://public.example/ipfs/bafyNormalImage".to_string(),
            cid: Some("bafyNormalImage".to_string()),
        }]
    );

    let requests = server.received_requests().await.unwrap();
    let [zip_request, zip_retry, normal_request] = requests.as_slice() else {
        panic!("expected two ZIP PUTs followed by one image PUT");
    };
    assert_eq!(zip_retry.url, zip_request.url);
    for header in [
        "content-type",
        "content-length",
        "x-amz-content-sha256",
        "x-amz-tagging",
    ] {
        assert_eq!(zip_retry.headers[header], zip_request.headers[header]);
    }
    let archive_stem = zip_request
        .url
        .path()
        .strip_prefix("/bucket/")
        .unwrap()
        .strip_suffix(".zip")
        .unwrap();
    assert_eq!(
        zip_request
            .url
            .query_pairs()
            .find(|(name, _)| name == "decompress-zip")
            .map(|(_, value)| value.into_owned()),
        Some(format!("{archive_stem}/"))
    );
    assert!(!zip_request
        .url
        .query_pairs()
        .any(|(name, _)| name == "decompress-zip-result"));
    assert_eq!(
        zip_request
            .headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/zip"
    );
    assert!(zip_request
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("AWS4-HMAC-SHA256 ")));

    assert_eq!(
        zip_request.headers["content-length"],
        zip_bytes.len().to_string()
    );
    assert_eq!(
        zip_request.headers["x-amz-content-sha256"],
        sha256_hex(&zip_bytes)
    );
    assert!(!zip_request.headers.contains_key("transfer-encoding"));
    assert!(!zip_request.headers.contains_key("content-md5"));
    // Compare streaming authentication with rust-s3's ordinary buffered
    // signer for the same payload, timestamp, headers and extraction query.
    use ::s3::request::Request;
    let authorization = zip_request.headers["authorization"].to_str().unwrap();
    let signed_names = authorization
        .split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap();
    assert!(signed_names.split(';').any(|name| name == "x-amz-tagging"));
    let mut signed_headers = reqwest::header::HeaderMap::new();
    for name in signed_names.split(';') {
        signed_headers.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            zip_request.headers[name].clone(),
        );
    }
    let mut reference_bucket = uploader.bucket.clone();
    reference_bucket.add_query("decompress-zip", &format!("{archive_stem}/"));
    let key = zip_request.url.path().strip_prefix("/bucket/").unwrap();
    let mut reference = ::s3::request::tokio_backend::ReqwestRequest::new(
        &reference_bucket,
        key,
        ::s3::command::Command::PutObject {
            content: &zip_bytes,
            content_type: "application/zip",
            custom_headers: None,
            multipart: None,
        },
    )
    .await
    .unwrap();
    let datetime = chrono::NaiveDateTime::parse_from_str(
        zip_request.headers["x-amz-date"].to_str().unwrap(),
        "%Y%m%dT%H%M%SZ",
    )
    .unwrap()
    .and_utc();
    reference.datetime = std::time::SystemTime::from(datetime).into();
    assert_eq!(
        authorization,
        reference.authorization(&signed_headers).await.unwrap()
    );

    assert!(normal_request.url.path().ends_with(".png"));
    assert!(normal_request.url.query().is_none());
    assert!(!normal_request
        .url
        .query_pairs()
        .any(|(name, _)| name == "decompress-zip"));
    for url in pairs
        .iter()
        .chain(normal_pairs.iter())
        .flat_map(|pair| [&pair.preview_url, &pair.public_url])
    {
        assert!(!url.contains("bafyHttpArchiveCidMustNotAppear"));
        assert!(!url.contains("page001.jpg"));
        assert!(!url.contains("dir/page002.png"));
    }
}

#[tokio::test]
async fn ipfs3_zip_archive_single_put_classifies_only_explicit_extension_unsupported_errors() {
    use wiremock::matchers::{body_bytes, method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let not_implemented =
        "<Error><Code>NotImplemented</Code><Message>not implemented</Message></Error>";
    let unsupported_operation =
        "<Error><Code>UnsupportedOperation</Code><Message>unsupported</Message></Error>";
    let method_not_allowed =
        "<Error><Code>MethodNotAllowed</Code><Message>method not allowed</Message></Error>";
    let access_denied = "<Error><Code>AccessDenied</Code><Message>access denied</Message></Error>";
    let malformed_not_implemented =
        "<Error><Code>NotImplemented</Code><Message>not implemented</Message>";
    for (name, status, body, falls_back) in [
        ("http_200_not_implemented", 200, not_implemented, true),
        ("canonical_501_not_implemented", 501, not_implemented, true),
        (
            "non_5xx_unsupported_operation",
            400,
            unsupported_operation,
            true,
        ),
        ("http_405_method_not_allowed", 405, method_not_allowed, true),
        ("non_405_method_not_allowed", 400, method_not_allowed, false),
        ("access_denied", 403, access_denied, false),
        ("http_500_not_implemented", 500, not_implemented, false),
        ("raw_501", 501, "not implemented", false),
        ("malformed_501", 501, malformed_not_implemented, false),
        ("malformed_success", 200, "<DecompressZipResult>", false),
    ] {
        let server = MockServer::start().await;
        let zip_bytes = zip_fixture(&[ZipEntryFixture::stored(b"page001.jpg")]);
        let expected_attempts = if status == 500 { 3 } else { 1 };
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/\d{14}-archive-[0-9a-f]{8}\.zip$"))
            .and(body_bytes(zip_bytes.clone()))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .expect(expected_attempts)
            .mount(&server)
            .await;

        let uploader = IpfS3Uploader::from_config(&complete_ipfs3_config(
            &server.uri(),
            "https://public.example/ipfs/",
        ))
        .unwrap();
        uploader.standard_multipart.mark_unsupported();
        let entries = vec!["page001.jpg".to_string()];
        let result = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                path: zip_fixture_file(&zip_bytes).path(),
                entry_names: &entries,
                resume_context: None,
            })
            .await;

        if falls_back {
            assert!(
                result.unwrap().is_none(),
                "{name} should use the per-image fallback"
            );
            assert_eq!(
                uploader.single_put_zip_extract.state(),
                CapabilityState::Unsupported,
                "{name} should cache ZIP extraction as unsupported"
            );
        } else {
            assert!(result.is_err(), "{name} must remain an error");
            assert_eq!(
                uploader.single_put_zip_extract.state(),
                CapabilityState::Unknown,
                "{name} must not cache ZIP extraction as unsupported"
            );
        }
    }
}

#[derive(Clone, Copy)]
enum ImageS3Request {
    Create,
    ListParts,
    UploadPart,
    Complete,
    Abort,
    Head,
    OrdinaryPut,
}

impl wiremock::Match for ImageS3Request {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let query_has = |name: &str| request.url.query_pairs().any(|(actual, _)| actual == name);
        match self {
            Self::Create => request.method.as_str() == "POST" && query_has("uploads"),
            Self::ListParts => {
                request.method.as_str() == "GET"
                    && query_has("uploadId")
                    && query_has("part-number-marker")
            }
            Self::UploadPart => {
                request.method.as_str() == "PUT" && query_has("uploadId") && query_has("partNumber")
            }
            Self::Complete => {
                request.method.as_str() == "POST" && query_has("uploadId") && !query_has("uploads")
            }
            Self::Abort => request.method.as_str() == "DELETE" && query_has("uploadId"),
            Self::Head => request.method.as_str() == "HEAD",
            Self::OrdinaryPut => {
                request.method.as_str() == "PUT"
                    && !query_has("uploadId")
                    && !query_has("partNumber")
            }
        }
    }
}

fn image_s3_request_key(request: &wiremock::Request) -> String {
    request
        .url
        .path()
        .strip_prefix("/bucket/")
        .unwrap_or_else(|| panic!("unexpected bucket path: {}", request.url.path()))
        .to_string()
}

const IMAGE_UPLOAD_ID: &str = "image-upload-id";

#[derive(Clone)]
struct ImageCreateResponder;

impl wiremock::Respond for ImageCreateResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let key = image_s3_request_key(request);
        wiremock::ResponseTemplate::new(200).set_body_string(format!(
            "<InitiateMultipartUploadResult><Bucket>bucket</Bucket><Key>{key}</Key><UploadId>{IMAGE_UPLOAD_ID}</UploadId></InitiateMultipartUploadResult>"
        ))
    }
}

#[derive(Clone)]
struct ImageEmptyListPartsResponder;

impl wiremock::Respond for ImageEmptyListPartsResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let key = image_s3_request_key(request);
        wiremock::ResponseTemplate::new(200).set_body_string(format!(
            "<ListPartsResult><Bucket>bucket</Bucket><Key>{key}</Key><UploadId>{IMAGE_UPLOAD_ID}</UploadId><PartNumberMarker>0</PartNumberMarker><NextPartNumberMarker>0</NextPartNumberMarker><MaxParts>1000</MaxParts><IsTruncated>false</IsTruncated></ListPartsResult>"
        ))
    }
}

#[derive(Clone)]
enum ImageCompleteBody {
    Standard { etag: String },
    Error { status: u16, code: String },
    Malformed,
}

impl wiremock::Respond for ImageCompleteBody {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let key = image_s3_request_key(request);
        let (status, body) = match self {
            Self::Standard { etag } => (
                200,
                format!(
                    "<CompleteMultipartUploadResult><Location>ignored</Location><Bucket>bucket</Bucket><Key>{key}</Key><ETag>{etag}</ETag></CompleteMultipartUploadResult>"
                ),
            ),
            Self::Error { status, code } => (
                *status,
                format!("<Error><Code>{code}</Code><Message>safe message</Message></Error>"),
            ),
            Self::Malformed => (200, "<CompleteMultipartUploadResult>".to_string()),
        };
        wiremock::ResponseTemplate::new(status).set_body_string(body)
    }
}

#[derive(Clone)]
struct ImageFailThenSucceedPart {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl wiremock::Respond for ImageFailThenSucceedPart {
    fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3 {
            wiremock::ResponseTemplate::new(503)
        } else {
            wiremock::ResponseTemplate::new(200).insert_header("ETag", "\"part-one\"")
        }
    }
}

#[derive(Clone)]
struct ImageEmptyThenMissingListParts {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl wiremock::Respond for ImageEmptyThenMissingListParts {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            ImageEmptyListPartsResponder.respond(request)
        } else {
            wiremock::ResponseTemplate::new(404).set_body_string(
                "<Error><Code>NoSuchUpload</Code><Message>safe message</Message></Error>",
            )
        }
    }
}

async fn mount_image_multipart_success(server: &wiremock::MockServer, complete_etag: &str) {
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(ImageS3Request::Create)
        .respond_with(ImageCreateResponder)
        .expect(1)
        .mount(server)
        .await;
    Mock::given(ImageS3Request::ListParts)
        .respond_with(ImageEmptyListPartsResponder)
        .expect(1)
        .mount(server)
        .await;
    Mock::given(ImageS3Request::UploadPart)
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"part-one\""))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(ImageS3Request::Complete)
        .respond_with(ImageCompleteBody::Standard {
            etag: complete_etag.to_string(),
        })
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_image_ordinary_put(
    server: &wiremock::MockServer,
    etag: Option<&str>,
    expected_calls: u64,
) {
    use wiremock::{Mock, ResponseTemplate};

    let mut response = ResponseTemplate::new(200);
    if let Some(etag) = etag {
        response = response.insert_header("ETag", etag);
    }
    Mock::given(ImageS3Request::OrdinaryPut)
        .respond_with(response)
        .expect(expected_calls)
        .mount(server)
        .await;
}

async fn mount_no_image_ordinary_put(server: &wiremock::MockServer) {
    mount_image_ordinary_put(server, Some("\"unexpected-cid\""), 0).await;
}

#[derive(Clone)]
struct ZipStandardThenImageCompleteResponder;

impl wiremock::Respond for ZipStandardThenImageCompleteResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let key = image_s3_request_key(request);
        let etag = if key.ends_with(".zip") {
            "archive-cid"
        } else {
            "image-cid"
        };
        wiremock::ResponseTemplate::new(200).set_body_string(format!(
            "<CompleteMultipartUploadResult><Bucket>bucket</Bucket><Key>{key}</Key><ETag>{etag}</ETag></CompleteMultipartUploadResult>"
        ))
    }
}

fn one_mib_image() -> Vec<u8> {
    vec![0x5a; 1024 * 1024]
}

async fn write_terminal_manifest(
    path: &std::path::Path,
    provider: &str,
    uploader_identity_sha256: &str,
    object_key: &str,
    upload_id: &str,
) {
    let manifest = serde_json::json!({
        "version": 1,
        "provider": provider,
        "uploader_identity_sha256": uploader_identity_sha256,
        "object_key": object_key,
        "logical_object_id": "image-0",
        "object_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "object_len": 123,
        "content_type": "image/jpeg",
        "part_size": PART_SIZE,
        "upload_id": upload_id,
        "zip_extraction_prefix": null,
        "requested_entries_sha256": null,
    });
    tokio::fs::write(path, serde_json::to_vec(&manifest).unwrap())
        .await
        .unwrap();
}

fn multipart_complete_parts(request: &wiremock::Request) -> Vec<(u32, String)> {
    #[derive(Deserialize)]
    #[serde(rename = "CompleteMultipartUpload")]
    struct CompleteMultipartUpload {
        #[serde(rename = "Part")]
        parts: Vec<CompletePart>,
    }
    #[derive(Deserialize)]
    struct CompletePart {
        #[serde(rename = "PartNumber")]
        part_number: u32,
        #[serde(rename = "ETag")]
        etag: String,
    }

    quick_xml::de::from_reader::<_, CompleteMultipartUpload>(std::io::Cursor::new(
        request.body.as_slice(),
    ))
    .unwrap()
    .parts
    .into_iter()
    .map(|part| (part.part_number, part.etag))
    .collect()
}

#[tokio::test]
async fn ipfs3_zip_multipart_uses_decompress_query_only_on_create_and_returns_ordered_entry_cids() {
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let first_page = vec![0x11; PART_SIZE];
    let second_page = vec![0x22; 64];
    let zip_bytes = zip_fixture(&[
        ZipEntryFixture {
            data: &first_page,
            ..ZipEntryFixture::stored(b"page001.jpg")
        },
        ZipEntryFixture {
            data: &second_page,
            ..ZipEntryFixture::stored(b"dir/page002.png")
        },
    ]);
    assert!(zip_bytes.len() > PART_SIZE);
    Mock::given(ImageS3Request::Create)
        .respond_with(ImageCreateResponder)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::ListParts)
        .respond_with(ImageEmptyListPartsResponder)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::UploadPart)
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"part-one\""))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::Complete)
        .respond_with(IpfS3ZipExtractResponder)
        .expect(1)
        .mount(&server)
        .await;
    mount_no_image_ordinary_put(&server).await;

    let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs");
    config.multipart_part_size_mb = 6;
    let uploader = IpfS3Uploader::from_config(&config).unwrap();
    let entries = vec!["page001.jpg".to_string(), "dir/page002.png".to_string()];

    let pairs = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            path: zip_fixture_file(&zip_bytes).path(),
            entry_names: &entries,
            resume_context: None,
        })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        pairs,
        vec![
            TelegraphImageUrlPair {
                preview_url: "https://public.example/ipfs/bafyFirst".to_string(),
                public_url: "https://public.example/ipfs/bafyFirst".to_string(),
                cid: Some("bafyFirst".to_string()),
            },
            TelegraphImageUrlPair {
                preview_url: "https://public.example/ipfs/bafySecond".to_string(),
                public_url: "https://public.example/ipfs/bafySecond".to_string(),
                cid: Some("bafySecond".to_string()),
            },
        ]
    );

    let requests = server.received_requests().await.unwrap();
    let uploaded = requests
        .iter()
        .filter(|request| ImageS3Request::UploadPart.matches(request))
        .map(|request| request.body.as_slice())
        .collect::<Vec<_>>();
    assert_eq!(
        uploaded,
        vec![&zip_bytes[..6 * 1024 * 1024], &zip_bytes[6 * 1024 * 1024..]]
    );
    let create = requests
        .iter()
        .find(|request| ImageS3Request::Create.matches(request))
        .unwrap();
    let create_queries = create.url.query_pairs().collect::<Vec<_>>();
    assert_eq!(
        create_queries
            .iter()
            .filter(|(name, _)| name == "uploads")
            .count(),
        1
    );
    let archive_stem = image_s3_request_key(create)
        .strip_suffix(".zip")
        .unwrap()
        .to_owned();
    assert_eq!(
        create_queries
            .iter()
            .filter(|(name, _)| name == "decompress-zip")
            .map(|(_, value)| value.as_ref())
            .collect::<Vec<_>>(),
        [format!("{archive_stem}/")]
    );
    assert!(!create_queries.iter().any(|(name, _)| {
        name == "decompress-zip-result" || name.to_ascii_lowercase().contains("sse")
    }));
    assert!(!create.headers.contains_key("x-amz-server-side-encryption"));
    for request in requests
        .iter()
        .filter(|request| !ImageS3Request::Create.matches(request))
    {
        assert!(request
            .url
            .query_pairs()
            .all(|(name, _)| { !name.to_ascii_lowercase().starts_with("decompress-") }));
    }
    let complete = requests
        .iter()
        .find(|request| ImageS3Request::Complete.matches(request))
        .unwrap();
    assert_eq!(
        multipart_complete_parts(complete),
        vec![
            (1, "\"part-one\"".to_string()),
            (2, "\"part-one\"".to_string()),
        ]
    );
    server.verify().await;
}

#[tokio::test]
async fn ipfs3_standard_complete_disables_only_zip_extension_and_keeps_image_multipart() {
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let first_page = vec![0x31; PART_SIZE];
    let second_page = vec![0x32; 64];
    let zip_bytes = zip_fixture(&[
        ZipEntryFixture {
            data: &first_page,
            ..ZipEntryFixture::stored(b"page001.jpg")
        },
        ZipEntryFixture {
            data: &second_page,
            ..ZipEntryFixture::stored(b"dir/page002.png")
        },
    ]);
    Mock::given(ImageS3Request::Create)
        .respond_with(ImageCreateResponder)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::ListParts)
        .respond_with(ImageEmptyListPartsResponder)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::UploadPart)
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"part-one\""))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::Complete)
        .respond_with(ZipStandardThenImageCompleteResponder)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::OrdinaryPut)
        .respond_with(IpfS3ZipExtractResponder)
        .expect(1)
        .mount(&server)
        .await;

    let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs");
    config.multipart_image_threshold_mb = 1;
    let uploader = IpfS3Uploader::from_config(&config).unwrap();
    let entries = vec!["page001.jpg".to_string(), "dir/page002.png".to_string()];
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("archive.json");

    let pairs = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            path: zip_fixture_file(&zip_bytes).path(),
            entry_names: &entries,
            resume_context: Some(UploadResumeContext {
                manifest_path: &manifest_path,
                logical_object_id: "gallery-zip",
            }),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pairs.len(), 2);
    assert_eq!(
        uploader.standard_multipart.state(),
        CapabilityState::Supported
    );
    assert_eq!(
        uploader.multipart_zip_extract.state(),
        CapabilityState::Unsupported
    );
    assert!(!manifest_path.exists());

    let image = one_mib_image();
    let image_pairs = uploader
        .upload_images_with_url_pairs(&[ImageUploadInput {
            filename: "image.bin",
            bytes: &image,
            resume_context: None,
        }])
        .await
        .unwrap();
    assert_eq!(
        image_pairs,
        [TelegraphImageUrlPair {
            preview_url: "https://public.example/ipfs/image-cid".to_string(),
            public_url: "https://public.example/ipfs/image-cid".to_string(),
            cid: Some("image-cid".to_string()),
        }]
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| ImageS3Request::Create.matches(request))
            .count(),
        2
    );
    server.verify().await;
}

#[tokio::test]
async fn ipfs3_zip_multipart_resume_preserves_object_key_prefix_and_fingerprint() {
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    let zip_bytes = zip_fixture(&[
        ZipEntryFixture::stored(b"page001.jpg"),
        ZipEntryFixture::stored(b"dir/page002.png"),
    ]);
    Mock::given(ImageS3Request::Create)
        .respond_with(ImageCreateResponder)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::ListParts)
        .respond_with(ImageEmptyListPartsResponder)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::UploadPart)
        .respond_with(ImageFailThenSucceedPart {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
        .expect(4)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::Complete)
        .respond_with(IpfS3ZipExtractResponder)
        .expect(1)
        .mount(&server)
        .await;
    mount_no_image_ordinary_put(&server).await;

    let config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs");
    let entries = vec!["page001.jpg".to_string(), "dir/page002.png".to_string()];
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("archive.json");
    let first = IpfS3Uploader::from_config(&config).unwrap();
    assert!(first
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            path: zip_fixture_file(&zip_bytes).path(),
            entry_names: &entries,
            resume_context: Some(UploadResumeContext {
                manifest_path: &manifest_path,
                logical_object_id: "gallery-zip",
            }),
        })
        .await
        .is_err());

    let manifest: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&manifest_path).await.unwrap()).unwrap();
    let object_key = manifest["object_key"].as_str().unwrap().to_string();
    assert_eq!(
        manifest["zip_extraction_prefix"].as_str(),
        object_key
            .strip_suffix(".zip")
            .map(|stem| format!("{stem}/"))
            .as_deref()
    );
    assert_eq!(
        manifest["requested_entries_sha256"].as_str(),
        Some(fingerprint_fields(&entries).as_str())
    );

    let pairs = IpfS3Uploader::from_config(&config)
        .unwrap()
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            path: zip_fixture_file(&zip_bytes).path(),
            entry_names: &entries,
            resume_context: Some(UploadResumeContext {
                manifest_path: &manifest_path,
                logical_object_id: "gallery-zip",
            }),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pairs.len(), 2);
    assert!(!manifest_path.exists());
    let requests = server.received_requests().await.unwrap();
    for request in requests.iter().filter(|request| {
        ImageS3Request::ListParts.matches(request)
            || ImageS3Request::UploadPart.matches(request)
            || ImageS3Request::Complete.matches(request)
    }) {
        assert_eq!(image_s3_request_key(request), object_key);
    }
    server.verify().await;
}

#[tokio::test]
async fn ipfs3_zip_multipart_changed_entry_order_aborts_once_and_replaces_session() {
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let zip_bytes = zip_fixture(&[
        ZipEntryFixture::stored(b"page001.jpg"),
        ZipEntryFixture::stored(b"dir/page002.png"),
    ]);
    Mock::given(ImageS3Request::Create)
        .respond_with(ImageCreateResponder)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::ListParts)
        .respond_with(ImageEmptyListPartsResponder)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::UploadPart)
        .respond_with(ImageFailThenSucceedPart {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
        .expect(4)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::Complete)
        .respond_with(IpfS3ZipExtractResponder)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::Abort)
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    mount_no_image_ordinary_put(&server).await;

    let config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs");
    let initial_entries = vec!["page001.jpg".to_string(), "dir/page002.png".to_string()];
    let changed_entries = vec!["dir/page002.png".to_string(), "page001.jpg".to_string()];
    assert_ne!(
        fingerprint_fields(&initial_entries),
        fingerprint_fields(&changed_entries)
    );
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("archive.json");
    let first = IpfS3Uploader::from_config(&config).unwrap();
    assert!(first
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            path: zip_fixture_file(&zip_bytes).path(),
            entry_names: &initial_entries,
            resume_context: Some(UploadResumeContext {
                manifest_path: &manifest_path,
                logical_object_id: "gallery-zip",
            }),
        })
        .await
        .is_err());

    let pairs = IpfS3Uploader::from_config(&config)
        .unwrap()
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            path: zip_fixture_file(&zip_bytes).path(),
            entry_names: &changed_entries,
            resume_context: Some(UploadResumeContext {
                manifest_path: &manifest_path,
                logical_object_id: "gallery-zip",
            }),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        pairs
            .iter()
            .map(|pair| pair.public_url.as_str())
            .collect::<Vec<_>>(),
        [
            "https://public.example/ipfs/bafySecond",
            "https://public.example/ipfs/bafyFirst",
        ]
    );
    assert!(!manifest_path.exists());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| ImageS3Request::Create.matches(request))
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| ImageS3Request::Abort.matches(request))
            .count(),
        1
    );
    for request in requests
        .iter()
        .filter(|request| !ImageS3Request::Create.matches(request))
    {
        assert!(request
            .url
            .query_pairs()
            .all(|(name, _)| { !name.to_ascii_lowercase().starts_with("decompress-") }));
    }
    server.verify().await;
}

#[tokio::test]
async fn s3_large_image_resumes_and_returns_existing_public_url_shape() {
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    let config = {
        let mut config = complete_s3_config(&server.uri(), "https://cdn.example/root/");
        config.multipart_image_threshold_mb = 1;
        config
    };
    Mock::given(ImageS3Request::Create)
        .respond_with(ImageCreateResponder)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::ListParts)
        .respond_with(ImageEmptyListPartsResponder)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::UploadPart)
        .respond_with(ImageFailThenSucceedPart {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
        .expect(4)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::Complete)
        .respond_with(ImageCompleteBody::Standard {
            etag: "\"s3-multipart-etag\"".to_string(),
        })
        .expect(1)
        .mount(&server)
        .await;
    mount_no_image_ordinary_put(&server).await;

    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("image-0.json");
    let bytes = one_mib_image();
    let first = S3Uploader::from_config(&config).unwrap();
    let error = first
        .upload_images(&[ImageUploadInput {
            filename: "image.bin",
            bytes: &bytes,
            resume_context: Some(UploadResumeContext {
                manifest_path: &manifest_path,
                logical_object_id: "image-0",
            }),
        }])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("multipart"));
    assert!(manifest_path.is_file());

    let second = S3Uploader::from_config(&config).unwrap();
    let urls = second
        .upload_images(&[ImageUploadInput {
            filename: "image.bin",
            bytes: &bytes,
            resume_context: Some(UploadResumeContext {
                manifest_path: &manifest_path,
                logical_object_id: "image-0",
            }),
        }])
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let create_key = requests
        .iter()
        .find(|request| ImageS3Request::Create.matches(request))
        .map(image_s3_request_key)
        .unwrap();
    for request in requests.iter().filter(|request| {
        ImageS3Request::ListParts.matches(request)
            || ImageS3Request::UploadPart.matches(request)
            || ImageS3Request::Complete.matches(request)
    }) {
        assert_eq!(image_s3_request_key(request), create_key);
    }
    assert_eq!(urls, [format!("https://cdn.example/root/{create_key}")]);
    assert!(
        !manifest_path.exists(),
        "the manifest is removed only after the public URL is produced"
    );
    server.verify().await;
}

#[tokio::test]
async fn s3_unsupported_multipart_setup_or_completion_uses_one_put_object() {
    use wiremock::{Mock, MockServer, ResponseTemplate};

    for operation in ["create", "list", "complete"] {
        let server = MockServer::start().await;
        let mut config = complete_s3_config(&server.uri(), "https://cdn.example/root/");
        config.multipart_image_threshold_mb = 1;
        match operation {
            "create" => {
                Mock::given(ImageS3Request::Create)
                    .respond_with(ImageCompleteBody::Error {
                        status: 405,
                        code: "NotImplemented".to_string(),
                    })
                    .expect(1)
                    .mount(&server)
                    .await;
            }
            "list" => {
                Mock::given(ImageS3Request::Create)
                    .respond_with(ImageCreateResponder)
                    .expect(1)
                    .mount(&server)
                    .await;
                Mock::given(ImageS3Request::ListParts)
                    .respond_with(ImageCompleteBody::Error {
                        status: 405,
                        code: "NotImplemented".to_string(),
                    })
                    .expect(1)
                    .mount(&server)
                    .await;
                Mock::given(ImageS3Request::Abort)
                    .respond_with(ResponseTemplate::new(204))
                    .expect(1)
                    .mount(&server)
                    .await;
            }
            "complete" => {
                Mock::given(ImageS3Request::Create)
                    .respond_with(ImageCreateResponder)
                    .expect(1)
                    .mount(&server)
                    .await;
                Mock::given(ImageS3Request::ListParts)
                    .respond_with(ImageEmptyListPartsResponder)
                    .expect(1)
                    .mount(&server)
                    .await;
                Mock::given(ImageS3Request::UploadPart)
                    .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"part-one\""))
                    .expect(1)
                    .mount(&server)
                    .await;
                Mock::given(ImageS3Request::Complete)
                    .respond_with(ImageCompleteBody::Error {
                        status: 200,
                        code: "NotImplemented".to_string(),
                    })
                    .expect(1)
                    .mount(&server)
                    .await;
                Mock::given(ImageS3Request::Abort)
                    .respond_with(ResponseTemplate::new(204))
                    .expect(1)
                    .mount(&server)
                    .await;
            }
            _ => unreachable!(),
        }
        mount_image_ordinary_put(&server, None, 1).await;
        let bytes = one_mib_image();

        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("image.json");
        let urls = S3Uploader::from_config(&config)
            .unwrap()
            .upload_images(&[ImageUploadInput {
                filename: "image.bin",
                bytes: &bytes,
                resume_context: Some(UploadResumeContext {
                    manifest_path: &manifest_path,
                    logical_object_id: "image",
                }),
            }])
            .await
            .unwrap();

        assert_eq!(urls.len(), 1, "{operation}");
        assert!(
            !manifest_path.exists(),
            "{operation} must settle its session before PUT fallback"
        );
        server.verify().await;
    }
}

#[tokio::test]
async fn s3_and_ipfs3_image_put_replays_transient_statuses_and_stops_on_forbidden() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let bytes = b"\x89PNG\r\n\x1a\n";
    let cdn = "https://cdn.example";
    let gateway = "https://ipfs.example/ipfs";
    for provider in [ImageUploadProvider::S3, ImageUploadProvider::IpfS3] {
        for statuses in [&[408u16, 429, 200][..], &[502, 403][..]] {
            let server = MockServer::start().await;
            let attempt = AtomicUsize::new(0);
            Mock::given(ImageS3Request::OrdinaryPut)
                .respond_with(move |_: &wiremock::Request| {
                    let status = statuses
                        .get(attempt.fetch_add(1, Ordering::Relaxed))
                        .copied()
                        .unwrap_or(400);
                    ResponseTemplate::new(status).insert_header("etag", "\"image-cid\"")
                })
                .expect(statuses.len() as u64)
                .mount(&server)
                .await;
            let result = ImageUploadConfig {
                provider,
                s3: Some(complete_s3_config(&server.uri(), cdn)),
                ipfs3: Some(complete_ipfs3_config(&server.uri(), gateway)),
                ..Default::default()
            }
            .build_uploader()
            .await
            .unwrap()
            .upload_images(&[ImageUploadInput {
                filename: "image.png",
                bytes,
                resume_context: None,
            }])
            .await;

            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), statuses.len(), "{provider:?} {statuses:?}");
            for request in &requests {
                assert_eq!(request.url, requests[0].url);
                assert_eq!(request.body.as_slice(), bytes);
            }
            if statuses.last() == Some(&403) {
                assert!(matches!(result, Err(Error::Api { status: 403, .. })));
            } else {
                let expected = if provider == ImageUploadProvider::S3 {
                    format!(
                        "{cdn}{}",
                        requests[0].url.path().strip_prefix("/bucket").unwrap()
                    )
                } else {
                    format!("{gateway}/image-cid")
                };
                assert_eq!(result.unwrap(), [expected]);
            }
        }
    }
}

#[tokio::test]
async fn s3_terminal_abort_attempts_matching_manifests_in_order_and_continues_after_failure() {
    use wiremock::matchers::{method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "archive-upload-id"))
        .respond_with(ResponseTemplate::new(500).set_body_string(
            "<Error><Code>InternalError</Code><Message>terminal-abort-response-body</Message></Error>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "image-upload-id"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let uploader = S3Uploader::from_config(&complete_s3_config(
        &server.uri(),
        "https://cdn.example/root/",
    ))
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let uploads_dir = temp.path().join("gallery.zip.uploads");
    tokio::fs::create_dir(&uploads_dir).await.unwrap();
    write_terminal_manifest(
        &uploads_dir.join("image-0.json"),
        "s3",
        &uploader.uploader_identity_sha256,
        "images/image.jpg",
        "image-upload-id",
    )
    .await;
    write_terminal_manifest(
        &uploads_dir.join("archive.json"),
        "s3",
        &uploader.uploader_identity_sha256,
        "archives/gallery.zip",
        "archive-upload-id",
    )
    .await;

    tokio::fs::write(uploads_dir.join("00-malformed.json"), b"not json")
        .await
        .unwrap();
    let error = uploader.abort_upload_state(&uploads_dir).await.unwrap_err();
    assert!(error.to_string().contains("Abort"));
    for forbidden in [
        "terminal-abort-response-body",
        "archives/gallery.zip",
        "archive-upload-id",
        "key",
        "secret",
    ] {
        assert!(
            !error.to_string().contains(forbidden),
            "terminal Abort error leaked {forbidden}"
        );
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests
            .iter()
            .map(|request| {
                request
                    .url
                    .query_pairs()
                    .find(|(name, _)| name == "uploadId")
                    .map(|(_, value)| value.into_owned())
                    .unwrap()
            })
            .collect::<Vec<_>>(),
        ["archive-upload-id", "image-upload-id"]
    );
    assert!(requests.iter().all(|request| {
        request
            .url
            .query_pairs()
            .all(|(name, _)| !name.to_ascii_lowercase().starts_with("decompress-"))
    }));
    server.verify().await;
}

#[tokio::test]
async fn s3_auth_transient_and_malformed_multipart_errors_do_not_put_fallback() {
    use wiremock::{Mock, MockServer};

    for (name, response) in [
        (
            "access denied",
            ImageCompleteBody::Error {
                status: 403,
                code: "AccessDenied".to_string(),
            },
        ),
        (
            "transient",
            ImageCompleteBody::Error {
                status: 503,
                code: "ServiceUnavailable".to_string(),
            },
        ),
        ("malformed", ImageCompleteBody::Malformed),
    ] {
        let server = MockServer::start().await;
        let mut config = complete_s3_config(&server.uri(), "https://cdn.example/root/");
        config.multipart_image_threshold_mb = 1;
        Mock::given(ImageS3Request::Create)
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        mount_no_image_ordinary_put(&server).await;
        let bytes = one_mib_image();

        assert!(
            S3Uploader::from_config(&config)
                .unwrap()
                .upload_images(&[ImageUploadInput {
                    filename: "image.bin",
                    bytes: &bytes,
                    resume_context: None,
                }])
                .await
                .is_err(),
            "{name} must stay retryable"
        );
        server.verify().await;
    }

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let request_lines = Arc::new(std::sync::Mutex::new(Vec::new()));
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let close_server = {
        let request_lines = Arc::clone(&request_lines);
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            while running.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        use std::io::Read;

                        let mut bytes = [0; 1024];
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                            .unwrap();
                        if let Ok(len) = stream.read(&mut bytes) {
                            if let Some(line) =
                                String::from_utf8_lossy(&bytes[..len]).lines().next()
                            {
                                request_lines.lock().unwrap().push(line.to_string());
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(error) => panic!("close test server accept failed: {error}"),
                }
            }
        })
    };
    let mut config = complete_s3_config(&endpoint, "https://cdn.example/root/");
    config.multipart_image_threshold_mb = 1;
    let bytes = one_mib_image();
    assert!(S3Uploader::from_config(&config)
        .unwrap()
        .upload_images(&[ImageUploadInput {
            filename: "image.bin",
            bytes: &bytes,
            resume_context: None,
        }])
        .await
        .is_err());
    running.store(false, std::sync::atomic::Ordering::SeqCst);
    close_server.join().unwrap();
    let request_lines = request_lines.lock().unwrap();
    assert!(!request_lines.is_empty());
    assert!(request_lines.iter().all(|line| line.starts_with("POST ")));
}

#[tokio::test]
async fn ipfs3_image_multipart_complete_uses_cid_etag_and_existing_gateways() {
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let cid = "bafy-image-cid";
    let server = MockServer::start().await;
    let mut config = complete_ipfs3_config(&server.uri(), &format!("{}/ipfs", server.uri()));
    config.multipart_image_threshold_mb = 1;
    config.preview_gateway_url = Some("https://preview.example/ipfs".to_string());
    config.warm_public_gateway_after_upload = true;
    mount_image_multipart_success(&server, &format!("\"{cid}\"")).await;
    mount_no_image_ordinary_put(&server).await;
    Mock::given(ImageS3Request::Head)
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let bytes = one_mib_image();

    let pairs = IpfS3Uploader::from_config(&config)
        .unwrap()
        .upload_images_with_url_pairs(&[ImageUploadInput {
            filename: "image.bin",
            bytes: &bytes,
            resume_context: None,
        }])
        .await
        .unwrap();

    assert_eq!(
        pairs,
        [TelegraphImageUrlPair {
            preview_url: format!("https://preview.example/ipfs/{cid}"),
            public_url: format!("{}/ipfs/{cid}", server.uri()),
            cid: Some(cid.to_string()),
        }]
    );
    for _ in 0..20 {
        if server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| ImageS3Request::Head.matches(request))
        {
            server.verify().await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("expected non-blocking public gateway warmup");
}

#[tokio::test]
async fn ipfs3_image_multipart_uses_head_recovered_cid() {
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let cid = "bafy-recovered-cid";
    let server = MockServer::start().await;
    let mut config = complete_ipfs3_config(&server.uri(), "https://ipfs.example/ipfs");
    config.multipart_image_threshold_mb = 1;
    Mock::given(ImageS3Request::Create)
        .respond_with(ImageCreateResponder)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::ListParts)
        .respond_with(ImageEmptyThenMissingListParts {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::UploadPart)
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(ImageS3Request::Head)
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", 1024 * 1024)
                .insert_header("ETag", format!(" \"{cid}\" ")),
        )
        .expect(1)
        .mount(&server)
        .await;
    mount_no_image_ordinary_put(&server).await;

    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("image-0.json");
    let bytes = one_mib_image();
    let first = IpfS3Uploader::from_config(&config).unwrap();
    assert!(first
        .upload_images(&[ImageUploadInput {
            filename: "image.bin",
            bytes: &bytes,
            resume_context: Some(UploadResumeContext {
                manifest_path: &manifest_path,
                logical_object_id: "image-0",
            }),
        }])
        .await
        .is_err());
    assert!(manifest_path.exists());

    let second = IpfS3Uploader::from_config(&config).unwrap();
    let urls = second
        .upload_images(&[ImageUploadInput {
            filename: "image.bin",
            bytes: &bytes,
            resume_context: Some(UploadResumeContext {
                manifest_path: &manifest_path,
                logical_object_id: "image-0",
            }),
        }])
        .await
        .unwrap();

    assert_eq!(urls, [format!("https://ipfs.example/ipfs/{cid}")]);
    assert!(!manifest_path.exists());
    server.verify().await;
}
