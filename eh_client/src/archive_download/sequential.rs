use super::archive_http_error;
use super::http::archive_get;
use crate::error::{Error, Result};
use crate::models::EhCookies;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
use tokio_util::io::StreamReader;

const ARCHIVE_DOWNLOAD_MAX_ATTEMPTS: usize = 4;

/// Threshold for "made progress": strictly greater than 10 KiB/s.
const PROGRESS_THRESHOLD_BYTES_PER_SEC: f64 = 10_240.0;

/// Returns true if the attempt transferred data fast enough to count as real progress.
/// - 10 KiB/s = 10240 bytes/s; strictly greater than (not equal to).
/// - `elapsed_secs == 0.0` returns false (prevents division by zero).
pub(super) fn made_progress(new_bytes: u64, elapsed_secs: f64) -> bool {
    elapsed_secs > 0.0 && (new_bytes as f64 / elapsed_secs) > PROGRESS_THRESHOLD_BYTES_PER_SEC
}

fn response_expected_total(
    headers: &reqwest::header::HeaderMap,
    existing_len: u64,
    append: bool,
) -> Option<u64> {
    let content_len = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())?;
    Some(if append {
        existing_len + content_len
    } else {
        content_len
    })
}

fn parse_content_range_header(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let range = value.strip_prefix("bytes ")?;
    let (bounds, total) = range.split_once('/')?;
    let (start, end) = bounds.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    if end < start {
        return None;
    }
    let total = if total == "*" {
        None
    } else {
        Some(total.parse::<u64>().ok()?)
    };
    Some((start, end, total))
}

fn validate_content_range(headers: &reqwest::header::HeaderMap, existing_len: u64) -> Result<u64> {
    let value = headers
        .get(CONTENT_RANGE)
        .ok_or_else(|| Error::Other("archive resume response missing Content-Range".into()))?
        .to_str()
        .map_err(|_| Error::Other("archive resume response has invalid Content-Range".into()))?;
    let (start, end, total) = parse_content_range_header(value)
        .ok_or_else(|| Error::Other("archive resume response has invalid Content-Range".into()))?;
    if start != existing_len {
        return Err(Error::Other(format!(
            "archive resume Content-Range starts at {start}, expected {existing_len}"
        )));
    }
    if let Some(total) = total {
        if end >= total {
            return Err(Error::Other(format!(
                "archive resume Content-Range end {end} exceeds total {total}"
            )));
        }
        if end + 1 != total {
            return Err(Error::Other(format!(
                "archive resume Content-Range ended at {}, expected final byte {}",
                end,
                total.saturating_sub(1)
            )));
        }
        return Ok(total);
    }
    Ok(end + 1)
}

