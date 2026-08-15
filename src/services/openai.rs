use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent,
        CreateChatCompletionRequestArgs,
    },
};
use async_trait::async_trait;

use crate::{
    config::{CondensedPromptConfig, Locale, OpenAiConfig as OpenAiSettings, RecapOpenAiConfig},
    db::models::{ChatHistory, TokenUsage},
    i18n::I18n,
    services::{
        prompts::{
            ANY_SUMMARIZATION_SYSTEM_PROMPT, CHAT_HISTORY_SUMMARIZATION_SYSTEM_PROMPT,
            CHECK_CONDENSED_OUTPUT_SYSTEM_PROMPT, CHECK_CONDENSED_OUTPUT_USER_PROMPT,
            CHECK_SUMMARY_JSON_SYSTEM_PROMPT, CHECK_SUMMARY_JSON_USER_PROMPT,
            ONE_CHAT_HISTORY_SUMMARIZATION_SYSTEM_PROMPT, PromptConfig, StructuredSummary,
            TopicSummary, render_any_summarization_user_prompt,
            render_one_chat_history_user_prompt, render_structured_summary_user_prompt,
        },
        rate_limit::GoRateLimiter,
    },
};

/// Go builds the OpenAI limiter as `ratelimit.New(1)` in `openai.NewClient`.
pub const GO_OPENAI_REQUESTS_PER_SECOND: u32 = 1;

/// Go's `SetPromptOperation("Summarize Any")`.
pub const SUMMARIZE_ANY_OPERATION: &str = "Summarize Any";

/// Go's `SetPromptOperation("Summarize One Chat History")`.
pub const SUMMARIZE_ONE_CHAT_HISTORY_OPERATION: &str = "Summarize One Chat History";

/// The seam for Go's `enableMetricRecordForTokens` side effect.
///
/// Go writes a `metric_open_ai_chat_completion_token_usages` row through
/// `c.ent` after a successful completion, and only logs a write failure. That
/// side effect needs a datastore handle, and this client has none: `OpenAiClient`
/// holds an `async_openai::Client` and prompt configuration, never a
/// [`crate::db::Database`]. Rather than invent a DB dependency inside this
/// slice, the usage is reported here and a caller that owns a pool can forward
/// it to [`crate::db::usage_metrics::create`], which is already ported and
/// takes exactly this shape.
///
/// No recorder installed is Go's `enableMetricRecordForTokens == false`, where
/// the whole block is skipped.
#[async_trait]
pub trait TokenUsageRecorder: Send + Sync {
    /// `model_name` is Go's `c.modelName`, the model that was *requested*, not
    /// the `model` the response echoes back.
    async fn record(
        &self,
        prompt_operation: &str,
        usage: TokenUsage,
        model_name: &str,
    ) -> Result<()>;
}

#[derive(Clone)]
pub struct OpenAiClient {
    client: Client<OpenAIConfig>,
    pub model: String,
    pub sarcastic_model: Option<String>,
    pub check_model: Option<String>,
    pub check_model_backup: Option<String>,
    token_limit: Option<u32>,
    pub prompt_config: PromptConfig,
    /// Go's `c.limiter`, shared by every clone as it is shared by every
    /// goroutine holding one `*OpenAIClient`.
    limiter: Arc<GoRateLimiter>,
    token_usage_recorder: Option<Arc<dyn TokenUsageRecorder>>,
}

