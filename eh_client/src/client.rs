use crate::error::{Error, Result};
use crate::models::{EhCookies, EhGallery, EhGalleryRef, RawApiResponse, RawGalleryMetaEntry};
use crate::parser;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, COOKIE, RANGE};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
use tokio_util::io::StreamReader;

const USER_AGENT_STR: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const ARCHIVE_CONNECT_TIMEOUT_SECS: u64 = 30;
const ARCHIVE_READ_TIMEOUT_SECS: u64 = 60;
const ARCHIVE_DOWNLOAD_MAX_ATTEMPTS: usize = 4;

/// Threshold for "made progress": strictly greater than 10 KiB/s.
const PROGRESS_THRESHOLD_BYTES_PER_SEC: f64 = 10_240.0;

/// Returns true if the attempt transferred data fast enough to count as real progress.
/// - 10 KiB/s = 10240 bytes/s; strictly greater than (not equal to).
/// - `elapsed_secs == 0.0` returns false (prevents division by zero).
fn made_progress(new_bytes: u64, elapsed_secs: f64) -> bool {
    elapsed_secs > 0.0 && (new_bytes as f64 / elapsed_secs) > PROGRESS_THRESHOLD_BYTES_PER_SEC
}

pub struct EhClient {
    http: reqwest::Client,
    base_url: String,
    pub(crate) api_url: String,
    cookies: EhCookies,
}

#[derive(Debug, Clone)]
pub struct ArchiveDownloadRequest {
    action_url: String,
    form_data: Vec<(String, String)>,
    /// Cost classification parsed from the archiver.php page for this form.
    /// Set by `prepare_archive_download()` so callers can gate the POST
    /// (`download_archive_with_request`) on their GP budget without spending GP.
    cost: parser::DownloadCost,
}

impl ArchiveDownloadRequest {
    fn from_archiver_key(
        base_url: &str,
        gid: u64,
        token: &str,
        archiver_key: &str,
        resolution: &str,
    ) -> Self {
        Self {
            action_url: format!(
                "{}/archiver.php?gid={}&token={}&or={}",
                base_url, gid, token, archiver_key
            ),
            form_data: archive_form_data_for_resolution(resolution),
            // The archiver_key path is only used when the page already contained
            // an unlocked-key URL (e.g. `or={key}` in the form action), which
            // means the download is free / already unlocked.
            cost: parser::DownloadCost::Unlocked,
        }
    }

    fn from_archiver_form(
        base_url: &str,
        form: parser::ArchiverForm,
        resolution: &str,
        cost: parser::DownloadCost,
    ) -> Self {
        let mut form_data = form.fields;
        apply_resolution_to_form_data(&mut form_data, resolution);
        Self {
            action_url: resolve_url(base_url, &form.action),
            form_data,
            cost,
        }
    }

    /// The parsed download cost for this request. Callers should check this
    /// before calling `download_archive_with_request()` to avoid spending GP.
    pub fn cost(&self) -> &parser::DownloadCost {
        &self.cost
    }
}

fn archive_form_data_for_resolution(resolution: &str) -> Vec<(String, String)> {
    let want_original = resolution == "original" || resolution.is_empty();
    let xres_val = if want_original {
        "org".to_string()
    } else {
        resolution.trim_end_matches('x').to_string()
    };

    vec![
        (
            "dlcheck".to_string(),
            if want_original {
                "Download Original Archive"
            } else {
                "Download Resample Archive"
            }
            .to_string(),
        ),
        ("hathdl_xres".to_string(), xres_val),
    ]
}

fn apply_resolution_to_form_data(form_data: &mut Vec<(String, String)>, resolution: &str) {
    let want_original = resolution == "original" || resolution.is_empty();
    if let Some((_, value)) = form_data.iter_mut().find(|(name, _)| name == "hathdl_xres") {
        *value = if want_original {
            "org".to_string()
        } else {
            resolution.trim_end_matches('x').to_string()
        };
    }
    if want_original {
        if let Some((_, value)) = form_data.iter_mut().find(|(name, _)| name == "dltype") {
            *value = "org".to_string();
        }
    }
    if !form_data.iter().any(|(name, _)| name == "dlcheck") {
        form_data.push((
            "dlcheck".to_string(),
            if want_original {
                "Download Original Archive"
            } else {
                "Download Resample Archive"
            }
            .to_string(),
        ));
    }
}

