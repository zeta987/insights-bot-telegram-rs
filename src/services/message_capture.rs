//! Telegram message preprocessing, ported from Go v1.0.0.
//!
//! Every rule here is pinned to
//! `internal/models/chathistories/chat_histories.go` at Go commit
//! `02aee8ce260165592e2152eb5a024a602e4eced1`:
//!
//! * `ExtractTextFromMessage` — caption wins over text, `message.Entities`
//!   drive Markdown link rewriting, `caption_entities` are never read.
//! * `extractTextWithSummarization` — texts of at least 300 Unicode runes go
//!   through `SummarizeOneChatHistory` on `context.Background()`.
//! * `extractTextFromMessage` — the outer both-empty guard.
//! * `assignReplyMessageDataForChatHistory` — the reply snapshot, which calls
//!   `extractTextWithSummarization` directly and therefore bypasses that guard.
//! * `SaveOneTelegramChatHistory` — the forwarding prefix and the row shape.
//! * `UpdateOneTelegramChatHistory` — the edited-message path.
//!
//! `FullNameFromFirstAndLastName` comes from `pkg/bots/tgbot/utils.go`, and the
//! CJK predicate behind it from `github.com/nekomeowww/xo@v1.9.6` `string.go`.
//!
//! The network and model calls Go makes inline are injected here as the
//! [`LinkPreviewer`] and [`Summarizer`] seams, so the whole module is testable
//! without touching the network.
//!
//! # Deliberate divergence from Go
//!
//! Go slices `textUTF16[entity.Offset : entity.Offset+entity.Length]` without
//! bounds checks, so a malformed or out-of-range entity panics the goroutine.
//! Rust cannot panic a bot process over attacker-shaped input, so an entity
//! whose UTF-16 range does not fit the message is skipped, exactly as Go skips
//! an entity whose preview failed. Every in-range case keeps Go's behaviour,
//! including UTF-16 ranges that cut a surrogate pair in half: those decode to
//! U+FFFD here just as `utf16.Decode` produces U+FFFD in Go.

use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::future::join_all;
use tracing::warn;

use crate::db::models::NewTelegramChatHistory;

/// Go wraps `linkprev.Preview` in `context.WithTimeout(.., 10*time.Second)`.
pub const LINK_PREVIEW_TIMEOUT: Duration = Duration::from_secs(10);

/// Go wraps `SummarizeAny` in `context.WithTimeout(.., time.Minute)`.
pub const TITLE_SUMMARIZATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Go summarizes a link title when `utf8.RuneCountInString(title) > 200`.
pub const TITLE_SUMMARIZATION_RUNE_THRESHOLD: usize = 200;

/// Go summarizes message text when `utf8.RuneCountInString(text) >= 300`.
pub const TEXT_SUMMARIZATION_RUNE_THRESHOLD: usize = 300;

// ---------------------------------------------------------------------------
// Input model
// ---------------------------------------------------------------------------

/// The pieces of a Telegram user Go reads while building a chat history row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapturedUser {
    pub id: i64,
    pub username: String,
    pub first_name: String,
    pub last_name: String,
}

/// The pieces of a Telegram chat Go reads while building a chat history row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapturedChat {
    pub id: i64,
    /// Telegram's wire spelling, such as `group` or `supergroup`.
    pub kind: String,
    pub title: String,
}

/// The entity types Go's `switch entity.Type` distinguishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedEntityKind {
    /// Go's `case "url"`: the href is the entity's own slice of the text.
    Url,
    /// Go's `case "text_link"`: the href is `entity.URL`, the title the slice.
    TextLink { url: String },
    /// Go's `default`: contributes no rewrite.
    Other,
}

/// One Telegram message entity, with Telegram's UTF-16 code unit indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedEntity {
    pub kind: CapturedEntityKind,
    /// Offset in UTF-16 code units, as Telegram counts them.
    pub offset: usize,
    /// Length in UTF-16 code units, as Telegram counts them.
    pub length: usize,
}

