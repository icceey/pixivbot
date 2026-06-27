use crate::error::{Error, Result};
use crate::models::{EhCookies, EhGallery, EhGalleryRef, RawApiResponse};
use crate::parser;
use reqwest::header::COOKIE;
use std::path::Path;
use tokio::io::AsyncWriteExt;

const USER_AGENT_STR: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub struct EhClient {
    http: reqwest::Client,
    base_url: String,
    pub(crate) api_url: String,
    cookies: EhCookies,
}

impl EhClient {
    pub fn new(base_url: &str, api_url: &str, cookies: EhCookies) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT_STR)
            .timeout(std::time::Duration::from_secs(60));

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
        Ok(raw
            .gmetadata
            .into_iter()
            .map(|m| m.into_gallery())
            .collect())
    }

    /// Get the archiver_key for a gallery.
    /// Step 1: Scrape gallery page for archiver.php URL (in onclick attribute).
    /// Step 2: GET the archiver.php URL and parse the response for the archiver_key.
    pub async fn get_archiver_key(&self, gid: u64, token: &str) -> Result<String> {
        // Step 1: Fetch gallery page to find archiver.php URL
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

        // Extract archiver.php URL from onclick attribute
        let (_archiver_gid, _archiver_token) = parser::parse_archiver_url(&gallery_html)
            .ok_or_else(|| Error::Parse("archiver URL not found in gallery page".into()))?;

        // Step 2: GET archiver.php to get the actual archiver_key
        let archiver_page_url =
            format!("{}/archiver.php?gid={}&token={}", self.base_url, gid, token);
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

        // Parse the archiver_key from the response
        parser::parse_archiver_key(&archiver_html)
            .ok_or_else(|| Error::Parse("archiver_key not found in archiver.php response".into()))
    }

    /// Download a gallery archive ZIP to the specified path.
    /// `archiver_key` is obtained from `get_archiver_key`.
    /// `resolution` controls quality: "780x"/"980x"/"1280x" (free resamples),
    /// "1600x"/"2400x" (donors), "original" (costs GP).
    pub async fn download_archive(
        &self,
        gid: u64,
        token: &str,
        archiver_key: &str,
        resolution: &str,
        dest: &Path,
    ) -> Result<u64> {
        // Step 1: POST to archiver.php to initiate download
        let archiver_url = self.build_archiver_url(gid, token, archiver_key);

        // Determine whether to download original or resampled archive
        let want_original = resolution == "original" || resolution.is_empty();

        // Build form data with hathdl_xres to select resolution
        let xres_val = if want_original {
            "org".to_string()
        } else {
            resolution.trim_end_matches('x').to_string()
        };

        let form_data = vec![
            (
                "dlcheck",
                if want_original {
                    "Download Original Archive"
                } else {
                    "Download Resample Archive"
                },
            ),
            ("hathdl_xres", xres_val.as_str()),
        ];

        let resp = self
            .http
            .post(&archiver_url)
            .header(COOKIE, self.cookies.to_header())
            .form(&form_data)
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

        // Step 3: Download the ZIP file — stream to disk, no cookies to H@H server
        let mut resp = self
            .http
            .get(&download_url)
            .timeout(std::time::Duration::from_secs(300))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("archive download returned {}", status),
                status: status.as_u16(),
            });
        }

        // Step 4: Stream to file (avoids loading entire archive into memory)
        let mut file = tokio::fs::File::create(dest).await?;
        let mut total: u64 = 0;
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk).await?;
            total += chunk.len() as u64;
        }
        file.flush().await?;
        drop(file);

        // Step 5: Validate that we got a real ZIP (not an error HTML page)
        validate_zip_magic(dest).await?;

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

        // Determine total pages (galleries split across multiple HTML pages)
        let total_pages = parser::parse_page_count(&gallery_html).unwrap_or(1);

        // Step 2: Collect all image page URLs from all gallery pages
        let mut all_image_urls: Vec<String> = parser::parse_image_page_urls(&gallery_html);

        for page_num in 1..total_pages {
            // Rate limit between page fetches
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
            all_image_urls.extend(urls);
        }

        // Deduplicate image URLs (preserve order, remove non-consecutive dups)
        let mut seen = std::collections::HashSet::new();
        all_image_urls.retain(|url| seen.insert(url.clone()));

        if all_image_urls.is_empty() {
            return Err(Error::Parse("no image page URLs found".into()));
        }

        // Step 3: Create ZIP and download each image into it
        let zip_file = tokio::fs::File::create(dest).await?;
        let mut zip_writer =
            zip::ZipWriter::new(std::io::BufWriter::new(zip_file.into_std().await));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        let mut total_bytes: u64 = 0;
        let mut images_downloaded: usize = 0;

        for (idx, image_page_url) in all_image_urls.iter().enumerate() {
            // Rate limit between image fetches (e-hentai limits ~5000/day)
            if idx > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }

            // Fetch the image page to get the actual image URL
            let resp = match self
                .http
                .get(image_page_url.as_str())
                .header(COOKIE, self.cookies.to_header())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    // Image page fetch failed, skip this image
                    let _ = e;
                    continue;
                }
            };

            if !resp.status().is_success() {
                continue;
            }

            let html = resp.text().await?;
            let image_url = match parser::parse_image_src(&html) {
                Some(u) => u,
                None => continue,
            };

            // Download the actual image (no cookies to image CDN)
            let img_resp = match self.http.get(&image_url).send().await {
                Ok(r) => r,
                Err(_) => continue,
            };

            if !img_resp.status().is_success() {
                continue;
            }

            // Get image bytes
            let img_bytes = img_resp.bytes().await?;

            // Stream image into ZIP entry
            let ext = image_url
                .rsplit('.')
                .next()
                .filter(|e| e.len() <= 4)
                .unwrap_or("jpg");
            let entry_name = format!("{:04}.{}", idx + 1, ext);

            zip_writer.start_file(&entry_name, options)?;
            std::io::Write::write_all(&mut zip_writer, &img_bytes)?;
            total_bytes += img_bytes.len() as u64;
            images_downloaded += 1;
        }

        zip_writer.finish()?;

        if images_downloaded == 0 {
            return Err(Error::Other(
                "all image downloads failed, no images in ZIP".into(),
            ));
        }

        Ok(total_bytes)
    }
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

/// Validate that a file starts with ZIP magic bytes (PK\x03\x04).
/// Prevents error HTML pages from being sent as "archive" files.
async fn validate_zip_magic(path: &Path) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut header = [0u8; 4];
    use std::io::ErrorKind;
    match file.read(&mut header).await {
        Ok(n) if n >= 4 => {
            if &header == b"PK\x03\x04" {
                Ok(())
            } else {
                Err(Error::Parse(
                    "downloaded file is not a valid ZIP (invalid magic bytes)".into(),
                ))
            }
        }
        Ok(_) => Err(Error::Parse("downloaded file too small to be a ZIP".into())),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
            Err(Error::Parse("downloaded file too small to be a ZIP".into()))
        }
        Err(e) => Err(e.into()),
    }
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
}
