use super::{MultipartOperation, ProviderKind};
use quick_xml::events::Event;
use quick_xml::name::QName;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt;

const MAX_PARTS_PER_REQUEST: u32 = 1_000;
const MAX_PART_NUMBER: u32 = 10_000;
const PRESIGNED_URL_EXPIRY_SECONDS: u32 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompletedPart {
    pub(super) part_number: u32,
    pub(super) etag: String,
    pub(super) size: u64,
}

pub(super) enum MultipartFailure {
    Unsupported {
        operation: MultipartOperation,
        status: u16,
        code: String,
    },
    NoSuchUpload {
        operation: MultipartOperation,
    },
    InvalidInventory(String),
    Service {
        operation: MultipartOperation,
        status: u16,
        code: Option<String>,
    },
    Protocol(String),
    Client(crate::Error),
}

impl fmt::Debug for MultipartFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported {
                operation, status, ..
            } => formatter
                .debug_struct("MultipartFailure::Unsupported")
                .field("operation", operation)
                .field("status", status)
                .finish(),
            Self::NoSuchUpload { operation } => formatter
                .debug_struct("MultipartFailure::NoSuchUpload")
                .field("operation", operation)
                .finish(),
            Self::InvalidInventory(_) => formatter
                .debug_tuple("MultipartFailure::InvalidInventory")
                .field(&"redacted")
                .finish(),
            Self::Service {
                operation, status, ..
            } => formatter
                .debug_struct("MultipartFailure::Service")
                .field("operation", operation)
                .field("status", status)
                .finish(),
            Self::Protocol(_) => formatter
                .debug_tuple("MultipartFailure::Protocol")
                .field(&"redacted")
                .finish(),
            Self::Client(_) => formatter
                .debug_tuple("MultipartFailure::Client")
                .field(&"redacted")
                .finish(),
        }
    }
}

impl fmt::Display for MultipartFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported {
                operation, status, ..
            } => write!(
                formatter,
                "S3 multipart {operation:?} is unsupported (HTTP {status})"
            ),
            Self::NoSuchUpload { operation } => {
                write!(
                    formatter,
                    "S3 multipart {operation:?} upload no longer exists"
                )
            }
            Self::InvalidInventory(_) => {
                write!(formatter, "S3 multipart part inventory is invalid")
            }
            Self::Service {
                operation, status, ..
            } => write!(
                formatter,
                "S3 multipart {operation:?} service request failed (HTTP {status})"
            ),
            Self::Protocol(_) => write!(formatter, "S3 multipart protocol response is invalid"),
            Self::Client(_) => write!(formatter, "S3 multipart client request failed"),
        }
    }
}

pub(super) async fn list_all_parts(
    http: &reqwest::Client,
    provider: ProviderKind,
    bucket: &s3::Bucket,
    key: &str,
    upload_id: &str,
) -> Result<Vec<CompletedPart>, MultipartFailure> {
    let mut marker = 0;
    let mut seen_part_numbers = HashSet::new();
    let mut completed_parts = Vec::new();

    loop {
        let response = {
            let mut query = HashMap::new();
            query.insert("uploadId".to_owned(), upload_id.to_owned());
            query.insert("part-number-marker".to_owned(), marker.to_string());
            query.insert("max-parts".to_owned(), MAX_PARTS_PER_REQUEST.to_string());
            let signed_url = bucket
                .presign_get(key, PRESIGNED_URL_EXPIRY_SECONDS, Some(query))
                .await
                .map_err(|error| classify_s3_error(MultipartOperation::ListParts, error))?;
            if signed_url_has_decompress_query(&signed_url) {
                return Err(client_failure(MultipartOperation::ListParts));
            }
            let result = http.get(&signed_url).send().await;
            drop(signed_url);
            result.map_err(|error| {
                MultipartFailure::Client(crate::Error::Http(error.without_url()))
            })?
        };
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|error| MultipartFailure::Client(crate::Error::Http(error.without_url())))?;
        classify_response(MultipartOperation::ListParts, status, &body)?;
        let page = parse_list_parts_result(&body)?;

        if page.bucket != bucket.name || page.key != key || page.upload_id != upload_id {
            return Err(protocol_failure());
        }
        let pagination = validate_pagination(provider, &page, marker)?;

        for part in page.parts {
            if !(1..=MAX_PART_NUMBER).contains(&part.part_number) || part.etag.trim().is_empty() {
                return Err(invalid_inventory_failure());
            }
            if !seen_part_numbers.insert(part.part_number) {
                return Err(invalid_inventory_failure());
            }
            if part.part_number <= marker {
                return Err(protocol_failure());
            }
            completed_parts.push(CompletedPart {
                part_number: part.part_number,
                etag: part.etag,
                size: part.size,
            });
        }

        match pagination {
            Pagination::CompleteInventory | Pagination::FinalPage => break,
            Pagination::Truncated { next_marker } => marker = next_marker,
        }
    }

    completed_parts.sort_unstable_by_key(|part| part.part_number);
    Ok(completed_parts)
}