impl OpenAiClient {
    pub fn new(
        cfg: &OpenAiSettings,
        recap: &RecapOpenAiConfig,
        condensed: &CondensedPromptConfig,
    ) -> Result<Self> {
        let mut builder = OpenAIConfig::new().with_api_key(&cfg.api_key);
        if let Some(base) = &cfg.api_base {
            builder = builder.with_api_base(base);
        }
        // `go-openai` returns the first API or transport error. async-openai's
        // default retries 429 and every 5xx for up to fifteen minutes, which
        // would change both request count and latency. A zero retry window
        // preserves the first attempt while preventing a second one.
        let retry_policy = backoff::ExponentialBackoff {
            max_elapsed_time: Some(Duration::ZERO),
            ..Default::default()
        };
        let client = Client::with_config(builder).with_backoff(retry_policy);
        let prompt_config = PromptConfig::from_config(recap, condensed);
        let sarcastic_model = Some(recap.condensed_model.clone());
        let check_model = recap.check_model.clone();
        let check_model_backup = recap.check_backups.first().cloned();

        // Go's `NewClient` builds the limiter and immediately takes from it,
        // which moves the first permission issue one second into the future.
        let limiter = Arc::new(GoRateLimiter::per_second(GO_OPENAI_REQUESTS_PER_SECOND));
        limiter.prime();

        Ok(Self {
            client,
            model: recap.primary_model.clone(),
            sarcastic_model,
            check_model,
            check_model_backup,
            token_limit: u32::try_from(recap.token_limit).ok(),
            prompt_config,
            limiter,
            token_usage_recorder: None,
        })
    }

    /// Replace the limiter, the way Go's tests hand in `ratelimit.New(1000)`
    /// so a completion is not throttled to one per second.
    #[must_use]
    pub fn with_rate_limiter(mut self, limiter: Arc<GoRateLimiter>) -> Self {
        self.limiter = limiter;
        self
    }

    /// Install the [`TokenUsageRecorder`], which is Go's
    /// `enableMetricRecordForTokens = true`.
    #[must_use]
    pub fn with_token_usage_recorder(mut self, recorder: Arc<dyn TokenUsageRecorder>) -> Self {
        self.token_usage_recorder = Some(recorder);
        self
    }

    pub fn rate_limiter(&self) -> &Arc<GoRateLimiter> {
        &self.limiter
    }