pub(crate) async fn download_sequential(
    http: &reqwest::Client,
    cookies: &EhCookies,
    download_url: &str,
    temp_path: &Path,
    mut first_response: Option<reqwest::Response>,
) -> Result<()> {
    let initial_len = tokio::fs::metadata(temp_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let total_start = Instant::now();
    let mut attempts = 0usize;
    let mut had_progress = false;
    let mut last_error: Option<Error> = None;

    for attempt in 1..=ARCHIVE_DOWNLOAD_MAX_ATTEMPTS {
        attempts = attempt;
        let before_len = tokio::fs::metadata(temp_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let start = Instant::now();

        match download_sequential_once(
            http,
            cookies,
            download_url,
            temp_path,
            first_response.take(),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                let elapsed = start.elapsed().as_secs_f64();
                let after_len = tokio::fs::metadata(temp_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(before_len);
                let new_bytes = after_len.saturating_sub(before_len);
                let attempt_made_progress = made_progress(new_bytes, elapsed);
                if attempt_made_progress {
                    had_progress = true;
                }
                tracing::warn!(
                    attempt,
                    max_attempts = ARCHIVE_DOWNLOAD_MAX_ATTEMPTS,
                    new_bytes,
                    elapsed_secs = elapsed,
                    attempt_made_progress,
                    had_progress,
                    error = %error,
                    "archive download attempt failed",
                );
                last_error = Some(error);
                // Sleep between attempts to avoid request burst on immediate failures.
                // Worst case: 4 immediate failures ≈ 3s extra, acceptable for a single worker tick.
                sleep(Duration::from_secs(1)).await;
            }
        }
    }

    match last_error {
        Some(error) if had_progress => {
            let final_len = tokio::fs::metadata(temp_path)
                .await
                .map(|m| m.len())
                .unwrap_or(initial_len);
            Err(Error::DownloadInProgress {
                inner: Box::new(error),
                attempts,
                bytes_delta: final_len.saturating_sub(initial_len),
                elapsed: total_start.elapsed(),
            })
        }
        Some(error) => Err(error),
        None => Err(Error::Other("archive download failed".into())),
    }
}

async fn download_sequential_once(
    http: &reqwest::Client,
    cookies: &EhCookies,
    download_url: &str,
    temp_path: &Path,
    response: Option<reqwest::Response>,
) -> Result<()> {
    let existing_len = tokio::fs::metadata(temp_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let resp = match response {
        Some(response) => response,
        None => {
            let mut request = archive_get(http, cookies, download_url);
            if existing_len > 0 {
                request = request.header(RANGE, format!("bytes={existing_len}-"));
            }
            request.send().await.map_err(archive_http_error)?
        }
    };
    let status = resp.status();
    if existing_len > 0 && status.as_u16() == 416 {
        let _ = tokio::fs::remove_file(temp_path).await;
        return Err(Error::Api {
            message: format!(
                "archive download returned {}; restarting without partial",
                status
            ),
            status: status.as_u16(),
        });
    }
    let mut expected_total_from_range = None;
    let append = if existing_len > 0 && status.as_u16() == 206 {
        expected_total_from_range = Some(validate_content_range(resp.headers(), existing_len)?);
        true
    } else if status.is_success() {
        if existing_len > 0 && status.as_u16() == 200 {
            let _ = tokio::fs::remove_file(temp_path).await;
        }
        false
    } else {
        return Err(Error::Api {
            message: format!("archive download returned {}", status),
            status: status.as_u16(),
        });
    };

    let expected_total = expected_total_from_range
        .or_else(|| response_expected_total(resp.headers(), existing_len, append));
    if append && expected_total == Some(existing_len) {
        return Ok(());
    }

    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }

    let file = options.open(temp_path).await?;
    let mut writer = tokio::io::BufWriter::with_capacity(2 * 1024 * 1024, file);
    // Convert the response byte stream into an AsyncRead, preserving the
    // historical `copy_buf` behavior while removing sensitive token URLs from
    // any reqwest error before it enters the I/O error chain.
    let stream = resp
        .bytes_stream()
        .map(|result| result.map_err(|error| std::io::Error::other(archive_http_error(error))));
    let mut reader = StreamReader::new(stream);

    // copy_buf reads from the stream and writes to BufWriter. Writes are
    // memcpy until the 2MB buffer fills, so the read loop runs almost
    // continuously, keeping the TCP receive window open.
    let copied = match tokio::io::copy_buf(&mut reader, &mut writer).await {
        Ok(copied) => {
            // On success, flush must succeed so written reflects bytes on disk.
            writer.flush().await?;
            copied
        }
        Err(error) => {
            // On stream error, flush best-effort to persist buffered bytes
            // so the .part file advances and resume Range/made_progress
            // reflect real downloaded data.
            let _ = writer.flush().await;
            return Err(error.into());
        }
    };
    let written = if append {
        existing_len + copied
    } else {
        copied
    };

    if let Some(expected_total) = expected_total {
        if written < expected_total {
            return Err(Error::Other(format!(
                "archive download ended at {written} bytes, expected {expected_total}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        made_progress, parse_content_range_header, response_expected_total, validate_content_range,
    };
    use reqwest::header::{HeaderMap, HeaderValue, CONTENT_LENGTH, CONTENT_RANGE};

    #[test]
    fn made_progress_uses_a_strict_10_kib_per_second_threshold() {
        assert!(!made_progress(10_240, 1.0));
        assert!(made_progress(10_241, 1.0));
        assert!(!made_progress(0, 1.0));
        assert!(!made_progress(99_999, 0.0));
        assert!(made_progress(20_000, 0.5));
        assert!(!made_progress(100, 10.0));
    }

    #[test]
    fn content_range_parser_and_expected_total_preserve_resume_rules() {
        assert_eq!(
            parse_content_range_header("bytes 12-19/20"),
            Some((12, 19, Some(20)))
        );
        assert_eq!(
            parse_content_range_header("bytes 12-19/*"),
            Some((12, 19, None))
        );
        assert_eq!(parse_content_range_header("bytes 19-12/20"), None);

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 12-19/20"));
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("8"));
        assert_eq!(validate_content_range(&headers, 12).unwrap(), 20);
        assert_eq!(response_expected_total(&headers, 12, true), Some(20));
        assert_eq!(response_expected_total(&headers, 0, false), Some(8));
    }

    #[test]
    fn content_range_validation_requires_the_final_byte() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 12-18/20"));

        assert_eq!(
            validate_content_range(&headers, 12)
                .unwrap_err()
                .to_string(),
            "archive resume Content-Range ended at 18, expected final byte 19"
        );
    }
}
