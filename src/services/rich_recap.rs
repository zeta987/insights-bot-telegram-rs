//! Go v1.0.0 Rich recap formatting primitives.
//!
//! This module owns only deterministic text transformations. Network delivery,
//! persistence, and OpenAI execution remain separate so the exact Markdown
//! contract can be tested without external services.

use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

use regex::{Captures, Regex};

/// Telegram's Rich Message text limit, measured in UTF-16 code units.
pub const RICH_MESSAGE_UTF16_UNIT_LIMIT: usize = 32_768;

const RICH_RECAP_DETAILS_OPEN: &str = "<details><summary>詳細總結</summary>\n\n";
const RICH_RECAP_DETAILS_CLOSE: &str = "\n\n</details>";
const CONDENSED_SUMMARY_FALLBACK_UNIT_LIMIT: usize = 120;
const MAX_REFERENCES_PER_MARKER: usize = 5;
const RICH_MARKDOWN_ESCAPABLE_CHARACTERS: &str = "\\*_{}[]()#+-.!|><~`";

static RICH_RECAP_REFERENCE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{tg-ref:([^{}\r\n]*)\}\}").expect("valid reference regex"));
static RICH_MARKDOWN_HTML_TAG_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)</?[a-z][^>\r\n]*>").expect("valid Rich Markdown HTML regex")
});
static RICH_MARKDOWN_INLINE_LINK_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"!?\[([^\]\r\n]+)\]\([^)\r\n]+\)").expect("valid inline link regex")
});
static RICH_MARKDOWN_BARE_URL_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^ \t\n\f\r<>()]+").expect("valid bare URL regex"));
static RICH_MARKDOWN_MENTION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|[^0-9A-Za-z_])@+([0-9A-Za-z_])").expect("valid mention regex")
});
static RICH_MARKDOWN_LINK_SPAN_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"!?\[[^\]\r\n]*\]\([^) \t\n\f\r]+\)").expect("valid protected link regex")
});
static RICH_MARKDOWN_INLINE_SPAN_PATTERNS: LazyLock<[Regex; 6]> = LazyLock::new(|| {
    [
        Regex::new(r"\*\*((?:\\.|[^*\r\n])*)\*\*").expect("valid bold regex"),
        Regex::new(r"__((?:\\.|[^_\r\n])*)__").expect("valid underline regex"),
        Regex::new(r"~~((?:\\.|[^~\r\n])*)~~").expect("valid strike regex"),
        Regex::new(r"`([^`\r\n]*)`").expect("valid inline code regex"),
        Regex::new(r"\*((?:\\.|[^*\r\n])*)\*").expect("valid italic regex"),
        Regex::new(r"_((?:\\.|[^_\r\n])*)_").expect("valid underscore italic regex"),
    ]
});
static CONDENSED_SUMMARY_LABEL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^[ \t\n\f\r]*(?:\*\*)?(?:濃縮總結|浓缩总结)(?:\*\*)?[ \t\n\f\r]*[：:]?[ \t\n\f\r]*(?:🤖(?:️)?[ \t\n\f\r]*)?",
    )
        .expect("valid condensed label regex")
});
static RICH_MARKDOWN_DETAILS_OPEN_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<details>[ \t\n\f\r]*<summary>([^<]*)</summary>[ \t\n\f\r]*")
        .expect("valid details regex")
});
static RICH_MARKDOWN_URL_LINK_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[([^\]\r\n]+)\]\((https?://[^) \t\n\f\r]+)\)").expect("valid URL link regex")
});
static RICH_MARKDOWN_USER_LINK_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[((?:\\.|[^\]\r\n])*)\]\(tg://user\?id=[0-9]+\)")
        .expect("valid Telegram user link regex")
});
static RICH_MARKDOWN_HEADING_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^#{1,6}[ \t]+").expect("valid heading regex"));
static RICH_MARKDOWN_QUOTE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[ \t]*>[ \t]?").expect("valid quote regex"));
static PLAIN_TEXT_CITATION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[^ \t\n\f\r]+[ \t]+\(https?://[^) \t\n\f\r]+\)")
        .expect("valid plain citation regex")
});

