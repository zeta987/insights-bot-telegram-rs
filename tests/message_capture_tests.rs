//! Task 5A — Telegram message preprocessing core.
//!
//! Behaviour is pinned to Go v1.0.0 commit
//! `02aee8ce260165592e2152eb5a024a602e4eced1`:
//! `internal/models/chathistories/chat_histories.go` for the extraction, the
//! reply snapshot, and the forwarding prefix, and `pkg/bots/tgbot/utils.go`
//! plus `github.com/nekomeowww/xo@v1.9.6` `string.go` for the full name.
//!
//! Every network and model call is served by a local fake, so nothing in this
//! file opens a socket: the only [`LinkPreviewer`] and [`Summarizer`] values
//! constructed here are the in-process fakes below and the deliberately
//! failing `UnavailableLinkPreviewer`.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use insights_bot_telegram_rs::services::message_capture::{
    CapturedChat, CapturedEntity, CapturedEntityKind, CapturedMessage, CapturedUser, LinkPreviewer,
    MessagePreprocessor, PreviewMeta, Summarizer, UnavailableLinkPreviewer, contains_cjk_char,
    decode_utf16_lossy, full_name_from_first_and_last_name, go_string_from_bytes, query_unescape,
    telegram_date_to_unix_millis,
};

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

/// What the fake previewer does on its next call.
enum PreviewOutcome {
    Meta(PreviewMeta),
    Failure,
    /// Sleeps, so the caller's deadline is the only thing that can end the call.
    Sleep(Duration),
}

#[derive(Default)]
struct FakeLinkPreviewer {
    outcomes: Mutex<VecDeque<PreviewOutcome>>,
    calls: Mutex<Vec<(String, Duration)>>,
}

impl FakeLinkPreviewer {
    fn with(outcomes: Vec<PreviewOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn titled(titles: &[&str]) -> Arc<Self> {
        Self::with(
            titles
                .iter()
                .map(|title| {
                    PreviewOutcome::Meta(PreviewMeta {
                        title: (*title).to_string(),
                        open_graph_title: String::new(),
                    })
                })
                .collect(),
        )
    }

    fn calls(&self) -> Vec<(String, Duration)> {
        self.calls.lock().expect("previewer calls").clone()
    }
}

#[async_trait]
impl LinkPreviewer for FakeLinkPreviewer {
    async fn preview(&self, url: &str, deadline: Duration) -> Result<PreviewMeta> {
        self.calls
            .lock()
            .expect("previewer calls")
            .push((url.to_string(), deadline));

        let outcome = {
            let mut outcomes = self.outcomes.lock().expect("previewer outcomes");
            outcomes.pop_front()
        };

        match outcome {
            Some(PreviewOutcome::Meta(meta)) => Ok(meta),
            Some(PreviewOutcome::Sleep(duration)) => {
                tokio::time::sleep(duration).await;
                Err(anyhow!("the caller's deadline should have fired first"))
            }
            Some(PreviewOutcome::Failure) | None => Err(anyhow!("preview failed")),
        }
    }
}

/// What the fake summarizer does on its next call.
enum SummaryOutcome {
    Choices(Vec<String>),
    Failure,
    Sleep(Duration),
}

#[derive(Default)]
struct FakeSummarizer {
    any_outcomes: Mutex<VecDeque<SummaryOutcome>>,
    one_outcomes: Mutex<VecDeque<SummaryOutcome>>,
    any_calls: Mutex<Vec<(String, Option<Duration>)>>,
    one_calls: Mutex<Vec<(String, Option<Duration>)>>,
}

impl FakeSummarizer {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn with_any(outcomes: Vec<SummaryOutcome>) -> Arc<Self> {
        Arc::new(Self {
            any_outcomes: Mutex::new(outcomes.into()),
            ..Default::default()
        })
    }

    fn with_one(outcomes: Vec<SummaryOutcome>) -> Arc<Self> {
        Arc::new(Self {
            one_outcomes: Mutex::new(outcomes.into()),
            ..Default::default()
        })
    }

    fn any_calls(&self) -> Vec<(String, Option<Duration>)> {
        self.any_calls.lock().expect("any calls").clone()
    }

