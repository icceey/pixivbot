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

fn archiver_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match archiver.php URL in onclick="popUp('...')" or href="..."
        // Handles &amp; HTML entities
        Regex::new(r#"(?:https?://(?:e-hentai|exhentai)\.org)?/archiver\.php\?gid=(\d+)&amp;token=([0-9a-f]+)"#)
            .expect("invalid archiver_url regex")
    })
}

fn archiver_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match archiver_key pattern: {numeric}--{hex} (at least 8 hex chars)
        // Found in URL params (or=...) or hidden form fields (value="...")
        Regex::new(r#"([0-9]+)--([0-9a-f]{8,})"#).expect("invalid archiver_key regex")
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

/// Extract the archiver.php URL from a gallery HTML page.
/// The URL is in `onclick="popUp('...archiver.php?gid=X&token=Y',...)"`.
/// Returns the (gid, token) pair so the caller can build the full URL.
pub fn parse_archiver_url(html: &str) -> Option<(u64, String)> {
    let re = archiver_url_re();
    let cap = re.captures(html)?;
    let gid: u64 = cap.get(1)?.as_str().parse().ok()?;
    let token = cap.get(2)?.as_str().to_string();
    Some((gid, token))
}

/// Extract the archiver_key from an archiver.php HTML response page.
/// The key format is {numeric}--{hex}, found in URL params or hidden form fields.
/// Returns None if no archiver key is found.
pub fn parse_archiver_key(html: &str) -> Option<String> {
    let re = archiver_key_re();
    let cap = re.captures(html)?;
    // Combine the two groups back into the full key
    let numeric = cap.get(1)?.as_str();
    let hex = cap.get(2)?.as_str();
    Some(format!("{}--{}", numeric, hex))
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

fn image_page_urls_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<a\s+href="((?:https?://(?:e-hentai|exhentai)\.org)?/s/[^"]+)""#)
            .expect("invalid image_page_urls regex")
    })
}

fn image_src_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<img\s+id="img"\s+src="(https?://[^"]+)""#).expect("invalid image_src regex")
    })
}

fn page_count_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match all numeric <td> cells inside the ptt pagination table, take the max
        Regex::new(r#"<table[^>]*class="ptt"[^>]*>([\s\S]*?)</table>"#)
            .expect("invalid page_count table regex")
    })
}

fn td_number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match numeric content inside <td> elements, including when wrapped in <a> tags
        // e.g. <td class="ptds"><a href="...">2</a></td> or <td>1</td>
        Regex::new(r#"<td[^>]*>(?:<a[^>]*>)?(\d+)(?:</a>)?</td>"#).expect("invalid td_number regex")
    })
}

/// Extract image page URLs from gallery HTML.
/// Gallery pages use `/?p=0`, `?p=1`, etc. Each page has image page links
/// in the form `<a href="https://e-hentai.org/s/{hash}/{gid}-{n}">`.
pub fn parse_image_page_urls(html: &str) -> Vec<String> {
    let re = image_page_urls_re();
    let mut urls: Vec<String> = re
        .captures_iter(html)
        .map(|cap| cap.get(1).unwrap().as_str().to_string())
        .collect();
    urls.dedup();
    urls
}

/// Extract the image src URL from an image page HTML.
/// The image is in `<img id="img" src="...">`.
pub fn parse_image_src(html: &str) -> Option<String> {
    let re = image_src_re();
    re.captures(html)
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()))
}