    /// Sarcastic condensed single-sentence summary with emoji.
    pub async fn sarcastic_condense(&self, content: &str) -> Result<String> {
        let model = self.sarcastic_model.as_ref().unwrap_or(&self.model).clone();

        let user_prompt = self.prompt_config.render_sarcastic_user_prompt(content)?;

        let req = CreateChatCompletionRequestArgs::default()
            .model(&model)
            .messages(vec![
                ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                    content: self.prompt_config.sarcastic_system_prompt.clone().into(),
                    name: None,
                }),
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(user_prompt),
                    name: None,
                }),
            ])
            .max_tokens(200u32) // Short response expected.
            .build()?;

        let resp = self
            .client
            .chat()
            .create(req)
            .await
            .context("sarcastic condense failed")?;

        let text = resp
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_else(|| {
                tracing::warn!("sarcastic_condense: no content in API response");
                "Summary unavailable.".to_string()
            });

        Ok(text.trim().to_string())
    }

    /// Structured JSON summarization with locale-aware output language.
    /// Returns (parsed topics, raw JSON text) — raw text is kept for check model repair.
    pub async fn recap_structured_locale(
        &self,
        content: &str,
        locale: &Locale,
    ) -> Result<(StructuredSummary, String)> {
        let language = match locale {
            Locale::ZhHans => "Simplified Chinese",
            Locale::ZhHant => "Traditional Chinese",
            Locale::En => "English",
        };

        let user_prompt = render_structured_summary_user_prompt(language, content)?;

        let req = CreateChatCompletionRequestArgs::default()
            .model(&self.model)
            .messages(vec![
                ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                    content: CHAT_HISTORY_SUMMARIZATION_SYSTEM_PROMPT.into(),
                    name: None,
                }),
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(user_prompt),
                    name: None,
                }),
            ])
            .max_tokens(self.token_limit.unwrap_or(8000))
            .build()?;

        let resp = self
            .client
            .chat()
            .create(req)
            .await
            .context("structured summarization failed")?;

        let raw_text = resp
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_else(|| "[]".to_string());

        // Try to extract JSON from response (may be wrapped in markdown code block)
        let json_text = extract_json_from_response(&raw_text);

        // Try to parse JSON, return raw text alongside for potential check model repair.
        let summary: StructuredSummary = serde_json::from_str(&json_text).unwrap_or_else(|_| {
            tracing::warn!("failed to parse structured summary JSON");
            Vec::new()
        });

        Ok((summary, json_text))
    }

    /// Send a single chat completion request to the specified model for repair.
    async fn call_check_model(
        &self,
        model_name: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String> {
        let req = CreateChatCompletionRequestArgs::default()
            .model(model_name)
            .messages(vec![
                ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                    content: system_prompt.into(),
                    name: None,
                }),
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(user_prompt.to_string()),
                    name: None,
                }),
            ])
            .max_tokens(2000u32)
            .build()?;

        let resp = self
            .client
            .chat()
            .create(req)
            .await
            .context("check model call failed")?;

        resp.choices
            .first()
            .and_then(|c| c.message.content.clone())
            .map(|t| t.trim().to_string())
            .ok_or_else(|| anyhow::anyhow!("check model returned no content"))
    }

    /// Attempt to repair malformed segmented JSON using check model (+ backup).
    async fn repair_segmented_json(
        &self,
        raw_json: &str,
        trace: &mut CheckModelTrace,
    ) -> Option<StructuredSummary> {
        let check_model = self.check_model.as_ref()?;
        trace.model = check_model.clone();
        if let Some(backup) = &self.check_model_backup {
            trace.backup_model = backup.clone();
        }
        trace.attempted = true;

        let user_prompt = CHECK_SUMMARY_JSON_USER_PROMPT.replace("{{raw_json}}", raw_json);

        // Try primary check model
        if let Ok(repaired) = self
            .call_check_model(check_model, CHECK_SUMMARY_JSON_SYSTEM_PROMPT, &user_prompt)
            .await
        {
            let cleaned = extract_json_from_response(&repaired);
            if let Ok(summary) = serde_json::from_str::<StructuredSummary>(&cleaned) {
                tracing::info!("check model repaired segmented JSON (primary)");
                trace.succeeded = true;
                return Some(summary);
            }
        }

        // Try backup if available
        if let Some(backup) = &self.check_model_backup {
            trace.backup_used = true;
            if let Ok(repaired) = self
                .call_check_model(backup, CHECK_SUMMARY_JSON_SYSTEM_PROMPT, &user_prompt)
                .await
            {
                let cleaned = extract_json_from_response(&repaired);
                if let Ok(summary) = serde_json::from_str::<StructuredSummary>(&cleaned) {
                    tracing::info!("check model repaired segmented JSON (backup: {backup})");
                    trace.succeeded = true;
                    trace.backup_succeeded = true;
                    return Some(summary);
                }
            }
        }

        tracing::warn!("check model failed to repair segmented JSON");
        trace.failed = true;
        None
    }

    /// Attempt to repair malformed condensed output using check model (+ backup).
    async fn repair_condensed_output(
        &self,
        raw_output: &str,
        trace: &mut CheckModelTrace,
    ) -> Option<String> {
        let check_model = self.check_model.as_ref()?;
        trace.model = check_model.clone();
        if let Some(backup) = &self.check_model_backup {
            trace.backup_model = backup.clone();
        }
        trace.attempted = true;

        let user_prompt = CHECK_CONDENSED_OUTPUT_USER_PROMPT.replace("{{raw_output}}", raw_output);

        // Try primary check model
        if let Ok(repaired) = self
            .call_check_model(
                check_model,
                CHECK_CONDENSED_OUTPUT_SYSTEM_PROMPT,
                &user_prompt,
            )
            .await
            && !needs_condensed_repair(&repaired)
        {
            tracing::info!("check model repaired condensed output (primary)");
            trace.succeeded = true;
            return Some(repaired);
        }

        // Try backup if available
        if let Some(backup) = &self.check_model_backup {
            trace.backup_used = true;
            if let Ok(repaired) = self
                .call_check_model(backup, CHECK_CONDENSED_OUTPUT_SYSTEM_PROMPT, &user_prompt)
                .await
                && !needs_condensed_repair(&repaired)
            {
                tracing::info!("check model repaired condensed output (backup: {backup})");
                trace.succeeded = true;
                trace.backup_succeeded = true;
                return Some(repaired);
            }
        }

        tracing::warn!("check model failed to repair condensed output");
        trace.failed = true;
        None
    }

    /// Generate both condensed and segmented summaries for chat history.
    pub async fn generate_dual_recap(
        &self,
        history: &[ChatHistory],
        locale: &Locale,
        chat_id: i64,
        i18n: &I18n,
    ) -> Result<RecapOutput> {
        let formatted = format_messages(history);
        if formatted.is_empty() {
            anyhow::bail!("no messages to summarize");
        }

        let condensed_model_name = self
            .sarcastic_model
            .clone()
            .unwrap_or_else(|| self.model.clone());

        // Initialize trace
        let mut trace = RecapTrace {
            condensed_model: condensed_model_name,
            segmented_model: self.model.clone(),
            check: CheckModelTrace {
                model: self.check_model.clone().unwrap_or_default(),
                backup_model: self.check_model_backup.clone().unwrap_or_default(),
                ..Default::default()
            },
        };

        // Generate both summaries concurrently
        let (condensed_result, segmented_result) = tokio::join!(
            self.sarcastic_condense(&formatted),
            self.recap_structured_locale(&formatted, locale)
        );

        // Process condensed result, optionally repair with check model
        let mut condensed_summary = match condensed_result {
            Ok(text) => {
                if text.trim().is_empty() {
                    tracing::warn!("sarcastic_condense returned empty text");
                    "Summary generation failed".to_string()
                } else {
                    text
                }
            }
            Err(_) => {
                tracing::warn!("sarcastic_condense failed");
                "Summary generation failed".to_string()
            }
        };

        // Check model repair for condensed output
        if needs_condensed_repair(&condensed_summary) && self.check_model.is_some() {
            tracing::info!("condensed output needs repair, invoking check model");
            if let Some(repaired) = self
                .repair_condensed_output(&condensed_summary, &mut trace.check)
                .await
            {
                condensed_summary = repaired;
            }
        }

        // Process segmented result, optionally repair with check model
        let (segmented_summary, segmented_summary_html) = match segmented_result {
            Ok((topics, raw_json)) => {
                if topics.is_empty() && !raw_json.is_empty() && self.check_model.is_some() {
                    // JSON parsing failed, try check model repair
                    tracing::info!("segmented JSON parsing failed, invoking check model");
                    if let Some(repaired_topics) = self
                        .repair_segmented_json(&raw_json, &mut trace.check)
                        .await
                    {
                        (
                            format_topics_to_markdown(&repaired_topics, locale, chat_id, i18n),
                            format_topics_to_telegram_html(&repaired_topics, locale, chat_id, i18n),
                        )
                    } else {
                        let fallback = "No discussion topics identified.".to_string();
                        (fallback.clone(), fallback)
                    }
                } else if topics.is_empty() {
                    let fallback = "No discussion topics identified.".to_string();
                    (fallback.clone(), fallback)
                } else {
                    (
                        format_topics_to_markdown(&topics, locale, chat_id, i18n),
                        format_topics_to_telegram_html(&topics, locale, chat_id, i18n),
                    )
                }
            }
            Err(_) => {
                tracing::warn!("recap_structured_locale failed");
                let fallback = "Segmented summary generation failed".to_string();
                (fallback.clone(), fallback)
            }
        };

        Ok(RecapOutput {
            condensed_summary,
            segmented_summary,
            segmented_summary_html,
            trace,
            created_at: chrono::Utc::now().timestamp(),
        })
    }

    /// Go's `openai.Client.SummarizeAny`.
    ///
    /// Returns every choice's content in order, so an empty vector carries Go's
    /// `len(resp.Choices) == 0` to the caller instead of hiding it.
    pub async fn summarize_any(&self, content: &str) -> Result<Vec<String>> {
        self.summarize_with(
            ANY_SUMMARIZATION_SYSTEM_PROMPT,
            render_any_summarization_user_prompt(content),
            SUMMARIZE_ANY_OPERATION,
            "summarize any failed",
        )
        .await
    }

    /// Go's `openai.Client.SummarizeOneChatHistory`.
    pub async fn summarize_one_chat_history(&self, chat_history: &str) -> Result<Vec<String>> {
        self.summarize_with(
            ONE_CHAT_HISTORY_SUMMARIZATION_SYSTEM_PROMPT,
            render_one_chat_history_user_prompt(chat_history),
            SUMMARIZE_ONE_CHAT_HISTORY_OPERATION,
            "summarize one chat history failed",
        )
        .await
    }

    /// The shared shape of Go's two preprocessing completions: primary model,
    /// one system message, one user message, and no token ceiling.
    ///
    /// Go's ordering is load-bearing and reproduced exactly:
    ///
    /// 1. `c.limiter.Take()` runs first, once, before the prompt is even
    ///    rendered, so a call that goes on to fail has still consumed its
    ///    permission.
    /// 2. `c.modelName` is the model. Neither helper consults the backup model
    ///    list that `SummarizeChatHistories` falls back through.
    /// 3. A transport or API error returns immediately and records no metric.
    /// 4. Only a successful response reaches the metric block, and a metric
    ///    write failure never fails the call.
    async fn summarize_with(
        &self,
        system_prompt: &str,
        user_prompt: String,
        prompt_operation: &str,
        failure_context: &'static str,
    ) -> Result<Vec<String>> {
        self.limiter.take().await;

        let req = CreateChatCompletionRequestArgs::default()
            .model(&self.model)
            .messages(vec![
                ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                    content: system_prompt.into(),
                    name: None,
                }),
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(user_prompt),
                    name: None,
                }),
            ])
            .build()?;

        let resp = self
            .client
            .chat()
            .create(req)
            .await
            .context(failure_context)?;

        if let Some(recorder) = &self.token_usage_recorder {
            // Go reads `resp.Usage` unconditionally; an absent usage object is
            // Go's zero-valued struct.
            let usage = resp.usage.as_ref();
            let record_result = recorder
                .record(
                    prompt_operation,
                    TokenUsage {
                        prompt_tokens: usage
                            .map(|usage| i64::from(usage.prompt_tokens))
                            .unwrap_or(0),
                        completion_tokens: usage
                            .map(|usage| i64::from(usage.completion_tokens))
                            .unwrap_or(0),
                        total_tokens: usage
                            .map(|usage| i64::from(usage.total_tokens))
                            .unwrap_or(0),
                    },
                    &self.model,
                )
                .await;
            if record_result.is_err() {
                // Go logs metric persistence failures and still returns the
                // successful completion. Keep the log generic so neither
                // response details nor configured endpoint data are emitted.
                tracing::error!("failed to record OpenAI token usage");
            }
        }

        Ok(resp
            .choices
            .iter()
            .map(|choice| choice.message.content.clone().unwrap_or_default())
            .collect())
    }
}