    fn one_calls(&self) -> Vec<(String, Option<Duration>)> {
        self.one_calls.lock().expect("one calls").clone()
    }
}

async fn serve(
    outcomes: &Mutex<VecDeque<SummaryOutcome>>,
    calls: &Mutex<Vec<(String, Option<Duration>)>>,
    content: &str,
    deadline: Option<Duration>,
) -> Result<Vec<String>> {
    calls
        .lock()
        .expect("summarizer calls")
        .push((content.to_string(), deadline));

    let outcome = {
        let mut queued = outcomes.lock().expect("summarizer outcomes");
        queued.pop_front()
    };

    match outcome {
        Some(SummaryOutcome::Choices(choices)) => Ok(choices),
        Some(SummaryOutcome::Failure) => Err(anyhow!("summarization failed")),
        Some(SummaryOutcome::Sleep(duration)) => {
            tokio::time::sleep(duration).await;
            Err(anyhow!("the caller's deadline should have fired first"))
        }
        // An exhausted queue behaves like Go's empty `resp.Choices`.
        None => Ok(Vec::new()),
    }
}

#[async_trait]
impl Summarizer for FakeSummarizer {
    async fn summarize_any(
        &self,
        content: &str,
        deadline: Option<Duration>,
    ) -> Result<Vec<String>> {
        serve(&self.any_outcomes, &self.any_calls, content, deadline).await
    }

    async fn summarize_one_chat_history(
        &self,
        content: &str,
        deadline: Option<Duration>,
    ) -> Result<Vec<String>> {
        serve(&self.one_outcomes, &self.one_calls, content, deadline).await
    }
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

fn preprocessor(
    previewer: Arc<FakeLinkPreviewer>,
    summarizer: Arc<FakeSummarizer>,
) -> MessagePreprocessor<Arc<FakeLinkPreviewer>, Arc<FakeSummarizer>> {
    MessagePreprocessor::new(previewer, summarizer)
}

fn url_entity(offset: usize, length: usize) -> CapturedEntity {
    CapturedEntity {
        kind: CapturedEntityKind::Url,
        offset,
        length,
    }
}

fn text_link_entity(offset: usize, length: usize, url: &str) -> CapturedEntity {
    CapturedEntity {
        kind: CapturedEntityKind::TextLink {
            url: url.to_string(),
        },
        offset,
        length,
    }
}

fn bold_entity(offset: usize, length: usize) -> CapturedEntity {
    CapturedEntity {
        kind: CapturedEntityKind::Other,
        offset,
        length,
    }
}

fn text_message(text: &str) -> CapturedMessage {
    CapturedMessage {
        message_id: 1,
        date: 1_700_000_000,
        chat: CapturedChat {
            id: -100_123,
            kind: "supergroup".to_string(),
            title: "Parity Lab".to_string(),
        },
        from: CapturedUser {
            id: 42,
            username: "author".to_string(),
            first_name: "John".to_string(),
            last_name: "Smith".to_string(),
        },
        text: text.to_string(),
        ..Default::default()
    }
}

/// `"https://example.com"` is 19 UTF-16 code units.
const EXAMPLE_URL: &str = "https://example.com";
const EXAMPLE_URL_UNITS: usize = 19;

// ---------------------------------------------------------------------------
// Caption precedence and entity selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn caption_wins_over_text() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut message = text_message("plain text body");
    message.caption = "caption body".to_string();

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        "caption body"
    );
}

#[tokio::test]
async fn text_is_used_when_the_caption_is_empty() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    assert_eq!(
        processor
            .extract_text_from_message(&text_message("plain text body"))
            .await,
        "plain text body"
    );
}

