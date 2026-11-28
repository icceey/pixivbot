use super::*;
use s3::creds::Credentials;
use s3::{Bucket, Region};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use wiremock::matchers::{method, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PART_SIZE: usize = 8 * 1024 * 1024;
const BUCKET: &str = "bucket";
const KEY: &str = "objects/archive.bin";
const UPLOAD_ID: &str = "upload-id";
const UPLOADER_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[tokio::test]
async fn multipart_retries_each_failed_part_and_completes_in_order() {
    let server = MockServer::start().await;
    mount_create(&server).await;
    mount_empty_list_parts(&server).await;
    for part_number in 1..=3 {
        let attempts = AtomicUsize::new(0);
        Mock::given(method("PUT"))
            .and(query_param("uploadId", UPLOAD_ID))
            .and(query_param("partNumber", part_number.to_string()))
            .respond_with(move |_: &wiremock::Request| {
                if part_number > 1 && attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                    return ResponseTemplate::new(503);
                }
                ResponseTemplate::new(200).insert_header("ETag", format!("\"part-{part_number}\""))
            })
            .expect(if part_number > 1 { 3 } else { 1 })
            .mount(&server)
            .await;
    }
    mount_complete(&server).await;

    let part_size = MIN_PART_SIZE as usize;
    let bytes = (0..2 * part_size + 17)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let mut request = standard_request(&bytes, None);
    request.part_size = part_size as u64;
    let result = upload_multipart(
        test_bucket(&server).as_ref(),
        &reqwest::Client::new(),
        request,
    )
    .await
    .unwrap();
    assert!(matches!(result, MultipartOutcome::Completed(_)));

    let requests = server.received_requests().await.unwrap();
    assert_request_sequence(
        &requests,
        &[
            "POST", "GET", "PUT", "PUT", "PUT", "PUT", "PUT", "PUT", "PUT", "POST",
        ],
    );
    assert_eq!(
        query_value(&requests[1], "uploadId").as_deref(),
        Some(UPLOAD_ID)
    );
    assert_eq!(
        requests
            .iter()
            .filter_map(|request| query_value(request, "partNumber"))
            .collect::<Vec<_>>(),
        ["1", "2", "2", "2", "3", "3", "3"],
    );
    assert_eq!(
        complete_parts(requests.last().unwrap()),
        vec![
            (1, "\"part-1\"".to_owned()),
            (2, "\"part-2\"".to_owned()),
            (3, "\"part-3\"".to_owned()),
        ]
    );
    for request in requests.iter().filter(|request| request.method == "PUT") {
        let number = query_value(request, "partNumber")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let start = (number - 1) * part_size;
        assert_eq!(
            request.body,
            bytes[start..bytes.len().min(start + part_size)]
        );
    }
    assert_no_decompress_query(&requests);
    server.verify().await;
}

#[tokio::test]
async fn multipart_restart_lists_server_parts_and_uploads_only_the_missing_tail() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("archive.json");
    let bytes = (0..2 * PART_SIZE + 17)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let source_path = temp.path().join("archive.bin");
    tokio::fs::write(&source_path, &bytes).await.unwrap();
    let mut request = standard_request(&bytes, Some(&manifest_path));
    request.reader = Box::new(tokio::fs::File::open(&source_path).await.unwrap());
    let server = RawMultipartServer::start(bytes.len(), 3, vec![1, 2]);
    let first = upload_multipart(
        test_bucket_for_endpoint(&server.endpoint).as_ref(),
        &reqwest::Client::new(),
        request,
    )
    .await;
    assert!(first.is_err());
    assert!(manifest_path.is_file());
    let first_request_count = server.requests().len();

    let mut resumed_request = standard_request(&bytes, Some(&manifest_path));
    // A restart can change configuration, but the existing session's
    // server parts still have the original 8 MiB boundaries.
    resumed_request.part_size = MIN_PART_SIZE;
    resumed_request.reader = Box::new(tokio::fs::File::open(&source_path).await.unwrap());
    resumed_request.candidate_object_key = "objects/different-retry-key.bin".to_owned();
    let resumed = upload_multipart(
        test_bucket_for_endpoint(&server.endpoint).as_ref(),
        &reqwest::Client::new(),
        resumed_request,
    )
    .await
    .unwrap();
    let completed = match resumed {
        MultipartOutcome::Completed(completed) => completed,
        MultipartOutcome::Unsupported { .. } => {
            panic!("resumed upload unexpectedly unsupported")
        }
    };

    let requests = server.requests();
    assert_raw_request_sequence(&requests[first_request_count..], &["GET", "PUT", "POST"]);
    assert_eq!(
        raw_query_value(
            requests[first_request_count + 1].target.as_str(),
            "partNumber"
        ),
        Some("3")
    );
    assert_eq!(
        complete_parts_body(requests[first_request_count + 2].body.as_slice()),
        vec![
            (1, "\"part-1\"".to_owned()),
            (2, "\"part-2\"".to_owned()),
            (3, "\"part-3\"".to_owned()),
        ]
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.method == "POST" && raw_query_value(&request.target, "uploads").is_some()
            })
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .find(|request| {
                request.method == "PUT"
                    && raw_query_value(&request.target, "partNumber") == Some("3")
            })
            .map(|request| request.body.len()),
        Some(17)
    );
    assert_eq!(
        requests[first_request_count + 1].body,
        bytes[2 * PART_SIZE..]
    );
    assert_eq!(completed.object_key, KEY);
    assert!(manifest_path.is_file());

    completed.remove_manifest().await.unwrap();
    assert!(!manifest_path.exists());
}