/// Check if condensed output is malformed and needs check model repair.
/// Ported from Go `invalidCondensedOutputReason()`.
fn needs_condensed_repair(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.contains("```") {
        return true;
    }
    // JSON-like detection
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return true;
    }
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        return true;
    }
    false
}

/// Tracks whether the check model was invoked and its outcome.
#[derive(Debug, Clone, Default)]
pub struct CheckModelTrace {
    pub model: String,
    pub backup_model: String,
    pub attempted: bool,
    pub succeeded: bool,
    pub failed: bool,
    pub backup_used: bool,
    pub backup_succeeded: bool,
}

/// Full execution trace for a recap generation, paralleling Go's structure.
#[derive(Debug, Clone, Default)]
pub struct RecapTrace {
    pub condensed_model: String,
    pub segmented_model: String,
    pub check: CheckModelTrace,
}

impl RecapTrace {
    /// Build the three-line model status footer, joined by newline.
    pub fn build_status_lines(&self, locale: &Locale, i18n: &I18n) -> String {
        let condensed_line = i18n.t(
            *locale,
            "footer.condensed",
            &[("model", &self.condensed_model)],
        );
        let segmented_line = i18n.t(
            *locale,
            "footer.segmented",
            &[("model", &self.segmented_model)],
        );
        let check_line = self.format_check_line(locale, i18n);
        format!("{}\n{}\n{}", condensed_line, segmented_line, check_line)
    }