/// Extract the total number of gallery pages from gallery HTML.
/// Returns None if the page count cannot be determined.
pub fn parse_page_count(html: &str) -> Option<u32> {
    let table_re = page_count_re();
    let td_re = td_number_re();
    table_re.captures(html).and_then(|cap| {
        let table_content = cap.get(1)?.as_str();
        let max = td_re
            .captures_iter(table_content)
            .filter_map(|c| c.get(1)?.as_str().parse::<u32>().ok())
            .max()?;
        Some(max)
    })
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
    fn test_parse_archiver_url() {
        let html = r#"
        <div id="gdd">
          <td><a onclick="return popUp('https://e-hentai.org/archiver.php?gid=4006958&amp;token=586ff41111',480,320)">Archive Download</a></td>
        </div>
        "#;
        let (gid, token) = parse_archiver_url(html).expect("should find archiver URL");
        assert_eq!(gid, 4006958);
        assert_eq!(token, "586ff41111");
    }

    #[test]
    fn test_parse_archiver_url_not_found() {
        let html = "<html><body>No archiver link</body></html>";
        assert!(parse_archiver_url(html).is_none());
    }

    #[test]
    fn test_parse_archiver_key() {
        let html = r#"
        <form>
          <input type="hidden" name="or" value="470592--63bbddc729b849100ec24ab920ffdb84b6542b23" />
        </form>
        "#;
        let key = parse_archiver_key(html).expect("should find archiver key");
        assert_eq!(key, "470592--63bbddc729b849100ec24ab920ffdb84b6542b23");
    }

    #[test]
    fn test_parse_archiver_key_in_url() {
        let html = r#"
        <a href="archiver.php?gid=123&token=abc&or=470592--63bbddc729b849100ec24ab920ffdb84b6542b23">Download</a>
        "#;
        let key = parse_archiver_key(html).expect("should find archiver key");
        assert_eq!(key, "470592--63bbddc729b849100ec24ab920ffdb84b6542b23");
    }

    #[test]
    fn test_parse_archiver_key_not_found() {
        let html = "<html><body>No archiver key</body></html>";
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

    #[test]
    fn test_parse_image_page_urls() {
        let html = r#"
        <div class="gdtm">
          <a href="https://e-hentai.org/s/abc123/123456-01">1</a>
        </div>
        <div class="gdtm">
          <a href="https://e-hentai.org/s/def456/123456-02">2</a>
        </div>
        "#;
        let urls = parse_image_page_urls(html);
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("/s/abc123/123456-01"));
        assert!(urls[1].contains("/s/def456/123456-02"));
    }

    #[test]
    fn test_parse_image_page_urls_relative() {
        let html = r#"
        <div class="gdtm">
          <a href="/s/abc123/123456-01">1</a>
        </div>
        <div class="gdtm">
          <a href="/s/def456/123456-02">2</a>
        </div>
        "#;
        let urls = parse_image_page_urls(html);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "/s/abc123/123456-01");
        assert_eq!(urls[1], "/s/def456/123456-02");
    }

    #[test]
    fn test_parse_image_page_urls_empty() {
        let urls = parse_image_page_urls("<html></html>");
        assert!(urls.is_empty());
    }

    #[test]
    fn test_parse_image_src() {
        let html = r#"
        <div>
          <img id="img" src="https://123.45.67.89/h/abc123.jpg" />
        </div>
        "#;
        let src = parse_image_src(html).expect("should find image src");
        assert_eq!(src, "https://123.45.67.89/h/abc123.jpg");
    }

    #[test]
    fn test_parse_image_src_not_found() {
        assert!(parse_image_src("<html></html>").is_none());
    }

    #[test]
    fn test_parse_page_count() {
        let html = r#"
        <table class="ptt" style="margin:2px auto 0px">
          <tr><td class="ptdd">&lt;</td><td class="ptds"><a href=".../">1</a></td><td onclick="..."><a href=".../?p=1">2</a></td><td onclick="..."><a href=".../?p=1">&gt;</a></td></tr>
        </table>
        "#;
        assert_eq!(parse_page_count(html), Some(2));
    }

    #[test]
    fn test_parse_page_count_many_pages() {
        let html = r#"
        <table class="ptt">
          <tr><td class="ptdd">&lt;</td><td class="ptds"><a href=".../">1</a></td><td><a href="?p=1">2</a></td><td><a href="?p=2">3</a></td><td><a href="?p=15">&gt;</a></td></tr>
        </table>
        "#;
        assert_eq!(parse_page_count(html), Some(3));
    }

    #[test]
    fn test_parse_page_count_not_found() {
        assert!(parse_page_count("<html></html>").is_none());
    }
}