fn resolve_url(base_url: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("{}{}", base_url, url)
    } else {
        format!("{}/{}", base_url, url)
    }
}

fn is_ehentai_host(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "e-hentai.org" | "exhentai.org"))
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

impl EhClient {
    pub fn new(base_url: &str, api_url: &str, cookies: EhCookies) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT_STR)
            .connect_timeout(std::time::Duration::from_secs(ARCHIVE_CONNECT_TIMEOUT_SECS))
            .read_timeout(std::time::Duration::from_secs(ARCHIVE_READ_TIMEOUT_SECS));

        // For exhentai, bind to IPv4 to avoid CloudFlare blocks
        if base_url.contains("exhentai") {
            builder = builder.local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        }

        let http = builder.build()?;
        Ok(Self {
            http,
            base_url: base_url.to_string(),
            api_url: api_url.to_string(),
            cookies,
        })
    }

    /// Build a search URL from query, category bitmask, and page number.
    pub fn build_search_url(&self, query: &str, cats: u32, page: u32) -> String {
        format!(
            "{}/?f_search={}&f_cats={}&page={}",
            self.base_url,
            urlencoding::encode(query),
            cats,
            page
        )
    }

    /// Build an archiver.php URL.
    pub fn build_archiver_url(&self, gid: u64, token: &str, or: &str) -> String {
        format!(
            "{}/archiver.php?gid={}&token={}&or={}",
            self.base_url, gid, token, or
        )
    }

    async fn fetch_archiver_page(&self, gid: u64, token: &str) -> Result<(u64, String, String)> {
        let gallery_url = format!("{}/g/{}/{}/", self.base_url, gid, token);
        let resp = self
            .http
            .get(&gallery_url)
            .header(COOKIE, self.cookies.to_header())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("gallery page returned {}", status),
                status: status.as_u16(),
            });
        }
        let gallery_html = resp.text().await?;

        let (archiver_gid, archiver_token) = parser::parse_archiver_url(&gallery_html)
            .ok_or_else(|| Error::Parse("archiver URL not found in gallery page".into()))?;

        let archiver_page_url = format!(
            "{}/archiver.php?gid={}&token={}",
            self.base_url, archiver_gid, archiver_token
        );
        let resp = self
            .http
            .get(&archiver_page_url)
            .header(COOKIE, self.cookies.to_header())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("archiver.php returned {}", status),
                status: status.as_u16(),
            });
        }
        let archiver_html = resp.text().await?;

        Ok((archiver_gid, archiver_token, archiver_html))
    }

    /// Search for galleries. Returns gallery references parsed from HTML.
    pub async fn search(&self, query: &str, cats: u32, page: u32) -> Result<Vec<EhGalleryRef>> {
        let url = self.build_search_url(query, cats, page);
        let resp = self
            .http
            .get(&url)
            .header(COOKIE, self.cookies.to_header())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("search returned {}", status),
                status: status.as_u16(),
            });
        }
        let html = resp.text().await?;
        Ok(parser::parse_search_results(&html, &self.base_url))
    }

    /// Fetch gallery metadata via the api.php JSON endpoint.
    /// Max 25 galleries per request.
    pub async fn get_metadata(&self, gidlist: &[(u64, &str)]) -> Result<Vec<EhGallery>> {
        if gidlist.is_empty() {
            return Ok(Vec::new());
        }
        if gidlist.len() > 25 {
            return Err(Error::Other(
                "get_metadata: max 25 galleries per request".into(),
            ));
        }

        let gidlist_json: Vec<serde_json::Value> = gidlist
            .iter()
            .map(|(gid, token)| serde_json::json!([gid, token]))
            .collect();

        let body = serde_json::json!({
            "method": "gdata",
            "gidlist": gidlist_json,
            "namespace": 1
        });

        let resp = self
            .http
            .post(&self.api_url)
            .header(COOKIE, self.cookies.to_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("api.php returned {}", status),
                status: status.as_u16(),
            });
        }

        let raw: RawApiResponse = resp.json().await?;
        let galleries = raw
            .gmetadata
            .into_iter()
            .filter_map(|entry| match entry {
                RawGalleryMetaEntry::Gallery(m) => Some((*m).into_gallery()),
                RawGalleryMetaEntry::Error(e) => {
                    tracing::warn!(
                        gid = e.gid,
                        error = %e.error,
                        "Skipping E-Hentai metadata error entry"
                    );
                    None
                }
            })
            .collect();
        Ok(galleries)
    }

    /// Get the archiver_key for a gallery.
    /// Step 1: Scrape gallery page for archiver.php URL (in onclick attribute).
    /// Step 2: GET the archiver.php URL and parse the response for the archiver_key.
    pub async fn get_archiver_key(&self, gid: u64, token: &str) -> Result<String> {
        let (_, _, archiver_html) = self.fetch_archiver_page(gid, token).await?;

        // Parse the archiver_key from the response
        parser::parse_archiver_key(&archiver_html)
            .ok_or_else(|| Error::Parse("archiver_key not found in archiver.php response".into()))
    }

    /// Prepare the POST request needed to initiate an archive download.
    ///
    /// Also parses the archiver.php page for the download cost (Free / Unlocked
    /// / `N GP` / Insufficient / N/A / Unknown) and attaches it to the returned
    /// request so callers can decide whether to POST without spending GP.
    pub async fn prepare_archive_download(
        &self,
        gid: u64,
        token: &str,
        resolution: &str,
    ) -> Result<ArchiveDownloadRequest> {
        let (archiver_gid, archiver_token, archiver_html) =
            self.fetch_archiver_page(gid, token).await?;

        // Parse the cost from the archiver page for the requested resolution.
        // This happens before any POST, so it does not spend GP. The cost is
        // attached to the returned request regardless of whether we take the
        // archiver-key path or the form path below, so callers can gate the
        // POST on the configured GP budget even when the page contains an
        // archiver_key (a key-shaped token alone does NOT prove the download
        // is free - only the Download Cost text or the unlocked marker does).
        let cost = parser::parse_archive_download_cost(&archiver_html, resolution);

        if let Some(archiver_key) = parser::parse_archiver_key(&archiver_html) {
            let mut request = ArchiveDownloadRequest::from_archiver_key(
                &self.base_url,
                archiver_gid,
                &archiver_token,
                &archiver_key,
                resolution,
            );
            // Override the default Unlocked cost set by from_archiver_key with
            // the cost actually parsed from the page. This preserves the
            // conservative "Unknown => defer" behavior when the page contains
            // a key but the Download Cost text cannot be recognized.
            request.cost = cost;
            return Ok(request);
        }

        let form = parser::parse_archiver_form(&archiver_html, resolution).ok_or_else(|| {
            Error::Parse("archiver download form not found in archiver.php response".into())
        })?;
        Ok(ArchiveDownloadRequest::from_archiver_form(
            &self.base_url,
            form,
            resolution,
            cost,
        ))
    }

    /// Download a gallery archive ZIP to the specified path.
    /// `archiver_key` is obtained from `get_archiver_key`.
    /// `resolution` controls quality: "780x"/"980x"/"1280x" (free resamples),
    /// "1600x"/"2400x" (donors), "original" (costs GP).
    ///
    /// Streams to a temporary path, validates the ZIP, then atomically renames
    /// to `dest`. The temp file is removed on any error after creation.
    pub async fn download_archive(
        &self,
        gid: u64,
        token: &str,
        archiver_key: &str,
        resolution: &str,
        dest: &Path,
    ) -> Result<u64> {
        let request = ArchiveDownloadRequest::from_archiver_key(
            &self.base_url,
            gid,
            token,
            archiver_key,
            resolution,
        );
        self.download_archive_with_request(&request, dest).await
    }

    /// Download a gallery archive ZIP from a prepared archiver POST request.
    pub async fn download_archive_with_request(
        &self,
        request: &ArchiveDownloadRequest,
        dest: &Path,
    ) -> Result<u64> {
        // Step 1: POST to archiver.php to initiate download
        let resp = self
            .http
            .post(&request.action_url)
            .header(COOKIE, self.cookies.to_header())
            .form(&request.form_data)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("archiver.php returned {}", status),
                status: status.as_u16(),
            });
        }

        let html = resp.text().await?;

        // Step 2: Parse the JS redirect URL
        let download_url = parser::parse_archive_redirect(&html)
            .ok_or_else(|| Error::Parse("archive redirect URL not found".into()))?;

        // Step 4: Stream to temp file, validate, then rename atomically
        let temp_path = dest.with_extension("zip.part");
        self.download_archive_response_resumable(&download_url, &temp_path)
            .await?;

        // Step 5: Validate that we got a complete ZIP (not an error HTML page or corrupt resume)
        if let Err(e) = validate_complete_zip(&temp_path).await {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(e);
        }

        let total = tokio::fs::metadata(&temp_path).await?.len();

        // Step 6: Atomically rename temp to final dest
        if let Err(e) = tokio::fs::rename(&temp_path, dest).await {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(e.into());
        }

        Ok(total)
    }

    async fn download_archive_response_resumable(
        &self,
        download_url: &str,
        temp_path: &Path,
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

            match self
                .download_archive_response_once(download_url, temp_path)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
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
                        error = %e,
                        "archive download attempt failed",
                    );
                    last_error = Some(e);
                    // Sleep between attempts to avoid request burst on immediate failures.
                    // Worst case: 4 immediate failures ≈ 3s extra, acceptable for a single worker tick.
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }

        match last_error {
            Some(e) if had_progress => {
                let final_len = tokio::fs::metadata(temp_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(initial_len);
                Err(Error::DownloadInProgress {
                    inner: Box::new(e),
                    attempts,
                    bytes_delta: final_len.saturating_sub(initial_len),
                    elapsed: total_start.elapsed(),
                })
            }
            Some(e) => Err(e),
            None => Err(Error::Other("archive download failed".into())),
        }
    }

    async fn download_archive_response_once(
        &self,
        download_url: &str,
        temp_path: &Path,
    ) -> Result<()> {
        let existing_len = tokio::fs::metadata(temp_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        let mut request = self.http.get(download_url);
        if existing_len > 0 {
            request = request.header(RANGE, format!("bytes={existing_len}-"));
        }
        if is_ehentai_host(download_url) {
            request = request.header(COOKIE, self.cookies.to_header());
        }

        let resp = request.send().await?;
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

        // Convert the response byte stream into an AsyncRead, mapping reqwest
        // errors to io::Error so StreamReader can surface them.
        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let mut reader = StreamReader::new(stream);

        // copy_buf reads from the stream and writes to BufWriter. Writes are
        // memcpy until the 2MB buffer fills, so the read loop runs almost
        // continuously, keeping the TCP receive window open.
        let copied = match tokio::io::copy_buf(&mut reader, &mut writer).await {
            Ok(n) => {
                // On success, flush must succeed so written reflects bytes on disk.
                writer.flush().await?;
                n
            }
            Err(e) => {
                // On stream error, flush best-effort to persist buffered bytes
                // so the .part file advances and resume Range/made_progress
                // reflect real downloaded data.
                let _ = writer.flush().await;
                return Err(e.into());
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

    /// Get the base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns true if the client has authentication cookies (logged in).
    pub fn is_logged_in(&self) -> bool {
        self.cookies.ipb_member_id.is_some() && self.cookies.ipb_pass_hash.is_some()
    }

    /// Collect all image URLs from a gallery by scraping image pages.
    /// Returns a list of direct image URLs (on H@H servers).
    /// Used for Telegraph page creation without downloading images.
    pub async fn get_gallery_image_urls(&self, gid: u64, token: &str) -> Result<Vec<String>> {
        // Step 1: Fetch gallery page to get image page URLs and page count
        let gallery_url = format!("{}/g/{}/{}/", self.base_url, gid, token);
        let resp = self
            .http
            .get(&gallery_url)
            .header(COOKIE, self.cookies.to_header())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("gallery page returned {}", status),
                status: status.as_u16(),
            });
        }
        let gallery_html = resp.text().await?;

        let total_pages = parser::parse_page_count(&gallery_html).unwrap_or(1);

        // Step 2: Collect all image page URLs from all gallery pages
        let mut all_page_urls: Vec<String> = parser::parse_image_page_urls(&gallery_html);

        for page_num in 1..total_pages {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let page_url = format!("{}/g/{}/{}/?p={}", self.base_url, gid, token, page_num);
            let resp = self
                .http
                .get(&page_url)
                .header(COOKIE, self.cookies.to_header())
                .send()
                .await?;
            if !resp.status().is_success() {
                break;
            }
            let html = resp.text().await?;
            let urls = parser::parse_image_page_urls(&html);
            if urls.is_empty() {
                break;
            }
            all_page_urls.extend(urls);
        }

        // Dedup preserving order
        let mut seen = std::collections::HashSet::new();
        all_page_urls.retain(|url| seen.insert(url.clone()));

        // Normalize relative URLs to absolute
        let all_page_urls: Vec<String> = all_page_urls
            .into_iter()
            .map(|url| {
                if url.starts_with('/') {
                    format!("{}{}", self.base_url, url)
                } else {
                    url
                }
            })
            .collect();

        if all_page_urls.is_empty() {
            return Err(Error::Parse("no image page URLs found".into()));
        }

        // Step 3: Visit each image page and extract the direct image URL
        let mut image_urls = Vec::new();
        for (idx, page_url) in all_page_urls.iter().enumerate() {
            if idx > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            let resp = match self
                .http
                .get(page_url.as_str())
                .header(COOKIE, self.cookies.to_header())
                .send()
                .await
            {
                Ok(r) => r,
                Err(_) => continue,
            };
            if !resp.status().is_success() {
                continue;
            }
            let html = resp.text().await?;
            if let Some(src) = parser::parse_image_src(&html) {
                image_urls.push(src);
            }
        }

        if image_urls.is_empty() {
            return Err(Error::Parse(
                "no image URLs extracted from image pages".into(),
            ));
        }

        Ok(image_urls)
    }

    /// Download gallery images directly by scraping image pages.
    /// Used as a fallback when archive download is not available (no login).
    /// Downloads all images and packages them into a ZIP at `dest`.
    /// Returns total bytes written.
    /// Fails if any image page fetch, image src extraction, or image download fails.
    /// Writes ZIP incrementally to a temp path; only renames to `dest` on success.
    /// Cleans up the temp file and ensures `dest` does not exist on error.
    pub async fn download_gallery_images(&self, gid: u64, token: &str, dest: &Path) -> Result<u64> {
        // Step 1: Fetch gallery page to get image page URLs and page count
        let gallery_url = format!("{}/g/{}/{}/", self.base_url, gid, token);
        let resp = self
            .http
            .get(&gallery_url)
            .header(COOKIE, self.cookies.to_header())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("gallery page returned {}", status),
                status: status.as_u16(),
            });
        }
        let gallery_html = resp.text().await?;

        let total_pages = parser::parse_page_count(&gallery_html).unwrap_or(1);

        // Step 2: Collect all image page URLs from all gallery pages.
        // Once any URL has been found, later gallery page failures are hard errors.
        let mut all_image_urls: Vec<String> = parser::parse_image_page_urls(&gallery_html);
        let mut has_urls = !all_image_urls.is_empty();

        for page_num in 1..total_pages {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let page_url = format!("{}/g/{}/{}/?p={}", self.base_url, gid, token, page_num);

            let resp = match self
                .http
                .get(&page_url)
                .header(COOKIE, self.cookies.to_header())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    if has_urls {
                        return Err(fallback_error(format!(
                            "gallery page {page_num} fetch error: {e}"
                        )));
                    }
                    break;
                }
            };
            if !resp.status().is_success() {
                if has_urls {
                    return Err(fallback_error(format!(
                        "gallery page {page_num} returned {}",
                        resp.status()
                    )));
                }
                break;
            }
            let html = match resp.text().await {
                Ok(h) => h,
                Err(e) => {
                    if has_urls {
                        return Err(fallback_error(format!(
                            "gallery page {page_num} read error: {e}"
                        )));
                    }
                    return Err(e.into());
                }
            };
            let urls = parser::parse_image_page_urls(&html);
            if urls.is_empty() {
                break;
            }
            all_image_urls.extend(urls);
            has_urls = true;
        }

        // Deduplicate image URLs (preserve order)
        let mut seen = std::collections::HashSet::new();
        all_image_urls.retain(|url| seen.insert(url.clone()));

        if all_image_urls.is_empty() {
            return Err(Error::Parse("no image page URLs found".into()));
        }

        // Normalize relative URLs to absolute using base_url
        let image_page_urls: Vec<String> = all_image_urls
            .into_iter()
            .map(|url| {
                if url.starts_with('/') {
                    format!("{}{}", self.base_url, url)
                } else {
                    url
                }
            })
            .collect();

        let total_images = image_page_urls.len();

        // Step 3: Write ZIP incrementally to a temp path. Rename on success;
        // clean up both temp and dest on any error. The zip_writer is explicitly
        // dropped before every cleanup to avoid Windows file-locking issues.
        let temp_path = dest.with_extension("zip.part");
        let _ = std::fs::remove_file(&temp_path);

        let temp_file = std::fs::File::create(&temp_path)
            .map_err(|e| fallback_error(format!("cannot create temp file: {e}")))?;
        let buf_writer = std::io::BufWriter::new(temp_file);
        let mut zip_writer = zip::ZipWriter::new(buf_writer);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        let mut total_bytes: u64 = 0;

        for (idx, image_page_url) in image_page_urls.iter().enumerate() {
            if idx > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }

            // Fetch image page
            let resp = match self
                .http
                .get(image_page_url.as_str())
                .header(COOKIE, self.cookies.to_header())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    drop(zip_writer);
                    cleanup_paths(&temp_path, dest);
                    return Err(fallback_error(format!(
                        "page {}/{} request error: {e}",
                        idx + 1,
                        total_images
                    )));
                }
            };
            if !resp.status().is_success() {
                drop(zip_writer);
                cleanup_paths(&temp_path, dest);
                return Err(fallback_error(format!(
                    "page {}/{} returned {}",
                    idx + 1,
                    total_images,
                    resp.status()
                )));
            }

            let html = match resp.text().await {
                Ok(h) => h,
                Err(e) => {
                    drop(zip_writer);
                    cleanup_paths(&temp_path, dest);
                    return Err(fallback_error(format!(
                        "page {}/{} read error: {e}",
                        idx + 1,
                        total_images
                    )));
                }
            };

            let image_url = match parser::parse_image_src(&html) {
                Some(u) => u,
                None => {
                    drop(zip_writer);
                    cleanup_paths(&temp_path, dest);
                    return Err(fallback_error(format!(
                        "no image src on page {}/{}",
                        idx + 1,
                        total_images
                    )));
                }
            };

            // Download the actual image
            let img_resp = match self.http.get(&image_url).send().await {
                Ok(r) => r,
                Err(e) => {
                    drop(zip_writer);
                    cleanup_paths(&temp_path, dest);
                    return Err(fallback_error(format!(
                        "image {}/{} request error: {e}",
                        idx + 1,
                        total_images
                    )));
                }
            };
            if !img_resp.status().is_success() {
                drop(zip_writer);
                cleanup_paths(&temp_path, dest);
                return Err(fallback_error(format!(
                    "image {}/{} returned {}",
                    idx + 1,
                    total_images,
                    img_resp.status()
                )));
            }

            let img_bytes = match img_resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    drop(zip_writer);
                    cleanup_paths(&temp_path, dest);
                    return Err(fallback_error(format!(
                        "image {}/{} read error: {e}",
                        idx + 1,
                        total_images
                    )));
                }
            };

            let ext = image_url
                .rsplit('.')
                .next()
                .filter(|e| e.len() <= 4)
                .unwrap_or("jpg");
            let entry_name = format!("{:04}.{}", idx + 1, ext);

            // Write to ZIP
            match zip_writer.start_file(&entry_name, options) {
                Ok(()) => {}
                Err(e) => {
                    drop(zip_writer);
                    cleanup_paths(&temp_path, dest);
                    return Err(fallback_error(format!("zip start_file error: {e}")));
                }
            }
            match std::io::Write::write_all(&mut zip_writer, &img_bytes) {
                Ok(()) => {}
                Err(e) => {
                    drop(zip_writer);
                    cleanup_paths(&temp_path, dest);
                    return Err(fallback_error(format!("zip write error: {e}")));
                }
            }
            total_bytes += img_bytes.len() as u64;
        }

        // Finish the ZIP
        match zip_writer.finish() {
            Ok(_inner_writer) => {}
            Err(e) => {
                // zip_writer has been consumed by finish(); just clean up paths
                cleanup_paths(&temp_path, dest);
                return Err(fallback_error(format!("zip finish error: {e}")));
            }
        }

        // Atomically rename temp to final dest
        match std::fs::rename(&temp_path, dest) {
            Ok(()) => {}
            Err(e) => {
                cleanup_paths(&temp_path, dest);
                return Err(fallback_error(format!("rename error: {e}")));
            }
        }

        Ok(total_bytes)
    }
}