#[tokio::test]
async fn message_entities_are_applied_to_the_winning_caption() {
    let previewer = FakeLinkPreviewer::titled(&["Example"]);
    let processor = preprocessor(previewer.clone(), FakeSummarizer::new());

    let mut message = text_message("unrelated text");
    message.caption = format!("see {EXAMPLE_URL} now");
    message.entities = vec![url_entity(4, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("see [Example]({EXAMPLE_URL}) now")
    );
    assert_eq!(previewer.calls().len(), 1);
}

#[tokio::test]
async fn caption_entities_are_ignored() {
    let previewer = FakeLinkPreviewer::titled(&["Example"]);
    let processor = preprocessor(previewer.clone(), FakeSummarizer::new());

    let mut message = text_message("unrelated text");
    message.caption = format!("see {EXAMPLE_URL} now");
    message.caption_entities = vec![url_entity(4, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("see {EXAMPLE_URL} now")
    );
    assert!(previewer.calls().is_empty());
}

#[tokio::test]
async fn non_link_entities_contribute_no_rewrite() {
    let previewer = FakeLinkPreviewer::titled(&["Example"]);
    let processor = preprocessor(previewer.clone(), FakeSummarizer::new());

    let mut message = text_message("bold words here");
    message.entities = vec![bold_entity(0, 4)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        "bold words here"
    );
    assert!(previewer.calls().is_empty());
}

// ---------------------------------------------------------------------------
// Link rewriting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn url_entity_takes_its_href_from_the_slice_and_its_title_from_the_preview() {
    let previewer = FakeLinkPreviewer::titled(&["Example Domain"]);
    let processor = preprocessor(previewer.clone(), FakeSummarizer::new());

    let mut message = text_message(&format!("see {EXAMPLE_URL} now"));
    message.entities = vec![url_entity(4, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("see [Example Domain]({EXAMPLE_URL}) now")
    );
    assert_eq!(previewer.calls()[0].0, EXAMPLE_URL);
}

#[tokio::test]
async fn text_link_entity_takes_its_href_from_the_entity_and_its_title_from_the_slice() {
    let previewer = FakeLinkPreviewer::with(Vec::new());
    let processor = preprocessor(previewer.clone(), FakeSummarizer::new());

    let mut message = text_message("read the docs today");
    message.entities = vec![text_link_entity(9, 4, "https://docs.example.com")];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        "read the [docs](https://docs.example.com) today"
    );
    // A text_link never asks for a preview.
    assert!(previewer.calls().is_empty());
}

#[tokio::test]
async fn a_failed_preview_leaves_the_url_in_place() {
    let previewer = FakeLinkPreviewer::with(vec![PreviewOutcome::Failure]);
    let processor = preprocessor(previewer, FakeSummarizer::new());

    let mut message = text_message(&format!("see {EXAMPLE_URL} now"));
    message.entities = vec![url_entity(4, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("see {EXAMPLE_URL} now")
    );
}

#[tokio::test]
async fn links_are_applied_back_to_front_using_the_original_offsets() {
    let previewer = FakeLinkPreviewer::titled(&["One", "Two"]);
    let processor = preprocessor(previewer.clone(), FakeSummarizer::new());

    // "a " = 2, "https://one.example" = 19, " b " = 3, "https://two.example" = 19.
    let mut message = text_message("a https://one.example b https://two.example c");
    message.entities = vec![url_entity(2, 19), url_entity(24, 19)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        "a [One](https://one.example) b [Two](https://two.example) c"
    );

    let calls = previewer.calls();
    assert_eq!(calls[0].0, "https://one.example");
    assert_eq!(calls[1].0, "https://two.example");
}

#[tokio::test]
async fn offsets_are_utf16_code_units_across_a_surrogate_pair() {
    let previewer = FakeLinkPreviewer::titled(&["Example"]);
    let processor = preprocessor(previewer.clone(), FakeSummarizer::new());

    // U+1F3AE occupies two UTF-16 code units, so the URL starts at index 3.
    let mut message = text_message(&format!("\u{1F3AE} {EXAMPLE_URL} now"));
    message.entities = vec![url_entity(3, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("\u{1F3AE} [Example]({EXAMPLE_URL}) now")
    );
    assert_eq!(previewer.calls()[0].0, EXAMPLE_URL);
}

#[tokio::test]
async fn a_range_that_splits_a_surrogate_pair_decodes_to_replacement_characters() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    // Go's utf16.Decode turns each unpaired surrogate into U+FFFD instead of
    // failing, and so does this port.
    let mut message = text_message("\u{1F3AE}abc");
    message.entities = vec![text_link_entity(0, 1, "https://x.example")];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        "[\u{FFFD}](https://x.example)\u{FFFD}abc"
    );
}

// ---------------------------------------------------------------------------
// Title fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_title_falls_back_to_the_open_graph_title() {
    let previewer = FakeLinkPreviewer::with(vec![PreviewOutcome::Meta(PreviewMeta {
        title: String::new(),
        open_graph_title: "Open Graph Title".to_string(),
    })]);
    let processor = preprocessor(previewer, FakeSummarizer::new());

    let mut message = text_message(EXAMPLE_URL);
    message.entities = vec![url_entity(0, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("[Open Graph Title]({EXAMPLE_URL})")
    );
}

#[tokio::test]
async fn the_meta_title_wins_over_the_open_graph_title() {
    let previewer = FakeLinkPreviewer::with(vec![PreviewOutcome::Meta(PreviewMeta {
        title: "Meta Title".to_string(),
        open_graph_title: "Open Graph Title".to_string(),
    })]);
    let processor = preprocessor(previewer, FakeSummarizer::new());

    let mut message = text_message(EXAMPLE_URL);
    message.entities = vec![url_entity(0, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("[Meta Title]({EXAMPLE_URL})")
    );
}

#[tokio::test]
async fn both_titles_empty_yields_an_empty_markdown_title() {
    let previewer = FakeLinkPreviewer::with(vec![PreviewOutcome::Meta(PreviewMeta::default())]);
    let processor = preprocessor(previewer, FakeSummarizer::new());

    let mut message = text_message(EXAMPLE_URL);
    message.entities = vec![url_entity(0, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("[]({EXAMPLE_URL})")
    );
}

// ---------------------------------------------------------------------------
// Deadlines
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_preview_call_carries_the_ten_second_deadline() {
    let previewer = FakeLinkPreviewer::titled(&["Example"]);
    let processor = preprocessor(previewer.clone(), FakeSummarizer::new());

    let mut message = text_message(EXAMPLE_URL);
    message.entities = vec![url_entity(0, EXAMPLE_URL_UNITS)];
    processor.extract_text_from_message(&message).await;

    assert_eq!(previewer.calls()[0].1, Duration::from_secs(10));
}

#[tokio::test(start_paused = true)]
async fn a_preview_past_ten_seconds_is_cut_off_and_the_url_survives() {
    let previewer = FakeLinkPreviewer::with(vec![PreviewOutcome::Sleep(Duration::from_secs(11))]);
    let processor = preprocessor(previewer, FakeSummarizer::new());

    let mut message = text_message(&format!("see {EXAMPLE_URL} now"));
    message.entities = vec![url_entity(4, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("see {EXAMPLE_URL} now")
    );
}

#[tokio::test]
async fn the_title_summarization_call_carries_the_sixty_second_deadline() {
    let previewer = FakeLinkPreviewer::titled(&["\u{3042}".repeat(201).as_str()]);
    let summarizer =
        FakeSummarizer::with_any(vec![SummaryOutcome::Choices(vec!["Short".to_string()])]);
    let processor = preprocessor(previewer, summarizer.clone());

    let mut message = text_message(EXAMPLE_URL);
    message.entities = vec![url_entity(0, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("[Short]({EXAMPLE_URL})")
    );
    assert_eq!(summarizer.any_calls()[0].1, Some(Duration::from_secs(60)));
}

#[tokio::test(start_paused = true)]
async fn a_title_summarization_past_sixty_seconds_drops_the_link() {
    let previewer = FakeLinkPreviewer::titled(&["\u{3042}".repeat(201).as_str()]);
    let summarizer = FakeSummarizer::with_any(vec![SummaryOutcome::Sleep(Duration::from_secs(61))]);
    let processor = preprocessor(previewer, summarizer);

    let mut message = text_message(&format!("see {EXAMPLE_URL} now"));
    message.entities = vec![url_entity(4, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("see {EXAMPLE_URL} now")
    );
}

#[tokio::test]
async fn a_failed_title_summarization_drops_the_link() {
    let previewer = FakeLinkPreviewer::titled(&["\u{3042}".repeat(201).as_str()]);
    let summarizer = FakeSummarizer::with_any(vec![SummaryOutcome::Failure]);
    let processor = preprocessor(previewer, summarizer);

    let mut message = text_message(&format!("see {EXAMPLE_URL} now"));
    message.entities = vec![url_entity(4, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("see {EXAMPLE_URL} now")
    );
}

#[tokio::test]
async fn the_chat_history_summarization_call_carries_no_deadline() {
    let summarizer =
        FakeSummarizer::with_one(vec![SummaryOutcome::Choices(vec!["Summary".to_string()])]);
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), summarizer.clone());

    let message = text_message(&"a".repeat(300));
    assert_eq!(
        processor
            .extract_text_with_summarization(&message)
            .await
            .expect("summarization"),
        "Summary"
    );
    assert_eq!(summarizer.one_calls()[0].1, None);
}

// ---------------------------------------------------------------------------
// Rune thresholds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_title_of_exactly_two_hundred_runes_is_not_summarized() {
    let title = "\u{3042}".repeat(200);
    let previewer = FakeLinkPreviewer::titled(&[title.as_str()]);
    let summarizer = FakeSummarizer::new();
    let processor = preprocessor(previewer, summarizer.clone());

    let mut message = text_message(EXAMPLE_URL);
    message.entities = vec![url_entity(0, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("[{title}]({EXAMPLE_URL})")
    );
    assert!(summarizer.any_calls().is_empty());
}

#[tokio::test]
async fn a_title_of_two_hundred_and_one_runes_is_summarized() {
    let title = "\u{3042}".repeat(201);
    let previewer = FakeLinkPreviewer::titled(&[title.as_str()]);
    let summarizer =
        FakeSummarizer::with_any(vec![SummaryOutcome::Choices(vec!["Condensed".to_string()])]);
    let processor = preprocessor(previewer, summarizer.clone());

    let mut message = text_message(EXAMPLE_URL);
    message.entities = vec![url_entity(0, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("[Condensed]({EXAMPLE_URL})")
    );
    assert_eq!(summarizer.any_calls().len(), 1);
    assert_eq!(summarizer.any_calls()[0].0, title);
}

#[tokio::test]
async fn an_empty_choice_list_leaves_the_long_title_untouched() {
    let title = "\u{3042}".repeat(201);
    let previewer = FakeLinkPreviewer::titled(&[title.as_str()]);
    let summarizer = FakeSummarizer::with_any(vec![SummaryOutcome::Choices(Vec::new())]);
    let processor = preprocessor(previewer, summarizer);

    let mut message = text_message(EXAMPLE_URL);
    message.entities = vec![url_entity(0, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("[{title}]({EXAMPLE_URL})")
    );
}

#[tokio::test]
async fn text_of_two_hundred_and_ninety_nine_runes_is_not_summarized() {
    let summarizer = FakeSummarizer::new();
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), summarizer.clone());

    let body = "\u{3042}".repeat(299);
    assert_eq!(
        processor
            .extract_text_with_summarization(&text_message(&body))
            .await
            .expect("extraction"),
        body
    );
    assert!(summarizer.one_calls().is_empty());
}

#[tokio::test]
async fn text_of_three_hundred_runes_is_summarized() {
    let summarizer =
        FakeSummarizer::with_one(vec![SummaryOutcome::Choices(vec!["Digest".to_string()])]);
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), summarizer.clone());

    let body = "\u{3042}".repeat(300);
    assert_eq!(
        processor
            .extract_text_with_summarization(&text_message(&body))
            .await
            .expect("extraction"),
        "Digest"
    );
    assert_eq!(summarizer.one_calls().len(), 1);
    assert_eq!(summarizer.one_calls()[0].0, body);
}

#[tokio::test]
async fn an_empty_choice_list_from_the_chat_history_summarizer_yields_no_row() {
    let summarizer = FakeSummarizer::with_one(vec![SummaryOutcome::Choices(Vec::new())]);
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), summarizer);

    let message = text_message(&"a".repeat(300));
    assert_eq!(
        processor
            .extract_text_with_summarization(&message)
            .await
            .expect("extraction"),
        ""
    );
    assert!(
        processor
            .capture_message(&message)
            .await
            .expect("capture")
            .is_none()
    );
}

#[tokio::test]
async fn a_failing_chat_history_summarizer_propagates_its_error() {
    let summarizer = FakeSummarizer::with_one(vec![SummaryOutcome::Failure]);
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), summarizer);

    assert!(
        processor
            .capture_message(&text_message(&"a".repeat(300)))
            .await
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Percent unescaping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_decodable_href_is_replaced_by_its_unescaped_form() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut message = text_message("link");
    message.entities = vec![text_link_entity(0, 4, "https://example.com/a%20b+c")];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        "[link](https://example.com/a b c)"
    );
}

