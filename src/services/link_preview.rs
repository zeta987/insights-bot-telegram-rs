//! The production [`LinkPreviewer`], ported from Go v1.0.0.
//!
//! Behaviour is pinned to `pkg/linkprev/linkprev.go` and
//! `pkg/opengraph/opengraph.go` at Go commit
//! `02aee8ce260165592e2152eb5a024a602e4eced1`, plus the two libraries they lean
//! on: `github.com/imroc/req/v3@v3.43.7` for the HTTP side and
//! `github.com/PuerkitoBio/goquery@v1.9.2` (over `golang.org/x/net/html`) for
//! the HTML5 side.
//!
//! What Go does, end to end:
//!
//! 1. `req.C().SetUserAgent(..)` builds one client whose User-Agent is a Chrome
//!    111 / Edge 111 string, and `alterRequestForTwitter` swaps in a Googlebot
//!    User-Agent for three Twitter hosts so those servers return server-side
//!    rendered HTML.
//! 2. `request.Get(urlStr)` issues a plain `GET`, following up to ten
//!    redirects (`Client.defaultCheckRedirect`).
//! 3. `resp.IsSuccessState()` accepts `200..=299` and nothing else; every other
//!    status becomes `ErrRequestFailed`, and a transport failure becomes
//!    `ErrNetworkError`.
//! 4. The body is copied whole into a buffer and handed to
//!    `goquery.NewDocumentFromReader`, which is `html.Parse` and therefore
//!    never fails on malformed markup.
//! 5. `newMetaFrom` reads `head > title` and `head > meta[property='og:title']`.
//!
//! # Logging
//!
//! Nothing in this module logs the requested URL, the response body, the parsed
//! title, or any credential. Go's error strings embed the URL and a full
//! request/response dump; those are deliberately dropped here, so an operator
//! sees the failure class and, for a rejected status, the status code.
//!
//! # Deliberate divergence from Go
//!
//! * **Invalid UTF-8.** `golang.org/x/net/html` never validates UTF-8, so a Go
//!   title can carry raw undecodable bytes into the Markdown link. Rust strings
//!   cannot, so the body is decoded with [`String::from_utf8_lossy`] and each
//!   maximal invalid subpart becomes U+FFFD.
//!
//! # Destination policy
//!
//! Go applies none: `req.C()` dials through a stock `net.Dialer` with no
//! `Control` hook, no address filter, and no allow-list, so loopback, private,
//! and link-local destinations are all reachable, and every redirect hop is
//! followed to wherever it points. [`HttpLinkPreviewer::new`] preserves that
//! for parity. [`HttpLinkPreviewer::with_policy`] is the seam where a real
//! policy can be installed later without touching a line of the parsing code.

use std::{
    fmt,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::Result;
use async_trait::async_trait;
use encoding_rs::{Encoding, UTF_8};
use mime::Mime;
use reqwest::Client;
use scraper::{Html, Selector};
use url::Url;

use crate::services::message_capture::{LinkPreviewer, PreviewMeta};

/// Go's `req.C().SetUserAgent(..)` in `linkprev.NewClient`.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/111.0.0.0 Safari/537.36 Edg/111.0.1661.54";

/// Go's `alterRequestForTwitter` override, which asks for server-side rendered
/// HTML instead of the client-rendered shell.
pub const TWITTER_USER_AGENT: &str =
    "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";

/// The exact hosts Go's `lo.Contains` check lists.
pub const TWITTER_SSR_HOSTS: [&str; 3] = ["twitter.com", "vxtwitter.com", "fxtwitter.com"];

/// Go's `Client.defaultCheckRedirect`: "stopped after 10 redirects".
///
/// `reqwest`'s default policy is `Policy::limited(10)`, so an injected default
/// client already matches without configuration.
pub const MAX_REDIRECTS: usize = 10;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The failure classes Go's `linkprev` distinguishes.
///
/// No variant carries the URL or any part of the response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkPreviewError {
    /// Go's `ErrNetworkError`: no response arrived. Covers an unparseable URL,
    /// a dial or TLS failure, too many redirects, and the context deadline.
    Network,
    /// Go's `ErrRequestFailed`: a response arrived outside `200..=299`.
    RequestFailed { status: u16 },
    /// Go's `io.Copy` failure while draining a successful response.
    BodyRead,
    /// No Go counterpart: an injected [`UrlPolicy`] refused the destination.
    PolicyRejected,
}

