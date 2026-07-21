use super::artifacts::ArchiveArtifacts;
use super::http::{archive_get, archive_http_error};
use super::manifest::{ArchiveManifest, ManifestPart};
use super::part::{select_validator, SeedResponse};
use crate::error::Result;
use crate::models::EhCookies;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use std::time::Instant;

pub(super) enum MultipartInitialization {
    Ready {
        manifest: ArchiveManifest,
        seed: SeedResponse,
    },
    SequentialResponse(reqwest::Response),
    SequentialRestart,
}

pub(super) async fn initialize_multipart(
    http: &reqwest::Client,
    cookies: &EhCookies,
    download_url: &str,
    artifacts: &ArchiveArtifacts,
) -> Result<MultipartInitialization> {
    let request = archive_get(http, cookies, download_url).header(RANGE, "bytes=0-");
    let request_started_at = Instant::now();
    let response = request.send().await.map_err(archive_http_error)?;

    if response.status() == StatusCode::OK {
        return Ok(MultipartInitialization::SequentialResponse(response));
    }
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Ok(MultipartInitialization::SequentialRestart);
    }

    let Some(total_len) = initial_content_range_total(response.headers()) else {
        return Ok(MultipartInitialization::SequentialRestart);
    };
    let mut manifest = ArchiveManifest {
        version: 1,
        download_url: download_url.to_owned(),
        total_len,
        etag: None,
        last_modified: None,
        next_part_id: 1,
        parts: vec![ManifestPart {
            id: 0,
            start: 0,
            end: total_len,
        }],
    };
    select_validator(response.headers()).store_in_manifest(&mut manifest);

    if let Err(error) = create_initial_state(artifacts, &mut manifest).await {
        let _ = artifacts.remove_multipart_state().await;
        return Err(error);
    }

    Ok(MultipartInitialization::Ready {
        manifest,
        seed: SeedResponse {
            response,
            request_started_at,
        },
    })
}

async fn create_initial_state(
    artifacts: &ArchiveArtifacts,
    manifest: &mut ArchiveManifest,
) -> Result<()> {
    tokio::fs::create_dir_all(artifacts.parts_dir()).await?;
    tokio::fs::File::create(ArchiveManifest::part_path(artifacts, 0)).await?;
    manifest.write_atomic(artifacts).await
}

fn initial_content_range_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let range = value.strip_prefix("bytes ")?;
    let (bounds, total) = range.split_once('/')?;
    let (start, end) = bounds.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    (start == 0 && total != 0 && end.checked_add(1) == Some(total)).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::initial_content_range_total;
    use reqwest::header::{HeaderMap, HeaderValue, CONTENT_RANGE};

    #[test]
    fn initial_content_range_requires_the_entire_nonzero_archive() {
        let mut exact = HeaderMap::new();
        exact.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-7/8"));
        assert_eq!(initial_content_range_total(&exact), Some(8));

        for value in [
            "bytes 1-7/8",
            "bytes 0-6/8",
            "bytes 0-7/*",
            "bytes 0-0/0",
            "bytes 0-8/8",
            "items 0-7/8",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_RANGE, HeaderValue::from_str(value).unwrap());
            assert_eq!(initial_content_range_total(&headers), None, "{value}");
        }
    }
}
