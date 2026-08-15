//! Task 5B1 — the production `linkprev` adapter.
//!
//! Behaviour is pinned to Go v1.0.0 commit
//! `02aee8ce260165592e2152eb5a024a602e4eced1`: `pkg/linkprev/linkprev.go` and
//! `pkg/opengraph/opengraph.go`, over `github.com/imroc/req/v3@v3.43.7` and
//! `github.com/PuerkitoBio/goquery@v1.9.2`.
//!
//! Every request in this file goes to a `wiremock` server bound to loopback.
//! Nothing here resolves a public name or opens a socket off the machine.

use std::{sync::Arc, time::Duration};

use encoding_rs::{GBK, SHIFT_JIS};
use insights_bot_telegram_rs::services::{
    link_preview::{
        DEFAULT_USER_AGENT, GoParityUrlPolicy, HttpLinkPreviewer, LinkPreviewError,
        TWITTER_USER_AGENT, UrlPolicy, go_parity_http_client, parse_preview_meta,
    },
    message_capture::{LinkPreviewer, PreviewMeta},
};
use url::Url;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

/// Long enough that no test in this file ever hits it.
const GENEROUS_DEADLINE: Duration = Duration::from_secs(10);

fn previewer() -> HttpLinkPreviewer {
    HttpLinkPreviewer::new(go_parity_http_client().expect("http client"))
}

fn failure(error: anyhow::Error) -> LinkPreviewError {
    error
        .downcast_ref::<LinkPreviewError>()
        .cloned()
        .unwrap_or_else(|| panic!("expected a LinkPreviewError, got {error:?}"))
}

fn html_response(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/html; charset=utf-8")
        .set_body_string(body)
}

// ---------------------------------------------------------------------------
// `newMetaFrom`: goquery selection semantics
// ---------------------------------------------------------------------------

#[test]
fn a_title_and_an_open_graph_title_are_both_reported() {
    // Go's caller picks with `lo.Ternary(meta.Title != "", meta.Title,
    // meta.OpenGraph.Title)`, so the adapter must surface both and decide
    // nothing.
    let meta = parse_preview_meta(
        r#"<html><head>
             <title>Head Title</title>
             <meta property="og:title" content="Graph Title">
           </head><body></body></html>"#,
    );

    assert_eq!(
        meta,
        PreviewMeta {
            title: "Head Title".to_string(),
            open_graph_title: "Graph Title".to_string(),
        }
    );
}

#[test]
fn an_open_graph_title_stands_alone_when_there_is_no_title() {
    let meta = parse_preview_meta(
        r#"<html><head><meta property="og:title" content="Graph Only"></head></html>"#,
    );

    assert_eq!(meta.title, "");
    assert_eq!(meta.open_graph_title, "Graph Only");
}

#[test]
fn only_a_direct_child_of_head_counts_as_the_title() {
    // `head > title` is a child combinator. HTML5 parsing puts a `<title>` met
    // inside `<body>` into the body subtree, so it never matches.
    let meta = parse_preview_meta(
        r#"<html><head><meta property="og:title" content="Graph"></head>
           <body><div><title>Buried</title></div></body></html>"#,
    );

    assert_eq!(meta.title, "");
    assert_eq!(meta.open_graph_title, "Graph");
}

#[test]
fn the_head_title_wins_over_a_title_that_html5_parsing_moved_into_the_body() {
    let meta = parse_preview_meta(
        r#"<html><head><title>In Head</title></head>
           <body><title>In Body</title></body></html>"#,
    );

    assert_eq!(meta.title, "In Head");
}

#[test]
fn only_a_direct_child_of_head_counts_as_an_open_graph_meta() {
    let meta = parse_preview_meta(
        r#"<html><head><title>Head Title</title></head>
           <body><meta property="og:title" content="Late Graph"></body></html>"#,
    );

    assert_eq!(meta.title, "Head Title");
    assert_eq!(meta.open_graph_title, "");
}

#[test]
fn duplicate_head_titles_are_concatenated_the_way_goquery_text_does() {
    // goquery's `Selection.Text` walks every matched node, not just the first.
    let meta =
        parse_preview_meta("<html><head><title>Alpha</title><title>Beta</title></head></html>");

    assert_eq!(meta.title, "AlphaBeta");
}

#[test]
fn the_first_open_graph_title_wins_over_later_duplicates() {
    // `AttrOr` reads `Selection.Attr`, which only looks at the first node.
    let meta = parse_preview_meta(
        r#"<html><head>
             <meta property="og:title" content="First">
             <meta property="og:title" content="Second">
           </head></html>"#,
    );

    assert_eq!(meta.open_graph_title, "First");
}