#[tokio::test]
async fn a_malformed_escape_leaves_the_href_untouched() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut message = text_message("link");
    message.entities = vec![text_link_entity(0, 4, "https://example.com/%zz")];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        "[link](https://example.com/%zz)"
    );
}

#[tokio::test]
async fn an_escape_that_decodes_to_invalid_utf8_becomes_a_replacement_character() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut message = text_message("link");
    message.entities = vec![text_link_entity(0, 4, "https://example.com/%FF")];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        "[link](https://example.com/\u{FFFD})"
    );
}

#[test]
fn query_unescape_matches_go() {
    assert_eq!(query_unescape(b"plain"), Ok(b"plain".to_vec()));
    assert_eq!(query_unescape(b"a+b"), Ok(b"a b".to_vec()));
    assert_eq!(query_unescape(b"a%20b"), Ok(b"a b".to_vec()));
    assert_eq!(query_unescape(b"%41%62"), Ok(b"Ab".to_vec()));
    assert_eq!(query_unescape(b"%ff"), Ok(vec![0xFF]));
    assert!(query_unescape(b"%zz").is_err());
    assert!(query_unescape(b"%4").is_err());
    assert!(query_unescape(b"%").is_err());
}

#[test]
fn invalid_utf8_becomes_one_replacement_character_per_byte() {
    assert_eq!(go_string_from_bytes(b"ok"), "ok");
    assert_eq!(go_string_from_bytes(&[0xFF]), "\u{FFFD}");
    assert_eq!(
        go_string_from_bytes(&[b'a', 0xE0, 0x80, b'b']),
        "a\u{FFFD}\u{FFFD}b"
    );
    assert_eq!(go_string_from_bytes(&[0xF0, 0x9F]), "\u{FFFD}\u{FFFD}");
}

