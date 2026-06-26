use crate::models::EhGalleryRef;
use regex::Regex;
use std::sync::OnceLock;

fn search_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<a\s+href="(https?://(?:e-hentai|exhentai)\.org/g/(\d+)/([0-9a-f]+)/?)"[^>]*?>[\s\S]*?<div\s+class="glink">(.*?)</div>"#)
            .expect("invalid search regex")
    })
}

fn archiver_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"archiver\.php\?gid=\d+&token=[0-9a-f]+&or=([0-9]+--[0-9a-f]+)"#)
            .expect("invalid archiver_key regex")
    })
}

fn archive_redirect_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"document\.location\s*=\s*["'](https?://[^"']+/archive/[^"']+)["']"#)
            .expect("invalid archive_redirect regex")
    })
}

/// Parse search results HTML, extracting gallery references.
/// `base_url` is used to construct full gallery URLs if the HTML uses relative paths.
pub fn parse_search_results(html: &str, _base_url: &str) -> Vec<EhGalleryRef> {
    let re = search_re();
    re.captures_iter(html)
        .filter_map(|cap| {
            let url = cap.get(1)?.as_str().to_string();
            let gid: u64 = cap.get(2)?.as_str().parse().ok()?;
            let token = cap.get(3)?.as_str().to_string();
            let title = cap.get(4)?.as_str().trim().to_string();
            // posted_ts is not easily extractable from search HTML without date parsing;
            // the metadata API will provide it. Set to 0 as placeholder.
            Some(EhGalleryRef {
                gid,
                token,
                title,
                url,
                posted_ts: 0,
            })
        })
        .collect()
}

/// Extract the archiver_key from a gallery HTML page.
/// Returns None if no archiver link is found.
pub fn parse_archiver_key(html: &str) -> Option<String> {
    let re = archiver_key_re();
    re.captures(html)
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()))
}

/// Extract the archive download URL from the archiver.php HTML response.
/// Replaces `autostart=1` with `start=1` in the redirect URL.
/// Returns None if no redirect is found.
pub fn parse_archive_redirect(html: &str) -> Option<String> {
    let re = archive_redirect_re();
    re.captures(html)
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .map(|url| url.replace("autostart=1", "start=1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_HTML_SAMPLE: &str = r#"
    <div class="gl1t">
      <a href="https://e-hentai.org/g/123456/abcdef0123/">
        <img src="https://ehgt.org/t/abc.jpg" />
      </a>
      <div class="gl3t">
        <div class="glink">Sample Gallery Title</div>
      </div>
    </div>
    <div class="gl1t">
      <a href="https://e-hentai.org/g/789012/987654abcd/">
        <img src="https://ehgt.org/t/def.jpg" />
      </a>
      <div class="gl3t">
        <div class="glink">Second Gallery</div>
      </div>
    </div>
    "#;

    #[test]
    fn test_parse_search_results() {
        let results = parse_search_results(SEARCH_HTML_SAMPLE, "https://e-hentai.org");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].gid, 123456);
        assert_eq!(results[0].token, "abcdef0123");
        assert_eq!(results[0].title, "Sample Gallery Title");
        assert_eq!(results[1].gid, 789012);
        assert_eq!(results[1].token, "987654abcd");
    }

    #[test]
    fn test_parse_search_results_empty() {
        let results = parse_search_results(
            "<html><body>No results</body></html>",
            "https://e-hentai.org",
        );
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_archiver_key() {
        let html = r#"
        <a href="https://e-hentai.org/archiver.php?gid=123456&token=abcdef0123&or=470592--63bbddc729b849100ec24ab920ffdb84b6542b23">
          Archive Download
        </a>
        "#;
        let key = parse_archiver_key(html).expect("should find archiver key");
        assert_eq!(key, "470592--63bbddc729b849100ec24ab920ffdb84b6542b23");
    }

    #[test]
    fn test_parse_archiver_key_not_found() {
        let html = "<html><body>No archiver link</body></html>";
        assert!(parse_archiver_key(html).is_none());
    }

    #[test]
    fn test_parse_archive_redirect() {
        let html = r#"
        <script type="text/javascript">
        function gotonext() {
            document.getElementById("continue").innerHTML = "Please wait...";
            document.location = "http://123.45.67.89/archive/123456/abcdef0123/abcdef0123/0?autostart=1";
        }
        </script>
        "#;
        let url = parse_archive_redirect(html).expect("should find redirect URL");
        assert_eq!(
            url,
            "http://123.45.67.89/archive/123456/abcdef0123/abcdef0123/0?start=1"
        );
    }

    #[test]
    fn test_parse_archive_redirect_not_found() {
        let html = "<html><body>No redirect</body></html>";
        assert!(parse_archive_redirect(html).is_none());
    }
}
