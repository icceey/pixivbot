use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

pub(crate) const PART_SIZE: usize = 8 * 1024 * 1024;
pub(crate) const MAX_PARTS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    S3,
    IpfS3,
}

pub(crate) fn fingerprint_fields(fields: &[&str]) -> String {
    let mut hash = Sha256::new();
    for field in fields {
        hash.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
        hash.update(field.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn requested_entries_fingerprint(entry_names: &[String]) -> String {
    let fields: Vec<_> = entry_names.iter().map(String::as_str).collect();
    fingerprint_fields(&fields)
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

pub(crate) struct MultipartUploadRequest<'a> {
    pub provider: ProviderKind,
    pub uploader_identity_sha256: &'a str,
    pub candidate_object_key: String,
    pub logical_object_id: &'a str,
    pub bytes: &'a [u8],
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
}

enum CreateSessionResult {
    Active(ActiveSession),
    Unsupported(MultipartOperation),
}

enum HeadDecision {
    Recovered(CompletionEvidence),
    Replace,
}

fn part_count(len: usize) -> crate::Result<usize> {
    if len == 0 {
        return Err(crate::Error::Other(
            "multipart upload object is empty".into(),
        ));
    }
    let count = len.div_ceil(PART_SIZE);
    if count > MAX_PARTS {
        return Err(crate::Error::Other(format!(
            "multipart upload needs {count} parts, exceeding the 10000-part limit"
        )));
    }
    Ok(count)
}

fn expected_part_size(object_len: usize, part_number: u32) -> Option<usize> {
    let part_number = usize::try_from(part_number).ok()?;
    let start = part_number.checked_sub(1)?.checked_mul(PART_SIZE)?;
    (start < object_len).then(|| (object_len - start).min(PART_SIZE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileInvalid {
    DuplicatePartNumber,
    InvalidPartNumber,
    EmptyEtag,
    InvalidPartSize,
}

fn reconcile_parts(
    object_len: usize,
    listed: Vec<list_parts::CompletedPart>,
) -> Result<BTreeMap<u32, String>, ReconcileInvalid> {
    let total_parts = part_count(object_len).map_err(|_| ReconcileInvalid::InvalidPartNumber)?;
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
            != expected_part_size(object_len, part.part_number)
                .ok_or(ReconcileInvalid::InvalidPartNumber)? as u64
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
    request: MultipartUploadRequest<'_>,
) -> crate::Result<MultipartOutcome> {
    let total_parts = part_count(request.bytes.len())?;
    let object_sha256 = sha256_hex(request.bytes);
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
        object_sha256: &object_sha256,
        object_len: request.bytes.len() as u64,
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
            }),
            manifest::ManifestLoad::Missing => None,
            manifest::ManifestLoad::MalformedJson => {
                manifest::remove_manifest(path).await?;
                None
            }
            manifest::ManifestLoad::Stale { manifest, .. }
                if manifest.uploader_identity_sha256 != request.uploader_identity_sha256 =>
            {
                manifest::remove_manifest(path).await?;
                None
            }
            manifest::ManifestLoad::Stale { manifest, .. } => {
                if !is_safe_session_value(&manifest.object_key)
                    || !is_safe_session_value(&manifest.upload_id)
                {
                    return Err(crate::Error::Other(
                        "multipart stale manifest cannot be safely replaced".to_owned(),
                    ));
                }
                abort_for_replacement(bucket, &manifest.object_key, &manifest.upload_id).await?;
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
            )
            .await?
            {
                CreateSessionResult::Active(session) => session,
                CreateSessionResult::Unsupported(operation) => {
                    return Ok(MultipartOutcome::Unsupported { operation });
                }
            },
        };

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
                    request.bytes.len(),
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
            Err(failure @ list_parts::MultipartFailure::InvalidInventory(_)) => {
                replace_corrupt_session(
                    bucket,
                    &active,
                    request.manifest_path,
                    &mut replacement_used,
                    MultipartOperation::ListParts,
                )
                .await?;
                drop(failure);
                continue;
            }
            Err(failure) => {
                return finish_multipart_failure(bucket, &active, request.manifest_path, failure)
                    .await;
            }
        };
        let mut parts = match reconcile_parts(request.bytes.len(), listed_parts) {
            Ok(parts) => parts,
            Err(_) => {
                replace_corrupt_session(
                    bucket,
                    &active,
                    request.manifest_path,
                    &mut replacement_used,
                    MultipartOperation::ListParts,
                )
                .await?;
                continue;
            }
        };
        for part_number in 1..=u32::try_from(total_parts).expect("multipart part count is bounded")
        {
            if parts.contains_key(&part_number) {
                continue;
            }
            let part_size = expected_part_size(request.bytes.len(), part_number)
                .expect("multipart part number is in the upload plan");
            let start = (part_number as usize - 1) * PART_SIZE;
            let chunk = &request.bytes[start..start + part_size];
            let etag = match upload_part(
                bucket,
                &active.object_key,
                &active.upload_id,
                part_number,
                chunk,
                request.content_type,
            )
            .await
            {
                Ok(etag) => etag,
                Err(failure) => {
                    return finish_multipart_failure(
                        bucket,
                        &active,
                        request.manifest_path,
                        failure,
                    )
                    .await;
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
        let response = match bucket
            .complete_multipart_upload(&active.object_key, &active.upload_id, completed_parts)
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return finish_multipart_failure(
                    bucket,
                    &active,
                    request.manifest_path,
                    list_parts::classify_s3_error(MultipartOperation::Complete, error),
                )
                .await;
            }
        };
        let status = response.status_code();
        if let Some(failure) = list_parts::classify_embedded_s3_error(
            MultipartOperation::Complete,
            status,
            response.bytes(),
        ) {
            return finish_multipart_failure(bucket, &active, request.manifest_path, failure).await;
        }
        if !(200..300).contains(&status) {
            let failure = list_parts::classify_response(
                MultipartOperation::Complete,
                status,
                response.bytes(),
            )
            .expect_err("non-success multipart complete response must be classified as a failure");
            return finish_multipart_failure(bucket, &active, request.manifest_path, failure).await;
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
) -> crate::Result<()> {
    let mut directory = match tokio::fs::read_dir(uploads_dir).await {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut manifest_paths = Vec::new();
    let mut first_error = None;
    loop {
        let entry = match directory.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                first_error = Some(crate::Error::Io(error));
                break;
            }
        };
        match entry.file_type().await {
            Ok(file_type) if file_type.is_file() => {}
            Ok(_) => continue,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error.into());
                }
                continue;
            }
        }
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            manifest_paths.push(path);
        }
    }
    manifest_paths.sort_unstable();

    for path in manifest_paths {
        let manifest =
            match manifest::load_terminal_abort_manifest(&path, provider, uploader_identity_sha256)
                .await
            {
                Ok(Some(manifest)) => manifest,
                Ok(None) => continue,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    continue;
                }
            };
        if let Err(error) =
            abort_for_replacement(bucket, &manifest.object_key, &manifest.upload_id).await
        {
            if first_error.is_none() {
                first_error = Some(error);
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
        _ => Err(list_parts::MultipartFailure::Protocol(
            "successful S3 multipart UploadPart response had no usable ETag".to_owned(),
        )),
    }
}

