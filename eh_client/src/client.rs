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

    /// Get the archiver_key for a gallery by scraping its HTML page.
    pub async fn get_archiver_key(&self, gid: u64, token: &str) -> Result<String> {
        let url = format!("{}/g/{}/{}/", self.base_url, gid, token);
        let resp = self
            .http
            .get(&url)
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
        let html = resp.text().await?;
        parser::parse_archiver_key(&html)
            .ok_or_else(|| Error::Parse("archiver_key not found in gallery page".into()))
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

        let form_data = if want_original {
            vec![("dlcheck", "Download Original Archive")]
        } else {
            vec![("dlcheck", "Download Resample Archive")]
        };

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
        Ok(total)
    }

    /// Get the base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
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
