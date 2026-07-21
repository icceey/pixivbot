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

#[cfg(test)]
mod tests {
    use super::{archive_get, archive_http_error, is_ehentai_host};
    use crate::error::Error;
    use crate::models::EhCookies;
    use reqwest::header::COOKIE;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn is_ehentai_host_matches_only_exact_eh_domains() {
        assert!(is_ehentai_host(
            "https://e-hentai.org/archive/1/abc/file/0?start=1"
        ));
        assert!(is_ehentai_host(
            "https://exhentai.org/archive/1/abc/file/0?start=1"
        ));
        assert!(!is_ehentai_host(
            "https://sub.e-hentai.org/archive/1/abc/file/0?start=1"
        ));
        assert!(!is_ehentai_host(
            "http://127.0.0.1/archive/1/abc/file/0?start=1"
        ));
        assert!(!is_ehentai_host(
            "https://example.com/archive/1/abc/file/0?start=1"
        ));
    }

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

        let request = archive_get(&http, &cookies, "https://example.com/archive/1")
            .build()
            .unwrap();
        assert!(request.headers().get(COOKIE).is_none());
    }

    #[tokio::test]
    async fn archive_http_error_removes_sensitive_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/archive"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let url = format!("{}/archive?token=secret-token", server.uri());

        let error = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap_err();
        assert_eq!(error.url().map(reqwest::Url::as_str), Some(url.as_str()));

        let Error::Http(error) = archive_http_error(error) else {
            unreachable!("archive HTTP errors remain HTTP errors");
        };
        assert!(error.url().is_none());
        assert!(!error.to_string().contains("secret-token"));
    }
}