    fn format_check_line(&self, locale: &Locale, i18n: &I18n) -> String {
        let check = &self.check;
        if check.model.is_empty() {
            return i18n.t(*locale, "footer.check_not_configured", &[]);
        }
        if check.attempted && check.succeeded && check.backup_used {
            return i18n.t(
                *locale,
                "footer.check_backup_success",
                &[
                    ("model", &check.model),
                    ("backup_model", &check.backup_model),
                ],
            );
        }
        if check.attempted && check.failed && check.backup_used {
            return i18n.t(
                *locale,
                "footer.check_backup_failed",
                &[
                    ("model", &check.model),
                    ("backup_model", &check.backup_model),
                ],
            );
        }
        if check.attempted && check.failed {
            return i18n.t(*locale, "footer.check_failed", &[("model", &check.model)]);
        }
        if check.attempted && check.succeeded {
            return i18n.t(*locale, "footer.check_success", &[("model", &check.model)]);
        }
        // Not attempted = not triggered
        i18n.t(
            *locale,
            "footer.check_not_triggered",
            &[("model", &check.model)],
        )
    }
}

/// Full recap output with condensed and segmented summaries.
#[derive(Debug, Clone)]
pub struct RecapOutput {
    /// Condensed single-sentence summary with emoji.
    pub condensed_summary: String,
    /// Full segmented summary in Markdown+HTML (for Telegraph nodes).
    pub segmented_summary: String,
    /// Segmented summary in pure Telegram HTML (for inline messages).
    pub segmented_summary_html: String,
    /// Execution trace with model names and check model status.
    pub trace: RecapTrace,
    pub created_at: i64,
}