/// Helper: construct an `Error::Other` with the required fallback prefix.
fn fallback_error(message: impl Into<String>) -> Error {
    Error::Other(format!(
        "failed to download all gallery images: {}",
        message.into()
    ))
}

/// Best-effort remove both temp and dest paths.
fn cleanup_paths(temp: &Path, dest: &Path) {
    let _ = std::fs::remove_file(temp);
    let _ = std::fs::remove_file(dest);
}

/// Builder for EhClient (useful for testing).
pub struct EhClientBuilder {
    base_url: String,
    api_url: String,
    cookies: EhCookies,
}

impl Default for EhClientBuilder {
    fn default() -> Self {
        Self {
            base_url: "https://e-hentai.org".into(),
            api_url: "https://api.e-hentai.org/api.php".into(),
            cookies: EhCookies {
                nw: true,
                ..Default::default()
            },
        }
    }
}

impl EhClientBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn base_url(mut self, url: &str) -> Self {
        self.base_url = url.into();
        self
    }
    pub fn api_url(mut self, url: &str) -> Self {
        self.api_url = url.into();
        self
    }
    pub fn cookies(mut self, c: EhCookies) -> Self {
        self.cookies = c;
        self
    }
    pub fn build(self) -> EhClient {
        EhClient::new(&self.base_url, &self.api_url, self.cookies)
            .expect("failed to build EhClient")
    }
}

