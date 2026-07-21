use crate::archive_download::{
    archive_http_error, download_to_partial, ArchiveArtifacts, ArchiveDownloadOptions,
};
use crate::error::{Error, Result};
use crate::models::{EhCookies, EhGallery, EhGalleryRef, RawApiResponse, RawGalleryMetaEntry};
use crate::parser;
use reqwest::header::COOKIE;
use std::path::Path;

const USER_AGENT_STR: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const ARCHIVE_CONNECT_TIMEOUT_SECS: u64 = 30;
const ARCHIVE_READ_TIMEOUT_SECS: u64 = 60;

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
    /// Estimated bytes for the archive selected by this request's form.
    /// `None` means the archiver page did not provide a trustworthy size.
    estimated_size_bytes: Option<u64>,
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
            estimated_size_bytes: None,
        }
    }

    fn from_archiver_form(
        base_url: &str,
        form: parser::ArchiverForm,
        resolution: &str,
        cost: parser::DownloadCost,
        estimated_size_bytes: Option<u64>,
    ) -> Self {
        let mut form_data = form.fields;
        apply_resolution_to_form_data(&mut form_data, resolution);
        Self {
            action_url: resolve_url(base_url, &form.action),
            form_data,
            cost,
            estimated_size_bytes,
        }
    }

    /// The parsed download cost for this request. Callers should check this
    /// before calling `download_archive_with_request()` to avoid spending GP.
    pub fn cost(&self) -> &parser::DownloadCost {
        &self.cost
    }

    /// Estimated bytes for this request's selected archive, if the archiver
    /// page supplied a trustworthy value.
    pub fn estimated_size_bytes(&self) -> Option<u64> {
        self.estimated_size_bytes
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
            .await
            .map_err(archive_http_error)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("gallery page returned {}", status),
                status: status.as_u16(),
            });
        }
        let gallery_html = resp.text().await.map_err(archive_http_error)?;

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
            .await
            .map_err(archive_http_error)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("archiver.php returned {}", status),
                status: status.as_u16(),
            });
        }
        let archiver_html = resp.text().await.map_err(archive_http_error)?;

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
        let estimated_size_bytes =
            parser::parse_archive_download_estimated_size(&archiver_html, resolution);

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
            request.estimated_size_bytes = estimated_size_bytes;
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
            estimated_size_bytes,
        ))
    }

    /// Download a gallery archive ZIP to the specified path.
    /// `archiver_key` is obtained from `get_archiver_key`.
    /// `resolution` controls quality: "780x"/"980x"/"1280x" (free resamples),
    /// "1600x"/"2400x" (donors), "original" (costs GP).
    ///
    /// Streams to recoverable temporary state, validates the ZIP, then atomically
    /// renames to `dest`.
    pub async fn download_archive(
        &self,
        gid: u64,
        token: &str,
        archiver_key: &str,
        resolution: &str,
        dest: &Path,
    ) -> Result<u64> {
        self.download_archive_with_options(
            gid,
            token,
            archiver_key,
            resolution,
            dest,
            ArchiveDownloadOptions::default(),
        )
        .await
    }

    /// Download a gallery archive ZIP from a prepared archiver POST request.
    pub async fn download_archive_with_request(
        &self,
        request: &ArchiveDownloadRequest,
        dest: &Path,
    ) -> Result<u64> {
        self.download_archive_with_request_and_options(
            request,
            dest,
            ArchiveDownloadOptions::default(),
        )
        .await
    }

    /// Download a gallery archive ZIP with explicit transfer options.
    pub async fn download_archive_with_options(
        &self,
        gid: u64,
        token: &str,
        archiver_key: &str,
        resolution: &str,
        dest: &Path,
        options: ArchiveDownloadOptions,
    ) -> Result<u64> {
        let request = ArchiveDownloadRequest::from_archiver_key(
            &self.base_url,
            gid,
            token,
            archiver_key,
            resolution,
        );
        self.download_archive_with_request_and_options(&request, dest, options)
            .await
    }

    /// Download from a prepared archiver request with explicit transfer options.
    pub async fn download_archive_with_request_and_options(
        &self,
        request: &ArchiveDownloadRequest,
        dest: &Path,
        options: ArchiveDownloadOptions,
    ) -> Result<u64> {
        let options = options.validate()?;
        // Step 1: POST to archiver.php to initiate download
        let resp = self
            .http
            .post(&request.action_url)
            .header(COOKIE, self.cookies.to_header())
            .form(&request.form_data)
            .send()
            .await
            .map_err(archive_http_error)?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("archiver.php returned {}", status),
                status: status.as_u16(),
            });
        }

        let html = resp.text().await.map_err(archive_http_error)?;

        // Step 2: Parse the JS redirect URL
        let download_url = parser::parse_archive_redirect(&html)
            .ok_or_else(|| Error::Parse("archive redirect URL not found".into()))?;

        // Step 4: Stream to temp file, validate, then rename atomically
        let artifacts = ArchiveArtifacts::new(dest);
        download_to_partial(
            &self.http,
            &self.cookies,
            &download_url,
            &artifacts,
            options,
        )
        .await?;

        // Step 5: Validate that we got a complete ZIP (not an error HTML page or corrupt resume)
        if let Err(e) = validate_complete_zip(artifacts.assembly_scratch()).await {
            let _ = artifacts.remove_multipart_state().await;
            return Err(e);
        }

        let total = tokio::fs::metadata(artifacts.assembly_scratch())
            .await?
            .len();

        // Step 6: Atomically rename temp to final dest
        if let Err(e) = tokio::fs::rename(artifacts.assembly_scratch(), dest).await {
            let _ = tokio::fs::remove_file(artifacts.assembly_scratch()).await;
            return Err(e.into());
        }

        if artifacts.remove_parts_dir().await.is_err() {
            tracing::warn!("could not remove completed archive multipart state");
        }

        Ok(total)
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
    fn archiver_key_request_has_no_estimated_size_without_page_html() {
        let request = ArchiveDownloadRequest::from_archiver_key(
            "https://e-hentai.org",
            123456,
            "abcdef0123",
            "123456--abc123def456",
            "1280x",
        );

        assert_eq!(request.estimated_size_bytes(), None);
    }
}