#[test]
fn utf16_decoding_replaces_unpaired_surrogates() {
    assert_eq!(decode_utf16_lossy(&[0x0041, 0x0042]), "AB");
    assert_eq!(decode_utf16_lossy(&[0xD83C, 0xDFAE]), "\u{1F3AE}");
    assert_eq!(decode_utf16_lossy(&[0xD83C]), "\u{FFFD}");
    assert_eq!(decode_utf16_lossy(&[0xDFAE, 0x0041]), "\u{FFFD}A");
}

// ---------------------------------------------------------------------------
// Malformed entities
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_entity_reaching_past_the_text_is_skipped() {
    let previewer = FakeLinkPreviewer::titled(&["Example"]);
    let processor = preprocessor(previewer.clone(), FakeSummarizer::new());

    let mut message = text_message("short");
    message.entities = vec![url_entity(100, 5)];

    assert_eq!(processor.extract_text_from_message(&message).await, "short");
    assert!(previewer.calls().is_empty());
}

#[tokio::test]
async fn an_entity_that_overflows_its_end_index_is_skipped() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut message = text_message("short");
    message.entities = vec![text_link_entity(usize::MAX, 5, "https://x.example")];

    assert_eq!(processor.extract_text_from_message(&message).await, "short");
}

#[tokio::test]
async fn an_entity_ending_one_past_the_text_is_skipped_while_its_neighbour_survives() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut message = text_message("abcd");
    message.entities = vec![
        text_link_entity(0, 2, "https://ok.example"),
        text_link_entity(3, 2, "https://bad.example"),
    ];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        "[ab](https://ok.example)cd"
    );
}

