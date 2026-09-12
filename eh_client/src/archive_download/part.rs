use super::artifacts::ArchiveArtifacts;
use super::http::{archive_get, archive_http_error, parse_content_range_header};
use super::manifest::{ArchiveManifest, ManifestPart};
use crate::error::{Error, Result};
use crate::models::EhCookies;
use reqwest::header::{HeaderMap, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use reqwest::StatusCode;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

mod attempt;
mod worker;

pub(super) use worker::run_part;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Validator {
    StrongEtag(String),
    LastModified(String),
    None,
}

impl Validator {
    pub(super) fn from_manifest(manifest: &ArchiveManifest) -> Self {
        select_strong_etag(manifest.etag.as_deref())
            .map(Self::StrongEtag)
            .or_else(|| {
                select_last_modified(manifest.last_modified.as_deref()).map(Self::LastModified)
            })
            .unwrap_or(Self::None)
    }

    pub(super) fn store_in_manifest(&self, manifest: &mut ArchiveManifest) {
        manifest.etag = None;
        manifest.last_modified = None;
        match self {
            Self::StrongEtag(value) => manifest.etag = Some(value.clone()),
            Self::LastModified(value) => manifest.last_modified = Some(value.clone()),
            Self::None => {}
        }
    }

    fn apply_if_range(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::StrongEtag(value) | Self::LastModified(value) => request.header(IF_RANGE, value),
            Self::None => request,
        }
    }

    fn matches(&self, headers: &HeaderMap) -> bool {
        match self {
            Self::StrongEtag(value) => header_value(headers, ETAG) == Some(value),
            Self::LastModified(value) => header_value(headers, LAST_MODIFIED) == Some(value),
            Self::None => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PartFailureKind {
    Retryable,
    RestartSequential,
}

#[derive(Debug)]
pub(super) struct PartFailure {
    pub(super) kind: PartFailureKind,
    pub(super) error: Error,
    pub(super) attempts: usize,
}

impl PartFailure {
    pub(super) fn retryable_http(error: reqwest::Error, attempts: usize) -> Self {
        Self {
            kind: PartFailureKind::Retryable,
            error: archive_http_error(error),
            attempts,
        }
    }

    fn restart_sequential(message: &'static str) -> Self {
        Self {
            kind: PartFailureKind::RestartSequential,
            error: Error::Other(message.into()),
            attempts: 0,
        }
    }
}

/// A durable, part-relative progress observation. `window_delta` covers a
/// non-overlapping reporting window, while `durable_len` is the absolute file length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PartSample {
    pub(super) part_id: u64,
    pub(super) generation: u64,
    pub(super) durable_len: u64,
    pub(super) window_delta: u64,
    pub(super) elapsed: Duration,
}

impl PartSample {
    pub(super) fn is_rate_eligible(&self) -> bool {
        self.window_delta > 0 && self.elapsed >= Duration::from_secs(1)
    }
}

/// `applied` is an acknowledgement barrier: a future worker must wait for it
/// before treating the sample as incorporated by the coordinator.
#[derive(Debug)]
pub(super) struct PartSampleEvent {
    pub(super) sample: PartSample,
    pub(super) applied: oneshot::Sender<()>,
}

#[derive(Debug)]
pub(super) enum PartExit {
    Complete { attempts_used: usize },
    Paused { attempts_used: usize },
    Failed(PartFailure),
}

#[derive(Debug)]
pub(super) struct SeedResponse {
    pub(super) response: reqwest::Response,
    pub(super) request_started_at: Instant,
}

pub(super) fn select_validator(headers: &HeaderMap) -> Validator {
    select_strong_etag(header_value(headers, ETAG))
        .map(Validator::StrongEtag)
        .or_else(|| {
            select_last_modified(header_value(headers, LAST_MODIFIED)).map(Validator::LastModified)
        })
        .unwrap_or(Validator::None)
}

pub(super) fn part_get<'a>(
    http: &'a reqwest::Client,
    cookies: &'a EhCookies,
    url: &'a str,
    start: u64,
    end: u64,
    validator: &Validator,
) -> reqwest::RequestBuilder {
    let inclusive_end = end
        .checked_sub(1)
        .expect("part request range must be non-empty");
    validator.apply_if_range(
        archive_get(http, cookies, url).header(RANGE, format!("bytes={start}-{inclusive_end}")),
    )
}

pub(super) fn validate_part_response(
    status: StatusCode,
    headers: &HeaderMap,
    requested_start: u64,
    requested_end: u64,
    total_len: u64,
    validator: &Validator,
    resuming_persisted_manifest: bool,
) -> std::result::Result<(), PartFailure> {
    if status != StatusCode::PARTIAL_CONTENT {
        if resuming_persisted_manifest && !status.is_success() {
            return Err(PartFailure {
                kind: PartFailureKind::Retryable,
                error: Error::Api {
                    message: format!("archive download returned {status}"),
                    status: status.as_u16(),
                },
                attempts: 0,
            });
        }
        return Err(PartFailure::restart_sequential(
            "archive part response did not return 206 Partial Content",
        ));
    }
    let Some(expected_end) = requested_end.checked_sub(1) else {
        return Err(PartFailure::restart_sequential(
            "archive part request range is empty",
        ));
    };
    let Some((start, end, total)) = parse_content_range(headers) else {
        return Err(PartFailure::restart_sequential(
            "archive part response has invalid Content-Range",
        ));
    };
    if start != requested_start || end != expected_end || total != total_len {
        return Err(PartFailure::restart_sequential(
            "archive part response Content-Range does not match the request",
        ));
    }
    if !validator.matches(headers) {
        return Err(PartFailure::restart_sequential(
            "archive part response validator does not match the manifest",
        ));
    }
    Ok(())
}

pub(super) fn requested_range(
    part: &ManifestPart,
    downloaded: u64,
) -> std::result::Result<Option<(u64, u64)>, PartFailure> {
    let part_len = part.len();
    if downloaded > part_len {
        return Err(PartFailure::restart_sequential(
            "archive part file exceeds its interval",
        ));
    }
    if downloaded == part_len {
        return Ok(None);
    }
    Ok(Some((part.start + downloaded, part.end)))
}

pub(super) async fn aggregate_downloaded_bytes(
    artifacts: &ArchiveArtifacts,
    manifest: &ArchiveManifest,
) -> Result<u64> {
    let mut total = 0_u64;
    for part in &manifest.parts {
        let metadata = tokio::fs::metadata(ArchiveManifest::part_path(artifacts, part.id)).await?;
        if !metadata.is_file() {
            return Err(Error::Other(format!(
                "archive part {} is not a regular file",
                part.id
            )));
        }
        let len = metadata.len();
        if len > part.len() {
            return Err(Error::Other(format!(
                "archive part {} exceeds its interval",
                part.id
            )));
        }
        total = total
            .checked_add(len)
            .ok_or_else(|| Error::Other("archive part byte total overflows u64".into()))?;
    }
    Ok(total)
}

fn header_value(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

fn select_strong_etag(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty() && !value.starts_with("W/")).then(|| value.to_owned())
}

fn select_last_modified(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_content_range(headers: &HeaderMap) -> Option<(u64, u64, u64)> {
    let (start, end, total) = parse_content_range_header(header_value(headers, CONTENT_RANGE)?)?;
    Some((start, end, total?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_download::manifest::{ArchiveManifest, ManifestPart};
    use crate::EhCookies;
    use reqwest::header::{
        HeaderMap, HeaderName, HeaderValue, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE,
    };
    use reqwest::StatusCode;

    const LAST_MODIFIED_VALUE: &str = "Tue, 21 Jul 2026 12:00:00 GMT";

    #[test]
    fn part_response_restarts_for_non_exact_ranges_and_statuses() {
        for headers in [
            part_headers("bytes 11-19/100"),
            part_headers("bytes 10-18/100"),
            part_headers("bytes 10-19/*"),
            part_headers("bytes 10-19/101"),
        ] {
            assert_restart(validate_part_response(
                StatusCode::PARTIAL_CONTENT,
                &headers,
                10,
                20,
                100,
                &Validator::None,
                false,
            ));
        }
        let headers = part_headers("bytes 10-19/100");
        for status in [StatusCode::OK, StatusCode::RANGE_NOT_SATISFIABLE] {
            assert_restart(validate_part_response(
                status,
                &headers,
                10,
                20,
                100,
                &Validator::None,
                false,
            ));
        }
    }

    #[test]
    fn validator_selects_stores_and_sends_strong_etags_or_last_modified() {
        let strong_headers = headers(&[(ETAG, " \"v1\" "), (LAST_MODIFIED, LAST_MODIFIED_VALUE)]);
        let strong = select_validator(&strong_headers);
        assert_eq!(strong, Validator::StrongEtag("\"v1\"".into()));

        let mut manifest = manifest(None, Some("stale"));
        strong.store_in_manifest(&mut manifest);
        assert_eq!(manifest.etag.as_deref(), Some("\"v1\""));
        assert_eq!(manifest.last_modified, None);
        assert_eq!(Validator::from_manifest(&manifest), strong);

        let request = part_get(
            &reqwest::Client::new(),
            &EhCookies::default(),
            "https://example.invalid/archive",
            12,
            100,
            &strong,
        )
        .build()
        .unwrap();
        assert_eq!(request.headers().get(RANGE).unwrap(), "bytes=12-99");
        assert_eq!(request.headers().get(IF_RANGE).unwrap(), "\"v1\"");

        let weak_headers = headers(&[
            (ETAG, "W/\"weak\""),
            (LAST_MODIFIED, "  Tue, 21 Jul 2026 12:00:00 GMT "),
        ]);
        let last_modified = select_validator(&weak_headers);
        assert_eq!(
            last_modified,
            Validator::LastModified(LAST_MODIFIED_VALUE.into())
        );
        last_modified.store_in_manifest(&mut manifest);
        assert_eq!(manifest.etag, None);
        assert_eq!(manifest.last_modified.as_deref(), Some(LAST_MODIFIED_VALUE));
        assert_eq!(Validator::from_manifest(&manifest), last_modified);
    }

    fn assert_restart<T: std::fmt::Debug>(result: std::result::Result<T, PartFailure>) {
        assert_eq!(result.unwrap_err().kind, PartFailureKind::RestartSequential);
    }

    fn part_headers(content_range: &str) -> HeaderMap {
        headers(&[(CONTENT_RANGE, content_range)])
    }

    fn headers(values: &[(HeaderName, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.insert(name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    fn manifest(etag: Option<&str>, last_modified: Option<&str>) -> ArchiveManifest {
        ArchiveManifest {
            version: 1,
            download_url: "https://example.invalid/archive".into(),
            total_len: 100,
            etag: etag.map(str::to_owned),
            last_modified: last_modified.map(str::to_owned),
            next_part_id: 2,
            parts: vec![ManifestPart {
                id: 1,
                start: 0,
                end: 100,
            }],
        }
    }
}
