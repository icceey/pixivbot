use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, DATE};
use s3::{error::S3Error, request::ResponseData};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};

pub(crate) const MIN_PART_SIZE: u64 = 5 * 1024 * 1024;
pub(crate) const MAX_PART_SIZE: u64 = 5 * 1024 * 1024 * 1024;
pub(crate) const MAX_PARTS: usize = 10_000;
pub(crate) const IPFS3_DECOMPRESS_TAGGING: &str =
    "ipfs-s3%3Apin=true&ipfs-s3%3Aduration=1d&ipfs-s3%3Acontent=decompressed";
const TEMPORARY_MANIFEST_VERIFICATION_ERROR: &str =
    "multipart temporary manifest could not be safely verified";
const RESUME_UPLOADER_IDENTITY_MISMATCH_ERROR: &str =
    "multipart resume manifest belongs to a different uploader identity";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    S3,
    IpfS3,
}

pub(crate) fn fingerprint_fields(fields: &[impl AsRef<str>]) -> String {
    let mut hash = Sha256::new();
    for field in fields {
        let field = field.as_ref();
        hash.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
        hash.update(field.as_bytes());
    }
    hex::encode(hash.finalize())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(crate) async fn retry_put_object<F>(
    mut attempt: impl FnMut() -> F,
) -> Result<ResponseData, S3Error>
where
    F: Future<Output = Result<ResponseData, S3Error>>,
{
    let mut retries_remaining = 2;
    loop {
        let result = attempt().await;
        if retries_remaining == 0 {
            return result;
        }
        let retryable = match &result {
            Ok(response) => put_object_status_is_retryable(response.status_code()),
            Err(S3Error::HttpFailWithBody(status, _)) => put_object_status_is_retryable(*status),
            Err(S3Error::Reqwest(error)) => put_object_transport_is_retryable(error),
            Err(_) => false,
        };
        if !retryable {
            return result;
        }
        retries_remaining -= 1;
    }
}

fn put_object_status_is_retryable(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn put_object_transport_is_retryable(error: &reqwest::Error) -> bool {
    use std::error::Error;
    use std::io::ErrorKind;

    if let Some(status) = error.status() {
        return put_object_status_is_retryable(status.as_u16());
    }
    // `Response::bytes()` classifies interrupted HTTP body transfer as decode;
    // ETag/XML interpretation happens separately from this retry policy.
    if error.is_decode() {
        return true;
    }
    if !(error.is_timeout() || error.is_connect() || (error.is_request() && error.url().is_some()))
    {
        return false;
    }
    let mut source = error.source();
    while let Some(cause) = source {
        // On send, a nested reqwest body error comes from the local ZIP reader.
        if cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_body)
            || cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                matches!(
                    io.kind(),
                    ErrorKind::InvalidData | ErrorKind::InvalidInput | ErrorKind::PermissionDenied
                )
            })
        {
            return false;
        }
        source = cause.source();
    }
    true
}