#[tokio::test]
async fn a_zero_length_entity_inserts_an_empty_title() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut message = text_message("abcd");
    message.entities = vec![text_link_entity(2, 0, "https://x.example")];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        "ab[](https://x.example)cd"
    );
}

// ---------------------------------------------------------------------------
// Row assembly, forwarding, and the reply snapshot
// ---------------------------------------------------------------------------

fn reply_author() -> CapturedUser {
    CapturedUser {
        id: 7,
        username: "replier".to_string(),
        first_name: "Ada".to_string(),
        last_name: "Lovelace".to_string(),
    }
}

#[tokio::test]
async fn a_plain_message_becomes_a_row() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let row = processor
        .capture_message(&text_message("hello there"))
        .await
        .expect("capture")
        .expect("row");

    assert_eq!(row.chat_id, -100_123);
    assert_eq!(row.chat_type, "supergroup");
    assert_eq!(row.chat_title, "Parity Lab");
    assert_eq!(row.message_id, 1);
    assert_eq!(row.user_id, 42);
    assert_eq!(row.username, "author");
    assert_eq!(row.full_name, "John Smith");
    assert_eq!(row.text, "hello there");
    assert_eq!(row.chatted_at, 1_700_000_000_000);
    assert_eq!(row.replied_to_message_id, 0);
    assert_eq!(row.replied_to_text, "");
}