/// A Telegram message reduced to what the Go preprocessing reads.
///
/// Absent strings are empty strings, matching Go's zero values, so the
/// `message.Text == "" && message.Caption == ""` guard ports literally.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapturedMessage {
    pub message_id: i64,
    /// Telegram's `date`, in Unix seconds.
    pub date: i64,
    pub chat: CapturedChat,
    pub from: CapturedUser,
    pub text: String,
    pub caption: String,
    /// Go's `message.Entities`, the only entity list it rewrites.
    pub entities: Vec<CapturedEntity>,
    /// Go never reads `caption_entities`; carried here to keep that provable.
    pub caption_entities: Vec<CapturedEntity>,
    pub forward_from: Option<CapturedUser>,
    pub forward_from_chat: Option<CapturedChat>,
    pub reply_to_message: Option<Box<CapturedMessage>>,
}

/// What Go's `UpdateOneTelegramChatHistory` would write, without the write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditedMessageCapture {
    pub chat_id: i64,
    pub message_id: i64,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Injected seams
// ---------------------------------------------------------------------------

/// The subset of `linkprev.Meta` Go reads: `Title`, then `OpenGraph.Title`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewMeta {
    pub title: String,
    pub open_graph_title: String,
}

/// Go's `linkprev.Client.Preview`.
#[async_trait]
pub trait LinkPreviewer: Send + Sync {
    /// `deadline` mirrors the Go context deadline the caller installed. The
    /// caller enforces it as well, so an implementation that ignores it still
    /// gets cut off at the Go timeout.
    async fn preview(&self, url: &str, deadline: Duration) -> Result<PreviewMeta>;
}

/// The two `openai.Client` methods Go's preprocessing calls.
///
/// Both return the choice contents in order, so an empty vector is Go's
/// `len(resp.Choices) == 0`.
#[async_trait]
pub trait Summarizer: Send + Sync {
    /// Go's `SummarizeAny`, called under a one-minute context.
    async fn summarize_any(&self, content: &str, deadline: Option<Duration>)
    -> Result<Vec<String>>;

    /// Go's `SummarizeOneChatHistory`, called on `context.Background()`, which
    /// is why `deadline` is `None` on every call the core makes.
    async fn summarize_one_chat_history(
        &self,
        content: &str,
        deadline: Option<Duration>,
    ) -> Result<Vec<String>>;
}

#[async_trait]
impl<T: LinkPreviewer + ?Sized> LinkPreviewer for std::sync::Arc<T> {
    async fn preview(&self, url: &str, deadline: Duration) -> Result<PreviewMeta> {
        (**self).preview(url, deadline).await
    }
}

#[async_trait]
impl<T: Summarizer + ?Sized> Summarizer for std::sync::Arc<T> {
    async fn summarize_any(
        &self,
        content: &str,
        deadline: Option<Duration>,
    ) -> Result<Vec<String>> {
        (**self).summarize_any(content, deadline).await
    }

    async fn summarize_one_chat_history(
        &self,
        content: &str,
        deadline: Option<Duration>,
    ) -> Result<Vec<String>> {
        (**self).summarize_one_chat_history(content, deadline).await
    }
}

// ---------------------------------------------------------------------------
// Core
// ---------------------------------------------------------------------------

/// A link rewrite Go stages before applying them back to front.
struct MarkdownLink {
    markdown: Vec<u16>,
    start: usize,
    end: usize,
}

/// Go's `chathistories.Model`, reduced to the preprocessing it performs.
pub struct MessagePreprocessor<P, S> {
    previewer: P,
    summarizer: S,
}