pub(super) fn classify_response(
    operation: MultipartOperation,
    status: u16,
    body: &[u8],
) -> Result<(), MultipartFailure> {
    if let Some(failure) = classify_embedded_s3_error(operation, status, body) {
        return Err(failure);
    }
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(MultipartFailure::Service {
            operation,
            status,
            code: None,
        })
    }
}

pub(super) fn classify_embedded_s3_error(
    operation: MultipartOperation,
    status: u16,
    body: &[u8],
) -> Option<MultipartFailure> {
    if !has_root(body, b"Error") {
        return None;
    }
    if !has_only_root(body, b"Error") {
        return Some(protocol_failure());
    }

    let error: S3ErrorBody = match quick_xml::de::from_reader(std::io::Cursor::new(body)) {
        Ok(error) => error,
        Err(_) => return Some(protocol_failure()),
    };
    if error.code.trim().is_empty() || error.message.trim().is_empty() {
        return Some(protocol_failure());
    }

    if status == 501
        && error.code == "NotImplemented"
        && is_canonical_not_implemented_operation(operation)
    {
        return Some(MultipartFailure::Unsupported {
            operation,
            status,
            code: error.code,
        });
    }
    if (500..600).contains(&status) {
        return Some(MultipartFailure::Service {
            operation,
            status,
            code: Some(error.code),
        });
    }
    if error.code == "NoSuchUpload" {
        return Some(MultipartFailure::NoSuchUpload { operation });
    }
    if is_explicit_unsupported_operation(operation, status, &error.code) {
        return Some(MultipartFailure::Unsupported {
            operation,
            status,
            code: error.code,
        });
    }
    Some(MultipartFailure::Service {
        operation,
        status,
        code: Some(error.code),
    })
}

pub(super) fn classify_s3_error(
    operation: MultipartOperation,
    error: s3::error::S3Error,
) -> MultipartFailure {
    match error {
        s3::error::S3Error::HttpFailWithBody(status, body) => {
            match classify_response(operation, status, body.as_bytes()) {
                Err(failure) => failure,
                Ok(()) => MultipartFailure::Service {
                    operation,
                    status,
                    code: None,
                },
            }
        }
        s3::error::S3Error::Reqwest(error) => {
            MultipartFailure::Client(crate::Error::Http(error.without_url()))
        }
        _ => client_failure(operation),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename = "ListPartsResult")]
struct ListPartsResult {
    #[serde(rename = "Bucket")]
    bucket: String,
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "UploadId")]
    upload_id: String,
    #[serde(rename = "PartNumberMarker")]
    part_number_marker: Option<u32>,
    #[serde(rename = "NextPartNumberMarker")]
    next_part_number_marker: Option<u32>,
    #[serde(rename = "MaxParts")]
    max_parts: Option<u32>,
    #[serde(rename = "IsTruncated")]
    is_truncated: Option<bool>,
    #[serde(rename = "Part", default)]
    parts: Vec<ListedPart>,
}