async fn create_session(
    bucket: &s3::Bucket,
    object_key: &str,
    content_type: &str,
    create_extension: &CreateExtension,
    manifest_path: Option<&Path>,
    manifest_identity: &manifest::ManifestIdentity<'_>,
) -> crate::Result<CreateSessionResult> {
    let create = match create_extension {
        CreateExtension::None => {
            bucket
                .initiate_multipart_upload(object_key, content_type)
                .await
        }
        CreateExtension::IpfS3DecompressZip { .. } => {
            let archive_stem = object_key.strip_suffix(".zip").ok_or_else(|| {
                crate::Error::Other(
                    "decompress-zip multipart object key must end in .zip".to_owned(),
                )
            })?;
            let mut create_bucket = bucket.clone();
            create_bucket.add_query("decompress-zip", &format!("{archive_stem}/"));
            create_bucket
                .initiate_multipart_upload(object_key, content_type)
                .await
        }
    };
    let created = match create {
        Ok(created) => created,
        Err(error) => {
            return match list_parts::classify_s3_error(MultipartOperation::Create, error) {
                list_parts::MultipartFailure::Unsupported { operation, .. } => {
                    Ok(CreateSessionResult::Unsupported(operation))
                }
                failure => Err(multipart_failure_error(failure)),
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
            best_effort_abort(bucket, abort_key, &created.upload_id).await;
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
            )?;
            manifest::write_manifest_atomic(path, &manifest).await
        }
        .await;
        if let Err(error) = persistence {
            best_effort_abort(bucket, object_key, &created.upload_id).await;
            return Err(error);
        }
    }
    Ok(CreateSessionResult::Active(ActiveSession {
        object_key: object_key.to_owned(),
        upload_id: created.upload_id,
        from_valid_manifest: false,
    }))
}

async fn finish_multipart_failure(
    bucket: &s3::Bucket,
    active: &ActiveSession,
    manifest_path: Option<&Path>,
    failure: list_parts::MultipartFailure,
) -> crate::Result<MultipartOutcome> {
    if matches!(failure, list_parts::MultipartFailure::Unsupported { .. }) {
        best_effort_abort(bucket, &active.object_key, &active.upload_id).await;
        if let Some(path) = manifest_path {
            manifest::remove_manifest(path).await?;
        }
    } else if manifest_path.is_none() {
        best_effort_abort(bucket, &active.object_key, &active.upload_id).await;
    }
    multipart_failure_outcome(failure)
}

async fn replace_corrupt_session(
    bucket: &s3::Bucket,
    active: &ActiveSession,
    manifest_path: Option<&Path>,
    replacement_used: &mut bool,
    operation: MultipartOperation,
) -> crate::Result<()> {
    if *replacement_used {
        return Err(replacement_exhausted(operation));
    }
    abort_for_replacement(bucket, &active.object_key, &active.upload_id).await?;
    remove_manifest_for_replacement(manifest_path).await?;
    claim_replacement_budget(replacement_used, operation)
}

async fn best_effort_abort(bucket: &s3::Bucket, key: &str, upload_id: &str) {
    let _ = bucket.abort_upload(key, upload_id).await;
}