impl<P, S> MessagePreprocessor<P, S>
where
    P: LinkPreviewer,
    S: Summarizer,
{
    pub fn new(previewer: P, summarizer: S) -> Self {
        Self {
            previewer,
            summarizer,
        }
    }

    /// Go's exported `ExtractTextFromMessage`.
    ///
    /// Caption wins over text, then every `url` and `text_link` entity in
    /// `message.Entities` is turned into `[title](href)` and spliced back in
    /// from the last entity to the first, using the original UTF-16 offsets.
    pub async fn extract_text_from_message(&self, message: &CapturedMessage) -> String {
        let text = if message.caption.is_empty() {
            &message.text
        } else {
            &message.caption
        };

        let mut text_utf16: Vec<u16> = text.encode_utf16().collect();

        let links = join_all(
            message
                .entities
                .iter()
                .map(|entity| self.build_markdown_link(entity, &text_utf16)),
        )
        .await;

        for link in links.iter().rev().flatten() {
            // Go indexes the live buffer here, so a rewrite that pushed an
            // earlier entity's end past the current length would panic there.
            if link.start > link.end || link.end > text_utf16.len() {
                warn!("skipping a link rewrite whose range no longer fits the text");
                continue;
            }

            let mut rewritten =
                Vec::with_capacity(link.start + link.markdown.len() + text_utf16.len() - link.end);
            rewritten.extend_from_slice(&text_utf16[..link.start]);
            rewritten.extend_from_slice(&link.markdown);
            rewritten.extend_from_slice(&text_utf16[link.end..]);
            text_utf16 = rewritten;
        }

        decode_utf16_lossy(&text_utf16)
    }

    /// One iteration of Go's `lop.Map` over `message.Entities`.
    ///
    /// `None` is Go's `MarkdownLink{[]uint16{}, -1, -1}` sentinel.
    async fn build_markdown_link(
        &self,
        entity: &CapturedEntity,
        text_utf16: &[u16],
    ) -> Option<MarkdownLink> {
        let start = entity.offset;
        let end = start.checked_add(entity.length)?;
        if end > text_utf16.len() {
            // Deliberate divergence: Go panics on this slice.
            warn!("skipping an entity whose UTF-16 range falls outside the message");
            return None;
        }

        let (mut title, mut href) = match &entity.kind {
            CapturedEntityKind::Url => {
                let href = decode_utf16_lossy(&text_utf16[start..end]);

                let preview = tokio::time::timeout(
                    LINK_PREVIEW_TIMEOUT,
                    self.previewer.preview(&href, LINK_PREVIEW_TIMEOUT),
                )
                .await;

                let meta = match preview {
                    Ok(Ok(meta)) => meta,
                    Ok(Err(_)) => {
                        warn!("failed to generate link preview; leaving the entity unrewritten");
                        return None;
                    }
                    Err(_) => {
                        warn!("link preview timed out; leaving the entity unrewritten");
                        return None;
                    }
                };

                let title = if meta.title.is_empty() {
                    meta.open_graph_title
                } else {
                    meta.title
                };

                (title, href)
            }
            CapturedEntityKind::TextLink { url } => {
                (decode_utf16_lossy(&text_utf16[start..end]), url.clone())
            }
            CapturedEntityKind::Other => return None,
        };

        if title.chars().count() > TITLE_SUMMARIZATION_RUNE_THRESHOLD {
            let summarized = tokio::time::timeout(
                TITLE_SUMMARIZATION_TIMEOUT,
                self.summarizer
                    .summarize_any(&title, Some(TITLE_SUMMARIZATION_TIMEOUT)),
            )
            .await;

            let choices = match summarized {
                Ok(Ok(choices)) => choices,
                Ok(Err(_)) => {
                    warn!("failed to summarize a link title; leaving the entity unrewritten");
                    return None;
                }
                Err(_) => {
                    warn!("link title summarization timed out; leaving the entity unrewritten");
                    return None;
                }
            };

            // Go leaves the long title untouched when there are no choices.
            if let Some(first) = choices.first() {
                title = first.clone();
            }
        }

        if let Ok(unescaped) = query_unescape(href.as_bytes()) {
            href = go_string_from_bytes(&unescaped);
        }

        let markdown = format!("[{title}]({href})").encode_utf16().collect();

        Some(MarkdownLink {
            markdown,
            start,
            end,
        })
    }

    /// Go's unexported `extractTextWithSummarization`.
    pub async fn extract_text_with_summarization(
        &self,
        message: &CapturedMessage,
    ) -> Result<String> {
        let text = self.extract_text_from_message(message).await;
        if text.is_empty() {
            return Ok(String::new());
        }

        if text.chars().count() >= TEXT_SUMMARIZATION_RUNE_THRESHOLD {
            // Go passes context.Background(): no deadline at all.
            let choices = self
                .summarizer
                .summarize_one_chat_history(&text, None)
                .await?;
            return Ok(choices.first().cloned().unwrap_or_default());
        }

        Ok(text)
    }

    /// Go's unexported `extractTextFromMessage`, including its both-empty guard.
    pub async fn extract_text_guarded(&self, message: Option<&CapturedMessage>) -> Result<String> {
        let Some(message) = message else {
            return Ok(String::new());
        };
        if message.text.is_empty() && message.caption.is_empty() {
            warn!("message text is empty");
            return Ok(String::new());
        }

        let text = self.extract_text_with_summarization(message).await?;
        if text.is_empty() {
            warn!("message text is empty");
            return Ok(String::new());
        }

        Ok(text)
    }

    /// Go's `SaveOneTelegramChatHistory` up to, but not including, the write.
    ///
    /// `None` is Go's early `return nil` for an empty extraction.
    pub async fn capture_message(
        &self,
        message: &CapturedMessage,
    ) -> Result<Option<NewTelegramChatHistory>> {
        let text = self.extract_text_guarded(Some(message)).await?;
        if text.is_empty() {
            return Ok(None);
        }

        // Go checks ForwardFrom first and only then ForwardFromChat.
        let text = if let Some(forward_from) = &message.forward_from {
            let name = full_name_from_first_and_last_name(
                &forward_from.first_name,
                &forward_from.last_name,
            );
            format!("[forwarded from {name}]: {text}")
        } else if let Some(forward_from_chat) = &message.forward_from_chat {
            format!("[forwarded from {}]: {}", forward_from_chat.title, text)
        } else {
            text
        };

        let mut row = NewTelegramChatHistory {
            chat_id: message.chat.id,
            chat_type: message.chat.kind.clone(),
            chat_title: message.chat.title.clone(),
            message_id: message.message_id,
            user_id: message.from.id,
            username: message.from.username.clone(),
            full_name: full_name_from_first_and_last_name(
                &message.from.first_name,
                &message.from.last_name,
            ),
            text,
            chatted_at: telegram_date_to_unix_millis(message.date),
            ..Default::default()
        };

        self.assign_reply_message_data(&mut row, message).await?;

        Ok(Some(row))
    }

    /// Go's `assignReplyMessageDataForChatHistory`.
    ///
    /// It calls `extractTextWithSummarization` directly, so the replied-to
    /// message never goes through the both-empty guard.
    async fn assign_reply_message_data(
        &self,
        row: &mut NewTelegramChatHistory,
        message: &CapturedMessage,
    ) -> Result<()> {
        let Some(reply) = &message.reply_to_message else {
            return Ok(());
        };

        let replied_to_text = self.extract_text_with_summarization(reply).await?;
        if replied_to_text.is_empty() {
            return Ok(());
        }

        row.replied_to_message_id = reply.message_id;
        row.replied_to_user_id = reply.from.id;
        row.replied_to_full_name =
            full_name_from_first_and_last_name(&reply.from.first_name, &reply.from.last_name);
        row.replied_to_username = reply.from.username.clone();
        row.replied_to_text = replied_to_text;
        row.replied_to_chat_type = reply.chat.kind.clone();

        Ok(())
    }

    /// Go's `UpdateOneTelegramChatHistory` up to, but not including, the write.
    pub async fn capture_edited_message(
        &self,
        message: Option<&CapturedMessage>,
    ) -> Result<Option<EditedMessageCapture>> {
        let Some(message) = message else {
            return Ok(None);
        };
        if message.text.is_empty() && message.caption.is_empty() {
            warn!("message text is empty");
            return Ok(None);
        }

        let text = self.extract_text_with_summarization(message).await?;
        if text.is_empty() {
            warn!("message text is empty");
            return Ok(None);
        }

        Ok(Some(EditedMessageCapture {
            chat_id: message.chat.id,
            message_id: message.message_id,
            text,
        }))
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Go's `time.Unix(int64(message.Date), 0).UnixMilli()`.
///
/// Go computes `unixSec * 1000` in `int64`, so the wrap on absurd inputs is
/// reproduced rather than clamped.
pub fn telegram_date_to_unix_millis(date_seconds: i64) -> i64 {
    date_seconds.wrapping_mul(1_000)
}

/// Go's `tgbot.FullNameFromFirstAndLastName`.
///
/// A CJK last name is placed first, which puts a Chinese, Japanese, or Korean
/// name into family-name-first order.
pub fn full_name_from_first_and_last_name(first_name: &str, last_name: &str) -> String {
    if last_name.is_empty() {
        return first_name.to_string();
    }
    if first_name.is_empty() {
        return last_name.to_string();
    }

    let first_is_cjk = contains_cjk_char(first_name);
    let last_is_cjk = contains_cjk_char(last_name);

    if first_is_cjk && !last_is_cjk {
        return format!("{first_name} {last_name}");
    }
    if !first_is_cjk && last_is_cjk {
        return format!("{last_name} {first_name}");
    }
    if first_is_cjk && last_is_cjk {
        return format!("{last_name} {first_name}");
    }

    format!("{first_name} {last_name}")
}

/// Go's `xo.ContainsCJKChar`: Han, Hangul, Hiragana, Katakana, or the
/// `U+3001`–`U+303D` CJK punctuation span.
pub fn contains_cjk_char(s: &str) -> bool {
    s.chars().any(|character| {
        let code_point = character as u32;
        in_ranges(code_point, HAN)
            || in_ranges(code_point, HANGUL)
            || in_ranges(code_point, HIRAGANA)
            || in_ranges(code_point, KATAKANA)
            || (0x3001..=0x303D).contains(&code_point)
    })
}

/// One `unicode.RangeTable` entry: inclusive low, inclusive high, stride.
type ScriptRange = (u32, u32, u32);

fn in_ranges(code_point: u32, ranges: &[ScriptRange]) -> bool {
    ranges.iter().any(|&(low, high, stride)| {
        code_point >= low && code_point <= high && (code_point - low).is_multiple_of(stride)
    })
}

/// `unicode.Han`, dumped from the Go toolchain (Unicode 15.0.0).
const HAN: &[ScriptRange] = &[
    (0x2E80, 0x2E99, 1),
    (0x2E9B, 0x2EF3, 1),
    (0x2F00, 0x2FD5, 1),
    (0x3005, 0x3007, 2),
    (0x3021, 0x3029, 1),
    (0x3038, 0x303B, 1),
    (0x3400, 0x4DBF, 1),
    (0x4E00, 0x9FFF, 1),
    (0xF900, 0xFA6D, 1),
    (0xFA70, 0xFAD9, 1),
    (0x1_6FE2, 0x1_6FE3, 1),
    (0x1_6FF0, 0x1_6FF1, 1),
    (0x2_0000, 0x2_A6DF, 1),
    (0x2_A700, 0x2_B739, 1),
    (0x2_B740, 0x2_B81D, 1),
    (0x2_B820, 0x2_CEA1, 1),
    (0x2_CEB0, 0x2_EBE0, 1),
    (0x2_F800, 0x2_FA1D, 1),
    (0x3_0000, 0x3_134A, 1),
    (0x3_1350, 0x3_23AF, 1),
];

/// `unicode.Hangul`, dumped from the Go toolchain (Unicode 15.0.0).
const HANGUL: &[ScriptRange] = &[
    (0x1100, 0x11FF, 1),
    (0x302E, 0x302F, 1),
    (0x3131, 0x318E, 1),
    (0x3200, 0x321E, 1),
    (0x3260, 0x327E, 1),
    (0xA960, 0xA97C, 1),
    (0xAC00, 0xD7A3, 1),
    (0xD7B0, 0xD7C6, 1),
    (0xD7CB, 0xD7FB, 1),
    (0xFFA0, 0xFFBE, 1),
    (0xFFC2, 0xFFC7, 1),
    (0xFFCA, 0xFFCF, 1),
    (0xFFD2, 0xFFD7, 1),
    (0xFFDA, 0xFFDC, 1),
];

/// `unicode.Hiragana`, dumped from the Go toolchain (Unicode 15.0.0).
const HIRAGANA: &[ScriptRange] = &[
    (0x3041, 0x3096, 1),
    (0x309D, 0x309F, 1),
    (0x1_B001, 0x1_B11F, 1),
    (0x1_B132, 0x1_B150, 30),
    (0x1_B151, 0x1_B152, 1),
    (0x1_F200, 0x1_F200, 1),
];

/// `unicode.Katakana`, dumped from the Go toolchain (Unicode 15.0.0).
const KATAKANA: &[ScriptRange] = &[
    (0x30A1, 0x30FA, 1),
    (0x30FD, 0x30FF, 1),
    (0x31F0, 0x31FF, 1),
    (0x32D0, 0x32FE, 1),
    (0x3300, 0x3357, 1),
    (0xFF66, 0xFF6F, 1),
    (0xFF71, 0xFF9D, 1),
    (0x1_AFF0, 0x1_AFF3, 1),
    (0x1_AFF5, 0x1_AFFB, 1),
    (0x1_AFFD, 0x1_AFFE, 1),
    (0x1_B000, 0x1_B120, 288),
    (0x1_B121, 0x1_B122, 1),
    (0x1_B155, 0x1_B164, 15),
    (0x1_B165, 0x1_B167, 1),
];

/// Go's `string(utf16.Decode(..))`: unpaired surrogates become U+FFFD.
pub fn decode_utf16_lossy(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// The escape Go's `url.QueryUnescape` rejects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryUnescapeError;

/// Go's `url.QueryUnescape`.
///
/// Percent triples are decoded, `+` becomes a space, and a malformed percent
/// escape is an error, which leaves the caller's href untouched. The result is
/// a byte string because Go's is: percent escapes can carry non-UTF-8 bytes.
pub fn query_unescape(input: &[u8]) -> Result<Vec<u8>, QueryUnescapeError> {
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            b'%' => {
                if index + 2 >= input.len()
                    || !is_hex(input[index + 1])
                    || !is_hex(input[index + 2])
                {
                    return Err(QueryUnescapeError);
                }
                index += 3;
            }
            _ => index += 1,
        }
    }

    let mut out = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            b'%' => {
                out.push(unhex(input[index + 1]) << 4 | unhex(input[index + 2]));
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }

    Ok(out)
}

/// Go's `ishex`: `0-9`, `a-f`, `A-F`.
fn is_hex(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}

fn unhex(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => byte - b'A' + 10,
    }
}