/// Format user name for display: prefer full_name, fallback to username if full_name is too long.
fn format_user_name(full_name: &str, username: &str) -> String {
    // If full_name is >= 10 chars and username exists, use username
    if full_name.chars().count() >= 10 && !username.is_empty() {
        return username.to_string();
    }
    // Remove # characters from full_name
    if !full_name.is_empty() {
        return full_name.replace('#', "");
    }
    if !username.is_empty() {
        return username.to_string();
    }
    "unknown".to_string()
}

/// Format chat history messages for LLM input.
/// Uses format: `msgId:{id}: {name} sent: {text}`
fn format_messages(history: &[ChatHistory]) -> String {
    let mut lines = Vec::new();
    for h in history.iter() {
        // Skip empty text messages
        if h.text.is_empty() {
            continue;
        }
        let sender = format_user_name(&h.from_full_name, &h.from_username);
        // Format: msgId:{id}: {name} sent: {text}
        lines.push(format!(
            "msgId:{}: {} sent: {}",
            h.message_id, sender, h.text
        ));
    }
    lines.join("\n")
}

/// Extract JSON from response that may be wrapped in markdown code block.
fn extract_json_from_response(text: &str) -> String {
    let trimmed = text.trim();

    // Try to extract from markdown code block
    if trimmed.starts_with("```") {
        // Find the end of the first line (language specifier)
        if let Some(first_newline) = trimmed.find('\n') {
            let after_lang = &trimmed[first_newline + 1..];
            // Find closing ```
            if let Some(end_pos) = after_lang.rfind("```") {
                return after_lang[..end_pos].trim().to_string();
            }
        }
    }

    // Return as-is if not wrapped
    trimmed.to_string()
}