#[tokio::test]
async fn changed_file_part_cannot_poison_a_resumable_session() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("archive.json");
    let source_path = temp.path().join("archive.bin");
    let bytes = vec![7; PART_SIZE + 17];
    tokio::fs::write(&source_path, &bytes).await.unwrap();
    let server = MockServer::start().await;
    mount_create(&server).await;
    let uploaded_first = Arc::new(AtomicBool::new(false));
    let listed_first = uploaded_first.clone();
    Mock::given(method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            let parts = if listed_first.load(Ordering::SeqCst) {
                vec![(1, "\"part-1\"".to_owned(), PART_SIZE as u64)]
            } else {
                Vec::new()
            };
            ResponseTemplate::new(200)
                .set_body_string(list_parts_response_for(BUCKET, KEY, UPLOAD_ID, &parts))
        })
        .expect(2)
        .mount(&server)
        .await;
    let changed_path = source_path.clone();
    Mock::given(method("PUT"))
        .and(query_param("partNumber", "1"))
        .respond_with(move |_: &wiremock::Request| {
            use std::io::Seek;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&changed_path)
                .unwrap();
            file.seek(std::io::SeekFrom::Start(PART_SIZE as u64))
                .unwrap();
            file.write_all(&[8; 17]).unwrap();
            uploaded_first.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200).insert_header("ETag", "\"part-1\"")
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(query_param("partNumber", "2"))
        .and(wiremock::matchers::body_bytes(vec![7; 17]))
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"part-2\""))
        .expect(1)
        .mount(&server)
        .await;
    mount_complete(&server).await;

    let mut request = standard_request(&bytes, Some(&manifest_path));
    request.reader = Box::new(tokio::fs::File::open(&source_path).await.unwrap());
    let error = match upload_multipart(
        test_bucket(&server).as_ref(),
        &reqwest::Client::new(),
        request,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("changed source must not upload the modified part"),
    };
    assert!(error.to_string().contains("source changed"), "{error}");
    assert!(manifest_path.is_file());
    assert_request_sequence(
        &server.received_requests().await.unwrap(),
        &["POST", "GET", "PUT"],
    );

    tokio::fs::write(&source_path, &bytes).await.unwrap();
    let mut request = standard_request(&bytes, Some(&manifest_path));
    request.reader = Box::new(tokio::fs::File::open(&source_path).await.unwrap());
    let resumed = upload_multipart(
        test_bucket(&server).as_ref(),
        &reqwest::Client::new(),
        request,
    )
    .await
    .unwrap();
    assert!(matches!(resumed, MultipartOutcome::Completed(_)));
    assert_request_sequence(
        &server.received_requests().await.unwrap(),
        &["POST", "GET", "PUT", "GET", "PUT", "POST"],
    );
    server.verify().await;
}