/// Validate that a file is a complete ZIP archive.
async fn validate_complete_zip(path: &Path) -> Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| Error::Parse(format!("downloaded file is not a complete ZIP: {e}")))?;
        if archive.is_empty() {
            return Err(Error::Parse("downloaded ZIP is empty".into()));
        }

        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(|e| {
                Error::Parse(format!(
                    "downloaded ZIP entry {index} cannot be opened: {e}"
                ))
            })?;
            let name = entry.name().to_string();
            std::io::copy(&mut entry, &mut std::io::sink()).map_err(|e| {
                Error::Parse(format!("downloaded ZIP entry {name} is invalid: {e}"))
            })?;
        }

        Ok(())
    })
    .await
    .map_err(|e| Error::Other(format!("ZIP validation task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_search_url_basic() {
        let client = EhClientBuilder::new()
            .base_url("https://e-hentai.org")
            .build();
        let url = client.build_search_url("female:elf", 0, 0);
        assert_eq!(
            url,
            "https://e-hentai.org/?f_search=female%3Aelf&f_cats=0&page=0"
        );
    }

    #[test]
    fn test_build_search_url_with_cats() {
        let client = EhClientBuilder::new()
            .base_url("https://e-hentai.org")
            .build();
        let url = client.build_search_url("artist:wlop", 3, 2);
        assert!(url.contains("f_cats=3"));
        assert!(url.contains("page=2"));
    }

    #[test]
    fn test_build_api_url() {
        let client = EhClientBuilder::new()
            .base_url("https://e-hentai.org")
            .api_url("https://api.e-hentai.org/api.php")
            .build();
        assert_eq!(client.api_url, "https://api.e-hentai.org/api.php");
    }

    #[test]
    fn test_build_archiver_url() {
        let client = EhClientBuilder::new()
            .base_url("https://e-hentai.org")
            .build();
        let url = client.build_archiver_url(123456, "abcdef0123", "780x");
        assert_eq!(
            url,
            "https://e-hentai.org/archiver.php?gid=123456&token=abcdef0123&or=780x"
        );
    }

    #[test]
    fn archive_download_cookie_host_check_only_matches_eh_domains() {
        assert!(is_ehentai_host(
            "https://e-hentai.org/archive/1/abc/file/0?start=1"
        ));
        assert!(is_ehentai_host(
            "https://exhentai.org/archive/1/abc/file/0?start=1"
        ));
        assert!(!is_ehentai_host(
            "http://127.0.0.1/archive/1/abc/file/0?start=1"
        ));
        assert!(!is_ehentai_host(
            "https://example.com/archive/1/abc/file/0?start=1"
        ));
    }

    #[test]
    fn test_made_progress_threshold() {
        // Exactly 10KB/s (10240 bytes in 1.0s) → NOT progress (strictly greater)
        assert!(!made_progress(10240, 1.0));
        // One byte above threshold → progress
        assert!(made_progress(10241, 1.0));
        // Zero bytes → never progress
        assert!(!made_progress(0, 1.0));
        // Zero elapsed → false (prevents division by zero)
        assert!(!made_progress(99999, 0.0));
        // Large transfer, small elapsed → progress
        assert!(made_progress(20000, 0.5));
        // Small transfer, large elapsed → no progress
        assert!(!made_progress(100, 10.0));
    }
}