enum Pagination {
    CompleteInventory,
    FinalPage,
    Truncated { next_marker: u32 },
}

fn validate_pagination(
    provider: ProviderKind,
    page: &ListPartsResult,
    requested_marker: u32,
) -> Result<Pagination, MultipartFailure> {
    let standard_fields_present = page.part_number_marker.is_some()
        && page.max_parts.is_some()
        && page.is_truncated.is_some();
    let pagination_fields_missing = page.part_number_marker.is_none()
        && page.next_part_number_marker.is_none()
        && page.max_parts.is_none()
        && page.is_truncated.is_none();

    match provider {
        ProviderKind::S3 => validate_standard_pagination(page, requested_marker),
        ProviderKind::IpfS3 if standard_fields_present => {
            validate_standard_pagination(page, requested_marker)
        }
        ProviderKind::IpfS3 if pagination_fields_missing && requested_marker == 0 => {
            Ok(Pagination::CompleteInventory)
        }
        ProviderKind::IpfS3 => Err(protocol_failure()),
    }
}

fn validate_standard_pagination(
    page: &ListPartsResult,
    requested_marker: u32,
) -> Result<Pagination, MultipartFailure> {
    if page.part_number_marker != Some(requested_marker)
        || page.max_parts != Some(MAX_PARTS_PER_REQUEST)
    {
        return Err(protocol_failure());
    }

    if !page.is_truncated.ok_or_else(protocol_failure)? {
        return Ok(Pagination::FinalPage);
    }

    let next_marker = page.next_part_number_marker.ok_or_else(protocol_failure)?;
    if !(1..=MAX_PART_NUMBER).contains(&next_marker) || next_marker <= requested_marker {
        return Err(protocol_failure());
    }
    Ok(Pagination::Truncated { next_marker })
}

#[derive(Debug, Deserialize)]
#[serde(rename = "Part")]
struct ListedPart {
    #[serde(rename = "PartNumber")]
    part_number: u32,
    #[serde(rename = "ETag")]
    etag: String,
    #[serde(rename = "Size")]
    size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "Error")]
struct S3ErrorBody {
    #[serde(rename = "Code")]
    code: String,
    #[serde(rename = "Message")]
    message: String,
}

fn parse_list_parts_result(body: &[u8]) -> Result<ListPartsResult, MultipartFailure> {
    if !has_only_root(body, b"ListPartsResult") {
        return Err(protocol_failure());
    }
    quick_xml::de::from_reader(std::io::Cursor::new(body)).map_err(|_| protocol_failure())
}

fn has_root(body: &[u8], expected: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(body);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                return element.name().as_ref() == expected;
            }
            Ok(Event::Decl(_) | Event::Comment(_) | Event::PI(_) | Event::DocType(_)) => {}
            Ok(Event::Text(text)) if text.iter().all(u8::is_ascii_whitespace) => {}
            Ok(Event::Eof) | Ok(_) | Err(_) => return false,
        }
    }
}

fn has_only_root(body: &[u8], expected: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(body);
    let mut buffer = Vec::new();
    let root_is_empty = loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) if element.name().as_ref() == expected => break false,
            Ok(Event::Empty(element)) if element.name().as_ref() == expected => break true,
            Ok(Event::Decl(_) | Event::Comment(_) | Event::PI(_) | Event::DocType(_)) => {}
            Ok(Event::Text(text)) if text.iter().all(u8::is_ascii_whitespace) => {}
            Ok(_) | Err(_) => return false,
        }
    };
    if !root_is_empty && reader.read_to_end(QName(expected)).is_err() {
        return false;
    }

    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Eof) => return true,
            Ok(Event::Comment(_) | Event::PI(_)) => {}
            Ok(Event::Text(text)) if text.iter().all(u8::is_ascii_whitespace) => {}
            Ok(_) | Err(_) => return false,
        }
    }
}