#[tokio::test]
async fn multipart_transient_auth_and_malformed_failures_never_become_unsupported() {
    let server = MockServer::start().await;
    mount_create(&server).await;
    mount_empty_list_parts(&server).await;
    Mock::given(method("PUT"))
        .and(query_param("uploadId", UPLOAD_ID))
        .and(query_param("partNumber", "1"))
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", " \t "))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("uploadId", UPLOAD_ID))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("archive.json");
    let bytes = vec![4; PART_SIZE];
    let result = upload_multipart(
        test_bucket(&server).as_ref(),
        &reqwest::Client::new(),
        standard_request(&bytes, Some(&manifest_path)),
    )
    .await;
    assert!(result.is_err());
    assert!(manifest_path.exists());
    server.verify().await;

    assert_malformed_create_failure().await;
    for operation in [
        MultipartOperation::ListParts,
        MultipartOperation::UploadPart,
        MultipartOperation::Complete,
    ] {
        assert_active_service_failure_preserves_manifest(operation).await;
    }
}

#[tokio::test]
async fn multipart_complete_classifies_http_200_error_roots_before_provider_parsing() {
    let server = MockServer::start().await;
    mount_create(&server).await;
    mount_empty_list_parts(&server).await;
    Mock::given(method("PUT"))
        .and(query_param("uploadId", UPLOAD_ID))
        .and(query_param("partNumber", "1"))
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"part-1\""))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("uploadId", UPLOAD_ID))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(s3_error_xml("UnsupportedOperation")),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", UPLOAD_ID))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("archive.json");
    let bytes = vec![4; PART_SIZE];
    let result = upload_multipart(
        test_bucket(&server).as_ref(),
        &reqwest::Client::new(),
        standard_request(&bytes, Some(&manifest_path)),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        MultipartOutcome::Unsupported {
            operation: MultipartOperation::Complete
        }
    ));
    assert!(!manifest_path.exists());
    server.verify().await;
}

#[tokio::test]
async fn s3_head_matching_length_recovers_lost_complete() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("uploadId", UPLOAD_ID))
        .respond_with(ResponseTemplate::new(404).set_body_string(s3_error_xml("NoSuchUpload")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", PART_SIZE))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("archive.json");
    let bytes = vec![4; PART_SIZE];
    write_valid_manifest(&manifest_path, &bytes, UPLOADER_ID).await;
    let mut request = standard_request(&bytes, Some(&manifest_path));
    request.head_recovery = HeadRecovery::S3ByLength;

    let result = upload_multipart(
        test_bucket(&server).as_ref(),
        &reqwest::Client::new(),
        request,
    )
    .await
    .unwrap();
    let completed = match result {
        MultipartOutcome::Completed(completed) => completed,
        MultipartOutcome::Unsupported { .. } => panic!("lost Complete must recover"),
    };
    assert_eq!(completed.object_key, KEY);
    assert!(matches!(
        completed.evidence,
        CompletionEvidence::RecoveredHead { etag: None }
    ));
    assert!(manifest_path.exists());
    server.verify().await;
}

#[tokio::test]
async fn provider_or_uploader_identity_mismatch_preserves_manifest_without_network_requests() {
    let old_uploader_identity = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    for (case, provider) in [
        ("uploader-identity", ProviderKind::S3),
        ("provider", ProviderKind::IpfS3),
    ] {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("archive.json");
        let bytes = vec![4; PART_SIZE];
        let old_object_key = format!("old-{case}-object-key-sentinel");
        let old_upload_id = format!("old-{case}-upload-id-sentinel");
        let object_sha256 = sha256_hex(&bytes);
        let old_identity = manifest::ManifestIdentity {
            provider,
            uploader_identity_sha256: old_uploader_identity,
            logical_object_id: "gallery-123",
            object_sha256: &object_sha256,
            object_len: bytes.len() as u64,
            content_type: "application/octet-stream",
            requested_entries_sha256: None,
        };
        let old_manifest = manifest::new_manifest(
            &old_identity,
            old_object_key.clone(),
            old_upload_id.clone(),
            PART_SIZE as u64,
        )
        .unwrap();
        manifest::write_manifest_atomic(&manifest_path, &old_manifest)
            .await
            .unwrap();
        let persisted_manifest = tokio::fs::read_to_string(&manifest_path).await.unwrap();

        let error = match upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, Some(&manifest_path)),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("{case} mismatch must not resume or create a session"),
        };

        assert_eq!(
            error.to_string(),
            "multipart resume manifest belongs to a different uploader identity"
        );
        assert!(manifest_path.is_file());
        assert_eq!(
            tokio::fs::read_to_string(&manifest_path).await.unwrap(),
            persisted_manifest
        );
        let manifest_path_display = manifest_path.to_string_lossy();
        for forbidden in [
            manifest_path_display.as_ref(),
            old_object_key.as_str(),
            old_upload_id.as_str(),
            old_uploader_identity,
            UPLOADER_ID,
            "AKIA_TEST_SENTINEL",
            "secret-test-sentinel",
        ] {
            assert!(
                !error.to_string().contains(forbidden),
                "{case} resume error leaked {forbidden}"
            );
        }
        assert!(server.received_requests().await.unwrap().is_empty());
        server.verify().await;
    }
}

