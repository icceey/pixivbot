use crate::error::Error;
use crate::models::EhCookies;
use reqwest::header::COOKIE;

pub(super) fn is_ehentai_host(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "e-hentai.org" | "exhentai.org"))
}

pub(super) fn archive_get<'a>(
    http: &'a reqwest::Client,
    cookies: &'a EhCookies,
    url: &'a str,
) -> reqwest::RequestBuilder {
    let request = http.get(url);
    if is_ehentai_host(url) {
        request.header(COOKIE, cookies.to_header())
    } else {
        request
    }
}

pub(crate) fn archive_http_error(error: reqwest::Error) -> Error {
    Error::Http(error.without_url())
}

pub(super) fn parse_content_range_header(value: &str) -> Option<(u64, u64, Option<u64>)> {
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

#[cfg(test)]
mod tests {
    use super::archive_get;
    use crate::models::EhCookies;
    use reqwest::header::COOKIE;

    #[test]
    fn archive_get_adds_cookies_only_for_eh_hosts() {
        let http = reqwest::Client::new();
        let cookies = EhCookies {
            ipb_member_id: Some("member".into()),
            ipb_pass_hash: Some("pass".into()),
            igneous: Some("igneous".into()),
            nw: true,
        };

        for url in [
            "https://e-hentai.org/archive/1",
            "https://exhentai.org/archive/1",
        ] {
            let request = archive_get(&http, &cookies, url).build().unwrap();
            assert_eq!(
                request.headers().get(COOKIE).unwrap(),
                "ipb_member_id=member; ipb_pass_hash=pass; igneous=igneous; nw=1"
            );
        }

        for url in [
            "https://example.com/archive/1",
            "https://sub.e-hentai.org/archive/1",
            "http://127.0.0.1/archive/1",
        ] {
            let request = archive_get(&http, &cookies, url).build().unwrap();
            assert!(request.headers().get(COOKIE).is_none(), "{url}");
        }
    }
}