#[test]
fn a_first_open_graph_title_without_content_shadows_a_later_complete_one() {
    // This is the sharp edge of `AttrOr`: it does not fall through to the next
    // matched node, so the default wins even though a usable tag exists.
    let meta = parse_preview_meta(
        r#"<html><head>
             <title>Head Title</title>
             <meta property="og:title">
             <meta property="og:title" content="Never Read">
           </head></html>"#,
    );

    assert_eq!(meta.title, "Head Title");
    assert_eq!(meta.open_graph_title, "");
}

#[test]
fn the_title_is_trimmed_and_the_open_graph_title_is_not() {
    // `Title` goes through `strings.TrimSpace`; `OpenGraph.Title` does not.
    let meta = parse_preview_meta(
        "<html><head>\n  <title>\n   Padded Title \n  </title>\n  \
         <meta property=\"og:title\" content=\"  Padded Graph  \">\n</head></html>",
    );

    assert_eq!(meta.title, "Padded Title");
    assert_eq!(meta.open_graph_title, "  Padded Graph  ");
}

#[test]
fn a_whitespace_only_open_graph_title_is_not_empty() {
    // Go's `meta.OpenGraph.Title == ""` guard is a raw string comparison, so
    // whitespace keeps `newMetaFrom` from resetting the struct, and the
    // caller's ternary then selects that whitespace as the link title.
    let meta =
        parse_preview_meta(r#"<html><head><meta property="og:title" content="   "></head></html>"#);

    assert_eq!(meta.title, "");
    assert_eq!(meta.open_graph_title, "   ");
    assert_ne!(meta, PreviewMeta::default());
}

#[test]
fn a_document_with_neither_title_yields_the_zero_meta() {
    // Go's `newMetaFrom` returns `Meta{}` outright in this case.
    for document in [
        "<html><head></head><body>Just words</body></html>",
        "<html><head><title>   </title></head></html>",
        "",
        "not html at all",
    ] {
        assert_eq!(
            parse_preview_meta(document),
            PreviewMeta::default(),
            "document should have produced the zero meta: {document:?}"
        );
    }
}

#[test]
fn malformed_html_is_recovered_rather_than_rejected() {
    // `goquery.NewDocumentFromReader` wraps `html.Parse`, which has no failure
    // mode for bad markup, so `Preview` never returns a parse error.
    let unterminated = parse_preview_meta("<html><head><title>Unterminated");
    assert_eq!(unterminated.title, "Unterminated");

    let tag_soup = parse_preview_meta(
        r#"<HTML><HEAD><TITLE>Shouty<P><meta property="og:title" content="Soup"></head>"#,
    );
    assert_eq!(
        tag_soup.title,
        r#"Shouty<P><meta property="og:title" content="Soup"></head>"#
    );

    let stray_close = parse_preview_meta("</title><html><head><title>Recovered</title></head>");
    assert_eq!(stray_close.title, "Recovered");
}

#[test]
fn character_references_in_the_title_are_decoded() {
    let meta =
        parse_preview_meta("<html><head><title>Tom &amp; Jerry &#233;</title></head></html>");

    assert_eq!(meta.title, "Tom & Jerry é");
}

// ---------------------------------------------------------------------------
// `alterRequestForTwitter`
// ---------------------------------------------------------------------------

#[test]
fn the_three_twitter_hosts_ask_for_server_rendered_html() {
    for host in ["twitter.com", "vxtwitter.com", "fxtwitter.com"] {
        let url = format!("https://{host}/a/status/1");
        assert_eq!(HttpLinkPreviewer::user_agent_for(&url), TWITTER_USER_AGENT);
    }

    for raw in [
        "https://example.com/",
        "https://x.com/",
        "https://twitter.com.evil.test/",
        "https://Twitter.com/a",
        "https://twitter.com:443/a",
    ] {
        assert_eq!(HttpLinkPreviewer::user_agent_for(raw), DEFAULT_USER_AGENT);
    }

    assert_eq!(
        HttpLinkPreviewer::user_agent_for("https://bot@twitter.com/a"),
        TWITTER_USER_AGENT
    );
}

// ---------------------------------------------------------------------------
// `Client.request`: method, headers, status, body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_request_is_a_get_carrying_gos_chrome_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(html_response(
            "<html><head><title>Fetched</title></head></html>",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let meta = previewer()
        .preview(&format!("{}/page", server.uri()), GENEROUS_DEADLINE)
        .await
        .expect("preview");

    assert_eq!(meta.title, "Fetched");

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, wiremock::http::Method::GET);
    assert!(requests[0].body.is_empty(), "Go sends no request body");
    assert_eq!(
        requests[0]
            .headers
            .get("user-agent")
            .expect("user-agent")
            .to_str()
            .expect("ASCII user-agent"),
        DEFAULT_USER_AGENT
    );
}