#[tokio::test]
async fn terminal_abort_recovers_atomic_state_after_redacted_http_failure() {
    let server = MockServer::start().await;
    let response_body = "<Error><Code>AccessDenied</Code><RequestId>private-response-body-sentinel</RequestId></Error>";
    let failing_request = Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(403).set_body_string(response_body))
        .expect(1)
        .mount_as_scoped(&server)
        .await;
    let temp = tempfile::tempdir().unwrap();
    let uploads_dir = temp.path().join("gallery.zip.uploads");
    tokio::fs::create_dir(&uploads_dir).await.unwrap();
    let manifest_path = uploads_dir.join("manifest.json.tmp-crash");
    write_valid_manifest_for(
        &manifest_path,
        &vec![7; PART_SIZE],
        UPLOADER_ID,
        ProviderKind::IpfS3,
    )
    .await;

    let error = abort_upload_state(
        test_bucket(&server).as_ref(),
        ProviderKind::IpfS3,
        UPLOADER_ID,
        &uploads_dir,
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "multipart Abort failed (HTTP 403)");
    let diagnostic = format!("{error:?}: {error}");
    for forbidden in [
        "private-response-body-sentinel",
        KEY,
        UPLOAD_ID,
        manifest_path.to_str().unwrap(),
    ] {
        assert!(!diagnostic.contains(forbidden));
    }
    assert!(manifest_path.exists());
    drop(failing_request);
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", UPLOAD_ID))
        .respond_with(
            ResponseTemplate::new(404).set_body_string("<Error><Code>NoSuchUpload</Code></Error>"),
        )
        .expect(1)
        .mount(&server)
        .await;

    abort_upload_state(
        test_bucket(&server).as_ref(),
        ProviderKind::IpfS3,
        UPLOADER_ID,
        &uploads_dir,
    )
    .await
    .unwrap();

    server.verify().await;
}

#[tokio::test]
async fn terminal_abort_skips_malformed_formal_manifest_and_aborts_verified_manifest() {
    let server = MockServer::start().await;
    mount_abort_for(&server, UPLOAD_ID).await;
    let temp = tempfile::tempdir().unwrap();
    let uploads_dir = temp.path().join("gallery.zip.uploads");
    tokio::fs::create_dir(&uploads_dir).await.unwrap();
    tokio::fs::write(uploads_dir.join("archive.json"), b"not json")
        .await
        .unwrap();
    write_valid_manifest(
        &uploads_dir.join("z-matching.json"),
        &vec![7; PART_SIZE],
        UPLOADER_ID,
    )
    .await;

    abort_upload_state(
        test_bucket(&server).as_ref(),
        ProviderKind::S3,
        UPLOADER_ID,
        &uploads_dir,
    )
    .await
    .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_request_sequence(&requests, &["DELETE"]);
    assert_eq!(
        query_value(&requests[0], "uploadId").as_deref(),
        Some(UPLOAD_ID)
    );
    server.verify().await;
}

async fn mount_create(server: &MockServer) {
    mount_create_with_upload_id(server, UPLOAD_ID).await;
}

async fn mount_create_with_upload_id(server: &MockServer, upload_id: &str) {
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "<InitiateMultipartUploadResult><Bucket>{BUCKET}</Bucket><Key>{KEY}</Key><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
        )))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_empty_list_parts(server: &MockServer) {
    mount_empty_list_parts_for(server, UPLOAD_ID).await;
}