/// Escape HTML special characters for Telegram HTML parse mode.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Format structured topics into Telegram-compatible HTML (for inline messages).
/// Uses `<b>` for headers, preserves `<a>` links, and escapes AI-generated text.
pub fn format_topics_to_telegram_html(
    topics: &[TopicSummary],
    locale: &Locale,
    chat_id: i64,
    i18n: &I18n,
) -> String {
    if topics.is_empty() {
        return "No discussion topics identified.".to_string();
    }

    let participants_label = i18n.t(*locale, "labels.participants", &[]);
    let discussion_label = i18n.t(*locale, "labels.discussion", &[]);
    let conclusion_label = i18n.t(*locale, "labels.conclusion", &[]);
    let colon = i18n.t(*locale, "labels.colon", &[]);
    let comma = i18n.t(*locale, "labels.comma", &[]);

    let chat_cid = if chat_id < 0 {
        (chat_id.abs() - 1_000_000_000_000).to_string()
    } else {
        chat_id.to_string()
    };

    let mut output = Vec::new();

    for topic in topics {
        // Topic title: <b> with optional <a> link
        if topic.since_id > 0 {
            output.push(format!(
                "<b><a href=\"https://t.me/c/{}/{}\">{}</a></b>",
                chat_cid,
                topic.since_id,
                escape_html(&topic.topic_name)
            ));
        } else {
            output.push(format!("<b>{}</b>", escape_html(&topic.topic_name)));
        }

        // Participants
        let participants_str = topic
            .participants
            .iter()
            .map(|p| escape_html(p))
            .collect::<Vec<_>>()
            .join(&comma);
        output.push(format!(
            "{}{}{}",
            participants_label, colon, participants_str
        ));

        // Discussion
        output.push(format!("{}{}", discussion_label, colon));

        for point in &topic.discussion {
            let links: Vec<String> = point
                .key_ids
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    format!(
                        "<a href=\"https://t.me/c/{}/{}\">[{}]</a>",
                        chat_cid,
                        id,
                        i + 1
                    )
                })
                .collect();

            let links_str = if links.is_empty() {
                String::new()
            } else {
                format!(" {}", links.join(" "))
            };

            output.push(format!(" • {}{}", escape_html(&point.point), links_str));
        }

        // Conclusion (optional)
        if let Some(conclusion) = &topic.conclusion
            && !conclusion.is_empty()
        {
            output.push(format!(
                "{}{}{}",
                conclusion_label,
                colon,
                escape_html(conclusion)
            ));
        }

        output.push(String::new());
    }

    output.join("\n")
}

/// Format structured topics into Markdown text with locale-aware labels.
pub fn format_topics_to_markdown(
    topics: &[TopicSummary],
    locale: &Locale,
    chat_id: i64,
    i18n: &I18n,
) -> String {
    if topics.is_empty() {
        return "No discussion topics identified.".to_string();
    }

    // Locale-specific labels and punctuation from i18n
    let participants_label = i18n.t(*locale, "labels.participants", &[]);
    let discussion_label = i18n.t(*locale, "labels.discussion", &[]);
    let conclusion_label = i18n.t(*locale, "labels.conclusion", &[]);
    let colon = i18n.t(*locale, "labels.colon", &[]);
    let comma = i18n.t(*locale, "labels.comma", &[]);

    // Convert chat_id to t.me/c/ format (remove -100 prefix for supergroups)
    let chat_cid = if chat_id < 0 {
        (chat_id.abs() - 1_000_000_000_000).to_string()
    } else {
        chat_id.to_string()
    };

    let mut output = Vec::new();

    for topic in topics {
        // Topic title with optional link to since_id
        if topic.since_id > 0 {
            output.push(format!(
                "## <a href=\"https://t.me/c/{}/{}\">{}</a>",
                chat_cid, topic.since_id, topic.topic_name
            ));
        } else {
            output.push(format!("## {}", topic.topic_name));
        }

        // Participants
        let participants_str = topic.participants.join(&comma);
        output.push(format!(
            "{}{}{}",
            participants_label, colon, participants_str
        ));

        // Discussion
        output.push(format!("{}{}", discussion_label, colon));

        for point in &topic.discussion {
            // Format key_ids as links
            let links: Vec<String> = point
                .key_ids
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    format!(
                        "<a href=\"https://t.me/c/{}/{}\">[{}]</a>",
                        chat_cid,
                        id,
                        i + 1
                    )
                })
                .collect();

            let links_str = if links.is_empty() {
                String::new()
            } else {
                format!(" {}", links.join(" "))
            };

            output.push(format!(" - {}{}", point.point, links_str));
        }

        // Conclusion (optional)
        if let Some(conclusion) = &topic.conclusion
            && !conclusion.is_empty()
        {
            output.push(format!("{}{}{}", conclusion_label, colon, conclusion));
        }

        output.push(String::new()); // Empty line between topics
    }

    output.join("\n")
}