impl fmt::Display for LinkPreviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network => formatter.write_str("link preview network error"),
            Self::RequestFailed { status } => {
                write!(
                    formatter,
                    "link preview request failed, status code: {status}"
                )
            }
            Self::BodyRead => formatter.write_str("failed to read link preview response body"),
            Self::PolicyRejected => {
                formatter.write_str("link preview destination rejected by policy")
            }
        }
    }
}

impl std::error::Error for LinkPreviewError {}

// ---------------------------------------------------------------------------
// Destination policy seam
// ---------------------------------------------------------------------------

/// Decides whether a destination may be fetched at all.
///
/// This exists because Go has no such check and the parity adapter must not
/// pretend otherwise. Wiring a restrictive policy is a separate decision that
/// belongs to whoever constructs the previewer.
pub trait UrlPolicy: Send + Sync + fmt::Debug {
    /// Called once per preview, before the request leaves the process.
    ///
    /// Redirect hops are *not* re-checked: `reqwest` resolves the chain
    /// internally, exactly as Go's `net/http` does.
    fn admit(&self, url: &Url) -> Result<(), LinkPreviewError>;
}

/// Go's behaviour: every destination is admitted, loopback and private ranges
/// included.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoParityUrlPolicy;

impl UrlPolicy for GoParityUrlPolicy {
    fn admit(&self, _url: &Url) -> Result<(), LinkPreviewError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Previewer
// ---------------------------------------------------------------------------

/// Go's `linkprev.Client`.
#[derive(Clone)]
pub struct HttpLinkPreviewer {
    client: Client,
    policy: Arc<dyn UrlPolicy>,
}

impl fmt::Debug for HttpLinkPreviewer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpLinkPreviewer")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl HttpLinkPreviewer {
    /// Go's `linkprev.NewClient`, over an injected client.
    ///
    /// The destination policy is [`GoParityUrlPolicy`], so this constructor
    /// reaches loopback, private, and link-local addresses just as Go does.
    pub fn new(client: Client) -> Self {
        Self::with_policy(client, Arc::new(GoParityUrlPolicy))
    }

    /// The production construction seam: same parsing, different destination
    /// policy.
    pub fn with_policy(client: Client, policy: Arc<dyn UrlPolicy>) -> Self {
        Self { client, policy }
    }

    /// Go's `alterRequestForTwitter`, reduced to the header it picks.
    pub fn user_agent_for(raw_url: &str) -> &'static str {
        if raw_authority(raw_url).is_some_and(|authority| TWITTER_SSR_HOSTS.contains(&authority)) {
            TWITTER_USER_AGENT
        } else {
            DEFAULT_USER_AGENT
        }
    }

    /// Go's `Client.request`: one `GET`, a status check, then the whole body.
    async fn fetch(&self, url: &str, deadline: Duration) -> Result<String, LinkPreviewError> {
        // Go lets `req` parse the URL inside `request.Get`, and a parse failure
        // there surfaces as `ErrNetworkError`.
        let parsed = Url::parse(url).map_err(|_| LinkPreviewError::Network)?;

        self.policy.admit(&parsed)?;

        // Go chooses the Twitter override from `url.Parse(urlStr).Host`, which
        // preserves host case and an explicit default port. Choose from the
        // raw string before reqwest's WHATWG URL serialization normalises it.
        let user_agent = Self::user_agent_for(url);

        let response = self
            .client
            .get(parsed.clone())
            .header(reqwest::header::USER_AGENT, user_agent)
            // Go attaches the caller's context, which bounds the request and
            // the body read together; `reqwest`'s request timeout does the same.
            .timeout(deadline)
            .send()
            .await
            .map_err(|_| LinkPreviewError::Network)?;

        let status = response.status();
        if !status.is_success() {
            // Go's `IsSuccessState` is `200 <= code <= 299`, which is exactly
            // `StatusCode::is_success`. Go drains the body into its error
            // string here; that body is dropped instead of logged.
            return Err(LinkPreviewError::RequestFailed {
                status: status.as_u16(),
            });
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .bytes()
            .await
            .map_err(|_| LinkPreviewError::BodyRead)?;

        Ok(decode_response_body(&body, content_type.as_deref()))
    }
}