/// Go's trace for one primary-plus-backup model generation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenerationModelExecutionTrace {
    pub primary_model: String,
    pub primary_used_model: String,
    pub primary_failed: bool,
    pub primary_failure_reason: String,
    pub backup_model: String,
    pub backup_used: bool,
    pub backup_used_model: String,
    pub backup_succeeded: bool,
    pub backup_failure_reason: String,
}

/// Go's detailed recap execution trace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecapExecutionTrace {
    pub generation: GenerationModelExecutionTrace,
}

/// Go's optional check-model execution trace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionalModelExecutionTrace {
    pub generation: GenerationModelExecutionTrace,
    pub attempted: bool,
    pub succeeded: bool,
    pub failure_reason: String,
}

/// Go's condensed recap generation and optional repair trace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CondensedExecutionTrace {
    pub generation: GenerationModelExecutionTrace,
    pub check: ConditionalModelExecutionTrace,
}

/// Inputs for Go's always-visible Rich recap prefix.
#[derive(Debug, Clone, Default)]
pub struct RichRecapSummaryConfig<'a> {
    pub title: &'a str,
    pub hours: i64,
    pub automatic: bool,
    pub initiator_name: &'a str,
    pub initiator_user_id: i64,
    pub condensed_summary: &'a str,
    pub general_group_notice: bool,
    pub subscription_chat_title: &'a str,
    pub condensed_trace: Option<&'a CondensedExecutionTrace>,
    pub recap_trace: Option<&'a RecapExecutionTrace>,
}

/// Escape untrusted plain text for application-authored Rich Markdown.
#[must_use]
pub fn escape_rich_markdown_text(text: &str) -> String {
    let normalized = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
    let trimmed = normalized.trim();
    let mut escaped = String::with_capacity(trimmed.len());

    for character in trimmed.chars() {
        if RICH_MARKDOWN_ESCAPABLE_CHARACTERS.contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }

    escaped
}

/// Remove only the escapes introduced by [`escape_rich_markdown_text`].
#[must_use]
pub fn unescape_rich_markdown_text(text: &str) -> String {
    let mut characters = text.chars().peekable();
    let mut unescaped = String::with_capacity(text.len());

    while let Some(character) = characters.next() {
        if character == '\\'
            && let Some(next) = characters.peek().copied()
            && RICH_MARKDOWN_ESCAPABLE_CHARACTERS.contains(next)
        {
            unescaped.push(next);
            characters.next();
            continue;
        }
        unescaped.push(character);
    }

    unescaped
}

/// Keep the supported detailed recap structure while removing model-authored
/// HTML, external links, code-fence delimiters, and live mentions.
#[must_use]
pub fn sanitize_detailed_recap_markdown(markdown: &str) -> String {
    sanitize_rich_recap_markdown(markdown, |line| {
        let mut transformed = line.to_owned();
        let trimmed = transformed.trim();

        if trimmed.starts_with('>') {
            transformed = trimmed.trim_start_matches('>').trim().to_owned();
        }

        if transformed.trim().starts_with("* ") {
            let indent_length =
                transformed.len() - transformed.trim_start_matches([' ', '\t']).len();
            let indentation = &transformed[..indent_length];
            let content = transformed
                .trim()
                .strip_prefix("* ")
                .unwrap_or_default()
                .trim();
            transformed = format!("{indentation}- {content}");
        }

        if transformed.contains('|') {
            transformed = transformed.replace('|', "\\|");
        }

        transformed
    })
}

/// Preserve condensed recap visual structure while removing model-authored
/// HTML, external links, code-fence delimiters, and live mentions.
#[must_use]
pub fn sanitize_condensed_recap_markdown(markdown: &str) -> String {
    sanitize_rich_recap_markdown(markdown, |line| {
        let mut transformed = line.to_owned();
        let mut trimmed = transformed.trim();

        if let Some(content) = trimmed.strip_prefix("* ") {
            transformed = format!("- {}", content.trim());
            trimmed = transformed.trim();
        }

        if trimmed.starts_with("# ") || trimmed.starts_with("## ") {
            transformed = format!("### {}", trimmed.trim_start_matches('#').trim());
        }

        transformed
    })
}