fn is_explicit_unsupported_operation(
    operation: MultipartOperation,
    status: u16,
    code: &str,
) -> bool {
    match operation {
        MultipartOperation::ZipPut => {
            matches!(code, "NotImplemented" | "UnsupportedOperation")
                || (status == 405 && code == "MethodNotAllowed")
        }
        MultipartOperation::Create
        | MultipartOperation::ListParts
        | MultipartOperation::UploadPart
        | MultipartOperation::Complete => {
            matches!(
                code,
                "NotImplemented" | "UnsupportedOperation" | "MethodNotAllowed"
            )
        }
        MultipartOperation::Abort | MultipartOperation::Head => false,
    }
}

fn is_canonical_not_implemented_operation(operation: MultipartOperation) -> bool {
    matches!(
        operation,
        MultipartOperation::Create
            | MultipartOperation::ListParts
            | MultipartOperation::UploadPart
            | MultipartOperation::Complete
            | MultipartOperation::ZipPut
    )
}

fn signed_url_has_decompress_query(signed_url: &str) -> bool {
    reqwest::Url::parse(signed_url).ok().is_some_and(|url| {
        url.query_pairs()
            .any(|(name, _)| name.to_ascii_lowercase().starts_with("decompress-"))
    })
}

fn protocol_failure() -> MultipartFailure {
    MultipartFailure::Protocol("S3 multipart response violated the ListParts protocol".to_owned())
}

fn invalid_inventory_failure() -> MultipartFailure {
    MultipartFailure::InvalidInventory(
        "S3 multipart response contained an invalid part inventory".to_owned(),
    )
}