#[tokio::test]
async fn redirects_are_followed_to_the_final_document() {
    let server = MockServer::start().await;
    let target = format!("{}/final", server.uri());

    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", target.as_str()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/final"))
        .respond_with(html_response(
            "<html><head><title>After Redirect</title></head></html>",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let meta = previewer()
        .preview(&format!("{}/start", server.uri()), GENEROUS_DEADLINE)
        .await
        .expect("preview");

    assert_eq!(meta.title, "After Redirect");

    let requests = server.received_requests().await.expect("recorded requests");
    let final_request = requests
        .iter()
        .find(|request| request.url.path() == "/final")
        .expect("redirect target request");
    assert_eq!(
        final_request
            .headers
            .get("user-agent")
            .expect("user-agent")
            .to_str()
            .expect("ASCII user-agent"),
        DEFAULT_USER_AGENT
    );
}

#[tokio::test]
async fn a_redirect_loop_past_gos_limit_is_a_network_error() {
    // Go's `defaultCheckRedirect` stops after ten hops and returns an error
    // from `request.Get`, which `linkprev` wraps as `ErrNetworkError`.
    let server = MockServer::start().await;
    let target = format!("{}/loop", server.uri());
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", target.as_str()))
        .mount(&server)
        .await;

    let error = previewer()
        .preview(&target, GENEROUS_DEADLINE)
        .await
        .expect_err("a redirect loop cannot succeed");

    assert_eq!(failure(error), LinkPreviewError::Network);
}

#[tokio::test]
async fn every_status_outside_the_success_range_is_a_request_failure() {
    for status in [301u16, 400, 401, 404, 418, 500, 503] {
        let server = MockServer::start().await;
        // A bare 3xx with no `Location` is handed back to the caller by both
        // `net/http` and `reqwest`, so it reaches the status check.
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string("<html><head><title>Ignored</title></head></html>"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let error = previewer()
            .preview(&format!("{}/page", server.uri()), GENEROUS_DEADLINE)
            .await
            .expect_err("a non-2xx response cannot produce a preview");

        assert_eq!(failure(error), LinkPreviewError::RequestFailed { status });
    }
}

#[tokio::test]
async fn a_failure_message_never_leaks_the_url_or_the_body() {
    // Go's error string embeds the URL and a full request/response dump. This
    // port drops both.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/secret-path"))
        .respond_with(ResponseTemplate::new(403).set_body_string("super secret body"))
        .mount(&server)
        .await;

    let url = format!("{}/secret-path", server.uri());
    let error = previewer()
        .preview(&url, GENEROUS_DEADLINE)
        .await
        .expect_err("403 cannot produce a preview");

    let rendered = format!("{error:#}");
    assert!(
        !rendered.contains("secret-path"),
        "leaked the URL: {rendered}"
    );
    assert!(
        !rendered.contains("super secret"),
        "leaked the body: {rendered}"
    );
    assert!(
        rendered.contains("403"),
        "the status code is the useful part: {rendered}"
    );
}

#[tokio::test]
async fn a_two_hundred_with_no_metadata_is_a_success_with_the_zero_meta() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/empty"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let meta = previewer()
        .preview(&format!("{}/empty", server.uri()), GENEROUS_DEADLINE)
        .await
        .expect("204 is inside Go's success range");

    assert_eq!(meta, PreviewMeta::default());
}

#[tokio::test]
async fn undecodable_bytes_become_replacement_characters() {
    // `golang.org/x/net/html` carries raw bytes through, which a Rust `String`
    // cannot; each maximal invalid subpart becomes U+FFFD instead.
    let mut body = b"<html><head><title>caf".to_vec();
    body.push(0xE9); // Latin-1 `e` acute, invalid on its own in UTF-8.
    body.extend_from_slice(b"</title></head></html>");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/latin1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_bytes(body),
        )
        .expect(1)
        .mount(&server)
        .await;

    let meta = previewer()
        .preview(&format!("{}/latin1", server.uri()), GENEROUS_DEADLINE)
        .await
        .expect("preview");

    assert_eq!(meta.title, "caf\u{FFFD}");
}