async fn abort_for_replacement(
    bucket: &s3::Bucket,
    key: &str,
    upload_id: &str,
) -> crate::Result<()> {
    match bucket.abort_upload(key, upload_id).await {
        Ok(_) => Ok(()),
        Err(error) => match list_parts::classify_s3_error(MultipartOperation::Abort, error) {
            list_parts::MultipartFailure::NoSuchUpload { .. } => Ok(()),
            failure => Err(multipart_failure_error(failure)),
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
    expected_len: usize,
    recovery: HeadRecovery,
) -> crate::Result<HeadDecision> {
    let (head, status) = bucket.head_object(object_key).await.map_err(|error| {
        multipart_failure_error(list_parts::classify_s3_error(
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
    let expected_len = u64::try_from(expected_len).unwrap_or(u64::MAX);
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
        HeadRecovery::Never => Ok(HeadDecision::Replace),
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

fn multipart_failure_error(failure: list_parts::MultipartFailure) -> crate::Error {
    match failure {
        list_parts::MultipartFailure::Client(error) => error,
        failure => crate::Error::Other(failure.to_string()),
    }
}

fn is_safe_session_value(value: &str) -> bool {
    !value.trim().is_empty() && !value.contains('\0')
}

fn multipart_failure_outcome(
    failure: list_parts::MultipartFailure,
) -> crate::Result<MultipartOutcome> {
    match failure {
        list_parts::MultipartFailure::Unsupported { operation, .. } => {
            Ok(MultipartOutcome::Unsupported { operation })
        }
        failure => Err(multipart_failure_error(failure)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s3::creds::Credentials;
    use s3::{Bucket, Region};
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;
    use wiremock::matchers::{any, method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const BUCKET: &str = "task-five-bucket";
    const KEY: &str = "objects/archive.bin";
    const UPLOAD_ID: &str = "upload-id";
    const REPLACEMENT_UPLOAD_ID: &str = "replacement-upload-id";
    const UPLOADER_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Clone)]
    struct CreateManifestParentFile {
        manifest_parent: std::path::PathBuf,
    }

    impl wiremock::Respond for CreateManifestParentFile {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            std::fs::write(&self.manifest_parent, b"file").unwrap();
            ResponseTemplate::new(200).set_body_string(format!(
                "<InitiateMultipartUploadResult><Bucket>{BUCKET}</Bucket><Key>{KEY}</Key><UploadId>{UPLOAD_ID}</UploadId></InitiateMultipartUploadResult>"
            ))
        }
    }

    #[tokio::test]
    async fn multipart_rejects_more_than_ten_thousand_parts_before_create() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let error = part_count(MAX_PARTS * PART_SIZE + 1).unwrap_err();
        assert_eq!(
            error.to_string(),
            "multipart upload needs 10001 parts, exceeding the 10000-part limit"
        );
        server.verify().await;
    }

    #[test]
    fn reconcile_parts_requires_the_exact_server_inventory() {
        let object_len = 2 * PART_SIZE + 17;
        let part = |part_number, etag: &str, size| list_parts::CompletedPart {
            part_number,
            etag: etag.to_owned(),
            size,
        };
        let reconciled = reconcile_parts(
            object_len,
            vec![
                part(2, "\"part-2\"", PART_SIZE as u64),
                part(1, "\"part-1\"", PART_SIZE as u64),
                part(3, "\"part-3\"", 17),
            ],
        )
        .unwrap();
        assert_eq!(
            reconciled.into_iter().collect::<Vec<_>>(),
            vec![
                (1, "\"part-1\"".to_owned()),
                (2, "\"part-2\"".to_owned()),
                (3, "\"part-3\"".to_owned()),
            ]
        );

        let cases = [
            (
                vec![
                    part(1, "\"one\"", PART_SIZE as u64),
                    part(1, "\"two\"", PART_SIZE as u64),
                ],
                ReconcileInvalid::DuplicatePartNumber,
            ),
            (
                vec![part(0, "\"zero\"", PART_SIZE as u64)],
                ReconcileInvalid::InvalidPartNumber,
            ),
            (
                vec![part(4, "\"four\"", PART_SIZE as u64)],
                ReconcileInvalid::InvalidPartNumber,
            ),
            (
                vec![part(1, " \t", PART_SIZE as u64)],
                ReconcileInvalid::EmptyEtag,
            ),
            (
                vec![part(1, "\"short\"", PART_SIZE as u64 - 1)],
                ReconcileInvalid::InvalidPartSize,
            ),
            (
                vec![part(3, "\"long\"", 18)],
                ReconcileInvalid::InvalidPartSize,
            ),
        ];
        for (listed, expected) in cases {
            assert_eq!(reconcile_parts(object_len, listed), Err(expected));
        }
    }

    #[tokio::test]
    async fn fresh_multipart_creates_lists_uploads_sequential_parts_and_completes_sorted() {
        let server = MockServer::start().await;
        mount_create(&server).await;
        mount_empty_list_parts(&server).await;
        for part_number in 1..=3 {
            Mock::given(method("PUT"))
                .and(query_param("uploadId", UPLOAD_ID))
                .and(query_param("partNumber", part_number.to_string()))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("ETag", format!("\"part-{part_number}\"")),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        mount_complete(&server).await;

        let bytes = vec![7; 2 * PART_SIZE + 17];
        let result = upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, None),
        )
        .await
        .unwrap();
        assert!(matches!(result, MultipartOutcome::Completed(_)));

        let requests = server.received_requests().await.unwrap();
        assert_request_sequence(&requests, &["POST", "GET", "PUT", "PUT", "PUT", "POST"]);
        assert_eq!(
            query_value(&requests[1], "uploadId").as_deref(),
            Some(UPLOAD_ID)
        );
        assert_eq!(
            query_value(&requests[2], "partNumber").as_deref(),
            Some("1")
        );
        assert_eq!(
            query_value(&requests[3], "partNumber").as_deref(),
            Some("2")
        );
        assert_eq!(
            query_value(&requests[4], "partNumber").as_deref(),
            Some("3")
        );
        assert_eq!(
            complete_parts(&requests[5]),
            vec![
                (1, "\"part-1\"".to_owned()),
                (2, "\"part-2\"".to_owned()),
                (3, "\"part-3\"".to_owned()),
            ]
        );
        assert_no_decompress_query(&requests);
        server.verify().await;
    }

    #[tokio::test]
    async fn ipfs_cid_etags_are_xml_escaped_only_when_complete_is_serialized() {
        let server = MockServer::start().await;
        mount_create(&server).await;
        mount_empty_list_parts(&server).await;
        for (part_number, etag) in [(1, "\"bafy&one\""), (2, "\"bafy<two>\"")] {
            Mock::given(method("PUT"))
                .and(query_param("uploadId", UPLOAD_ID))
                .and(query_param("partNumber", part_number.to_string()))
                .respond_with(ResponseTemplate::new(200).insert_header("ETag", etag))
                .expect(1)
                .mount(&server)
                .await;
        }
        mount_complete(&server).await;

        let bytes = vec![3; PART_SIZE + 1];
        let result = upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, None),
        )
        .await
        .unwrap();
        assert!(matches!(result, MultipartOutcome::Completed(_)));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            complete_parts(requests.last().unwrap()),
            vec![
                (1, "\"bafy&one\"".to_owned()),
                (2, "\"bafy<two>\"".to_owned()),
            ]
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn manifest_persistence_failure_aborts_created_session_before_uploading_parts() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let manifest_parent = temp.path().join("not-a-directory");
        let manifest_path = manifest_parent.join("manifest.json");
        Mock::given(method("POST"))
            .and(query_param("uploads", ""))
            .respond_with(CreateManifestParentFile {
                manifest_parent: manifest_parent.clone(),
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(query_param("uploadId", UPLOAD_ID))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let bytes = vec![5; PART_SIZE];
        let error = match upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, Some(&manifest_path)),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("persistence failure must not complete the upload"),
        };
        assert!(matches!(error, crate::Error::Io(_)));
        assert!(!manifest_path.exists());

        let requests = server.received_requests().await.unwrap();
        assert_request_sequence(&requests, &["POST", "DELETE"]);
        assert_no_decompress_query(&requests);
        server.verify().await;
    }

    #[tokio::test]
    async fn multipart_restart_lists_server_parts_and_uploads_only_the_missing_tail() {
        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("archive.json");
        let bytes = vec![7; 2 * PART_SIZE + 17];
        let server = RawMultipartServer::start(bytes.len(), 3, vec![1, 2]);
        let first = upload_multipart(
            test_bucket_for_endpoint(&server.endpoint).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, Some(&manifest_path)),
        )
        .await;
        assert!(first.is_err());
        assert!(manifest_path.is_file());
        let first_request_count = server.requests().len();

        let mut resumed_request = standard_request(&bytes, Some(&manifest_path));
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
                    request.method == "POST"
                        && raw_query_value(&request.target, "uploads").is_some()
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
        assert_eq!(completed.object_key, KEY);
        assert!(manifest_path.is_file());

        completed.remove_manifest().await.unwrap();
        assert!(!manifest_path.exists());
    }

    #[tokio::test]
    async fn accepted_part_with_lost_response_is_not_retransmitted_when_listed() {
        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("archive.json");
        let bytes = vec![9; PART_SIZE];
        let server = RawMultipartServer::start(bytes.len(), 1, vec![1]);
        let first = upload_multipart(
            test_bucket_for_endpoint(&server.endpoint).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, Some(&manifest_path)),
        )
        .await;
        assert!(first.is_err());
        assert!(manifest_path.is_file());

        let mut resumed_request = standard_request(&bytes, Some(&manifest_path));
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
        assert_eq!(
            requests
                .iter()
                .filter(|request| {
                    request.method == "PUT"
                        && raw_query_value(&request.target, "partNumber") == Some("1")
                })
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .find(|request| request.method == "PUT")
                .map(|request| request.body.len()),
            Some(PART_SIZE)
        );
        assert_eq!(
            complete_parts_body(
                &requests
                    .iter()
                    .find(|request| {
                        request.method == "POST"
                            && raw_query_value(&request.target, "uploadId").is_some()
                    })
                    .unwrap()
                    .body,
            ),
            vec![(1, "\"part-1\"".to_owned())]
        );
        assert_eq!(completed.object_key, KEY);
    }

    #[tokio::test]
    async fn local_manifest_never_skips_a_part_absent_from_list_parts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(query_param("uploads", ""))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        mount_empty_list_parts(&server).await;
        for part_number in 1..=2 {
            Mock::given(method("PUT"))
                .and(query_param("uploadId", UPLOAD_ID))
                .and(query_param("partNumber", part_number.to_string()))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("ETag", format!("\"part-{part_number}\"")),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        mount_complete(&server).await;

        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("archive.json");
        let bytes = vec![5; PART_SIZE + 1];
        let object_sha256 = sha256_hex(&bytes);
        let identity = manifest::ManifestIdentity {
            provider: ProviderKind::S3,
            uploader_identity_sha256: UPLOADER_ID,
            logical_object_id: "gallery-123",
            object_sha256: &object_sha256,
            object_len: bytes.len() as u64,
            content_type: "application/octet-stream",
            requested_entries_sha256: None,
        };
        let manifest =
            manifest::new_manifest(&identity, KEY.to_owned(), UPLOAD_ID.to_owned()).unwrap();
        manifest::write_manifest_atomic(&manifest_path, &manifest)
            .await
            .unwrap();
        assert!(!tokio::fs::read_to_string(&manifest_path)
            .await
            .unwrap()
            .contains("etag"));

        let mut request = standard_request(&bytes, Some(&manifest_path));
        request.candidate_object_key = "objects/different-retry-key.bin".to_owned();
        let result = upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            request,
        )
        .await
        .unwrap();
        assert!(matches!(result, MultipartOutcome::Completed(_)));

        let requests = server.received_requests().await.unwrap();
        assert_request_sequence(&requests, &["GET", "PUT", "PUT", "POST"]);
        assert_eq!(
            complete_parts(requests.last().unwrap()),
            vec![(1, "\"part-1\"".to_owned()), (2, "\"part-2\"".to_owned()),]
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn multipart_only_classifies_explicit_operation_codes_as_unsupported() {
        for operation in [
            MultipartOperation::Create,
            MultipartOperation::ListParts,
            MultipartOperation::UploadPart,
            MultipartOperation::Complete,
        ] {
            for code in ["NotImplemented", "UnsupportedOperation", "MethodNotAllowed"] {
                assert_explicit_unsupported(operation, code).await;
            }
        }
    }

    #[tokio::test]
    async fn multipart_transient_auth_and_malformed_failures_never_become_unsupported() {
        let cases = vec![
            (400, b"raw bad request".to_vec()),
            (403, b"raw forbidden".to_vec()),
            (405, b"raw method not allowed".to_vec()),
            (403, s3_error_xml("AccessDenied").into_bytes()),
            (403, s3_error_xml("InvalidAccessKeyId").into_bytes()),
            (500, s3_error_xml("NotImplemented").into_bytes()),
            (503, b"raw unavailable".to_vec()),
            (200, b"<Error><Code>AccessDenied</Code>".to_vec()),
        ];
        for operation in [
            MultipartOperation::Create,
            MultipartOperation::ListParts,
            MultipartOperation::UploadPart,
            MultipartOperation::Complete,
        ] {
            for (status, body) in &cases {
                let failure = list_parts::classify_response(operation, *status, body).unwrap_err();
                assert!(
                    !matches!(failure, list_parts::MultipartFailure::Unsupported { .. }),
                    "{operation:?} HTTP {status} must not fall back"
                );
            }
        }

        let server = MockServer::start().await;
        mount_create(&server).await;
        mount_empty_list_parts(&server).await;
        Mock::given(method("PUT"))
            .and(query_param("uploadId", UPLOAD_ID))
            .and(query_param("partNumber", "1"))
            .respond_with(ResponseTemplate::new(200).insert_header("ETag", " \t "))
            .expect(1)
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
        assert!(matches!(
            list_parts::classify_embedded_s3_error(
                MultipartOperation::Complete,
                200,
                s3_error_xml("AccessDenied").as_bytes(),
            ),
            Some(list_parts::MultipartFailure::Service {
                operation: MultipartOperation::Complete,
                status: 200,
                code: Some(ref code),
            }) if code == "AccessDenied"
        ));
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
    async fn zip_head_never_guesses_entry_cids_and_starts_one_replacement() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("uploadId", UPLOAD_ID))
            .respond_with(ResponseTemplate::new(404).set_body_string(s3_error_xml("NoSuchUpload")))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", PART_SIZE)
                    .insert_header("ETag", "\"archive-cid-must-not-recover\""),
            )
            .expect(1)
            .mount(&server)
            .await;
        mount_create_with_upload_id(&server, REPLACEMENT_UPLOAD_ID).await;
        mount_empty_list_parts_for(&server, REPLACEMENT_UPLOAD_ID).await;
        mount_upload_part_for(&server, REPLACEMENT_UPLOAD_ID, 1, "\"replacement-part\"").await;
        mount_complete_for(&server, REPLACEMENT_UPLOAD_ID).await;

        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("archive.json");
        let bytes = vec![4; PART_SIZE];
        write_valid_manifest(&manifest_path, &bytes, UPLOADER_ID).await;
        let result = upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, Some(&manifest_path)),
        )
        .await
        .unwrap();
        let completed = match result {
            MultipartOutcome::Completed(completed) => completed,
            MultipartOutcome::Unsupported { .. } => panic!("ZIP HEAD must start a replacement"),
        };
        assert!(matches!(
            completed.evidence,
            CompletionEvidence::Response(_)
        ));
        assert!(manifest_path.exists());
        server.verify().await;
    }

    #[tokio::test]
    async fn multipart_never_sends_options() {
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
        mount_complete(&server).await;

        let bytes = vec![4; PART_SIZE];
        let result = upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, None),
        )
        .await;
        assert!(matches!(result, Ok(MultipartOutcome::Completed(_))));
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method.as_str() != "OPTIONS"));
        server.verify().await;
    }

    #[tokio::test]
    async fn missing_valid_session_after_head_404_uses_one_replacement() {
        let server = MockServer::start().await;
        mount_no_such_upload_list(&server, UPLOAD_ID).await;
        mount_head(&server, 404, None, None).await;
        mount_create_with_upload_id(&server, REPLACEMENT_UPLOAD_ID).await;
        mount_empty_list_parts_for(&server, REPLACEMENT_UPLOAD_ID).await;
        mount_upload_part_for(&server, REPLACEMENT_UPLOAD_ID, 1, "\"replacement-part\"").await;
        mount_complete_for(&server, REPLACEMENT_UPLOAD_ID).await;

        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("archive.json");
        let bytes = vec![4; PART_SIZE];
        write_valid_manifest(&manifest_path, &bytes, UPLOADER_ID).await;

        let result = upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, Some(&manifest_path)),
        )
        .await
        .unwrap();
        let completed = match result {
            MultipartOutcome::Completed(completed) => completed,
            MultipartOutcome::Unsupported { .. } => panic!("replacement must remain multipart"),
        };
        assert_eq!(completed.object_key, KEY);
        assert!(manifest_path.exists());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| {
                    request.method.as_str() == "POST" && query_value(request, "uploads").is_some()
                })
                .count(),
            1
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn second_no_such_upload_errors_without_a_third_session() {
        let server = MockServer::start().await;
        mount_no_such_upload_list(&server, UPLOAD_ID).await;
        mount_head(&server, 404, None, None).await;
        mount_create_with_upload_id(&server, REPLACEMENT_UPLOAD_ID).await;
        mount_no_such_upload_list(&server, REPLACEMENT_UPLOAD_ID).await;
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

        let error = match upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, Some(&manifest_path)),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("a second missing session must not create a third session"),
        };
        assert!(error.to_string().contains("ListParts"));
        assert!(manifest_path.exists());
        server.verify().await;
    }

    #[tokio::test]
    async fn invalid_inventory_and_wrong_size_abort_then_replace_once() {
        for parts in [
            vec![
                (1, "\"duplicate-one\"".to_owned(), PART_SIZE as u64),
                (1, "\"duplicate-two\"".to_owned(), PART_SIZE as u64),
            ],
            vec![(1, "\"wrong-size\"".to_owned(), (PART_SIZE - 1) as u64)],
        ] {
            let server = MockServer::start().await;
            mount_list_parts_for(&server, UPLOAD_ID, &parts).await;
            mount_abort_for(&server, UPLOAD_ID).await;
            mount_create_with_upload_id(&server, REPLACEMENT_UPLOAD_ID).await;
            mount_empty_list_parts_for(&server, REPLACEMENT_UPLOAD_ID).await;
            mount_upload_part_for(&server, REPLACEMENT_UPLOAD_ID, 1, "\"replacement-part\"").await;
            mount_complete_for(&server, REPLACEMENT_UPLOAD_ID).await;

            let temp = tempfile::tempdir().unwrap();
            let manifest_path = temp.path().join("archive.json");
            let bytes = vec![4; PART_SIZE];
            write_valid_manifest(&manifest_path, &bytes, UPLOADER_ID).await;
            let result = upload_multipart(
                test_bucket(&server).as_ref(),
                &reqwest::Client::new(),
                standard_request(&bytes, Some(&manifest_path)),
            )
            .await;
            assert!(matches!(result, Ok(MultipartOutcome::Completed(_))));
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn malformed_and_stale_manifests_replace_without_cross_endpoint_abort() {
        let cases = ["malformed", "same-uploader-stale", "uploader-mismatch"];
        for case in cases {
            let server = MockServer::start().await;
            if case == "same-uploader-stale" {
                mount_abort_for(&server, UPLOAD_ID).await;
            } else {
                Mock::given(method("DELETE"))
                    .respond_with(ResponseTemplate::new(500))
                    .expect(0)
                    .mount(&server)
                    .await;
            }
            mount_create_with_upload_id(&server, REPLACEMENT_UPLOAD_ID).await;
            mount_empty_list_parts_for(&server, REPLACEMENT_UPLOAD_ID).await;
            mount_upload_part_for(&server, REPLACEMENT_UPLOAD_ID, 1, "\"replacement-part\"").await;
            mount_complete_for(&server, REPLACEMENT_UPLOAD_ID).await;

            let temp = tempfile::tempdir().unwrap();
            let manifest_path = temp.path().join("archive.json");
            let bytes = vec![4; PART_SIZE];
            match case {
                "malformed" => tokio::fs::write(&manifest_path, b"not json").await.unwrap(),
                "same-uploader-stale" => {
                    let old_bytes = vec![3; PART_SIZE];
                    write_valid_manifest(&manifest_path, &old_bytes, UPLOADER_ID).await
                }
                "uploader-mismatch" => {
                    write_valid_manifest(
                        &manifest_path,
                        &bytes,
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    )
                    .await
                }
                _ => unreachable!(),
            }

            let result = upload_multipart(
                test_bucket(&server).as_ref(),
                &reqwest::Client::new(),
                standard_request(&bytes, Some(&manifest_path)),
            )
            .await;
            assert!(matches!(result, Ok(MultipartOutcome::Completed(_))));
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn replacement_keeps_manifest_when_abort_does_not_confirm_cleanup() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(query_param("uploadId", UPLOAD_ID))
            .respond_with(ResponseTemplate::new(403).set_body_string(s3_error_xml("AccessDenied")))
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
        let old_bytes = vec![3; PART_SIZE];
        write_valid_manifest(&manifest_path, &old_bytes, UPLOADER_ID).await;
        let error = match upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            standard_request(&bytes, Some(&manifest_path)),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("an unconfirmed Abort must prevent replacement"),
        };
        assert!(error.to_string().contains("Abort"));
        assert!(manifest_path.exists());
        server.verify().await;
    }

    #[tokio::test]
    async fn replacement_accepts_no_such_upload_from_abort() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(query_param("uploadId", UPLOAD_ID))
            .respond_with(ResponseTemplate::new(404).set_body_string(s3_error_xml("NoSuchUpload")))
            .expect(1)
            .mount(&server)
            .await;
        mount_create_with_upload_id(&server, REPLACEMENT_UPLOAD_ID).await;
        mount_empty_list_parts_for(&server, REPLACEMENT_UPLOAD_ID).await;
        mount_upload_part_for(&server, REPLACEMENT_UPLOAD_ID, 1, "\"replacement-part\"").await;
        mount_complete_for(&server, REPLACEMENT_UPLOAD_ID).await;

        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("archive.json");
        let bytes = vec![4; PART_SIZE];
        let old_bytes = vec![3; PART_SIZE];
        write_valid_manifest(&manifest_path, &old_bytes, UPLOADER_ID).await;
        assert!(matches!(
            upload_multipart(
                test_bucket(&server).as_ref(),
                &reqwest::Client::new(),
                standard_request(&bytes, Some(&manifest_path)),
            )
            .await,
            Ok(MultipartOutcome::Completed(_))
        ));
        server.verify().await;
    }

    #[tokio::test]
    async fn unsupported_manifest_removal_failure_blocks_fallback() {
        let server = MockServer::start().await;
        mount_abort_for(&server, UPLOAD_ID).await;
        let bucket = test_bucket(&server);
        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("manifest-is-a-directory");
        tokio::fs::create_dir(&manifest_path).await.unwrap();
        let active = ActiveSession {
            object_key: KEY.to_owned(),
            upload_id: UPLOAD_ID.to_owned(),
            from_valid_manifest: true,
        };

        let result = finish_multipart_failure(
            bucket.as_ref(),
            &active,
            Some(&manifest_path),
            list_parts::MultipartFailure::Unsupported {
                operation: MultipartOperation::ListParts,
                status: 405,
                code: "NotImplemented".to_owned(),
            },
        )
        .await;
        assert!(result.is_err());
        assert!(manifest_path.is_dir());
        server.verify().await;
    }

    #[tokio::test]
    async fn ipfs3_image_head_requires_matching_length_and_nonempty_cid_etag() {
        let server = MockServer::start().await;
        mount_no_such_upload_list(&server, UPLOAD_ID).await;
        mount_head(
            &server,
            200,
            Some(PART_SIZE),
            Some("\"bafy-recovered-cid\""),
        )
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
        write_valid_manifest_for(&manifest_path, &bytes, UPLOADER_ID, ProviderKind::IpfS3).await;
        let mut request = standard_request(&bytes, Some(&manifest_path));
        request.provider = ProviderKind::IpfS3;
        request.head_recovery = HeadRecovery::IpfS3ImageByLengthAndEtag;

        let result = upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            request,
        )
        .await
        .unwrap();
        let completed = match result {
            MultipartOutcome::Completed(completed) => completed,
            MultipartOutcome::Unsupported { .. } => panic!("head recovery must complete"),
        };
        assert!(matches!(
            completed.evidence,
            CompletionEvidence::RecoveredHead { ref etag }
                if etag.as_deref() == Some("\"bafy-recovered-cid\"")
        ));
        assert!(manifest_path.exists());
        server.verify().await;
    }

    #[tokio::test]
    async fn head_recovery_rejects_nonrecoverable_responses_and_keeps_manifest() {
        let cases = [
            (403, None, None, HeadRecovery::S3ByLength),
            (500, None, None, HeadRecovery::S3ByLength),
            (200, Some(PART_SIZE - 1), None, HeadRecovery::S3ByLength),
            (200, None, None, HeadRecovery::S3ByLength),
            (
                200,
                Some(PART_SIZE),
                Some(" \t "),
                HeadRecovery::IpfS3ImageByLengthAndEtag,
            ),
        ];
        for (status, content_length, etag, head_recovery) in cases {
            let server = MockServer::start().await;
            mount_no_such_upload_list(&server, UPLOAD_ID).await;
            mount_head(&server, status, content_length, etag).await;
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
            request.head_recovery = head_recovery;

            assert!(upload_multipart(
                test_bucket(&server).as_ref(),
                &reqwest::Client::new(),
                request,
            )
            .await
            .is_err());
            assert!(manifest_path.exists());
            server.verify().await;
        }

        let server = MockServer::start().await;
        mount_no_such_upload_list(&server, UPLOAD_ID).await;
        Mock::given(method("HEAD"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("Content-Length", "9223372036854775808"),
            )
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
        assert!(upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            request,
        )
        .await
        .is_err());
        assert!(manifest_path.exists());
        server.verify().await;
    }

    #[test]
    fn multipart_capability_is_a_process_local_tristate_hint() {
        let capability = MultipartCapability::default();
        assert_eq!(capability.state(), CapabilityState::Unknown);
        capability.mark_supported();
        assert_eq!(capability.state(), CapabilityState::Supported);
        capability.mark_unsupported();
        assert_eq!(capability.state(), CapabilityState::Unsupported);
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

    async fn mount_upload_part_for(
        server: &MockServer,
        upload_id: &str,
        part_number: u32,
        etag: &str,
    ) {
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

    async fn mount_no_such_upload_list(server: &MockServer, upload_id: &str) {
        Mock::given(method("GET"))
            .and(query_param("uploadId", upload_id))
            .and(query_param("part-number-marker", "0"))
            .respond_with(ResponseTemplate::new(404).set_body_string(s3_error_xml("NoSuchUpload")))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_list_parts_for(
        server: &MockServer,
        upload_id: &str,
        parts: &[(u32, String, u64)],
    ) {
        Mock::given(method("GET"))
            .and(query_param("uploadId", upload_id))
            .and(query_param("part-number-marker", "0"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(list_parts_response_for(BUCKET, KEY, upload_id, parts)),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_head(
        server: &MockServer,
        status: u16,
        content_length: Option<usize>,
        etag: Option<&str>,
    ) {
        let mut response = ResponseTemplate::new(status);
        if let Some(content_length) = content_length {
            response = response.insert_header("Content-Length", content_length);
        }
        if let Some(etag) = etag {
            response = response.insert_header("ETag", etag);
        }
        Mock::given(method("HEAD"))
            .respond_with(response)
            .expect(1)
            .mount(server)
            .await;
    }

    async fn assert_explicit_unsupported(operation: MultipartOperation, code: &str) {
        let server = MockServer::start().await;
        let bytes = vec![4; PART_SIZE];
        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("archive.json");
        let needs_active_session = operation != MultipartOperation::Create;
        let complete_status = if code == "UnsupportedOperation" {
            200
        } else {
            405
        };

        match operation {
            MultipartOperation::Create => {
                Mock::given(method("POST"))
                    .and(query_param("uploads", ""))
                    .respond_with(ResponseTemplate::new(405).set_body_string(s3_error_xml(code)))
                    .expect(1)
                    .mount(&server)
                    .await;
                Mock::given(method("DELETE"))
                    .respond_with(ResponseTemplate::new(500))
                    .expect(0)
                    .mount(&server)
                    .await;
            }
            MultipartOperation::ListParts => {
                write_valid_manifest(&manifest_path, &bytes, UPLOADER_ID).await;
                Mock::given(method("GET"))
                    .and(query_param("uploadId", UPLOAD_ID))
                    .respond_with(ResponseTemplate::new(405).set_body_string(s3_error_xml(code)))
                    .expect(1)
                    .mount(&server)
                    .await;
                mount_abort_for(&server, UPLOAD_ID).await;
            }
            MultipartOperation::UploadPart => {
                mount_create(&server).await;
                mount_empty_list_parts(&server).await;
                Mock::given(method("PUT"))
                    .and(query_param("uploadId", UPLOAD_ID))
                    .and(query_param("partNumber", "1"))
                    .respond_with(ResponseTemplate::new(405).set_body_string(s3_error_xml(code)))
                    .expect(1)
                    .mount(&server)
                    .await;
                mount_abort_for(&server, UPLOAD_ID).await;
            }
            MultipartOperation::Complete => {
                mount_create(&server).await;
                mount_empty_list_parts(&server).await;
                mount_upload_part_for(&server, UPLOAD_ID, 1, "\"part-1\"").await;
                Mock::given(method("POST"))
                    .and(query_param("uploadId", UPLOAD_ID))
                    .respond_with(
                        ResponseTemplate::new(complete_status).set_body_string(s3_error_xml(code)),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                mount_abort_for(&server, UPLOAD_ID).await;
            }
            MultipartOperation::ZipPut | MultipartOperation::Abort | MultipartOperation::Head => {
                unreachable!()
            }
        }

        let result = upload_multipart(
            test_bucket(&server).as_ref(),
            &reqwest::Client::new(),
            standard_request(
                &bytes,
                needs_active_session.then_some(manifest_path.as_path()),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            MultipartOutcome::Unsupported {
                operation: actual_operation
            } if actual_operation == operation
        ));
        if needs_active_session {
            assert!(!manifest_path.exists());
        }
        server.verify().await;
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
                    .expect(1)
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
            bytes,
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
        let manifest =
            manifest::new_manifest(&identity, KEY.to_owned(), UPLOAD_ID.to_owned()).unwrap();
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
                            Ok((stream, _)) => {
                                serve_raw_multipart_request(stream, &requests, &state)
                            }
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
                                expected_part_size(state.object_len, *part_number).unwrap() as u64,
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
                if part_number == state.lost_response_part && state.lost_response_calls == 0 {
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

    fn list_parts_response(parts: &[(u32, String, u64)]) -> String {
        list_parts_response_for(BUCKET, KEY, UPLOAD_ID, parts)
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
}
