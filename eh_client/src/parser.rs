use crate::models::EhGalleryRef;
use regex::Regex;
use std::sync::LazyLock;

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

static SEARCH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a\s+href="(https?://(?:e-hentai|exhentai)\.org/g/(\d+)/([0-9a-f]+)/?)"[^>]*?>[\s\S]*?<div\s+class="glink">(.*?)</div>"#)
        .expect("invalid search regex")
});

static ARCHIVER_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:https?://(?:e-hentai|exhentai)\.org)?/archiver\.php\?gid=(\d+)&amp;token=([0-9a-f]+)"#)
        .expect("invalid archiver_url regex")
});

static ARCHIVER_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"([0-9]+)--([0-9a-f]{8,})"#).expect("invalid archiver_key regex")
});

static ARCHIVE_REDIRECT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"document\.location\s*=\s*["'](https?://[^"']+/archive/[^"']+)["']"#)
        .expect("invalid archive_redirect regex")
});

static ARCHIVER_FORM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<form\b([^>]*)>(.*?)</form>"#).expect("invalid archiver_form regex")
});

static INPUT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<input\b([^>]*)>"#).expect("invalid input regex"));

fn form_fields(body: &str) -> Vec<(String, String)> {
    INPUT_RE
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
pub fn parse_search_results(html: &str, _base_url: &str) -> Vec<EhGalleryRef> {
    SEARCH_RE
        .captures_iter(html)
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
    let cap = ARCHIVER_URL_RE.captures(html)?;
    let gid: u64 = cap.get(1)?.as_str().parse().ok()?;
    let token = cap.get(2)?.as_str().to_string();
    Some((gid, token))
}

/// Extract the archiver_key from an archiver.php HTML response page.
/// The key format is {numeric}--{hex}, found in URL params or hidden form fields.
/// Returns None if no archiver key is found.
pub fn parse_archiver_key(html: &str) -> Option<String> {
    let cap = ARCHIVER_KEY_RE.captures(html)?;
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
    let form = find_archiver_form(html, resolution_dltype(resolution))?;
    Some(ArchiverForm {
        action: attr_value(form.get(1)?.as_str(), "action")?,
        fields: form_fields(form.get(2)?.as_str()),
    })
}

fn find_archiver_form<'a>(html: &'a str, target_dltype: &str) -> Option<regex::Captures<'a>> {
    for cap in ARCHIVER_FORM_RE.captures_iter(html) {
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
            return Some(cap);
        }
    }

    None
}

pub(crate) fn archive_form_is_available(html: &str, resolution: &str) -> bool {
    let dltype = resolution_dltype(resolution);
    let Some(form) = find_archiver_form(html, dltype) else {
        return false;
    };
    if parse_form_download_cost(html, dltype) == DownloadCost::Unavailable {
        return false;
    }

    // Consume complete attributes so "disabled" inside a quoted value or an
    // attribute such as data-disabled does not disable the download button.
    static ATTRIBUTE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)([^\s=/>]+)(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+))?"#)
            .expect("invalid attribute regex")
    });
    INPUT_RE.captures_iter(&form[2]).any(|input| {
        attr_value(&input[1], "name").as_deref() == Some("dlcheck")
            && !ATTRIBUTE_RE
                .captures_iter(&input[1])
                .any(|attr| attr[1].eq_ignore_ascii_case("disabled"))
    })
}

/// Extract the archive download URL from the archiver.php HTML response.
/// Normalizes the redirect URL so H@H starts the archive download.
/// Returns None if no redirect is found.
pub fn parse_archive_redirect(html: &str) -> Option<String> {
    ARCHIVE_REDIRECT_RE
        .captures(html)
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

static UNLOCKED_RESAMPLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)You unlocked a\s*<strong>\s*resample\s*</strong>\s*download of this archive"#,
    )
    .expect("invalid unlocked_resample regex")
});

/// Match `Download Cost: &nbsp; <strong>{cost}</strong>` text and return the inner cost string.
static DOWNLOAD_COST_STRONG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)Download Cost:\s*(?:&nbsp;)?\s*<strong[^>]*>(.*?)</strong>"#)
        .expect("invalid download_cost_strong regex")
});

/// Match a `dltype` hidden input value inside a form.
static DLTYPE_VALUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<input[^>]*name=["']dltype["'][^>]*value=["']([^"']+)["']"#)
        .expect("invalid dltype_value regex")
});

static ESTIMATED_SIZE_STRONG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)Estimated\s+Size\s*:\s*(?:&nbsp;|&#160;|&#xA0;)?\s*<strong[^>]*>\s*([^<]*?)\s*</strong>"#,
    )
    .expect("invalid estimated size regex")
});

static MIB_SIZE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(\d+)(?:\.(\d+))?\s+MiB\s*$"#).expect("invalid MiB size regex")
});