#[tokio::test]
async fn a_message_without_text_or_caption_produces_no_row() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut message = text_message("");
    message.entities = vec![text_link_entity(0, 0, "https://x.example")];

    assert!(
        processor
            .capture_message(&message)
            .await
            .expect("capture")
            .is_none()
    );
}

#[tokio::test]
async fn the_reply_snapshot_bypasses_the_both_empty_guard() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    // The very message that yields no row on its own still produces a reply
    // snapshot, because Go's reply path calls extractTextWithSummarization
    // directly instead of the guarded extractTextFromMessage.
    let mut reply = text_message("");
    reply.message_id = 99;
    reply.from = reply_author();
    reply.chat.kind = "group".to_string();
    reply.entities = vec![text_link_entity(0, 0, "https://x.example")];

    let mut message = text_message("answering you");
    message.reply_to_message = Some(Box::new(reply));

    let row = processor
        .capture_message(&message)
        .await
        .expect("capture")
        .expect("row");

    assert_eq!(row.replied_to_message_id, 99);
    assert_eq!(row.replied_to_user_id, 7);
    assert_eq!(row.replied_to_full_name, "Ada Lovelace");
    assert_eq!(row.replied_to_username, "replier");
    assert_eq!(row.replied_to_text, "[](https://x.example)");
    assert_eq!(row.replied_to_chat_type, "group");
}

#[tokio::test]
async fn an_empty_reply_extraction_leaves_the_snapshot_columns_untouched() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut reply = text_message("");
    reply.message_id = 99;
    reply.from = reply_author();

    let mut message = text_message("answering you");
    message.reply_to_message = Some(Box::new(reply));

    let row = processor
        .capture_message(&message)
        .await
        .expect("capture")
        .expect("row");

    assert_eq!(row.replied_to_message_id, 0);
    assert_eq!(row.replied_to_user_id, 0);
    assert_eq!(row.replied_to_full_name, "");
    assert_eq!(row.replied_to_username, "");
    assert_eq!(row.replied_to_text, "");
    assert_eq!(row.replied_to_chat_type, "");
}

#[tokio::test]
async fn forward_from_wins_over_forward_from_chat() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut message = text_message("relayed words");
    message.forward_from = Some(CapturedUser {
        id: 5,
        username: "origin".to_string(),
        first_name: "Grace".to_string(),
        last_name: "Hopper".to_string(),
    });
    message.forward_from_chat = Some(CapturedChat {
        id: -100_999,
        kind: "channel".to_string(),
        title: "Broadcast".to_string(),
    });

    let row = processor
        .capture_message(&message)
        .await
        .expect("capture")
        .expect("row");

    assert_eq!(row.text, "[forwarded from Grace Hopper]: relayed words");
}

#[tokio::test]
async fn forward_from_chat_is_the_fallback() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let mut message = text_message("relayed words");
    message.forward_from_chat = Some(CapturedChat {
        id: -100_999,
        kind: "channel".to_string(),
        title: "Broadcast".to_string(),
    });

    let row = processor
        .capture_message(&message)
        .await
        .expect("capture")
        .expect("row");

    assert_eq!(row.text, "[forwarded from Broadcast]: relayed words");
}

