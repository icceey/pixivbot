use crate::models::EhGalleryRef;
use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiverForm {
    pub action: String,
    pub fields: Vec<(String, String)>,
}

fn resolution_dltype(resolution: &str) -> &'static str {
    if resolution.is_empty() || resolution == "original" {
        "org"
    } else {
        "res"
    }
}

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

fn archiver_form_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<form\b([^>]*)>(.*?)</form>"#).expect("invalid archiver_form regex")
    })
}

fn input_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?is)<input\b([^>]*)>"#).expect("invalid input regex"))
}

fn form_fields(body: &str) -> Vec<(String, String)> {
    input_re()
        .captures_iter(body)
        .filter_map(|input| {
            let attrs = input.get(1)?.as_str();
            let name = attr_value(attrs, "name")?;
            if name.is_empty() {
                return None;
            }
            let value = attr_value(attrs, "value").unwrap_or_default();
            Some((name, value))
        })
        .collect()
}

fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r#"(?is)\b{}\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
        regex::escape(name)
    ))
    .expect("invalid attr regex");
    let cap = re.captures(attrs)?;
    cap.get(1)
        .or_else(|| cap.get(2))
        .or_else(|| cap.get(3))
        .map(|m| decode_html_attr(m.as_str()))
}

fn decode_html_attr(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&#x26;", "&")
        .replace("&#X26;", "&")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&#X22;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&#X27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
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

/// Extract the direct archiver download form for a validated resolution.
///
/// `original` selects `dltype=org`; supported resamples (`780x`, `980x`, and
/// `1280x`) select `dltype=res`. The separate H@H Downloader form is ignored.
pub fn parse_archiver_form(html: &str, resolution: &str) -> Option<ArchiverForm> {
    let target_dltype = resolution_dltype(resolution);

    for cap in archiver_form_re().captures_iter(html) {
        let attrs = cap.get(1)?.as_str();
        let body = cap.get(2)?.as_str();
        let Some(action) = attr_value(attrs, "action") else {
            continue;
        };
        if !action.contains("archiver.php") {
            continue;
        }

        let fields = form_fields(body);

        if fields
            .iter()
            .any(|(name, value)| name == "dltype" && value == target_dltype)
        {
            return Some(ArchiverForm { action, fields });
        }
    }

    None
}

/// Extract the archive download URL from the archiver.php HTML response.
/// Normalizes the redirect URL so H@H starts the archive download.
/// Returns None if no redirect is found.
pub fn parse_archive_redirect(html: &str) -> Option<String> {
    let re = archive_redirect_re();
    re.captures(html)
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .map(|url| {
            let url = url.replace("autostart=1", "start=1");
            if url.contains("start=1") {
                url
            } else if url.contains('?') {
                format!("{url}&start=1")
            } else {
                format!("{url}?start=1")
            }
        })
}

/// Cost classification for an archiver.php download form.
///
/// Returned by `parse_archive_download_cost`. Callers may POST only permitted
/// costs; `Insufficient`, `Unavailable`, and `Unknown` temporarily defer without POST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadCost {
    /// `Free!` - no GP will be charged.
    Free,
    /// `You unlocked a resample download of this archive on <date>` was present
    /// and the requested resolution is a resample. POST is safe; no GP charged.
    Unlocked,
    /// `{n} GP` - POST will charge `n` GP (auto-converts credits if GP insufficient).
    Gp(u64),
    /// `Insufficient Funds` - account lacks GP and credits to auto-convert.
    /// Callers temporarily defer without POST.
    Insufficient,
    /// `N/A` - the selected resolution is unavailable.
    /// Callers temporarily defer without POST.
    Unavailable,
    /// Could not parse the cost text. Callers temporarily defer without POST.
    Unknown,
}

impl DownloadCost {
    /// Returns true if POSTing this form will not charge GP.
    pub fn is_free(&self) -> bool {
        matches!(self, DownloadCost::Free | DownloadCost::Unlocked)
    }

    /// Returns the GP cost if this variant is `Gp(n)`, else None.
    pub fn gp_amount(&self) -> Option<u64> {
        match self {
            DownloadCost::Gp(n) => Some(*n),
            _ => None,
        }
    }
}