/// Replace controlled virtual-message references with at most five unique,
/// whitelisted Telegram supergroup links.
#[must_use]
pub fn resolve_rich_recap_references(
    markdown: &str,
    chat_id: i64,
    chat_type: &str,
    virtual_to_real: &HashMap<i64, i64>,
) -> String {
    RICH_RECAP_REFERENCE_PATTERN
        .replace_all(markdown, |captures: &Captures<'_>| {
            if chat_type != "supergroup" {
                return String::new();
            }

            let mut links = Vec::with_capacity(MAX_REFERENCES_PER_MARKER);
            let mut seen_real_ids = HashSet::with_capacity(MAX_REFERENCES_PER_MARKER);
            for raw_id in captures
                .get(1)
                .map_or("", |capture| capture.as_str())
                .split(',')
            {
                let Ok(virtual_id) = raw_id.trim().parse::<i64>() else {
                    continue;
                };
                if virtual_id <= 0 {
                    continue;
                }

                let Some(real_id) = virtual_to_real.get(&virtual_id).copied() else {
                    continue;
                };
                if real_id <= 0 || !seen_real_ids.insert(real_id) {
                    continue;
                }

                links.push(format!(
                    "[{}](https://t.me/c/{}/{real_id})",
                    links.len() + 1,
                    format_chat_id(chat_id)
                ));
                if links.len() == MAX_REFERENCES_PER_MARKER {
                    break;
                }
            }

            links.join(" ")
        })
        .into_owned()
}

/// Render the exact five-line model provenance footer used by Go v1.0.0.
#[must_use]
pub fn build_rich_recap_model_info(
    condensed_trace: Option<&CondensedExecutionTrace>,
    recap_trace: Option<&RecapExecutionTrace>,
) -> String {
    let condensed_model = condensed_trace.map_or_else(
        || "資訊不可用".to_owned(),
        |trace| {
            let mut generation = trace.generation.clone();
            if trace.check.succeeded
                && generation.backup_used
                && !generation.backup_used_model.trim().is_empty()
            {
                generation.backup_succeeded = true;
            }
            generation_model_name(&generation)
        },
    );

    let detail_model = recap_trace.map_or_else(
        || "資訊不可用".to_owned(),
        |trace| generation_model_name(&trace.generation),
    );

    let check_model = condensed_trace.map_or_else(
        || "資訊不可用".to_owned(),
        |trace| {
            let configured = trace.check.generation.primary_model.trim();
            if configured.is_empty() {
                "未設定".to_owned()
            } else if trace.check.attempted {
                generation_model_name(&trace.check.generation)
            } else {
                configured.to_owned()
            }
        },
    );

    [
        "> **模型資訊**".to_owned(),
        ">".to_owned(),
        format!(
            "> - 濃縮總結：{}",
            escape_rich_markdown_text(&condensed_model)
        ),
        format!("> - 詳細總結：{}", escape_rich_markdown_text(&detail_model)),
        format!("> - Check：{}", escape_rich_markdown_text(&check_model)),
    ]
    .join("\n")
}

/// Place the condensed summary before collapsible detailed Rich Markdown and
/// split oversized output into independently valid containers.
#[must_use]
pub fn compose_rich_recap_messages(
    condensed_summary: &str,
    detailed_summaries: &[String],
) -> Vec<String> {
    let condensed_summary = normalize_rich_markdown(condensed_summary);
    let detailed_summary = join_rich_markdown_summaries(detailed_summaries);

    if condensed_summary.is_empty() && detailed_summary.is_empty() {
        return Vec::new();
    }
    if detailed_summary.is_empty() {
        return pack_rich_markdown_blocks(
            &split_rich_markdown_blocks(&condensed_summary),
            RICH_MESSAGE_UTF16_UNIT_LIMIT,
            RICH_MESSAGE_UTF16_UNIT_LIMIT,
        );
    }

    let wrapper_units = utf16_units(&format!(
        "{RICH_RECAP_DETAILS_OPEN}{RICH_RECAP_DETAILS_CLOSE}"
    ));
    let detail_body_limit = RICH_MESSAGE_UTF16_UNIT_LIMIT - wrapper_units;
    let mut condensed_prefix = if condensed_summary.is_empty() {
        String::new()
    } else {
        format!("{condensed_summary}\n\n")
    };
    let prefix_units = utf16_units(&condensed_prefix);
    let mut first_detail_body_limit = detail_body_limit.saturating_sub(prefix_units);

    let mut messages = Vec::new();
    if prefix_units >= detail_body_limit {
        messages.extend(pack_rich_markdown_blocks(
            &split_rich_markdown_blocks(&condensed_summary),
            RICH_MESSAGE_UTF16_UNIT_LIMIT,
            RICH_MESSAGE_UTF16_UNIT_LIMIT,
        ));
        condensed_prefix.clear();
        first_detail_body_limit = detail_body_limit;
    }

    let detail_chunks = pack_rich_markdown_blocks(
        &split_rich_markdown_blocks(&detailed_summary),
        first_detail_body_limit,
        detail_body_limit,
    );
    for (index, detail_chunk) in detail_chunks.into_iter().enumerate() {
        let prefix = if index == 0 {
            condensed_prefix.as_str()
        } else {
            ""
        };
        messages.push(format!(
            "{prefix}{RICH_RECAP_DETAILS_OPEN}{detail_chunk}{RICH_RECAP_DETAILS_CLOSE}"
        ));
    }

    messages
}