/// Convert a decimal MiB value to bytes, rounding up so the caller never
/// underestimates the displayed archive size.
fn parse_mib_size_bytes(text: &str) -> Option<u64> {
    const MIB_BYTES: u128 = 1024 * 1024;

    let captures = MIB_SIZE_RE.captures(text)?;
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
    let form = find_archiver_form(html, target_dltype)?;
    // Do not bind a missing size on the selected form to a later form's size.
    let after_form = &html[form.get(0)?.end()..];
    let before_next_form = match ARCHIVER_FORM_RE.find(after_form) {
        Some(next_form) => &after_form[..next_form.start()],
        None => after_form,
    };
    ESTIMATED_SIZE_STRONG_RE
        .captures(before_next_form)
        .and_then(|captures| captures.get(1))
        .and_then(|size| parse_mib_size_bytes(size.as_str()))
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
    let cost_caps: Vec<_> = DOWNLOAD_COST_STRONG_RE.captures_iter(html).collect();
    let dltype_caps: Vec<_> = DLTYPE_VALUE_RE.captures_iter(html).collect();

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

    // A lone unbound cost can describe a legacy key-only page, but must not
    // price a different direct-download form (especially after a fallback).
    if cost_caps.len() == 1 && dltype_caps.is_empty() {
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
/// Pass the selected resolution after any fallback so the returned cost
/// reflects the form that will actually be posted.
///
/// Returns `DownloadCost::Unknown` if the page structure cannot be recognized
/// (e.g. neither `dltype=org` nor `dltype=res` form is present). Callers
/// temporarily defer without POST rather than treating that state as a permanent
/// archive-policy failure.
pub fn parse_archive_download_cost(html: &str, resolution: &str) -> DownloadCost {
    let target_dltype = resolution_dltype(resolution);

    if target_dltype == "res" && UNLOCKED_RESAMPLE_RE.is_match(html) {
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

static IMAGE_PAGE_URLS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a\s+href="((?:https?://(?:e-hentai|exhentai)\.org)?/s/[^"]+)""#)
        .expect("invalid image_page_urls regex")
});

static IMAGE_SRC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<img\s+id="img"\s+src="(https?://[^"]+)""#).expect("invalid image_src regex")
});

static PAGE_COUNT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<table[^>]*class="ptt"[^>]*>([\s\S]*?)</table>"#)
        .expect("invalid page_count table regex")
});

static TD_NUMBER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<td[^>]*>(?:<a[^>]*>)?(\d+)(?:</a>)?</td>"#).expect("invalid td_number regex")
});

/// Extract image page URLs from gallery HTML.
/// Gallery pages use `/?p=0`, `?p=1`, etc. Each page has image page links
/// in the form `<a href="https://e-hentai.org/s/{hash}/{gid}-{n}">`.
pub fn parse_image_page_urls(html: &str) -> Vec<String> {
    let mut urls: Vec<String> = IMAGE_PAGE_URLS_RE
        .captures_iter(html)
        .map(|cap| cap.get(1).unwrap().as_str().to_string())
        .collect();
    urls.dedup();
    urls
}

/// Extract the image src URL from an image page HTML.
/// The image is in `<img id="img" src="...">`.
pub fn parse_image_src(html: &str) -> Option<String> {
    IMAGE_SRC_RE
        .captures(html)
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()))
}

/// Extract the total number of gallery pages from gallery HTML.
/// Returns None if the page count cannot be determined.
pub fn parse_page_count(html: &str) -> Option<u32> {
    PAGE_COUNT_RE.captures(html).and_then(|cap| {
        let table_content = cap.get(1)?.as_str();
        let max = TD_NUMBER_RE
            .captures_iter(table_content)
            .filter_map(|c| c.get(1)?.as_str().parse::<u32>().ok())
            .max()?;
        Some(max)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_archive_download_cost_original_uses_form_cost_with_hathdl_table() {
        let html = r#"
            <div>Download Cost: <strong>8,800 GP</strong></div>
            <form action="/archiver.php"><input name="dltype" value="org" /></form>
            <div>Download Cost: <strong>218 GP</strong></div>
            <form action="/archiver.php"><input name="dltype" value="res" /></form>
            <form id="hathdl_form"><input name="hathdl_xres" value="" /></form>
            <table><tr><td><strong>Original</strong></td><td><strong>N/A</strong></td></tr></table>
        "#;
        assert_eq!(
            parse_archive_download_cost(html, "original"),
            DownloadCost::Gp(8800)
        );
    }

    #[test]
    fn test_parse_archive_download_estimated_size_rejects_untrusted_or_invalid_values() {
        let oversized = r#"
            <form action="/archiver.php?gid=1&amp;token=abc" method="post">
                <input type="hidden" name="dltype" value="org" />
            </form>
            <p>Estimated Size: <strong>18446744073709551615 MiB</strong></p>
        "#;
        assert_eq!(
            parse_archive_download_estimated_size(oversized, "original"),
            None
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
}