#[async_trait]
impl LinkPreviewer for HttpLinkPreviewer {
    async fn preview(&self, url: &str, deadline: Duration) -> Result<PreviewMeta> {
        let body = self.fetch(url, deadline).await?;
        Ok(parse_preview_meta(&body))
    }
}

/// A `reqwest::Client` configured the way Go's `req.C()` is.
///
/// The redirect limit and the absence of a destination filter are already
/// `reqwest`'s defaults; the overall client timeout matches Go's two minutes,
/// which the caller's ten-second deadline always beats in practice.
pub fn go_parity_http_client() -> reqwest::Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .timeout(Duration::from_secs(120))
        .build()
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn head_title_selector() -> &'static Selector {
    static SELECTOR: OnceLock<Selector> = OnceLock::new();
    SELECTOR.get_or_init(|| Selector::parse("head > title").expect("static selector"))
}

fn head_open_graph_title_selector() -> &'static Selector {
    static SELECTOR: OnceLock<Selector> = OnceLock::new();
    SELECTOR.get_or_init(|| {
        Selector::parse("head > meta[property='og:title']").expect("static selector")
    })
}

fn meta_selector() -> &'static Selector {
    static SELECTOR: OnceLock<Selector> = OnceLock::new();
    SELECTOR.get_or_init(|| Selector::parse("meta").expect("static selector"))
}

/// Go's `newMetaFrom`, reduced to the two fields the caller reads.
///
/// * `Title` is `strings.TrimSpace(doc.Find("head > title").Text())`. goquery's
///   `Text` concatenates every matched element's descendant text, so two
///   `<title>` children of `<head>` produce one joined string.
/// * `OpenGraph.Title` is
///   `doc.Find("head > meta[property='og:title']").AttrOr("content", "")`.
///   `AttrOr` reads the *first* matched node only, so a duplicated `og:title`
///   never reaches the second tag, and a first tag without a `content`
///   attribute yields the empty string even when a later one has it. Note that
///   Go does **not** trim this value, unlike the title.
/// * Both empty is Go's `return Meta{}`. `Meta` carries nine more fields that
///   the caller never reads, and zeroing two of two here is the same
///   observable result.
pub fn parse_preview_meta(html: &str) -> PreviewMeta {
    let document = Html::parse_document(html);

    let title: String = document
        .select(head_title_selector())
        .flat_map(|element| element.text())
        .collect();
    let title = title.trim().to_string();

    let open_graph_title = document
        .select(head_open_graph_title_selector())
        .next()
        .and_then(|element| element.value().attr("content"))
        .unwrap_or_default()
        .to_string();

    if title.is_empty() && open_graph_title.is_empty() {
        return PreviewMeta::default();
    }

    PreviewMeta {
        title,
        open_graph_title,
    }
}

/// The raw equivalent of Go's `parsedURL.Host` for an absolute request URL.
fn raw_authority(raw_url: &str) -> Option<&str> {
    let scheme_end = raw_url.find("://")?;
    let remainder = &raw_url[scheme_end + 3..];
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    Some(
        authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host),
    )
}

fn decode_response_body(body: &[u8], content_type: Option<&str>) -> String {
    let Some(content_type) = content_type else {
        return String::from_utf8_lossy(body).into_owned();
    };
    if !["text", "json", "xml", "html", "java"]
        .iter()
        .any(|kind| content_type.contains(kind))
    {
        return String::from_utf8_lossy(body).into_owned();
    }

    if let Ok(media_type) = content_type.parse::<Mime>()
        && let Some(charset) = media_type.get_param(mime::CHARSET)
    {
        let label = charset.as_str().to_ascii_lowercase();
        if label.contains("utf-8") || label.contains("utf8") {
            return String::from_utf8_lossy(body).into_owned();
        }
        return Encoding::for_label(label.as_bytes()).map_or_else(
            || String::from_utf8_lossy(body).into_owned(),
            |encoding| decode_with(encoding, body),
        );
    }

    match sniff_encoding(&body[..body.len().min(512)]) {
        Some(SniffedEncoding::Decode(encoding)) => decode_with(encoding, body),
        Some(SniffedEncoding::Raw) | None => String::from_utf8_lossy(body).into_owned(),
    }
}