/// Derive a 120-UTF-16-unit condensed fallback from detailed summaries.
#[must_use]
pub fn fallback_condensed_summary(summarizations: &[String], default_summary: &str) -> String {
    let joined = summarizations.join(" ").trim().to_owned();
    let protected_patterns = rich_markdown_protected_patterns();
    let (head, _) = split_fragment_at_preferred_cut(
        &joined,
        CONDENSED_SUMMARY_FALLBACK_UNIT_LIMIT,
        &protected_patterns,
    );
    let head = head.trim();
    if head.is_empty() {
        default_summary.trim().to_owned()
    } else {
        head.to_owned()
    }
}

/// Build the always-visible Rich Markdown prefix shared by every recap mode.
#[must_use]
pub fn build_rich_recap_summary(config: &RichRecapSummaryConfig<'_>) -> String {
    let mut blocks = vec![rich_recap_heading(config.title)];
    if let Some(metadata) = rich_recap_metadata(config) {
        blocks.push(metadata);
    }
    if !config.subscription_chat_title.trim().is_empty() {
        blocks.push(format!(
            "> 📬 這是您訂閱的 **{}** 群組定時聊天回顧。",
            escape_rich_markdown_text(config.subscription_chat_title)
        ));
    }

    blocks.push("## 濃縮總結".to_owned());
    let cleaned = clean_condensed_summary_for_display(config.condensed_summary);
    let condensed = sanitize_condensed_recap_markdown(&cleaned);
    if !condensed.trim().is_empty() {
        blocks.push(condensed);
    }
    if config.general_group_notice {
        blocks.push(
            "> 💡 一般群組來源暫時不顯示原訊息引用；升級為 supergroup 後即可建立連結。".to_owned(),
        );
    }
    blocks.push("---".to_owned());
    blocks.push(build_rich_recap_model_info(
        config.condensed_trace,
        config.recap_trace,
    ));

    blocks.join("\n\n")
}

/// Convert application-generated Rich Markdown to the exact Go plain-text
/// fallback representation.
#[must_use]
pub fn rich_markdown_to_plain_text(markdown: &str) -> String {
    let mut plain = markdown.replace("\r\n", "\n");
    plain = RICH_MARKDOWN_DETAILS_OPEN_PATTERN
        .replace_all(&plain, "$1\n\n")
        .into_owned();
    plain = plain.replace("</details>", "");
    plain = RICH_MARKDOWN_USER_LINK_PATTERN
        .replace_all(&plain, "$1")
        .into_owned();
    plain = RICH_MARKDOWN_URL_LINK_PATTERN
        .replace_all(&plain, "$1 ($2)")
        .into_owned();
    plain = RICH_MARKDOWN_HEADING_PATTERN
        .replace_all(&plain, "")
        .into_owned();
    plain = RICH_MARKDOWN_QUOTE_PATTERN
        .replace_all(&plain, "")
        .into_owned();
    plain = plain.replace("```", "").replace("~~~", "");
    for pattern in RICH_MARKDOWN_INLINE_SPAN_PATTERNS.iter() {
        plain = pattern.replace_all(&plain, "$1").into_owned();
    }

    unescape_rich_markdown_text(&plain).trim().to_owned()
}