fn client_failure(operation: MultipartOperation) -> MultipartFailure {
    MultipartFailure::Client(crate::Error::Other(format!(
        "S3 multipart {operation:?} client request failed"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use s3::creds::Credentials;
    use s3::{Bucket, Region};
    use wiremock::matchers::{method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const BUCKET: &str = "task-four-bucket";
    const KEY: &str = "object-key";
    const UPLOAD_ID: &str = "upload-id";

    #[tokio::test]
    async fn list_parts_follows_markers_and_preserves_quoted_etags() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("part-number-marker", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_string(list_parts_xml(
                BUCKET,
                KEY,
                UPLOAD_ID,
                0,
                2,
                true,
                &[(1, "\"cid-one\"", 10), (2, "\"cid-two\"", 20)],
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("part-number-marker", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(list_parts_xml(
                BUCKET,
                KEY,
                UPLOAD_ID,
                2,
                3,
                false,
                &[(3, "\"cid-three\"", 30)],
            )))
            .expect(1)
            .mount(&server)
            .await;

        let parts = list_all_parts(
            &reqwest::Client::new(),
            ProviderKind::S3,
            &test_bucket(&server),
            KEY,
            UPLOAD_ID,
        )
        .await
        .unwrap();
        assert_eq!(
            parts,
            vec![
                CompletedPart {
                    part_number: 1,
                    etag: "\"cid-one\"".to_owned(),
                    size: 10,
                },
                CompletedPart {
                    part_number: 2,
                    etag: "\"cid-two\"".to_owned(),
                    size: 20,
                },
                CompletedPart {
                    part_number: 3,
                    etag: "\"cid-three\"".to_owned(),
                    size: 30,
                },
            ]
        );

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for (request, marker) in requests.iter().zip(["0", "2"]) {
            assert_eq!(query_value(request, "uploadId").as_deref(), Some(UPLOAD_ID));
            assert_eq!(
                query_value(request, "part-number-marker").as_deref(),
                Some(marker)
            );
            assert_eq!(query_value(request, "max-parts").as_deref(), Some("1000"));
            assert_eq!(
                query_value(request, "X-Amz-Algorithm").as_deref(),
                Some("AWS4-HMAC-SHA256")
            );
            assert!(query_value(request, "X-Amz-Credential").is_some());
            assert!(query_value(request, "X-Amz-Signature").is_some());
            assert!(request
                .url
                .query_pairs()
                .all(|(name, _)| !name.to_ascii_lowercase().starts_with("decompress-")));
        }
    }

    #[tokio::test]
    async fn ipfs3_accepts_target_list_parts_responses_without_pagination_but_s3_rejects_them() {
        let fixtures = [
            (
                ipfs3_list_parts_without_pagination_xml(BUCKET, KEY, UPLOAD_ID, &[]),
                Vec::new(),
            ),
            (
                ipfs3_list_parts_without_pagination_xml(
                    BUCKET,
                    KEY,
                    UPLOAD_ID,
                    &[(1, "\"cid-one\"", 10)],
                ),
                vec![CompletedPart {
                    part_number: 1,
                    etag: "\"cid-one\"".to_owned(),
                    size: 10,
                }],
            ),
        ];

        for (body, expected_parts) in fixtures {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .expect(2)
                .mount(&server)
                .await;

            let parts = list_all_parts(
                &reqwest::Client::new(),
                ProviderKind::IpfS3,
                &test_bucket(&server),
                KEY,
                UPLOAD_ID,
            )
            .await
            .unwrap();
            assert_eq!(parts, expected_parts);

            let error = list_all_parts(
                &reqwest::Client::new(),
                ProviderKind::S3,
                &test_bucket(&server),
                KEY,
                UPLOAD_ID,
            )
            .await
            .unwrap_err();
            assert_failure_kind(error, FailureKind::Protocol);
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn list_parts_rejects_ipfs3_mixed_or_nonfirst_pagination_omission() {
        let mixed = format!(
            "<ListPartsResult><Bucket>{BUCKET}</Bucket><Key>{KEY}</Key><UploadId>{UPLOAD_ID}</UploadId><PartNumberMarker>0</PartNumberMarker><MaxParts>1000</MaxParts></ListPartsResult>"
        );
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(mixed))
            .expect(1)
            .mount(&server)
            .await;
        let error = list_all_parts(
            &reqwest::Client::new(),
            ProviderKind::IpfS3,
            &test_bucket(&server),
            KEY,
            UPLOAD_ID,
        )
        .await
        .unwrap_err();
        assert_failure_kind(error, FailureKind::Protocol);
        server.verify().await;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("part-number-marker", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_string(list_parts_xml(
                BUCKET,
                KEY,
                UPLOAD_ID,
                0,
                1,
                true,
                &[(1, "etag-one", 1)],
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("part-number-marker", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                ipfs3_list_parts_without_pagination_xml(
                    BUCKET,
                    KEY,
                    UPLOAD_ID,
                    &[(2, "etag-two", 1)],
                ),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let error = list_all_parts(
            &reqwest::Client::new(),
            ProviderKind::IpfS3,
            &test_bucket(&server),
            KEY,
            UPLOAD_ID,
        )
        .await
        .unwrap_err();
        assert_failure_kind(error, FailureKind::Protocol);
        server.verify().await;
    }

    #[tokio::test]
    async fn list_parts_allows_a_nontruncated_s3_final_page_without_next_marker() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                list_parts_xml_without_next_marker(
                    BUCKET,
                    KEY,
                    UPLOAD_ID,
                    0,
                    false,
                    &[(1, "etag-one", 1)],
                ),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let parts = list_all_parts(
            &reqwest::Client::new(),
            ProviderKind::S3,
            &test_bucket(&server),
            KEY,
            UPLOAD_ID,
        )
        .await
        .unwrap();
        assert_eq!(
            parts,
            vec![CompletedPart {
                part_number: 1,
                etag: "etag-one".to_owned(),
                size: 1,
            }]
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn list_parts_rejects_identity_marker_and_inventory_violations() {
        let cases = [
            (
                list_parts_xml("wrong-bucket", KEY, UPLOAD_ID, 0, 1, false, &[]),
                FailureKind::Protocol,
            ),
            (
                list_parts_xml(BUCKET, "wrong-key", UPLOAD_ID, 0, 1, false, &[]),
                FailureKind::Protocol,
            ),
            (
                list_parts_xml(BUCKET, KEY, "wrong-upload", 0, 1, false, &[]),
                FailureKind::Protocol,
            ),
            (
                list_parts_xml(BUCKET, KEY, UPLOAD_ID, 1, 1, false, &[]),
                FailureKind::Protocol,
            ),
            (
                list_parts_xml(BUCKET, KEY, UPLOAD_ID, 0, 1, false, &[(0, "etag", 1)]),
                FailureKind::Inventory,
            ),
            (
                list_parts_xml(BUCKET, KEY, UPLOAD_ID, 0, 1, false, &[(10_001, "etag", 1)]),
                FailureKind::Inventory,
            ),
            (
                list_parts_xml(BUCKET, KEY, UPLOAD_ID, 0, 1, false, &[(1, " \t ", 1)]),
                FailureKind::Inventory,
            ),
            (
                list_parts_xml(BUCKET, KEY, UPLOAD_ID, 0, 0, true, &[(1, "etag", 1)]),
                FailureKind::Protocol,
            ),
        ];

        for (body, expected) in cases {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;
            let error = list_all_parts(
                &reqwest::Client::new(),
                ProviderKind::S3,
                &test_bucket(&server),
                KEY,
                UPLOAD_ID,
            )
            .await
            .unwrap_err();
            assert_failure_kind(error, expected);
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("part-number-marker", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_string(list_parts_xml(
                BUCKET,
                KEY,
                UPLOAD_ID,
                0,
                1,
                true,
                &[(1, "etag-one", 1)],
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("part-number-marker", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(list_parts_xml(
                BUCKET,
                KEY,
                UPLOAD_ID,
                1,
                1,
                false,
                &[(1, "etag-one", 1)],
            )))
            .mount(&server)
            .await;
        let error = list_all_parts(
            &reqwest::Client::new(),
            ProviderKind::S3,
            &test_bucket(&server),
            KEY,
            UPLOAD_ID,
        )
        .await
        .unwrap_err();
        assert_failure_kind(error, FailureKind::Inventory);
    }

    #[test]
    fn classifies_only_well_formed_embedded_s3_errors() {
        for operation in [
            MultipartOperation::Create,
            MultipartOperation::ListParts,
            MultipartOperation::UploadPart,
            MultipartOperation::Complete,
            MultipartOperation::ZipPut,
        ] {
            for code in ["NotImplemented", "UnsupportedOperation", "MethodNotAllowed"] {
                let error =
                    classify_response(operation, 405, s3_error_xml(code).as_bytes()).unwrap_err();
                assert!(matches!(
                    error,
                    MultipartFailure::Unsupported {
                        operation: actual_operation,
                        status: 405,
                        code: actual_code,
                    } if actual_operation == operation && actual_code == code
                ));
            }
        }
        let head = classify_response(
            MultipartOperation::Head,
            405,
            s3_error_xml("MethodNotAllowed").as_bytes(),
        )
        .unwrap_err();
        assert!(matches!(
            head,
            MultipartFailure::Service { status: 405, .. }
        ));

        let bare_405 =
            classify_response(MultipartOperation::ListParts, 405, b"not XML").unwrap_err();
        assert!(matches!(
            bare_405,
            MultipartFailure::Service { code: None, .. }
        ));
        let access_denied = classify_response(
            MultipartOperation::ListParts,
            403,
            s3_error_xml("AccessDenied").as_bytes(),
        )
        .unwrap_err();
        assert!(matches!(
            access_denied,
            MultipartFailure::Service {
                status: 403,
                code: Some(ref code),
                ..
            } if code == "AccessDenied"
        ));
        let server_error = classify_response(
            MultipartOperation::ListParts,
            500,
            s3_error_xml("NotImplemented").as_bytes(),
        )
        .unwrap_err();
        assert!(matches!(
            server_error,
            MultipartFailure::Service {
                status: 500,
                code: Some(ref code),
                ..
            } if code == "NotImplemented"
        ));
        let no_such_upload = classify_response(
            MultipartOperation::ListParts,
            404,
            s3_error_xml("NoSuchUpload").as_bytes(),
        )
        .unwrap_err();
        assert!(matches!(
            no_such_upload,
            MultipartFailure::NoSuchUpload {
                operation: MultipartOperation::ListParts
            }
        ));

        assert!(classify_embedded_s3_error(
            MultipartOperation::ListParts,
            200,
            b"<ListPartsResult><Code>NoSuchUpload</Code><Message>no</Message></ListPartsResult>",
        )
        .is_none());
        assert!(matches!(
            classify_embedded_s3_error(
                MultipartOperation::ListParts,
                200,
                b"<Error><Code>NoSuchUpload</Code>",
            ),
            Some(MultipartFailure::Protocol(_))
        ));
        assert!(matches!(
            classify_s3_error(
                MultipartOperation::ListParts,
                s3::error::S3Error::HttpFailWithBody(404, s3_error_xml("NoSuchUpload")),
            ),
            MultipartFailure::NoSuchUpload {
                operation: MultipartOperation::ListParts
            }
        ));
    }

    #[test]
    fn canonical_501_not_implemented_is_unsupported_only_for_transfer_operations() {
        for operation in [
            MultipartOperation::Create,
            MultipartOperation::ListParts,
            MultipartOperation::UploadPart,
            MultipartOperation::Complete,
            MultipartOperation::ZipPut,
        ] {
            let error =
                classify_response(operation, 501, s3_error_xml("NotImplemented").as_bytes())
                    .unwrap_err();
            assert!(matches!(
                error,
                MultipartFailure::Unsupported {
                    operation: actual_operation,
                    status: 501,
                    code: actual_code,
                } if actual_operation == operation && actual_code == "NotImplemented"
            ));
        }

        for operation in [MultipartOperation::Head, MultipartOperation::Abort] {
            let error =
                classify_response(operation, 501, s3_error_xml("NotImplemented").as_bytes())
                    .unwrap_err();
            assert!(matches!(
                error,
                MultipartFailure::Service {
                    operation: actual_operation,
                    status: 501,
                    code: Some(ref actual_code),
                } if actual_operation == operation && actual_code == "NotImplemented"
            ));
        }

        let bare = classify_response(MultipartOperation::ListParts, 501, b"not XML").unwrap_err();
        assert!(matches!(
            bare,
            MultipartFailure::Service {
                status: 501,
                code: None,
                ..
            }
        ));
        let malformed = classify_response(
            MultipartOperation::ListParts,
            501,
            b"<Error><Code>NotImplemented</Code>",
        )
        .unwrap_err();
        assert!(matches!(malformed, MultipartFailure::Protocol(_)));

        let server_error = classify_response(
            MultipartOperation::ListParts,
            500,
            s3_error_xml("NotImplemented").as_bytes(),
        )
        .unwrap_err();
        assert!(matches!(
            server_error,
            MultipartFailure::Service {
                status: 500,
                code: Some(ref code),
                ..
            } if code == "NotImplemented"
        ));
    }

    #[tokio::test]
    async fn list_parts_transport_failure_redacts_signed_url_and_credentials() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let close_connection = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);
        });
        let bucket = test_bucket_for_endpoint(&endpoint);
        let failure = list_all_parts(
            &reqwest::Client::new(),
            ProviderKind::S3,
            &bucket,
            "object-key-sentinel",
            "upload-id-sentinel",
        )
        .await
        .unwrap_err();
        close_connection.join().unwrap();
        assert!(matches!(
            failure,
            MultipartFailure::Client(crate::Error::Http(_))
        ));

        let rendered = format!("{failure} {failure:?}");
        for secret in [
            endpoint.as_str(),
            "X-Amz-",
            "Credential",
            "Signature",
            "object-key-sentinel",
            "upload-id-sentinel",
            "AKIA_TEST_SENTINEL",
            "secret-test-sentinel",
        ] {
            assert!(!rendered.contains(secret), "leaked {secret}");
        }
    }

    #[tokio::test]
    async fn list_parts_accepts_more_than_ten_small_pages() {
        let server = MockServer::start().await;
        for marker in 0..=10 {
            Mock::given(method("GET"))
                .and(query_param("part-number-marker", marker.to_string()))
                .respond_with(ResponseTemplate::new(200).set_body_string(list_parts_xml(
                    BUCKET,
                    KEY,
                    UPLOAD_ID,
                    marker,
                    marker + 1,
                    marker < 10,
                    &[(marker + 1, "etag", 1)],
                )))
                .expect(1)
                .mount(&server)
                .await;
        }

        let parts = list_all_parts(
            &reqwest::Client::new(),
            ProviderKind::S3,
            &test_bucket(&server),
            KEY,
            UPLOAD_ID,
        )
        .await
        .unwrap();
        assert_eq!(parts.len(), 11);
        assert_eq!(
            parts
                .into_iter()
                .map(|part| part.part_number)
                .collect::<Vec<_>>(),
            (1..=11).collect::<Vec<_>>()
        );
    }

    #[derive(Clone, Copy)]
    enum FailureKind {
        Protocol,
        Inventory,
    }

    fn assert_failure_kind(failure: MultipartFailure, expected: FailureKind) {
        match expected {
            FailureKind::Protocol => assert!(matches!(failure, MultipartFailure::Protocol(_))),
            FailureKind::Inventory => {
                assert!(matches!(failure, MultipartFailure::InvalidInventory(_)))
            }
        }
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

    fn query_value(request: &wiremock::Request, name: &str) -> Option<String> {
        request
            .url
            .query_pairs()
            .find(|(actual, _)| actual == name)
            .map(|(_, value)| value.into_owned())
    }

    fn s3_error_xml(code: &str) -> String {
        format!("<Error><Code>{code}</Code><Message>safe message</Message></Error>")
    }

    fn list_parts_xml(
        bucket: &str,
        key: &str,
        upload_id: &str,
        marker: u32,
        next_marker: u32,
        is_truncated: bool,
        parts: &[(u32, &str, u64)],
    ) -> String {
        let parts = parts
            .iter()
            .map(|(number, etag, size)| {
                format!("<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag><Size>{size}</Size></Part>")
            })
            .collect::<String>();
        format!(
            "<ListPartsResult><Bucket>{bucket}</Bucket><Key>{key}</Key><UploadId>{upload_id}</UploadId><PartNumberMarker>{marker}</PartNumberMarker><NextPartNumberMarker>{next_marker}</NextPartNumberMarker><MaxParts>1000</MaxParts><IsTruncated>{is_truncated}</IsTruncated>{parts}</ListPartsResult>"
        )
    }

    fn list_parts_xml_without_next_marker(
        bucket: &str,
        key: &str,
        upload_id: &str,
        marker: u32,
        is_truncated: bool,
        parts: &[(u32, &str, u64)],
    ) -> String {
        let parts = parts
            .iter()
            .map(|(number, etag, size)| {
                format!("<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag><Size>{size}</Size></Part>")
            })
            .collect::<String>();
        format!(
            "<ListPartsResult><Bucket>{bucket}</Bucket><Key>{key}</Key><UploadId>{upload_id}</UploadId><PartNumberMarker>{marker}</PartNumberMarker><MaxParts>1000</MaxParts><IsTruncated>{is_truncated}</IsTruncated>{parts}</ListPartsResult>"
        )
    }

    fn ipfs3_list_parts_without_pagination_xml(
        bucket: &str,
        key: &str,
        upload_id: &str,
        parts: &[(u32, &str, u64)],
    ) -> String {
        let parts = parts
            .iter()
            .map(|(number, etag, size)| {
                format!("<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag><Size>{size}</Size></Part>")
            })
            .collect::<String>();
        format!(
            "<ListPartsResult><Bucket>{bucket}</Bucket><Key>{key}</Key><UploadId>{upload_id}</UploadId>{parts}</ListPartsResult>"
        )
    }
}