// ---------------------------------------------------------------------------
// Edited messages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_edited_message_yields_its_new_text() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    let capture = processor
        .capture_edited_message(Some(&text_message("corrected words")))
        .await
        .expect("capture")
        .expect("edit");

    assert_eq!(capture.chat_id, -100_123);
    assert_eq!(capture.message_id, 1);
    assert_eq!(capture.text, "corrected words");
}

#[tokio::test]
async fn an_absent_or_empty_edited_message_yields_nothing() {
    let processor = preprocessor(FakeLinkPreviewer::with(Vec::new()), FakeSummarizer::new());

    assert!(
        processor
            .capture_edited_message(None)
            .await
            .expect("capture")
            .is_none()
    );
    assert!(
        processor
            .capture_edited_message(Some(&text_message("")))
            .await
            .expect("capture")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn full_name_covers_every_empty_and_non_empty_combination() {
    assert_eq!(full_name_from_first_and_last_name("", ""), "");
    assert_eq!(full_name_from_first_and_last_name("John", ""), "John");
    assert_eq!(full_name_from_first_and_last_name("", "Smith"), "Smith");
    assert_eq!(
        full_name_from_first_and_last_name("John", "Smith"),
        "John Smith"
    );
}

#[test]
fn full_name_orders_cjk_names_family_name_first() {
    // Both CJK: the last name leads.
    assert_eq!(full_name_from_first_and_last_name("小明", "王"), "王 小明");
    // Hangul and Katakana count as CJK too.
    assert_eq!(full_name_from_first_and_last_name("길동", "홍"), "홍 길동");
    assert_eq!(
        full_name_from_first_and_last_name("タロウ", "ヤマダ"),
        "ヤマダ タロウ"
    );
    // Only the last name is CJK: it still leads.
    assert_eq!(full_name_from_first_and_last_name("John", "王"), "王 John");
    // Only the first name is CJK: the Latin last name trails.
    assert_eq!(
        full_name_from_first_and_last_name("太郎", "Smith"),
        "太郎 Smith"
    );
    // Neither is CJK: plain Latin order.
    assert_eq!(
        full_name_from_first_and_last_name("Ada", "Lovelace"),
        "Ada Lovelace"
    );
}

#[test]
fn cjk_detection_matches_the_go_predicate() {
    assert!(!contains_cjk_char(""));
    assert!(!contains_cjk_char("Ada Lovelace"));
    assert!(!contains_cjk_char("\u{1F600}"));
    assert!(contains_cjk_char("漢"));
    assert!(contains_cjk_char("한"));
    assert!(contains_cjk_char("ひらがな"));
    assert!(contains_cjk_char("カタカナ"));
    // U+3001-U+303D, the explicit punctuation span in the Go helper.
    assert!(contains_cjk_char("、"));
    assert!(contains_cjk_char("〜"));
    // U+3005 and U+3007 are Han with a stride of two; U+3006 is not.
    assert!(contains_cjk_char("\u{3005}"));
    assert!(contains_cjk_char("\u{3007}"));
    // Supplementary-plane Han.
    assert!(contains_cjk_char("\u{20000}"));
}

#[test]
fn telegram_seconds_become_unix_milliseconds() {
    assert_eq!(telegram_date_to_unix_millis(0), 0);
    assert_eq!(telegram_date_to_unix_millis(1), 1_000);
    assert_eq!(
        telegram_date_to_unix_millis(1_700_000_000),
        1_700_000_000_000
    );
    assert_eq!(telegram_date_to_unix_millis(-1), -1_000);
}

// ---------------------------------------------------------------------------
// The placeholder production previewer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_unavailable_previewer_fails_instead_of_inventing_a_title() {
    let processor = MessagePreprocessor::new(UnavailableLinkPreviewer, FakeSummarizer::new());

    let mut message = text_message(&format!("see {EXAMPLE_URL} now"));
    message.entities = vec![url_entity(4, EXAMPLE_URL_UNITS)];

    assert_eq!(
        processor.extract_text_from_message(&message).await,
        format!("see {EXAMPLE_URL} now")
    );
}