/// Split plain text by Telegram's UTF-16 accounting while keeping citation
/// labels attached to their URLs.
#[must_use]
pub fn split_plain_text(text: &str, limit: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() || limit == 0 {
        return Vec::new();
    }

    let protected_patterns = [&*PLAIN_TEXT_CITATION_PATTERN];
    let mut parts = Vec::with_capacity(utf16_units(text) / limit + 1);
    let mut remaining = text.to_owned();

    while utf16_units(&remaining) > limit {
        let (head, tail) = split_fragment_at_preferred_cut(&remaining, limit, &protected_patterns);
        if head.is_empty() {
            break;
        }
        parts.push(head);
        remaining = tail;
    }
    if !remaining.is_empty() {
        parts.push(remaining);
    }

    parts
}

fn sanitize_rich_recap_markdown(markdown: &str, transform_line: impl Fn(&str) -> String) -> String {
    let mut normalized = normalize_rich_markdown(markdown);
    normalized = RICH_MARKDOWN_INLINE_LINK_PATTERN
        .replace_all(&normalized, "$1")
        .into_owned();
    normalized = RICH_MARKDOWN_HTML_TAG_PATTERN
        .replace_all(&normalized, "")
        .into_owned();
    normalized = RICH_MARKDOWN_BARE_URL_PATTERN
        .replace_all(&normalized, "")
        .into_owned();
    normalized = neutralize_rich_markdown_mentions(&normalized);

    let sanitized = normalized
        .lines()
        .filter(|line| rich_markdown_fence_marker(line.trim()).is_none())
        .map(|line| {
            transform_line(line)
                .trim_end_matches([' ', '\t'])
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");

    normalize_rich_markdown(&sanitized)
}

fn neutralize_rich_markdown_mentions(markdown: &str) -> String {
    RICH_MARKDOWN_MENTION_PATTERN
        .replace_all(markdown, |captures: &Captures<'_>| {
            format!("{}{}", &captures[1], &captures[2])
        })
        .into_owned()
}

fn normalize_rich_markdown(markdown: &str) -> String {
    markdown
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_owned()
}

fn rich_markdown_fence_marker(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn format_chat_id(chat_id: i64) -> String {
    let formatted = chat_id.to_string();
    formatted
        .strip_prefix("-100")
        .map_or(formatted.clone(), str::to_owned)
}

fn pick_model_name<'a>(candidates: impl IntoIterator<Item = &'a str>) -> String {
    candidates
        .into_iter()
        .find(|candidate| !candidate.trim().is_empty())
        .map_or_else(|| "unknown-model".to_owned(), str::to_owned)
}

fn append_unique_model_name(models: &mut Vec<String>, candidate: &str) {
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate == "unknown-model" {
        return;
    }
    if !models.iter().any(|model| model == candidate) {
        models.push(candidate.to_owned());
    }
}

fn generation_model_name(trace: &GenerationModelExecutionTrace) -> String {
    let mut used_models = Vec::with_capacity(2);
    append_unique_model_name(&mut used_models, &trace.primary_used_model);
    if trace.backup_used && trace.backup_succeeded {
        let backup = pick_model_name([
            trace.backup_used_model.as_str(),
            trace.backup_model.as_str(),
        ]);
        append_unique_model_name(&mut used_models, &backup);
    }
    if !used_models.is_empty() {
        return used_models.join("、");
    }
    if trace.primary_failed {
        return "資訊不可用".to_owned();
    }

    pick_model_name([
        trace.primary_used_model.as_str(),
        trace.primary_model.as_str(),
    ])
}

fn rich_recap_heading(title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        return "# 聊天回顧".to_owned();
    }
    if title.starts_with('【') && title.ends_with('】') {
        return format!("# {}聊天回顧", escape_rich_markdown_text(title));
    }
    format!("# 【{}】聊天回顧", escape_rich_markdown_text(title))
}

fn rich_recap_user_mention(name: &str, user_id: i64) -> String {
    let name = if name.trim().is_empty() {
        "未知用戶"
    } else {
        name.trim()
    };
    let escaped_name = escape_rich_markdown_text(name);
    if user_id <= 0 {
        escaped_name
    } else {
        format!("[{escaped_name}](tg://user?id={user_id})")
    }
}