async fn mount_empty_list_parts_for(server: &MockServer, upload_id: &str) {
    Mock::given(method("GET"))
        .and(query_param("uploadId", upload_id))
        .and(query_param("part-number-marker", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "<ListPartsResult><Bucket>{BUCKET}</Bucket><Key>{KEY}</Key><UploadId>{upload_id}</UploadId><PartNumberMarker>0</PartNumberMarker><NextPartNumberMarker>0</NextPartNumberMarker><MaxParts>1000</MaxParts><IsTruncated>false</IsTruncated></ListPartsResult>"
        )))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_complete(server: &MockServer) {
    mount_complete_for(server, UPLOAD_ID).await;
}

async fn mount_complete_for(server: &MockServer, upload_id: &str) {
    Mock::given(method("POST"))
        .and(query_param("uploadId", upload_id))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_upload_part_for(server: &MockServer, upload_id: &str, part_number: u32, etag: &str) {
    Mock::given(method("PUT"))
        .and(query_param("uploadId", upload_id))
        .and(query_param("partNumber", part_number.to_string()))
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", etag))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_abort_for(server: &MockServer, upload_id: &str) {
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", upload_id))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(server)
        .await;
}

async fn assert_malformed_create_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(ResponseTemplate::new(200).set_body_string("<not-a-create-response"))
        .expect(1)
        .mount(&server)
        .await;
    let bytes = vec![4; PART_SIZE];
    assert!(upload_multipart(
        test_bucket(&server).as_ref(),
        &reqwest::Client::new(),
        standard_request(&bytes, None),
    )
    .await
    .is_err());
    server.verify().await;
}

async fn assert_active_service_failure_preserves_manifest(operation: MultipartOperation) {
    let server = MockServer::start().await;
    let bytes = vec![4; PART_SIZE];
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join("archive.json");
    match operation {
        MultipartOperation::ListParts => {
            write_valid_manifest(&manifest_path, &bytes, UPLOADER_ID).await;
            Mock::given(method("GET"))
                .and(query_param("uploadId", UPLOAD_ID))
                .respond_with(
                    ResponseTemplate::new(403).set_body_string(s3_error_xml("AccessDenied")),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        MultipartOperation::UploadPart => {
            mount_create(&server).await;
            mount_empty_list_parts(&server).await;
            Mock::given(method("PUT"))
                .and(query_param("uploadId", UPLOAD_ID))
                .and(query_param("partNumber", "1"))
                .respond_with(ResponseTemplate::new(503))
                .expect(3)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(query_param("uploadId", UPLOAD_ID))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
        }
        MultipartOperation::Complete => {
            mount_create(&server).await;
            mount_empty_list_parts(&server).await;
            mount_upload_part_for(&server, UPLOAD_ID, 1, "\"part-1\"").await;
            Mock::given(method("POST"))
                .and(query_param("uploadId", UPLOAD_ID))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(s3_error_xml("AccessDenied")),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        MultipartOperation::Create
        | MultipartOperation::ZipPut
        | MultipartOperation::Abort
        | MultipartOperation::Head => unreachable!(),
    }
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    assert!(upload_multipart(
        test_bucket(&server).as_ref(),
        &reqwest::Client::new(),
        standard_request(&bytes, Some(&manifest_path)),
    )
    .await
    .is_err());
    assert!(manifest_path.exists());
    server.verify().await;
}

fn standard_request<'a>(
    bytes: &'a [u8],
    manifest_path: Option<&'a Path>,
) -> MultipartUploadRequest<'a> {
    MultipartUploadRequest {
        provider: ProviderKind::S3,
        uploader_identity_sha256: UPLOADER_ID,
        candidate_object_key: KEY.to_owned(),
        logical_object_id: "gallery-123",
        reader: Box::new(std::io::Cursor::new(bytes)),
        object_len: bytes.len() as u64,
        object_sha256: sha256_hex(bytes),
        part_size: PART_SIZE as u64,
        content_type: "application/octet-stream",
        manifest_path,
        create_extension: CreateExtension::None,
        head_recovery: HeadRecovery::Never,
    }
}

async fn write_valid_manifest(path: &Path, bytes: &[u8], uploader_identity: &str) {
    write_valid_manifest_for(path, bytes, uploader_identity, ProviderKind::S3).await;
}

async fn write_valid_manifest_for(
    path: &Path,
    bytes: &[u8],
    uploader_identity: &str,
    provider: ProviderKind,
) {
    let object_sha256 = sha256_hex(bytes);
    let identity = manifest::ManifestIdentity {
        provider,
        uploader_identity_sha256: uploader_identity,
        logical_object_id: "gallery-123",
        object_sha256: &object_sha256,
        object_len: bytes.len() as u64,
        content_type: "application/octet-stream",
        requested_entries_sha256: None,
    };
    let manifest = manifest::new_manifest(
        &identity,
        KEY.to_owned(),
        UPLOAD_ID.to_owned(),
        PART_SIZE as u64,
    )
    .unwrap();
    manifest::write_manifest_atomic(path, &manifest)
        .await
        .unwrap();
}

fn s3_error_xml(code: &str) -> String {
    format!("<Error><Code>{code}</Code><Message>safe message</Message></Error>")
}

fn test_bucket(server: &MockServer) -> Box<Bucket> {
    test_bucket_for_endpoint(&server.uri())
}

fn test_bucket_for_endpoint(endpoint: &str) -> Box<Bucket> {
    let credentials = Credentials::new(
        Some("AKIA_TEST_SENTINEL"),
        Some("secret-test-sentinel"),
        None,
        None,
        None,
    )
    .unwrap();
    Bucket::new(
        BUCKET,
        Region::Custom {
            region: "us-east-1".to_owned(),
            endpoint: endpoint.to_owned(),
        },
        credentials,
    )
    .unwrap()
    .with_path_style()
}

struct RawMultipartServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<RawRequest>>>,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone)]
struct RawRequest {
    method: String,
    target: String,
    body: Vec<u8>,
}

