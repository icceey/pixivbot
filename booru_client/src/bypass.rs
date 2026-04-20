//! Bot-protection bypass via an external FlareSolverr proxy.
//!
//! FlareSolverr is a separately-deployed HTTP service that drives a real
//! browser (Chromium) to solve Cloudflare's JavaScript challenge,
//! Cloudflare Turnstile auto-pass, and similar low-interaction challenges.
//! See: <https://github.com/FlareSolverr/FlareSolverr>
//!
//! This module is intentionally narrow: it only supports the `request.get`
//! command with an optional reusable session id. It does NOT solve
//! interactive captchas (reCAPTCHA v2, hCaptcha visible challenges); those
//! require a paid third-party solver service.
//!
//! Note: Image downloads from booru sites are issued by Telegram's servers
//! when the bot sends a media URL. Therefore this bypass only applies to
//! the JSON API requests this crate makes; image hotlinks are unaffected.
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Configuration for routing API requests through FlareSolverr.
#[derive(Debug, Clone)]
pub struct BypassConfig {
    /// FlareSolverr v1 endpoint, e.g. `http://flaresolverr:8191/v1`.
    pub endpoint: String,
    /// Maximum time FlareSolverr is allowed to spend on a single solve, in ms.
    pub max_timeout_ms: u32,
    /// Optional FlareSolverr session id. Reusing a session keeps cookies and
    /// the underlying browser tab warm across requests, dramatically reducing
    /// per-request latency. Sessions must be created out-of-band via the
    /// FlareSolverr `sessions.create` command; this client does not manage
    /// session lifecycle.
    pub session: Option<String>,
}

impl BypassConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            max_timeout_ms: 60_000,
            session: None,
        }
    }

    pub fn with_max_timeout_ms(mut self, ms: u32) -> Self {
        self.max_timeout_ms = ms;
        self
    }

    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }
}

#[derive(Serialize)]
struct FlareRequest<'a> {
    cmd: &'a str,
    url: String,
    #[serde(rename = "maxTimeout")]
    max_timeout: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<&'a str>,
}

#[derive(Deserialize)]
struct FlareResponse {
    status: String,
    #[serde(default)]
    message: String,
    solution: Option<FlareSolution>,
}

#[derive(Deserialize)]
struct FlareSolution {
    status: u16,
    response: String,
}

/// Sends `target_url` (already fully-qualified with query string) through
/// FlareSolverr. Returns `(upstream_status, upstream_body)` where the body
/// has had FlareSolverr's `<html><body><pre>...</pre></body></html>` wrapper
/// stripped (when present) and the small set of HTML entities the wrapper
/// emits decoded.
pub(crate) async fn solve(
    http: &reqwest::Client,
    cfg: &BypassConfig,
    target_url: &str,
) -> Result<(u16, String)> {
    let body = FlareRequest {
        cmd: "request.get",
        url: target_url.to_string(),
        max_timeout: cfg.max_timeout_ms,
        session: cfg.session.as_deref(),
    };

    // Allow FlareSolverr a few seconds beyond its own internal timeout before
    // we give up on the HTTP call to it.
    let req = http
        .post(&cfg.endpoint)
        .json(&body)
        .timeout(Duration::from_millis(cfg.max_timeout_ms as u64 + 5_000));

    let resp = req.send().await?;
    let raw = resp.text().await?;

    let parsed: FlareResponse = serde_json::from_str(&raw).map_err(|e| {
        tracing::debug!("FlareSolverr returned non-JSON envelope: {}", raw);
        Error::Api {
            message: format!("FlareSolverr response parse error: {}", e),
            status: 0,
        }
    })?;

    if parsed.status != "ok" {
        return Err(Error::Api {
            message: format!("FlareSolverr error: {}", parsed.message),
            status: 0,
        });
    }

    let solution = parsed.solution.ok_or_else(|| Error::Api {
        message: "FlareSolverr response missing 'solution'".to_string(),
        status: 0,
    })?;

    Ok((solution.status, strip_html_wrapper(&solution.response)))
}

/// FlareSolverr wraps non-HTML upstream bodies (such as JSON API responses)
/// inside `<html>...<pre>BODY</pre>...</html>` because the browser displays
/// them that way. Strip the wrapper if present and decode the entities the
/// wrapper escapes.
fn strip_html_wrapper(body: &str) -> String {
    let trimmed = body.trim();
    let inner = match (trimmed.find("<pre"), trimmed.rfind("</pre>")) {
        (Some(start), Some(end)) if end > start => trimmed[start..]
            .find('>')
            .map(|i| start + i + 1)
            .filter(|&i| i <= end)
            .map(|i| &trimmed[i..end])
            .unwrap_or(trimmed),
        _ => trimmed,
    };
    decode_html_entities(inner)
}

fn decode_html_entities(s: &str) -> String {
    // Decode the small fixed set of entities the FlareSolverr wrapper emits.
    // &amp; must be decoded last so we don't double-decode strings that
    // legitimately contained the literal sequence "&amp;...".
    let intermediate = s
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");
    intermediate.replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_pre_wrapper_extracts_inner_text() {
        let wrapped = r#"<html><head></head><body><pre style="word-wrap: break-word;">{"a":1}</pre></body></html>"#;
        assert_eq!(strip_html_wrapper(wrapped), "{\"a\":1}");
    }

    #[test]
    fn strip_pre_wrapper_decodes_entities() {
        let wrapped = r#"<pre>{&quot;url&quot;:&quot;a&amp;b&quot;}</pre>"#;
        assert_eq!(strip_html_wrapper(wrapped), r#"{"url":"a&b"}"#);
    }

    #[test]
    fn strip_pre_wrapper_returns_unwrapped_when_no_pre() {
        assert_eq!(strip_html_wrapper("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn strip_pre_wrapper_handles_surrounding_whitespace() {
        let wrapped = "  <html><body><pre>data</pre></body></html>  ";
        assert_eq!(strip_html_wrapper(wrapped), "data");
    }

    #[test]
    fn strip_pre_wrapper_no_double_decode_amp() {
        // A response that legitimately contains "&amp;" must not become "&".
        // (Wrapper would have escaped it as "&amp;amp;" if it were a literal
        // ampersand — so seeing a single "&amp;" means a literal "&".)
        assert_eq!(strip_html_wrapper("<pre>a&amp;b</pre>"), "a&b");
    }

    #[test]
    fn bypass_config_builder() {
        let cfg = BypassConfig::new("http://x:8191/v1")
            .with_max_timeout_ms(30_000)
            .with_session("s1");
        assert_eq!(cfg.endpoint, "http://x:8191/v1");
        assert_eq!(cfg.max_timeout_ms, 30_000);
        assert_eq!(cfg.session.as_deref(), Some("s1"));
    }

    #[test]
    fn bypass_config_defaults() {
        let cfg = BypassConfig::new("http://x:8191/v1");
        assert_eq!(cfg.max_timeout_ms, 60_000);
        assert!(cfg.session.is_none());
    }
}