fn rich_recap_metadata(config: &RichRecapSummaryConfig<'_>) -> Option<String> {
    if config.automatic {
        if config.hours > 0 {
            return Some(format!("_自動產生 **{} 小時**總結_", config.hours));
        }
        return Some("_自動產生_".to_owned());
    }

    if config.initiator_name.trim().is_empty() && config.initiator_user_id <= 0 {
        return None;
    }
    let initiator = rich_recap_user_mention(config.initiator_name, config.initiator_user_id);
    if config.hours > 0 {
        Some(format!(
            "_用戶 {initiator} 發起 **{} 小時**總結_",
            config.hours
        ))
    } else {
        Some(format!("_由用戶 {initiator} 發起_"))
    }
}

fn clean_condensed_summary_for_display(summary: &str) -> String {
    let summary = summary.trim();
    let cleaned = CONDENSED_SUMMARY_LABEL_PATTERN
        .replace(summary, "")
        .trim()
        .to_owned();
    if cleaned.is_empty() {
        summary.to_owned()
    } else {
        cleaned
    }
}

fn join_rich_markdown_summaries(summaries: &[String]) -> String {
    summaries
        .iter()
        .map(|summary| normalize_rich_markdown(summary))
        .filter(|summary| !summary.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn split_rich_markdown_blocks(markdown: &str) -> Vec<String> {
    let markdown = normalize_rich_markdown(markdown);
    if markdown.is_empty() {
        return Vec::new();
    }

    let mut blocks = Vec::new();
    let mut current_lines = Vec::new();
    let mut in_fence = false;
    let mut fence_marker = "";

    for line in markdown.split('\n') {
        let trimmed = line.trim();
        if let Some(marker) = rich_markdown_fence_marker(trimmed) {
            if !in_fence {
                in_fence = true;
                fence_marker = marker;
            } else if trimmed.starts_with(fence_marker) {
                in_fence = false;
                fence_marker = "";
            }
            current_lines.push(line.to_owned());
            continue;
        }

        if trimmed.is_empty() && !in_fence {
            flush_rich_markdown_block(&mut current_lines, &mut blocks);
            continue;
        }
        current_lines.push(line.to_owned());
    }
    flush_rich_markdown_block(&mut current_lines, &mut blocks);

    merge_standalone_headings(blocks)
}

fn flush_rich_markdown_block(current_lines: &mut Vec<String>, blocks: &mut Vec<String>) {
    let block = current_lines.join("\n").trim().to_owned();
    if !block.is_empty() {
        blocks.push(block);
    }
    current_lines.clear();
}

fn merge_standalone_headings(blocks: Vec<String>) -> Vec<String> {
    let mut merged = Vec::with_capacity(blocks.len());
    let mut index = 0;
    while index < blocks.len() {
        if is_standalone_markdown_heading(&blocks[index]) && index + 1 < blocks.len() {
            merged.push(format!("{}\n\n{}", blocks[index], blocks[index + 1]));
            index += 2;
        } else {
            merged.push(blocks[index].clone());
            index += 1;
        }
    }
    merged
}

fn is_standalone_markdown_heading(block: &str) -> bool {
    if block.contains('\n') {
        return false;
    }
    let trimmed = block.trim();
    (1..=6).any(|level| trimmed.starts_with(&format!("{} ", "#".repeat(level))))
}

fn pack_rich_markdown_blocks(
    blocks: &[String],
    first_limit: usize,
    later_limit: usize,
) -> Vec<String> {
    if blocks.is_empty() || first_limit == 0 || later_limit == 0 {
        return Vec::new();
    }

    let protected_patterns = rich_markdown_protected_patterns();
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_count = 0;
    let mut current_limit = first_limit;

    for original_block in blocks {
        let mut remaining_block = original_block.clone();
        let mut remaining_count = utf16_units(&remaining_block);

        while !remaining_block.is_empty() {
            let (separator, separator_count) = if current.is_empty() {
                ("", 0)
            } else {
                ("\n\n", 2)
            };

            if current_count + separator_count + remaining_count <= current_limit {
                current.push_str(separator);
                current.push_str(&remaining_block);
                current_count += separator_count + remaining_count;
                remaining_block.clear();
                continue;
            }

            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_count = 0;
                current_limit = later_limit;
                continue;
            }

            let (head, tail) = split_fragment_at_preferred_cut(
                &remaining_block,
                current_limit,
                &protected_patterns,
            );
            if head.is_empty() {
                return chunks;
            }

            chunks.push(head);
            current_limit = later_limit;
            remaining_block = tail;
            remaining_count = utf16_units(&remaining_block);
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn split_fragment_at_preferred_cut(
    fragment: &str,
    limit: usize,
    protected_patterns: &[&Regex],
) -> (String, String) {
    let runes = fragment.chars().collect::<Vec<_>>();
    let mut rune_limit = rune_count_within_utf16_units(&runes, limit);
    if rune_limit >= runes.len() {
        return (fragment.to_owned(), String::new());
    }
    if rune_limit < 1 {
        // Preserve Go's progress rule even when one scalar exceeds the budget.
        rune_limit = 1;
    }

    let mut cut = preferred_fragment_cut(&runes, rune_limit);
    cut = adjust_cut_for_protected_spans(fragment, &runes, cut, rune_limit, protected_patterns);

    let mut head = runes[..cut].iter().collect::<String>().trim().to_owned();
    let mut tail = runes[cut..].iter().collect::<String>().trim().to_owned();
    if head.is_empty() {
        head = runes[..rune_limit].iter().collect();
        tail = runes[rune_limit..]
            .iter()
            .collect::<String>()
            .trim()
            .to_owned();
    }
    (head, tail)
}

fn preferred_fragment_cut(runes: &[char], limit: usize) -> usize {
    let mut cut = limit;
    for index in (limit / 2..limit).rev() {
        if matches!(runes[index], '\n' | ' ' | '\t' | '。' | '！' | '？' | '；') {
            cut = index + 1;
            break;
        }
    }
    cut
}

fn adjust_cut_for_protected_spans(
    fragment: &str,
    runes: &[char],
    cut: usize,
    limit: usize,
    protected_patterns: &[&Regex],
) -> usize {
    let bounded_fragment;
    let fragment = if runes.len() > 2 * limit {
        bounded_fragment = runes[..2 * limit].iter().collect::<String>();
        bounded_fragment.as_str()
    } else {
        fragment
    };

    let mut protected_start = cut;
    let mut protected_end = 0;
    for pattern in protected_patterns {
        for matched in pattern.find_iter(fragment) {
            let start = fragment[..matched.start()].chars().count();
            let end = start + matched.as_str().chars().count();
            if start >= cut || cut >= end {
                continue;
            }
            if start > 0 && start < protected_start {
                protected_start = start;
            }
            if start == 0 && end <= limit && end > protected_end {
                protected_end = end;
            }
        }
    }

    let mut adjusted = protected_start;
    if adjusted == cut && protected_end != 0 {
        adjusted = protected_end;
    }
    if adjusted > 0 && adjusted < runes.len() && runes[adjusted - 1] == '\\' {
        adjusted -= 1;
    }
    if adjusted == 0 || adjusted > limit {
        cut
    } else {
        adjusted
    }
}

fn rich_markdown_protected_patterns() -> [&'static Regex; 7] {
    [
        &RICH_MARKDOWN_LINK_SPAN_PATTERN,
        &RICH_MARKDOWN_INLINE_SPAN_PATTERNS[0],
        &RICH_MARKDOWN_INLINE_SPAN_PATTERNS[1],
        &RICH_MARKDOWN_INLINE_SPAN_PATTERNS[2],
        &RICH_MARKDOWN_INLINE_SPAN_PATTERNS[3],
        &RICH_MARKDOWN_INLINE_SPAN_PATTERNS[4],
        &RICH_MARKDOWN_INLINE_SPAN_PATTERNS[5],
    ]
}

fn utf16_units(text: &str) -> usize {
    text.encode_utf16().count()
}

fn rune_count_within_utf16_units(runes: &[char], limit: usize) -> usize {
    let mut used = 0;
    for (index, rune) in runes.iter().enumerate() {
        let units = rune.len_utf16();
        if used + units > limit {
            return index;
        }
        used += units;
    }
    runes.len()
}