struct RawServerState {
    object_len: usize,
    lost_response_part: u32,
    listed_parts: Vec<u32>,
    create_calls: usize,
    list_calls: usize,
    lost_response_calls: usize,
}

impl RawMultipartServer {
    fn start(object_len: usize, lost_response_part: u32, listed_parts: Vec<u32>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));
        let state = Arc::new(Mutex::new(RawServerState {
            object_len,
            lost_response_part,
            listed_parts,
            create_calls: 0,
            list_calls: 0,
            lost_response_calls: 0,
        }));
        let worker = {
            let requests = Arc::clone(&requests);
            let running = Arc::clone(&running);
            std::thread::spawn(move || {
                while running.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => serve_raw_multipart_request(stream, &requests, &state),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            panic!("raw multipart test server accept failed: {error}")
                        }
                    }
                }
            })
        };
        Self {
            endpoint,
            requests,
            running,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<RawRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for RawMultipartServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.worker.take().unwrap().join();
    }
}

fn serve_raw_multipart_request(
    mut stream: TcpStream,
    requests: &Arc<Mutex<Vec<RawRequest>>>,
    state: &Arc<Mutex<RawServerState>>,
) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = read_raw_request(&mut stream).unwrap();
    let response = {
        let mut state = state.lock().unwrap();
        let is_create =
            request.method == "POST" && raw_query_value(&request.target, "uploads").is_some();
        let is_list =
            request.method == "GET" && raw_query_value(&request.target, "uploadId").is_some();
        let part_number = (request.method == "PUT")
            .then(|| raw_query_value(&request.target, "partNumber"))
            .flatten()
            .and_then(|value| value.parse::<u32>().ok());
        let is_complete =
            request.method == "POST" && raw_query_value(&request.target, "uploadId").is_some();

        if is_create {
            state.create_calls += 1;
            let object_key = raw_object_key(&request.target).unwrap();
            let upload_id = if state.create_calls == 1 {
                UPLOAD_ID
            } else {
                "unexpected-fresh-upload-id"
            };
            Some((
                200,
                Vec::new(),
                format!(
                    "<InitiateMultipartUploadResult><Bucket>{BUCKET}</Bucket><Key>{object_key}</Key><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
                )
                .into_bytes(),
            ))
        } else if is_list {
            state.list_calls += 1;
            let upload_id = raw_query_value(&request.target, "uploadId").unwrap();
            let parts = if state.list_calls > 1 && upload_id == UPLOAD_ID {
                state
                    .listed_parts
                    .iter()
                    .map(|part_number| {
                        (
                            *part_number,
                            format!("\"part-{part_number}\""),
                            expected_part_size(
                                state.object_len as u64,
                                PART_SIZE as u64,
                                *part_number,
                            )
                            .unwrap(),
                        )
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            Some((
                200,
                Vec::new(),
                list_parts_response_for(
                    BUCKET,
                    raw_object_key(&request.target).unwrap(),
                    upload_id,
                    &parts,
                )
                .into_bytes(),
            ))
        } else if let Some(part_number) = part_number {
            if part_number == state.lost_response_part && state.lost_response_calls < 3 {
                state.lost_response_calls += 1;
                None
            } else {
                Some((
                    200,
                    vec![("ETag", format!("\"part-{part_number}\""))],
                    Vec::new(),
                ))
            }
        } else if is_complete {
            Some((200, Vec::new(), Vec::new()))
        } else {
            Some((500, Vec::new(), Vec::new()))
        }
    };
    requests.lock().unwrap().push(request);
    if let Some((status, headers, body)) = response {
        write_raw_response(&mut stream, status, &headers, &body).unwrap();
    } else {
        stream.shutdown(Shutdown::Both).unwrap();
    }
}

fn read_raw_request(stream: &mut TcpStream) -> std::io::Result<RawRequest> {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 8192];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    let mut lines = headers.split("\r\n");
    let mut request_line = lines.next().unwrap().split_ascii_whitespace();
    let method = request_line.next().unwrap().to_owned();
    let target = request_line.next().unwrap().to_owned();
    let content_length = lines
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
        })
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let mut chunk = [0; 8192];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(RawRequest {
        method,
        target,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

fn write_raw_response(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(&str, String)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut response = format!(
        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes())?;
    stream.write_all(body)
}

fn raw_query_value<'a>(target: &'a str, name: &str) -> Option<&'a str> {
    target.split_once('?').and_then(|(_, query)| {
        query.split('&').find_map(|item| {
            let (actual, value) = item.split_once('=').unwrap_or((item, ""));
            (actual == name).then_some(value)
        })
    })
}