/// Sign the known payload hash without materializing the file in rust-s3's
/// PutObject body. Keep its endpoint, credentials, headers and timeout behavior.
pub(crate) async fn put_object_stream(
    bucket: &s3::Bucket,
    key: &str,
    file: &tokio::fs::File,
    len: u64,
    sha256: &str,
    content_type: &str,
) -> Result<ResponseData, S3Error> {
    use hmac::{Hmac, KeyInit, Mac};
    use s3::request::Request;

    let request = s3::request::tokio_backend::ReqwestRequest::new(
        bucket,
        key,
        s3::command::Command::PutObject {
            content: &[],
            content_type,
            custom_headers: None,
            multipart: None,
        },
    )
    .await?;
    let mut headers = request.headers().await?;
    headers.remove(AUTHORIZATION);
    // The empty placeholder body must not supply the streamed body's MD5.
    // SigV4's payload hash still authenticates the entire file.
    headers.remove("content-md5");
    let date = headers.remove(DATE);
    headers.insert(CONTENT_LENGTH, len.to_string().parse()?);
    headers.insert(
        HeaderName::from_static("x-amz-content-sha256"),
        sha256.parse()?,
    );
    let url = request.url()?;
    let canonical = s3::signing::canonical_request("PUT", &url, &headers, sha256)?;
    let string_to_sign = request.string_to_sign(&canonical)?;
    let mut hmac = Hmac::<Sha256>::new_from_slice(&request.signing_key().await?)
        .expect("HMAC-SHA256 accepts keys of any length");
    hmac.update(string_to_sign.as_bytes());
    let authorization = s3::signing::authorization_header(
        &bucket
            .access_key()
            .await?
            .ok_or_else(|| S3Error::HttpFailWithBody(401, "missing S3 access key".into()))?,
        &request.datetime(),
        &bucket.region(),
        &s3::signing::signed_header_string(&headers),
        &hex::encode(hmac.finalize().into_bytes()),
    )?;
    headers.insert(AUTHORIZATION, authorization.parse()?);
    if let Some(date) = date {
        headers.insert(DATE, date);
    }
    let mut reader = file.try_clone().await?;
    reader.rewind().await?;
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(reader.take(len)));
    let builder = bucket.http_client().put(url).headers(headers).body(body);
    let builder = if let Some(timeout) = bucket.request_timeout() {
        builder.timeout(timeout)
    } else {
        builder
    };
    let response = builder.send().await?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(key, value)| {
            (
                key.to_string(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let body = response.bytes().await.map_err(|error| {
        // A known rejection controls retry eligibility even if its body could not be read.
        if (200..300).contains(&status) {
            S3Error::Reqwest(error)
        } else {
            S3Error::HttpFailWithBody(status, error.to_string())
        }
    })?;
    Ok(ResponseData::new(body, status, headers))
}

pub(crate) fn uploader_identity_fingerprint(
    provider: ProviderKind,
    endpoint: &str,
    region: &str,
    bucket: &str,
    path_style: bool,
) -> String {
    let provider = match provider {
        ProviderKind::S3 => "s3",
        ProviderKind::IpfS3 => "ipf_s3",
    };
    let path_style = if path_style { "true" } else { "false" };
    fingerprint_fields(&[provider, endpoint, region, bucket, path_style])
}

mod list_parts;
mod manifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MultipartOperation {
    Create,
    ListParts,
    UploadPart,
    Complete,
    ZipPut,
    Abort,
    Head,
}

pub(crate) fn zip_put_extension_is_explicitly_unsupported(status: u16, body: &[u8]) -> bool {
    matches!(
        list_parts::classify_response(MultipartOperation::ZipPut, status, body),
        Err(list_parts::MultipartFailure::Unsupported { .. })
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CreateExtension {
    None,
    IpfS3DecompressZip { requested_entries_sha256: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadRecovery {
    S3ByLength,
    IpfS3ImageByLengthAndEtag,
    Never,
}

#[derive(Debug, Default)]
pub(crate) struct MultipartCapability(AtomicU8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilityState {
    Unknown,
    Supported,
    Unsupported,
}

impl MultipartCapability {
    pub(crate) fn state(&self) -> CapabilityState {
        match self.0.load(Ordering::Acquire) {
            1 => CapabilityState::Supported,
            2 => CapabilityState::Unsupported,
            _ => CapabilityState::Unknown,
        }
    }

    pub(crate) fn mark_supported(&self) {
        self.0.store(1, Ordering::Release);
    }

    pub(crate) fn mark_unsupported(&self) {
        self.0.store(2, Ordering::Release);
    }
}

pub(crate) trait UploadReader: AsyncRead + AsyncSeek + Unpin + Send {}
impl<T: AsyncRead + AsyncSeek + Unpin + Send> UploadReader for T {}

pub(crate) struct MultipartUploadRequest<'a> {
    pub provider: ProviderKind,
    pub uploader_identity_sha256: &'a str,
    pub candidate_object_key: String,
    pub logical_object_id: &'a str,
    pub reader: Box<dyn UploadReader + 'a>,
    pub object_len: u64,
    pub object_sha256: String,
    pub part_size: u64,
    pub content_type: &'a str,
    pub manifest_path: Option<&'a Path>,
    pub create_extension: CreateExtension,
    pub head_recovery: HeadRecovery,
}

pub(crate) enum CompletionEvidence {
    Response(s3::request::ResponseData),
    RecoveredHead { etag: Option<String> },
}

pub(crate) struct CompletedUpload {
    pub object_key: String,
    pub evidence: CompletionEvidence,
    manifest_path: Option<PathBuf>,
}

impl CompletedUpload {
    pub(crate) async fn remove_manifest(&self) -> crate::Result<()> {
        match &self.manifest_path {
            Some(path) => manifest::remove_manifest(path).await,
            None => Ok(()),
        }
    }
}

pub(crate) enum MultipartOutcome {
    Completed(CompletedUpload),
    Unsupported { operation: MultipartOperation },
}

struct ActiveSession {
    object_key: String,
    upload_id: String,
    from_valid_manifest: bool,
    part_size: u64,
}

enum CreateSessionResult {
    Active(ActiveSession),
    Unsupported(MultipartOperation),
}

enum HeadDecision {
    Recovered(CompletionEvidence),
    Replace,
}

fn part_count(len: u64, part_size: u64) -> crate::Result<usize> {
    if !(MIN_PART_SIZE..=MAX_PART_SIZE).contains(&part_size) {
        return Err(crate::Error::Other(
            "multipart part size must be between 5 and 5120 MiB".into(),
        ));
    }
    if len == 0 {
        return Err(crate::Error::Other(
            "multipart upload object is empty".into(),
        ));
    }
    let count = len.div_ceil(part_size);
    if count > MAX_PARTS as u64 {
        return Err(crate::Error::Other(format!(
            "multipart upload needs {count} parts, exceeding the 10000-part limit"
        )));
    }
    Ok(count as usize)
}

fn expected_part_size(object_len: u64, part_size: u64, part_number: u32) -> Option<u64> {
    let start = u64::from(part_number)
        .checked_sub(1)?
        .checked_mul(part_size)?;
    (start < object_len).then(|| (object_len - start).min(part_size))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileInvalid {
    DuplicatePartNumber,
    InvalidPartNumber,
    EmptyEtag,
    InvalidPartSize,
}

fn reconcile_parts(
    object_len: u64,
    part_size: u64,
    listed: Vec<list_parts::CompletedPart>,
) -> Result<BTreeMap<u32, String>, ReconcileInvalid> {
    let total_parts =
        part_count(object_len, part_size).map_err(|_| ReconcileInvalid::InvalidPartNumber)?;
    let mut reconciled = BTreeMap::new();
    for part in listed {
        let part_number =
            usize::try_from(part.part_number).map_err(|_| ReconcileInvalid::InvalidPartNumber)?;
        if part_number == 0 || part_number > total_parts {
            return Err(ReconcileInvalid::InvalidPartNumber);
        }
        if part.etag.trim().is_empty() {
            return Err(ReconcileInvalid::EmptyEtag);
        }
        if part.size
            != expected_part_size(object_len, part_size, part.part_number)
                .ok_or(ReconcileInvalid::InvalidPartNumber)?
        {
            return Err(ReconcileInvalid::InvalidPartSize);
        }
        if reconciled.insert(part.part_number, part.etag).is_some() {
            return Err(ReconcileInvalid::DuplicatePartNumber);
        }
    }
    Ok(reconciled)
}

pub(crate) async fn upload_multipart(
    bucket: &s3::Bucket,
    http: &reqwest::Client,
    mut request: MultipartUploadRequest<'_>,
) -> crate::Result<MultipartOutcome> {
    let requested_entries_sha256 = match &request.create_extension {
        CreateExtension::None => None,
        CreateExtension::IpfS3DecompressZip {
            requested_entries_sha256,
        } => Some(requested_entries_sha256.as_str()),
    };
    let manifest_identity = manifest::ManifestIdentity {
        provider: request.provider,
        uploader_identity_sha256: request.uploader_identity_sha256,
        logical_object_id: request.logical_object_id,
        object_sha256: &request.object_sha256,
        object_len: request.object_len,
        content_type: request.content_type,
        requested_entries_sha256,
    };
    let mut replacement_used = false;
    let mut session = match request.manifest_path {
        Some(path) => match manifest::load_manifest(path, &manifest_identity).await? {
            manifest::ManifestLoad::Valid(manifest) => Some(ActiveSession {
                object_key: manifest.object_key,
                upload_id: manifest.upload_id,
                from_valid_manifest: true,
                part_size: manifest.part_size,
            }),
            manifest::ManifestLoad::Missing => None,
            manifest::ManifestLoad::MalformedJson => {
                manifest::remove_manifest(path).await?;
                None
            }
            manifest::ManifestLoad::Stale {
                reason:
                    manifest::ManifestMismatch::Provider | manifest::ManifestMismatch::UploaderIdentity,
                ..
            } => {
                return Err(crate::Error::Other(
                    RESUME_UPLOADER_IDENTITY_MISMATCH_ERROR.to_owned(),
                ));
            }
            manifest::ManifestLoad::Stale { manifest, .. } => {
                if !is_safe_session_value(&manifest.object_key)
                    || !is_safe_session_value(&manifest.upload_id)
                {
                    return Err(crate::Error::Other(
                        "multipart stale manifest cannot be safely replaced".to_owned(),
                    ));
                }
                abort_session(
                    bucket,
                    request.provider,
                    &manifest.object_key,
                    &manifest.upload_id,
                )
                .await?;
                manifest::remove_manifest(path).await?;
                claim_replacement_budget(&mut replacement_used, MultipartOperation::Create)?;
                None
            }
        },
        None => None,
    };
    let candidate_object_key = request.candidate_object_key;

    loop {
        let active = match session.take() {
            Some(session) => session,
            None => match create_session(
                bucket,
                &candidate_object_key,
                request.content_type,
                &request.create_extension,
                request.manifest_path,
                &manifest_identity,
                request.part_size,
            )
            .await?
            {
                CreateSessionResult::Active(session) => session,
                CreateSessionResult::Unsupported(operation) => {
                    return Ok(MultipartOutcome::Unsupported { operation });
                }
            },
        };

        let total_parts = part_count(request.object_len, active.part_size)?;
        let listed_parts = match list_parts::list_all_parts(
            http,
            request.provider,
            bucket,
            &active.object_key,
            &active.upload_id,
        )
        .await
        {
            Ok(parts) => parts,
            Err(list_parts::MultipartFailure::NoSuchUpload { .. })
                if active.from_valid_manifest =>
            {
                match recover_lost_complete(
                    bucket,
                    &active.object_key,
                    request.object_len,
                    request.head_recovery,
                )
                .await?
                {
                    HeadDecision::Recovered(evidence) => {
                        return Ok(MultipartOutcome::Completed(CompletedUpload {
                            object_key: active.object_key,
                            evidence,
                            manifest_path: request.manifest_path.map(Path::to_path_buf),
                        }));
                    }
                    HeadDecision::Replace => {
                        remove_manifest_for_replacement(request.manifest_path).await?;
                        claim_replacement_budget(
                            &mut replacement_used,
                            MultipartOperation::ListParts,
                        )?;
                        continue;
                    }
                }
            }
            Err(list_parts::MultipartFailure::InvalidInventory) => {
                replace_corrupt_session(
                    bucket,
                    request.provider,
                    &active,
                    request.manifest_path,
                    &mut replacement_used,
                    MultipartOperation::ListParts,
                )
                .await?;
                continue;
            }
            Err(failure) => {
                return finish_multipart_failure(
                    bucket,
                    request.provider,
                    &active,
                    request.manifest_path,
                    failure,
                )
                .await;
            }
        };
        let mut parts = match reconcile_parts(request.object_len, active.part_size, listed_parts) {
            Ok(parts) => parts,
            Err(_) => {
                replace_corrupt_session(
                    bucket,
                    request.provider,
                    &active,
                    request.manifest_path,
                    &mut replacement_used,
                    MultipartOperation::ListParts,
                )
                .await?;
                continue;
            }
        };
        let mut chunk = Vec::new();
        // Verify the fingerprint and pin each part's bytes before sending any
        // data. A later file mutation must not poison a resumable session, even
        // if this attempt is interrupted before Complete.
        let fingerprints = async {
            request.reader.rewind().await?;
            let mut digest = Sha256::new();
            let mut fingerprints = Vec::<[u8; 32]>::with_capacity(total_parts);
            for part_number in 1..=total_parts as u32 {
                let len = expected_part_size(request.object_len, active.part_size, part_number)
                    .expect("multipart part number is in the upload plan");
                let len = usize::try_from(len).map_err(|_| {
                    crate::Error::Other("multipart part size exceeds platform limits".into())
                })?;
                chunk.resize(len, 0);
                request.reader.read_exact(&mut chunk).await?;
                digest.update(&chunk);
                fingerprints.push(Sha256::digest(&chunk).into());
            }
            let mut extra = [0];
            if request.reader.read(&mut extra).await? != 0
                || hex::encode(digest.finalize()) != request.object_sha256
            {
                return Err(crate::Error::Other(
                    "multipart upload source changed after fingerprinting".into(),
                ));
            }
            Ok::<_, crate::Error>(fingerprints)
        }
        .await;
        let fingerprints = match fingerprints {
            Ok(fingerprints) => fingerprints,
            Err(error) => {
                return finish_multipart_failure(
                    bucket,
                    request.provider,
                    &active,
                    request.manifest_path,
                    list_parts::MultipartFailure::Client(error),
                )
                .await
            }
        };
        for (index, fingerprint) in fingerprints.iter().enumerate() {
            let part_number = index as u32 + 1;
            if parts.contains_key(&part_number) {
                continue;
            }
            let part_size = expected_part_size(request.object_len, active.part_size, part_number)
                .expect("multipart part number is in the upload plan")
                as usize;
            chunk.resize(part_size, 0);
            let start = u64::from(part_number - 1) * active.part_size;
            let read = async {
                request.reader.seek(std::io::SeekFrom::Start(start)).await?;
                request.reader.read_exact(&mut chunk).await?;
                if <[u8; 32]>::from(Sha256::digest(&chunk)) != *fingerprint {
                    return Err(crate::Error::Other(
                        "multipart upload source changed after fingerprinting".into(),
                    ));
                }
                Ok::<_, crate::Error>(())
            }
            .await;
            if let Err(error) = read {
                return finish_multipart_failure(
                    bucket,
                    request.provider,
                    &active,
                    request.manifest_path,
                    list_parts::MultipartFailure::Client(error),
                )
                .await;
            }
            let mut retries = 0;
            let etag = loop {
                match upload_part(
                    bucket,
                    &active.object_key,
                    &active.upload_id,
                    part_number,
                    &chunk,
                    request.content_type,
                )
                .await
                {
                    Ok(etag) => break etag,
                    Err(failure) if retries < 2 => {
                        retries += 1;
                        tracing::warn!(part_number, retries, error = %failure, "Retrying multipart part");
                    }
                    Err(failure) => {
                        return finish_multipart_failure(
                            bucket,
                            request.provider,
                            &active,
                            request.manifest_path,
                            failure,
                        )
                        .await;
                    }
                }
            };
            parts.insert(part_number, etag);
        }

        let completed_parts = parts
            .into_iter()
            .map(|(part_number, etag)| s3::serde_types::Part {
                part_number,
                etag: quick_xml::escape::escape(&etag).into_owned(),
            })
            .collect();
        // Preserve provider-specific bodies such as ipfS3 DecompressZipResult for our parsers.
        let response = match async {
            use s3::request::Request;

            let command = s3::command::Command::CompleteMultipartUpload {
                upload_id: &active.upload_id,
                data: s3::serde_types::CompleteMultipartUploadData {
                    parts: completed_parts,
                },
            };
            let complete_request = s3::request::tokio_backend::ReqwestRequest::new(
                bucket,
                &active.object_key,
                command,
            )
            .await?;
            complete_request.response_data(false).await
        }
        .await
        {
            Ok(response) => response,
            Err(error) => {
                return finish_multipart_failure(
                    bucket,
                    request.provider,
                    &active,
                    request.manifest_path,
                    list_parts::classify_s3_error(MultipartOperation::Complete, error),
                )
                .await;
            }
        };
        let status = response.status_code();
        if let Err(failure) =
            list_parts::classify_response(MultipartOperation::Complete, status, response.bytes())
        {
            return finish_multipart_failure(
                bucket,
                request.provider,
                &active,
                request.manifest_path,
                failure,
            )
            .await;
        }

        return Ok(MultipartOutcome::Completed(CompletedUpload {
            object_key: active.object_key,
            evidence: CompletionEvidence::Response(response),
            manifest_path: request.manifest_path.map(Path::to_path_buf),
        }));
    }
}

/// Best-effort terminal cleanup for sessions whose local manifests identify the
/// same provider and configured uploader. It never discovers unknown sessions.
pub(crate) async fn abort_upload_state(
    bucket: &s3::Bucket,
    provider: ProviderKind,
    uploader_identity_sha256: &str,
    uploads_dir: &Path,
) -> Result<(), crate::UploadStateError> {
    let mut directory = match tokio::fs::read_dir(uploads_dir).await {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(crate::UploadStateError::Io(error.kind())),
    };
    let mut manifest_paths = Vec::new();
    let mut first_error = None;
    loop {
        let entry = match directory.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                first_error = Some(crate::UploadStateError::Io(error.kind()));
                break;
            }
        };
        match entry.file_type().await {
            Ok(file_type) if file_type.is_file() => {}
            Ok(_) => continue,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(crate::UploadStateError::Io(error.kind()));
                }
                continue;
            }
        }
        let path = entry.path();
        if manifest::is_terminal_abort_manifest_candidate(&path) {
            manifest_paths.push((manifest::is_temporary_manifest(&path), path));
        }
    }
    manifest_paths.sort_unstable_by(|(_, left), (_, right)| left.cmp(right));

    for (is_temporary, path) in manifest_paths {
        let manifest =
            match manifest::load_terminal_abort_manifest(&path, provider, uploader_identity_sha256)
                .await
            {
                Ok(manifest::TerminalAbortManifestLoad::Verified(manifest)) => manifest,
                Ok(manifest::TerminalAbortManifestLoad::Unverifiable(reason)) => {
                    if first_error.is_none() {
                        first_error = Some(crate::UploadStateError::Manifest(reason));
                    }
                    continue;
                }
                Ok(_) => {
                    if is_temporary && first_error.is_none() {
                        first_error = Some(crate::UploadStateError::Manifest(
                            TEMPORARY_MANIFEST_VERIFICATION_ERROR,
                        ));
                    }
                    continue;
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    continue;
                }
            };
        if let Err(error) =
            abort_session(bucket, provider, &manifest.object_key, &manifest.upload_id).await
        {
            if first_error.is_none() {
                first_error = Some(match error {
                    list_parts::MultipartFailure::Service { status, .. }
                    | list_parts::MultipartFailure::Unsupported { status, .. } => {
                        crate::UploadStateError::AbortHttp(status)
                    }
                    list_parts::MultipartFailure::Client(crate::Error::Http(error))
                        if error.is_timeout() =>
                    {
                        crate::UploadStateError::AbortTimeout
                    }
                    list_parts::MultipartFailure::Client(_) => {
                        crate::UploadStateError::AbortTransport
                    }
                    _ => crate::UploadStateError::AbortProtocol,
                });
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn upload_part(
    bucket: &s3::Bucket,
    key: &str,
    upload_id: &str,
    part_number: u32,
    chunk: &[u8],
    content_type: &str,
) -> Result<String, list_parts::MultipartFailure> {
    use s3::request::Request;

    let command = s3::command::Command::PutObject {
        content: chunk,
        content_type,
        custom_headers: None,
        multipart: Some(s3::command::Multipart::new(part_number, upload_id)),
    };
    let request = s3::request::tokio_backend::ReqwestRequest::new(bucket, key, command)
        .await
        .map_err(|error| list_parts::classify_s3_error(MultipartOperation::UploadPart, error))?;
    let response = request
        .response()
        .await
        .map_err(|error| list_parts::classify_s3_error(MultipartOperation::UploadPart, error))?;
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let body = response.bytes().await.map_err(|error| {
        list_parts::MultipartFailure::Client(crate::Error::Http(error.without_url()))
    })?;
    list_parts::classify_response(MultipartOperation::UploadPart, status, &body)?;
    match etag {
        Some(etag) if !etag.trim().is_empty() => Ok(etag),
        _ => Err(list_parts::MultipartFailure::Protocol),
    }
}

async fn create_session(
    bucket: &s3::Bucket,
    object_key: &str,
    content_type: &str,
    create_extension: &CreateExtension,
    manifest_path: Option<&Path>,
    manifest_identity: &manifest::ManifestIdentity<'_>,
    part_size: u64,
) -> crate::Result<CreateSessionResult> {
    part_count(manifest_identity.object_len, part_size)?;
    let mut create_bucket = bucket.clone();
    if manifest_identity.provider == ProviderKind::IpfS3 {
        // ipfS3 restores Content-Length from this signed header when a proxy
        // changes the empty Init request to chunked. Keep it off body-bearing requests.
        create_bucket.extra_headers.insert(
            HeaderName::from_static("x-amz-decoded-content-length"),
            HeaderValue::from_static("0"),
        );
    }
    if let CreateExtension::IpfS3DecompressZip { .. } = create_extension {
        let archive_stem = object_key.strip_suffix(".zip").ok_or_else(|| {
            crate::Error::Other("decompress-zip multipart object key must end in .zip".to_owned())
        })?;
        create_bucket.add_query("decompress-zip", &format!("{archive_stem}/"));
        create_bucket.extra_headers.insert(
            HeaderName::from_static("x-amz-tagging"),
            HeaderValue::from_static(IPFS3_DECOMPRESS_TAGGING),
        );
    }
    let create = create_bucket
        .initiate_multipart_upload(object_key, content_type)
        .await;
    let created = match create {
        Ok(created) => created,
        Err(error) => {
            return match list_parts::classify_s3_error(MultipartOperation::Create, error) {
                list_parts::MultipartFailure::Unsupported { operation, .. } => {
                    Ok(CreateSessionResult::Unsupported(operation))
                }
                failure => Err(crate::Error::from(failure)),
            };
        }
    };
    if created.key != object_key || created.upload_id.trim().is_empty() {
        if !created.upload_id.trim().is_empty() {
            let abort_key = if created.key.trim().is_empty() {
                object_key
            } else {
                created.key.as_str()
            };
            let _ = abort_session(
                bucket,
                manifest_identity.provider,
                abort_key,
                &created.upload_id,
            )
            .await;
        }
        return Err(crate::Error::Other(
            "multipart create response did not identify the requested upload".to_owned(),
        ));
    }
    if let Some(path) = manifest_path {
        let persistence = async {
            let manifest = manifest::new_manifest(
                manifest_identity,
                object_key.to_owned(),
                created.upload_id.clone(),
                part_size,
            )?;
            manifest::write_manifest_atomic(path, &manifest).await
        }
        .await;
        if let Err(error) = persistence {
            let _ = abort_session(
                bucket,
                manifest_identity.provider,
                object_key,
                &created.upload_id,
            )
            .await;
            return Err(error);
        }
    }
    Ok(CreateSessionResult::Active(ActiveSession {
        object_key: object_key.to_owned(),
        upload_id: created.upload_id,
        from_valid_manifest: false,
        part_size,
    }))
}

async fn finish_multipart_failure(
    bucket: &s3::Bucket,
    provider: ProviderKind,
    active: &ActiveSession,
    manifest_path: Option<&Path>,
    failure: list_parts::MultipartFailure,
) -> crate::Result<MultipartOutcome> {
    if let Some(path) = manifest_path {
        if matches!(failure, list_parts::MultipartFailure::Unsupported { .. }) {
            abort_session(bucket, provider, &active.object_key, &active.upload_id).await?;
            manifest::remove_manifest(path).await?;
        }
    } else {
        let _ = abort_session(bucket, provider, &active.object_key, &active.upload_id).await;
    }
    multipart_failure_outcome(failure)
}

async fn replace_corrupt_session(
    bucket: &s3::Bucket,
    provider: ProviderKind,
    active: &ActiveSession,
    manifest_path: Option<&Path>,
    replacement_used: &mut bool,
    operation: MultipartOperation,
) -> crate::Result<()> {
    if *replacement_used {
        return Err(replacement_exhausted(operation));
    }
    abort_session(bucket, provider, &active.object_key, &active.upload_id).await?;
    remove_manifest_for_replacement(manifest_path).await?;
    claim_replacement_budget(replacement_used, operation)
}

async fn abort_session(
    bucket: &s3::Bucket,
    provider: ProviderKind,
    key: &str,
    upload_id: &str,
) -> Result<(), list_parts::MultipartFailure> {
    let result: Result<(), s3::error::S3Error> = async {
        if provider == ProviderKind::S3 {
            return bucket.abort_upload(key, upload_id).await;
        }
        use s3::request::Request;

        let request = s3::request::tokio_backend::ReqwestRequest::new(
            bucket,
            key,
            s3::command::Command::AbortMultipartUpload { upload_id },
        )
        .await?;
        let max_retries = u64::from(s3::get_retries());
        let mut retries = 0_u64;
        let response = loop {
            let send: Result<_, s3::error::S3Error> = async {
                let mut headers = request.headers().await?;
                headers.remove(AUTHORIZATION);
                // Proxies can omit Content-Length: 0 on DELETE without using chunked,
                // so ipfS3's decoded-length bridge cannot restore it for verification.
                headers.remove(CONTENT_LENGTH);
                // rust-s3 adds Date after signing; preserve that exclusion when re-signing.
                let date = headers.remove(DATE);
                let authorization = request.authorization(&headers).await?;
                headers.insert(AUTHORIZATION, authorization.parse()?);
                if let Some(date) = date {
                    headers.insert(DATE, date);
                }
                let builder = bucket
                    .http_client()
                    .delete(request.url()?)
                    .headers(headers)
                    .body(Vec::<u8>::new());
                let builder = if let Some(timeout) = bucket.request_timeout() {
                    builder.timeout(timeout)
                } else {
                    builder
                };
                Ok(builder.send().await?)
            }
            .await;
            match send {
                Ok(response) => break response,
                Err(error) if retries == max_retries => return Err(error),
                Err(_) => {
                    // Match rust-s3 response_data's transport retry budget and delay.
                    // HTTP statuses and response-body read errors are handled below.
                    retries += 1;
                    tracing::warn!(retries, "Retrying ipfS3 multipart Abort request");
                    tokio::time::sleep(std::time::Duration::from_secs(retries.pow(2))).await;
                }
            }
        };
        let status = response.status().as_u16();
        let body = response.bytes().await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(s3::error::S3Error::HttpFailWithBody(
                status,
                String::from_utf8(body.to_vec())?,
            ))
        }
    }
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(error) => match list_parts::classify_s3_error(MultipartOperation::Abort, error) {
            list_parts::MultipartFailure::NoSuchUpload { .. } => Ok(()),
            failure => Err(failure),
        },
    }
}

async fn remove_manifest_for_replacement(manifest_path: Option<&Path>) -> crate::Result<()> {
    if let Some(path) = manifest_path {
        manifest::remove_manifest(path).await?;
    }
    Ok(())
}

async fn recover_lost_complete(
    bucket: &s3::Bucket,
    object_key: &str,
    expected_len: u64,
    recovery: HeadRecovery,
) -> crate::Result<HeadDecision> {
    if recovery == HeadRecovery::Never {
        return Ok(HeadDecision::Replace);
    }
    let (head, status) = bucket.head_object(object_key).await.map_err(|error| {
        crate::Error::from(list_parts::classify_s3_error(
            MultipartOperation::Head,
            error,
        ))
    })?;
    if status == 404 {
        return Ok(HeadDecision::Replace);
    }
    if !(200..300).contains(&status) {
        return Err(crate::Error::Other(format!(
            "S3 multipart Head service request failed (HTTP {status})"
        )));
    }
    let content_length = head
        .content_length
        .and_then(|length| u64::try_from(length).ok())
        .ok_or_else(|| {
            crate::Error::Other(
                "S3 multipart Head response had no usable content length".to_owned(),
            )
        })?;
    if content_length != expected_len {
        return Err(crate::Error::Other(
            "S3 multipart Head response did not match the expected object length".to_owned(),
        ));
    }
    match recovery {
        HeadRecovery::S3ByLength => {
            Ok(HeadDecision::Recovered(CompletionEvidence::RecoveredHead {
                etag: None,
            }))
        }
        HeadRecovery::IpfS3ImageByLengthAndEtag => {
            let etag = head
                .e_tag
                .filter(|etag| !etag.trim().is_empty())
                .ok_or_else(|| {
                    crate::Error::Other("S3 multipart Head response had no usable ETag".to_owned())
                })?;
            Ok(HeadDecision::Recovered(CompletionEvidence::RecoveredHead {
                etag: Some(etag),
            }))
        }
        HeadRecovery::Never => unreachable!("Never recovery returns before HEAD"),
    }
}

fn claim_replacement_budget(
    replacement_used: &mut bool,
    operation: MultipartOperation,
) -> crate::Result<()> {
    if *replacement_used {
        return Err(replacement_exhausted(operation));
    }
    *replacement_used = true;
    Ok(())
}

fn replacement_exhausted(operation: MultipartOperation) -> crate::Error {
    crate::Error::Other(format!(
        "S3 multipart {operation:?} session replacement exhausted"
    ))
}

impl From<list_parts::MultipartFailure> for crate::Error {
    fn from(failure: list_parts::MultipartFailure) -> Self {
        match failure {
            list_parts::MultipartFailure::Client(error) => error,
            failure => crate::Error::Other(failure.to_string()),
        }
    }
}

fn is_safe_session_value(value: &str) -> bool {
    !value.trim().is_empty() && !value.contains('\0')
}

fn multipart_failure_outcome(
    failure: list_parts::MultipartFailure,
) -> crate::Result<MultipartOutcome> {
    match failure {
        list_parts::MultipartFailure::Unsupported { operation, .. }
            if operation != MultipartOperation::UploadPart =>
        {
            Ok(MultipartOutcome::Unsupported { operation })
        }
        failure => Err(crate::Error::from(failure)),
    }
}

#[cfg(test)]
mod tests;