fn unlocked_resample_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `You unlocked a <strong>resample</strong> download of this archive on <strong>{date}</strong>`
        Regex::new(
            r#"(?is)You unlocked a\s*<strong>\s*resample\s*</strong>\s*download of this archive"#,
        )
        .expect("invalid unlocked_resample regex")
    })
}

/// Match `Download Cost: &nbsp; <strong>{cost}</strong>` text and return the inner cost string.
fn download_cost_strong_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)Download Cost:\s*(?:&nbsp;)?\s*<strong[^>]*>(.*?)</strong>"#)
            .expect("invalid download_cost_strong regex")
    })
}

/// Match a `dltype` hidden input value inside a form.
fn dltype_value_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<input[^>]*name=["']dltype["'][^>]*value=["']([^"']+)["']"#)
            .expect("invalid dltype_value regex")
    })
}

fn estimated_size_strong_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?is)Estimated\s+Size\s*:\s*(?:&nbsp;|&#160;|&#xA0;)?\s*<strong[^>]*>\s*([^<]*?)\s*</strong>"#,
        )
        .expect("invalid estimated size regex")
    })
}

fn mib_size_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"^\s*(\d+)(?:\.(\d+))?\s+MiB\s*$"#).expect("invalid MiB size regex")
    })
}

/// Convert a decimal MiB value to bytes, rounding up so the caller never
/// underestimates the displayed archive size.
fn parse_mib_size_bytes(text: &str) -> Option<u64> {
    const MIB_BYTES: u128 = 1024 * 1024;

    let captures = mib_size_re().captures(text)?;
    let whole_mib: u128 = captures.get(1)?.as_str().parse().ok()?;
    let fraction = captures.get(2).map_or("", |value| value.as_str());
    let denominator = 10_u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let fractional_mib: u128 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().ok()?
    };
    let numerator = whole_mib
        .checked_mul(denominator)?
        .checked_add(fractional_mib)?;
    let bytes = numerator.checked_mul(MIB_BYTES)?.div_ceil(denominator);

    u64::try_from(bytes).ok()
}

fn parse_form_estimated_size(html: &str, target_dltype: &str) -> Option<u64> {
    for form in archiver_form_re().captures_iter(html) {
        let attrs = form.get(1)?.as_str();
        let body = form.get(2)?.as_str();
        let Some(action) = attr_value(attrs, "action") else {
            continue;
        };
        if !action.contains("archiver.php") {
            continue;
        }
        if !form_fields(body)
            .iter()
            .any(|(name, value)| name == "dltype" && value == target_dltype)
        {
            continue;
        }

        // `parse_archiver_form` selects the first matching form. Do not bind a
        // missing size on it to a later form's displayed size.
        let after_form = &html[form.get(0)?.end()..];
        let before_next_form = match archiver_form_re().find(after_form) {
            Some(next_form) => &after_form[..next_form.start()],
            None => after_form,
        };
        return estimated_size_strong_re()
            .captures(before_next_form)
            .and_then(|captures| captures.get(1))
            .and_then(|size| parse_mib_size_bytes(size.as_str()));
    }
    None
}