/// Go's `[]rune(s)` conversion of a byte string that may not be valid UTF-8.
///
/// Go's decoder resynchronises one byte at a time, emitting one U+FFFD per
/// undecodable byte, which is what this reproduces.
pub fn go_string_from_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;

    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                out.push_str(valid);
                return out;
            }
            Err(error) => {
                let (valid, invalid) = rest.split_at(error.valid_up_to());
                // `valid` is UTF-8 by construction.
                out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                let skipped = error.error_len().unwrap_or(invalid.len()).max(1);
                for _ in 0..skipped {
                    out.push(char::REPLACEMENT_CHARACTER);
                }
                rest = &invalid[skipped.min(invalid.len())..];
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Adapters for the wiring that lands in the next slice
// ---------------------------------------------------------------------------

/// A [`LinkPreviewer`] that always fails, which Go treats as "leave the URL
/// alone".
///
/// [`crate::services::link_preview::HttpLinkPreviewer`] is the production
/// implementation; this one stays as the offline seam, both for deployments
/// with no outbound HTTP and for tests that must never open a socket.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableLinkPreviewer;

#[async_trait]
impl LinkPreviewer for UnavailableLinkPreviewer {
    async fn preview(&self, _url: &str, _deadline: Duration) -> Result<PreviewMeta> {
        Err(anyhow!("link preview is not configured"))
    }
}

/// The real [`Summarizer`], backed by the existing OpenAI service.
pub struct OpenAiSummarizer {
    client: std::sync::Arc<crate::services::openai::OpenAiClient>,
}

impl OpenAiSummarizer {
    pub fn new(client: std::sync::Arc<crate::services::openai::OpenAiClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Summarizer for OpenAiSummarizer {
    async fn summarize_any(
        &self,
        content: &str,
        _deadline: Option<Duration>,
    ) -> Result<Vec<String>> {
        // The caller installs the Go deadline around this call.
        self.client.summarize_any(content).await
    }

    async fn summarize_one_chat_history(
        &self,
        content: &str,
        _deadline: Option<Duration>,
    ) -> Result<Vec<String>> {
        self.client.summarize_one_chat_history(content).await
    }
}

/// Go's `tgbotapi.MessageEntity`, read through teloxide's typed entity.
///
/// # The `text_link` href is normalised, and that is observable
///
/// Telegram's Bot API sends a `text_link` entity as
/// `{"type":"text_link","offset":..,"length":..,"url":"<raw string>"}`, and
/// `go-telegram-bot-api` stores `URL string`, so Go's
/// `href = entity.URL` is byte-for-byte what Telegram sent.
///
/// teloxide types the same field as
/// `MessageEntityKind::TextLink { url: reqwest::Url }`
/// (`teloxide-core-0.11.2/src/types/message_entity.rs`), so serde runs
/// `Url::parse` during deserialization and the raw string is gone by the time
/// any handler sees the message. Recovering it means `Url::to_string`, which
/// returns the WHATWG serialization, not the input:
///
/// | Telegram sends            | Go's `entity.URL`         | this adapter               |
/// |---------------------------|---------------------------|----------------------------|
/// | `https://example.com`     | `https://example.com`     | `https://example.com/`     |
/// | `HTTPS://Example.COM/a`   | `HTTPS://Example.COM/a`   | `https://example.com/a`    |
/// | `https://例え.jp/`        | `https://例え.jp/`        | `https://xn--r8jz45g.jp/`  |
///
/// The difference reaches the stored chat history, because the href goes
/// straight into `[title](href)`. It is unavoidable at the typed `Message`
/// level: the only way back to the raw string is parsing the raw `Update` JSON,
/// which is out of scope for this slice. `tests/message_entity_tests.rs` pins
/// the exact strings above.
///
/// Telegram's `offset` and `length` are already UTF-16 code unit counts, and
/// teloxide keeps them that way, so those are carried over untouched.
pub fn captured_entity_from_teloxide(entity: &teloxide::types::MessageEntity) -> CapturedEntity {
    use teloxide::types::MessageEntityKind;

    CapturedEntity {
        kind: match &entity.kind {
            MessageEntityKind::Url => CapturedEntityKind::Url,
            MessageEntityKind::TextLink { url } => CapturedEntityKind::TextLink {
                url: url.to_string(),
            },
            _ => CapturedEntityKind::Other,
        },
        offset: entity.offset,
        length: entity.length,
    }
}

/// Builds a [`CapturedMessage`] from a teloxide message.
///
/// See [`captured_entity_from_teloxide`] for the one representation difference
/// against Go: the `text_link` href arrives here normalised.
pub fn captured_message_from_teloxide(message: &teloxide::types::Message) -> CapturedMessage {
    use teloxide::types::MessageEntity;

    fn chat_kind(chat: &teloxide::types::Chat) -> String {
        if chat.is_private() {
            "private"
        } else if chat.is_supergroup() {
            "supergroup"
        } else if chat.is_channel() {
            "channel"
        } else {
            "group"
        }
        .to_string()
    }

    fn captured_chat(chat: &teloxide::types::Chat) -> CapturedChat {
        CapturedChat {
            id: chat.id.0,
            kind: chat_kind(chat),
            title: chat.title().unwrap_or_default().to_string(),
        }
    }

    fn captured_user(user: &teloxide::types::User) -> CapturedUser {
        CapturedUser {
            id: user.id.0 as i64,
            username: user.username.clone().unwrap_or_default(),
            first_name: user.first_name.clone(),
            last_name: user.last_name.clone().unwrap_or_default(),
        }
    }

    fn captured_entities(entities: Option<&[MessageEntity]>) -> Vec<CapturedEntity> {
        entities
            .unwrap_or_default()
            .iter()
            .map(captured_entity_from_teloxide)
            .collect()
    }

    CapturedMessage {
        message_id: message.id.0 as i64,
        date: message.date.timestamp(),
        chat: captured_chat(&message.chat),
        from: message.from.as_ref().map(captured_user).unwrap_or_default(),
        text: message.text().unwrap_or_default().to_string(),
        caption: message.caption().unwrap_or_default().to_string(),
        entities: captured_entities(message.entities()),
        caption_entities: captured_entities(message.caption_entities()),
        forward_from: message.forward_from_user().map(captured_user),
        forward_from_chat: message.forward_from_chat().map(captured_chat),
        reply_to_message: message
            .reply_to_message()
            .map(|reply| Box::new(captured_message_from_teloxide(reply))),
    }
}