#[tokio::test]
async fn valid_multibyte_utf8_survives_intact() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/utf8"))
        .respond_with(html_response(
            "<html><head><title>咖啡 ☕ é</title></head></html>",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let meta = previewer()
        .preview(&format!("{}/utf8", server.uri()), GENEROUS_DEADLINE)
        .await
        .expect("preview");

    assert_eq!(meta.title, "咖啡 ☕ é");
}

#[tokio::test]
async fn content_type_charset_is_transcoded_like_req_v3() {
    let document = "<html><head><title>繁體中文</title></head></html>";
    let (encoded, _, had_errors) = GBK.encode(document);
    assert!(!had_errors, "fixture must be representable in GBK");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/gbk"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=gbk")
                .set_body_bytes(encoded.into_owned()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let meta = previewer()
        .preview(&format!("{}/gbk", server.uri()), GENEROUS_DEADLINE)
        .await
        .expect("GBK preview");

    assert_eq!(meta.title, "繁體中文");
}

#[tokio::test]
async fn html_meta_charset_is_prescanned_like_req_v3() {
    let document = "<html><head><meta charset=\"shift_jis\"><title>日本語</title></head></html>";
    let (encoded, _, had_errors) = SHIFT_JIS.encode(document);
    assert!(!had_errors, "fixture must be representable in Shift_JIS");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/shift-jis"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_bytes(encoded.into_owned()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let meta = previewer()
        .preview(&format!("{}/shift-jis", server.uri()), GENEROUS_DEADLINE)
        .await
        .expect("Shift_JIS preview");

    assert_eq!(meta.title, "日本語");
}

#[tokio::test]
async fn the_deadline_maps_to_a_network_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            html_response("<html><head><title>Too Late</title></head></html>")
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;

    let error = previewer()
        .preview(
            &format!("{}/slow", server.uri()),
            Duration::from_millis(100),
        )
        .await
        .expect_err("the deadline must cut the request off");

    assert_eq!(failure(error), LinkPreviewError::Network);
}

#[tokio::test]
async fn an_unparseable_url_is_a_network_error_without_a_request() {
    // Go's `req` fails inside `request.Get`, which `linkprev` wraps as
    // `ErrNetworkError`.
    for raw in ["", "not a url", "://missing-scheme"] {
        let error = previewer()
            .preview(raw, GENEROUS_DEADLINE)
            .await
            .expect_err("an unparseable URL cannot be fetched");

        assert_eq!(failure(error), LinkPreviewError::Network);
    }
}

// ---------------------------------------------------------------------------
// Destination policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_parity_previewer_reaches_loopback_exactly_as_go_does() {
    // Go dials through a stock `net.Dialer` with no `Control` hook and no
    // address filter, so loopback, private, and link-local destinations are
    // all reachable. This test is the evidence for that inherited behaviour,
    // not an endorsement of it.
    let server = MockServer::start().await;
    let url = Url::parse(&server.uri()).expect("wiremock uri");
    assert!(
        url.host_str()
            .is_some_and(|host| host == "127.0.0.1" || host == "localhost"),
        "the mock must be on loopback: {url}"
    );

    Mock::given(method("GET"))
        .and(path("/internal"))
        .respond_with(html_response(
            "<html><head><title>Internal</title></head></html>",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let meta = HttpLinkPreviewer::with_policy(
        go_parity_http_client().expect("http client"),
        Arc::new(GoParityUrlPolicy),
    )
    .preview(&format!("{}/internal", server.uri()), GENEROUS_DEADLINE)
    .await
    .expect("Go reaches loopback, so the parity adapter does too");

    assert_eq!(meta.title, "Internal");
}

/// The kind of policy the wiring slice can install without touching parsing.
#[derive(Debug)]
struct RefuseEverything;

impl UrlPolicy for RefuseEverything {
    fn admit(&self, _url: &Url) -> Result<(), LinkPreviewError> {
        Err(LinkPreviewError::PolicyRejected)
    }
}

#[tokio::test]
async fn an_injected_policy_stops_the_request_before_it_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/internal"))
        .respond_with(html_response(
            "<html><head><title>Internal</title></head></html>",
        ))
        .expect(0)
        .mount(&server)
        .await;

    let error = HttpLinkPreviewer::with_policy(
        go_parity_http_client().expect("http client"),
        Arc::new(RefuseEverything),
    )
    .preview(&format!("{}/internal", server.uri()), GENEROUS_DEADLINE)
    .await
    .expect_err("the policy refuses every destination");

    assert_eq!(failure(error), LinkPreviewError::PolicyRejected);
    assert!(
        server
            .received_requests()
            .await
            .is_some_and(|requests| requests.is_empty()),
        "a rejected destination must not be dialled"
    );
}

#[tokio::test]
async fn a_policy_change_does_not_change_parsing() {
    let document = r#"<html><head><title>Same</title><meta property="og:title" content="Graph"></head></html>"#;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(html_response(document))
        .mount(&server)
        .await;

    let fetched = HttpLinkPreviewer::with_policy(
        go_parity_http_client().expect("http client"),
        Arc::new(GoParityUrlPolicy),
    )
    .preview(&format!("{}/page", server.uri()), GENEROUS_DEADLINE)
    .await
    .expect("preview");

    assert_eq!(fetched, parse_preview_meta(document));
}