fn parse_cost_text(text: &str) -> DownloadCost {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("Free!") || trimmed.eq_ignore_ascii_case("Free") {
        DownloadCost::Free
    } else if trimmed.eq_ignore_ascii_case("Insufficient Funds") {
        DownloadCost::Insufficient
    } else if trimmed.eq_ignore_ascii_case("N/A") || trimmed.is_empty() {
        DownloadCost::Unavailable
    } else {
        // Pattern: `{n} GP` where n may contain thousand separators (commas).
        let gp_re = Regex::new(r"(?i)^\s*([\d,]+)\s*GP\s*$").expect("invalid gp text regex");
        if let Some(cap) = gp_re.captures(trimmed) {
            if let Some(m) = cap.get(1) {
                let digits: String = m.as_str().chars().filter(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = digits.parse::<u64>() {
                    return DownloadCost::Gp(n);
                }
            }
        }
        DownloadCost::Unknown
    }
}

fn parse_form_download_cost(html: &str, target_dltype: &str) -> DownloadCost {
    let cost_caps: Vec<_> = download_cost_strong_re().captures_iter(html).collect();
    let dltype_caps: Vec<_> = dltype_value_re().captures_iter(html).collect();

    for cost_cap in &cost_caps {
        let cost_end = cost_cap.get(0).unwrap().end();
        if let Some(dltype_cap) = dltype_caps
            .iter()
            .find(|dltype_cap| dltype_cap.get(0).unwrap().start() >= cost_end)
        {
            let dltype = dltype_cap.get(1).unwrap().as_str();
            if dltype == target_dltype {
                return parse_cost_text(cost_cap.get(1).unwrap().as_str());
            }
        }
    }

    if cost_caps.len() == 1 {
        return parse_cost_text(cost_caps[0].get(1).unwrap().as_str());
    }

    DownloadCost::Unknown
}

/// Parse the GP/cost status of an archiver.php page for the given resolution.
///
/// `resolution` is expected to have passed client validation:
/// - `"original"` -> the original-archive form (`dltype=org`)
/// - `"780x"`, `"980x"`, or `"1280x"` -> the direct resample form (`dltype=res`)
///
/// The separate H@H Downloader form and table are ignored.
///
/// Resolution selection matches the form that `prepare_archive_download` will
/// actually POST, so the returned cost reflects what the server will charge.
///
/// Returns `DownloadCost::Unknown` if the page structure cannot be recognized
/// (e.g. neither `dltype=org` nor `dltype=res` form is present). Callers
/// temporarily defer without POST rather than treating that state as a permanent
/// archive-policy failure.
pub fn parse_archive_download_cost(html: &str, resolution: &str) -> DownloadCost {
    let target_dltype = resolution_dltype(resolution);

    if target_dltype == "res" && unlocked_resample_re().is_match(html) {
        return DownloadCost::Unlocked;
    }

    parse_form_download_cost(html, target_dltype)
}

/// Parse the selected archive's estimated size in bytes from an archiver.php page.
///
/// Selection follows `parse_archiver_form` and `parse_archive_download_cost`:
/// `original` uses `dltype=org`; validated resamples (`780x`, `980x`, and
/// `1280x`) use the direct `dltype=res` form. The separate H@H Downloader table
/// is ignored.
/// Returned byte counts round the displayed decimal MiB value up.
pub fn parse_archive_download_estimated_size(html: &str, resolution: &str) -> Option<u64> {
    parse_form_estimated_size(html, resolution_dltype(resolution))
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
    fn test_parse_archiver_form_ignores_hathdl_for_direct_resamples() {
        let html = r#"
        <form id="original" method="post" action="https://exhentai.org/archiver.php?gid=4034806&amp;token=org123def0">
           <input type="hidden" name="dltype" value="org" />
           <input type="hidden" name="org_sentinel" value="original-only" />
           <input type="submit" name="dlcheck" value="Download Original Archive" />
        </form>
        <form id="resample" method="post" action="https://exhentai.org/archiver.php?gid=4034806&amp;token=res123def0">
           <input type="hidden" name="dltype" value="res" />
           <input type="hidden" name="res_sentinel" value="resample-only" />
           <input type="submit" name="dlcheck" value="Download Resample Archive" />
        </form>
        <form id="hathdl_form" method="post" action="https://exhentai.org/archiver.php?gid=4034806&amp;token=hathdl123def0">
           <input type="hidden" name="hathdl_sentinel" value="hathdl-only" />
           <input type="hidden" name="hathdl_xres" value="" />
        </form>
        "#;

        let form = parse_archiver_form(html, "original").expect("should parse original form");
        assert_eq!(
            form.action,
            "https://exhentai.org/archiver.php?gid=4034806&token=org123def0"
        );
        assert_eq!(
            form.fields,
            vec![
                ("dltype".to_string(), "org".to_string()),
                ("org_sentinel".to_string(), "original-only".to_string()),
                (
                    "dlcheck".to_string(),
                    "Download Original Archive".to_string()
                ),
            ]
        );

        for resolution in ["780x", "980x", "1280x"] {
            let form = parse_archiver_form(html, resolution).expect("should parse resample form");
            assert_eq!(
                form.action,
                "https://exhentai.org/archiver.php?gid=4034806&token=res123def0"
            );
            assert!(form
                .fields
                .contains(&("res_sentinel".to_string(), "resample-only".to_string())));
            assert!(form
                .fields
                .contains(&("dltype".to_string(), "res".to_string())));
            assert!(!form.fields.iter().any(|(name, _)| name == "hathdl_xres"));
        }
    }

    #[test]
    fn test_parse_archiver_form_uses_resample_for_supported_resolutions() {
        let html = r#"
        <div>Download Cost: &nbsp; <strong>218 GP</strong></div>
        <form method="post" action="https://exhentai.org/archiver.php?gid=4034806&amp;token=res123def0">
           <input type="hidden" name="dltype" value="res" />
           <input type="hidden" name="res_sentinel" value="resample-only" />
           <input type="submit" name="dlcheck" value="Download Resample Archive" />
        </form>
        "#;

        for resolution in ["780x", "980x", "1280x"] {
            let form =
                parse_archiver_form(html, resolution).expect("should parse generic resample form");
            assert!(form
                .fields
                .contains(&("res_sentinel".to_string(), "resample-only".to_string())));
            assert_eq!(
                parse_archive_download_cost(html, resolution),
                DownloadCost::Gp(218)
            );
        }
    }

    #[test]
    fn test_hathdl_forms_do_not_affect_direct_resamples() {
        let prefix = r#"
<div>Download Cost: &nbsp; <strong>218 GP</strong></div>
<form method="post" action="https://exhentai.org/archiver.php?gid=1&amp;token=res">
    <input type="hidden" name="dltype" value="res" />
    <input type="hidden" name="res_sentinel" value="generic-res" />
</form>
"#;
        let table = r#"
<table><tr>
    <td><p>Original</p><p>419.6 MiB</p><p>8,800 GP</p></td>
    <td><p>800x</p><p>10.38 MiB</p><p>114 GP</p></td>
    <td><p>1280x</p><p>10.38 MiB</p><p>218 GP</p></td>
    <td><p>1920x</p><p>10.38 MiB</p><p>376 GP</p></td>
    <td><p>2560x</p><p>10.38 MiB</p><p>546 GP</p></td>
</tr></table>
"#;
        let invalid_forms = [
            r#"<form id="hathdl_form"><input name="hathdl_xres" value="" /></form>"#,
            r#"<form id="hathdl_form" action="https://exhentai.org/archiver.php?gid=1&amp;token=hathdl"><input name="other" value="" /></form>"#,
        ];

        for invalid_form in invalid_forms {
            let html = format!("{prefix}{invalid_form}{table}");
            let form = parse_archiver_form(&html, "1280x")
                .expect("low resample should fall back to generic form");
            assert!(form
                .fields
                .contains(&("res_sentinel".to_string(), "generic-res".to_string())));
            assert_eq!(
                parse_archive_download_cost(&html, "1280x"),
                DownloadCost::Gp(218)
            );
            assert_eq!(
                parse_archive_download_cost(&html, "980x"),
                DownloadCost::Gp(218)
            );
        }
    }

    #[test]
    fn test_parse_archiver_form_missing_requested_dltype_returns_none() {
        let html = r#"
        <form method="post" action="https://exhentai.org/archiver.php?gid=4034806&amp;token=org123def0">
           <input type="hidden" name="dltype" value="org" />
           <input type="hidden" name="org_sentinel" value="original-only" />
        </form>
        "#;

        assert!(parse_archiver_form(html, "1280x").is_none());
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
    fn test_parse_archive_redirect_adds_start_when_missing() {
        let html = r#"
        <script type="text/javascript">
        document.location = "https://hath.example/archive/4034806/hash/file/0";
        </script>
        "#;
        let url = parse_archive_redirect(html).expect("should find redirect URL");
        assert_eq!(
            url,
            "https://hath.example/archive/4034806/hash/file/0?start=1"
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

    // ---- parse_archive_download_cost tests ----

    const ARCHIVER_FREE_RESAMPLE_UNLOCKED: &str = r##"
<div id="db">
<div style="position:relative; width:370px; margin:4px auto 4px; padding-top:3px">
    <div style="width:180px; float:left">
        <div style="text-align:center; margin-top:4px">Download Cost: &nbsp; <strong>Free!</strong></div>
        <form action="https://exhentai.org/archiver.php?gid=4053260&amp;token=53ad37062b" method="post">
            <input type="hidden" name="dltype" value="org" />
            <div style="margin:3px auto"><input type="submit" name="dlcheck" value="Download Original Archive" style="width:180px" /></div>
        </form>
        <p>Estimated Size: &nbsp; <strong>28.52 MiB</strong></p>
    </div>
    <div style="width:180px; float:right">
        <div style="text-align:center; margin-top:4px">Download Cost: &nbsp; <strong>Free!</strong></div>
        <form action="https://exhentai.org/archiver.php?gid=4053260&amp;token=53ad37062b" method="post">
            <input type="hidden" name="dltype" value="res" />
            <div style="margin:3px auto"><input type="submit" name="dlcheck" value="Download Resample Archive" style="width:180px" /></div>
        </form>
        <p>Estimated Size: &nbsp; <strong>2.33 MiB</strong></p>
    </div>
    <div style="clear:both"></div>
</div>
<p>You unlocked a <strong>resample</strong> download of this archive on <strong>2026-07-17 20:26</strong> &nbsp;[<a href="#" onclick="return cancel_sessions()">cancel</a>]</p>
</div>
"##;

    const ARCHIVER_FREE_DEFAULT: &str = r#"
<div id="db">
<div style="position:relative; width:370px; margin:4px auto 4px; padding-top:3px">
    <div style="width:180px; float:left">
        <div style="text-align:center; margin-top:4px">Download Cost: &nbsp; <strong>Free!</strong></div>
        <form action="https://exhentai.org/archiver.php?gid=3635201&amp;token=30c972f597" method="post">
            <input type="hidden" name="dltype" value="org" />
            <div style="margin:3px auto"><input type="submit" name="dlcheck" value="Download Original Archive" style="width:180px" /></div>
        </form>
        <p>Estimated Size: &nbsp; <strong>40.98 MiB</strong></p>
    </div>
    <div style="width:180px; float:right">
        <div style="text-align:center; margin-top:4px">Download Cost: &nbsp; <strong>Free!</strong></div>
        <form action="https://exhentai.org/archiver.php?gid=3635201&amp;token=30c972f597" method="post">
            <input type="hidden" name="dltype" value="res" />
            <div style="margin:3px auto"><input type="submit" name="dlcheck" value="Download Resample Archive" style="width:180px" /></div>
        </form>
        <p>Estimated Size: &nbsp; <strong>3.60 MiB</strong></p>
    </div>
    <div style="clear:both"></div>
</div>
</div>
"#;

    const ARCHIVER_EHENTAI_FUNDS: &str = r#"
<div id="db">
<p>Current Funds:</p><p>13,468,433 GP [<a href="https://ehwiki.org/wiki/Gallery_Points" target="ehwiki">?</a>] &nbsp; 5,199 Credits [<a href="https://ehwiki.org/wiki/Credits" target="ehwiki">?</a>]</p>
<div style="position:relative; width:370px; margin:4px auto 4px; padding-top:3px">
    <div style="width:180px; float:left">
        <div style="text-align:center; margin-top:4px">Download Cost: &nbsp; <strong>Free!</strong></div>
        <form action="https://e-hentai.org/archiver.php?gid=4006273&amp;token=d2ccf02433" method="post">
            <input type="hidden" name="dltype" value="org" />
            <div style="margin:3px auto"><input type="submit" name="dlcheck" value="Download Original Archive" style="width:180px" /></div>
        </form>
        <p>Estimated Size: &nbsp; <strong>127.0 MiB</strong></p>
    </div>
    <div style="width:180px; float:right">
        <div style="text-align:center; margin-top:4px">Download Cost: &nbsp; <strong>Free!</strong></div>
        <form action="https://e-hentai.org/archiver.php?gid=4006273&amp;token=d2ccf02433" method="post">
            <input type="hidden" name="dltype" value="res" />
            <div style="margin:3px auto"><input type="submit" name="dlcheck" value="Download Resample Archive" style="width:180px" /></div>
        </form>
        <p>Estimated Size: &nbsp; <strong>7.29 MiB</strong></p>
    </div>
    <div style="clear:both"></div>
</div>
</div>
"#;

    const ARCHIVER_GP_REQUIRED: &str = r#"
<div id="db">
<div style="position:relative; width:370px; margin:4px auto 4px; padding-top:3px">
    <div style="width:180px; float:left">
        <div style="text-align:center; margin-top:4px">Download Cost: &nbsp; <strong>8,800 GP</strong></div>
        <form action="https://exhentai.org/archiver.php?gid=2284788&amp;token=7841d194d4" method="post">
            <input type="hidden" name="dltype" value="org" />
            <div style="margin:3px auto"><input type="submit" name="dlcheck" value="Download Original Archive" style="width:180px" /></div>
        </form>
        <p>Estimated Size: &nbsp; <strong>419.6 MiB</strong></p>
    </div>
    <div style="width:180px; float:right">
        <div style="text-align:center; margin-top:4px">Download Cost: &nbsp; <strong>218 GP</strong></div>
        <form action="https://exhentai.org/archiver.php?gid=2284788&amp;token=7841d194d4" method="post">
            <input type="hidden" name="dltype" value="res" />
            <div style="margin:3px auto"><input type="submit" name="dlcheck" value="Download Resample Archive" style="width:180px" /></div>
        </form>
        <p>Estimated Size: &nbsp; <strong>10.38 MiB</strong></p>
    </div>
    <div style="clear:both"></div>
</div>
</div>
"#;

    const ARCHIVER_GP_REQUIRED_WITH_HATHDL: &str = r#"
<div id="db">
    <div>Download Cost: &nbsp; <strong>8,800 GP</strong></div>
    <form action="https://exhentai.org/archiver.php?gid=2284788&amp;token=7841d194d4" method="post">
        <input type="hidden" name="dltype" value="org" />
    </form>
    <div>Download Cost: &nbsp; <strong>218 GP</strong></div>
    <form action="https://exhentai.org/archiver.php?gid=2284788&amp;token=7841d194d4" method="post">
        <input type="hidden" name="dltype" value="res" />
    </form>
    <form id="hathdl_form" action="https://exhentai.org/archiver.php?gid=2284788&amp;token=7841d194d4" method="post">
        <input type="hidden" id="hathdl_xres" name="hathdl_xres" value="" />
    </form>
    <table>
        <tr>
            <td><p><strong>Original</strong></p><p>419.6 MiB</p><p><strong>8,800 GP</strong></p></td>
            <td><p><strong>800x</strong></p><p>10.38 MiB</p><p><strong>114 GP</strong></p></td>
            <td><p><strong>1280x</strong></p><p>10.38 MiB</p><p><strong>218 GP</strong></p></td>
            <td><p><strong>1920x</strong></p><p>10.38 MiB</p><p><strong>376 GP</strong></p></td>
            <td><p><strong>2560x</strong></p><p>10.38 MiB</p><p><strong>546 GP</strong></p></td>
        </tr>
    </table>
</div>
"#;

    #[test]
    fn test_parse_archive_download_cost_free_original() {
        let cost = parse_archive_download_cost(ARCHIVER_FREE_DEFAULT, "original");
        assert_eq!(cost, DownloadCost::Free);
    }

    #[test]
    fn test_parse_archive_download_cost_free_resample() {
        let cost = parse_archive_download_cost(ARCHIVER_FREE_DEFAULT, "1280x");
        assert_eq!(cost, DownloadCost::Free);
    }

    #[test]
    fn test_parse_archive_download_cost_unlocked_resample() {
        let html = format!(
            r#"{ARCHIVER_FREE_RESAMPLE_UNLOCKED}
<form id="hathdl_form"><input type="hidden" name="hathdl_xres" value="" /></form>
<table><tr><td><p>1280x</p><p>2.33 MiB</p><p>218 GP</p></td></tr></table>"#
        );
        let cost = parse_archive_download_cost(&html, "1280x");
        assert_eq!(cost, DownloadCost::Unlocked);
    }

    #[test]
    fn test_parse_archive_download_cost_unlocked_marker_ignored_for_original() {
        // Even though the resample is unlocked, original downloads are NOT
        // automatically free. We fall through to the original form cost.
        let cost = parse_archive_download_cost(ARCHIVER_FREE_RESAMPLE_UNLOCKED, "original");
        assert_eq!(cost, DownloadCost::Free);
    }

    #[test]
    fn test_parse_archive_download_cost_ehentai_funds_original() {
        let cost = parse_archive_download_cost(ARCHIVER_EHENTAI_FUNDS, "original");
        assert_eq!(cost, DownloadCost::Free);
    }

    #[test]
    fn test_parse_archive_download_cost_gp_required_original() {
        let cost = parse_archive_download_cost(ARCHIVER_GP_REQUIRED, "original");
        assert_eq!(cost, DownloadCost::Gp(8800));
    }

    #[test]
    fn test_parse_archive_download_cost_gp_required_resample() {
        let cost = parse_archive_download_cost(ARCHIVER_GP_REQUIRED, "1280x");
        assert_eq!(cost, DownloadCost::Gp(218));
    }

    #[test]
    fn test_parse_archive_download_cost_ignores_hathdl_resolution_costs() {
        let html = format!(
            r#"<table><tr><td><p>1920x</p><p>irrelevant</p><p>Free</p></td></tr></table>{ARCHIVER_GP_REQUIRED_WITH_HATHDL}"#
        );
        for resolution in ["780x", "980x", "1280x"] {
            assert_eq!(
                parse_archive_download_cost(&html, resolution),
                DownloadCost::Gp(218),
                "unexpected cost for {resolution}"
            );
        }
    }

    #[test]
    fn test_parse_archive_download_cost_original_uses_form_cost_with_hathdl_table() {
        let html = ARCHIVER_GP_REQUIRED_WITH_HATHDL.replacen(
            r#"<td><p><strong>Original</strong></p><p>419.6 MiB</p><p><strong>8,800 GP</strong></p></td>"#,
            r#"<td><p><strong>Original</strong></p><p>419.6 MiB</p><p><strong>N/A</strong></p></td>"#,
            1,
        );

        assert_eq!(
            parse_archive_download_cost(&html, "original"),
            DownloadCost::Gp(8800)
        );
        assert_eq!(
            parse_archive_download_cost(&html, ""),
            DownloadCost::Gp(8800)
        );
    }

    #[test]
    fn test_parse_archive_download_cost_strips_thousand_separators() {
        let html = r#"
<div>Download Cost: &nbsp; <strong>747,708 GP</strong></div>
<form action="/archiver.php?gid=1&amp;token=abc" method="post">
    <input type="hidden" name="dltype" value="org" />
</form>
"#;
        let cost = parse_archive_download_cost(html, "original");
        assert_eq!(cost, DownloadCost::Gp(747708));
    }

    #[test]
    fn test_parse_archive_download_cost_insufficient_funds() {
        let html = r#"
<div>Download Cost: &nbsp; <strong>Insufficient Funds</strong></div>
<form action="/archiver.php?gid=1&amp;token=abc" method="post">
    <input type="hidden" name="dltype" value="org" />
</form>
"#;
        let cost = parse_archive_download_cost(html, "original");
        assert_eq!(cost, DownloadCost::Insufficient);
    }

    #[test]
    fn test_parse_archive_download_cost_na() {
        let html = r#"
<div>Download Cost: &nbsp; <strong>N/A</strong></div>
<form action="/archiver.php?gid=1&amp;token=abc" method="post">
    <input type="hidden" name="dltype" value="org" />
</form>
"#;
        let cost = parse_archive_download_cost(html, "original");
        assert_eq!(cost, DownloadCost::Unavailable);
    }

    #[test]
    fn test_parse_archive_download_cost_unknown_text() {
        let html = r#"
<div>Download Cost: &nbsp; <strong>Somewhere Over The Rainbow</strong></div>
<form action="/archiver.php?gid=1&amp;token=abc" method="post">
    <input type="hidden" name="dltype" value="org" />
</form>
"#;
        let cost = parse_archive_download_cost(html, "original");
        assert_eq!(cost, DownloadCost::Unknown);
    }

    #[test]
    fn test_parse_archive_download_cost_missing_returns_unknown() {
        let html = "<html><body>No archiver content</body></html>";
        let cost = parse_archive_download_cost(html, "original");
        assert_eq!(cost, DownloadCost::Unknown);
    }

    #[test]
    fn test_parse_archive_download_estimated_size_uses_matching_generic_form() {
        assert_eq!(
            parse_archive_download_estimated_size(ARCHIVER_FREE_DEFAULT, "original"),
            Some(42_970_645),
        );
        assert_eq!(
            parse_archive_download_estimated_size(ARCHIVER_FREE_DEFAULT, "1280x"),
            Some(3_774_874),
        );
    }

    #[test]
    fn test_parse_archive_download_estimated_size_ignores_hathdl_table() {
        let html = r#"
            <form action="/archiver.php?gid=1&amp;token=abc" method="post">
                <input type="hidden" name="dltype" value="res" />
            </form>
            <p>Estimated Size: <strong>2.33 MiB</strong></p>
            <form id="hathdl_form" action="/archiver.php?gid=1&amp;token=abc" method="post">
                <input type="hidden" name="hathdl_xres" value="" />
            </form>
            <table><tr>
                <td><p>Original</p><p>400.0 MiB</p><p>8,800 GP</p></td>
                <td><p>800x</p><p>8.0 MiB</p><p>114 GP</p></td>
                <td><p>1280x</p><p>12.5 MiB</p><p>218 GP</p></td>
                <td><p>1920x</p><p>19.25 MiB</p><p>376 GP</p></td>
                <td><p>2560x</p><p>25.0 MiB</p><p>546 GP</p></td>
            </tr></table>
        "#;

        assert_eq!(
            parse_archive_download_estimated_size(html, "980x"),
            Some(2_443_183),
        );
        assert_eq!(
            parse_archive_download_estimated_size(html, "1280x"),
            Some(2_443_183),
        );
    }

    #[test]
    fn test_parse_archive_download_estimated_size_rounds_decimal_mib_up() {
        let html = r#"
            <form action="/archiver.php?gid=1&amp;token=abc" method="post">
                <input type="hidden" name="dltype" value="org" />
            </form>
            <p>Estimated Size: <strong>1.0000001 MiB</strong></p>
        "#;

        assert_eq!(
            parse_archive_download_estimated_size(html, "original"),
            Some(1_048_577),
        );
    }

    #[test]
    fn test_parse_archive_download_estimated_size_rejects_untrusted_or_invalid_values() {
        let invalid_value = r#"
            <form action="/archiver.php?gid=1&amp;token=abc" method="post">
                <input type="hidden" name="dltype" value="org" />
            </form>
            <p>Estimated Size: <strong>NaN MiB</strong></p>
        "#;
        assert_eq!(
            parse_archive_download_estimated_size(invalid_value, "original"),
            None,
        );
        assert_eq!(
            parse_archive_download_estimated_size(
                &invalid_value.replace("NaN MiB", "2.33 MB"),
                "original",
            ),
            None,
        );
        assert_eq!(
            parse_archive_download_estimated_size(
                &invalid_value.replace("NaN MiB", "-1 MiB"),
                "original",
            ),
            None,
        );
        assert_eq!(
            parse_archive_download_estimated_size(
                &invalid_value.replace("NaN MiB", "18446744073709551615 MiB"),
                "original",
            ),
            None,
        );

        let other_form = r#"
            <form action="/archiver.php?gid=1&amp;token=abc" method="post">
                <input type="hidden" name="dltype" value="res" />
            </form>
            <form action="/archiver.php?gid=1&amp;token=abc" method="post">
                <input type="hidden" name="dltype" value="res" />
            </form>
            <p>Estimated Size: <strong>2.33 MiB</strong></p>
        "#;
        assert_eq!(
            parse_archive_download_estimated_size(other_form, "1280x"),
            None,
        );
    }

    #[test]
    fn test_download_cost_is_free() {
        assert!(DownloadCost::Free.is_free());
        assert!(DownloadCost::Unlocked.is_free());
        assert!(!DownloadCost::Gp(0).is_free());
        assert!(!DownloadCost::Insufficient.is_free());
        assert!(!DownloadCost::Unavailable.is_free());
        assert!(!DownloadCost::Unknown.is_free());
    }

    #[test]
    fn test_download_cost_gp_amount() {
        assert_eq!(DownloadCost::Gp(218).gp_amount(), Some(218));
        assert_eq!(DownloadCost::Free.gp_amount(), None);
        assert_eq!(DownloadCost::Unknown.gp_amount(), None);
    }
}