fn raw_object_key(target: &str) -> Option<&str> {
    target
        .split_once('?')
        .map(|(path, _)| path)
        .and_then(|path| path.strip_prefix(&format!("/{BUCKET}/")))
}

fn list_parts_response_for(
    bucket: &str,
    key: &str,
    upload_id: &str,
    parts: &[(u32, String, u64)],
) -> String {
    let parts = parts
        .iter()
        .map(|(part_number, etag, size)| {
            format!(
                "<Part><PartNumber>{part_number}</PartNumber><ETag>{etag}</ETag><Size>{size}</Size></Part>"
            )
        })
        .collect::<String>();
    format!(
        "<ListPartsResult><Bucket>{bucket}</Bucket><Key>{key}</Key><UploadId>{upload_id}</UploadId><PartNumberMarker>0</PartNumberMarker><NextPartNumberMarker>0</NextPartNumberMarker><MaxParts>1000</MaxParts><IsTruncated>false</IsTruncated>{parts}</ListPartsResult>"
    )
}

fn assert_raw_request_sequence(requests: &[RawRequest], expected: &[&str]) {
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        expected
    );
}

fn assert_request_sequence(requests: &[wiremock::Request], expected: &[&str]) {
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        expected
    );
}

fn assert_no_decompress_query(requests: &[wiremock::Request]) {
    for request in requests {
        assert!(request
            .url
            .query_pairs()
            .all(|(name, _)| { !name.to_ascii_lowercase().starts_with("decompress-") }));
    }
}

fn query_value(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(actual, _)| actual == name)
        .map(|(_, value)| value.into_owned())
}

fn complete_parts(request: &wiremock::Request) -> Vec<(u32, String)> {
    complete_parts_body(request.body.as_slice())
}

fn complete_parts_body(body: &[u8]) -> Vec<(u32, String)> {
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

    quick_xml::de::from_reader::<_, CompleteMultipartUpload>(std::io::Cursor::new(body))
        .unwrap()
        .parts
        .into_iter()
        .map(|part| (part.part_number, part.etag))
        .collect()
}