fn decode_with(encoding: &'static Encoding, body: &[u8]) -> String {
    let (decoded, _) = encoding.decode_with_bom_removal(body);
    decoded.into_owned()
}

#[derive(Clone, Copy)]
enum SniffedEncoding {
    Raw,
    Decode(&'static Encoding),
}

fn sniff_encoding(prefix: &[u8]) -> Option<SniffedEncoding> {
    if let Some((encoding, _)) = Encoding::for_bom(prefix) {
        return Some(if encoding == UTF_8 {
            SniffedEncoding::Raw
        } else {
            SniffedEncoding::Decode(encoding)
        });
    }

    let document = Html::parse_document(&String::from_utf8_lossy(prefix));
    for element in document.select(meta_selector()) {
        let direct = element.value().attr("charset");
        let pragma = element
            .value()
            .attr("http-equiv")
            .is_some_and(|value| value.eq_ignore_ascii_case("content-type"));
        let from_content = element
            .value()
            .attr("content")
            .and_then(charset_from_meta_content)
            .filter(|_| pragma);

        if let Some(label) = direct.or(from_content)
            && let Some(decision) = encoding_from_meta_label(label)
        {
            return Some(decision);
        }
    }
    None
}

fn encoding_from_meta_label(label: &str) -> Option<SniffedEncoding> {
    let label = label.trim().to_ascii_lowercase();
    let encoding = Encoding::for_label(label.as_bytes())?;
    if label.starts_with("utf-16") || encoding == UTF_8 {
        Some(SniffedEncoding::Raw)
    } else {
        Some(SniffedEncoding::Decode(encoding))
    }
}

fn charset_from_meta_content(content: &str) -> Option<&str> {
    let lower = content.to_ascii_lowercase();
    let mut remaining = lower.as_str();
    loop {
        let charset_at = remaining.find("charset")?;
        remaining = &remaining[charset_at + "charset".len()..];
        remaining = remaining.trim_start_matches([' ', '\t', '\n', '\u{000C}', '\r']);
        let Some(after_equals) = remaining.strip_prefix('=') else {
            continue;
        };
        remaining = after_equals.trim_start_matches([' ', '\t', '\n', '\u{000C}', '\r']);
        let first = remaining.as_bytes().first().copied()?;
        if matches!(first, b'\'' | b'"') {
            let quote = char::from(first);
            let value = &remaining[1..];
            let end = value.find(quote)?;
            let original_at = content.len() - remaining.len() + 1;
            return content.get(original_at..original_at + end);
        }
        let end = remaining
            .find([';', ' ', '\t', '\n', '\u{000C}', '\r'])
            .unwrap_or(remaining.len());
        let original_at = content.len() - remaining.len();
        return content.get(original_at..original_at + end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_twitter_hosts_take_the_googlebot_user_agent() {
        for host in TWITTER_SSR_HOSTS {
            let url = format!("https://{host}/nekomeowww/status/1");
            assert_eq!(HttpLinkPreviewer::user_agent_for(&url), TWITTER_USER_AGENT);
        }
    }

    #[test]
    fn every_other_host_keeps_the_chrome_user_agent() {
        for raw in [
            "https://example.com/a",
            // Go matches the host exactly, with no subdomain handling.
            "https://mobile.twitter.com/a",
            "https://x.com/a",
            // A port is part of Go's `parsedURL.Host`.
            "https://twitter.com:8443/a",
        ] {
            assert_eq!(HttpLinkPreviewer::user_agent_for(raw), DEFAULT_USER_AGENT);
        }
    }
}
