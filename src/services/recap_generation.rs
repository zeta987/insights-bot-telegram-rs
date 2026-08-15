use std::{collections::HashMap, fmt, time::Duration};

use async_trait::async_trait;

use crate::{
    config::RecapOpenAiConfig,
    db::{
        Database,
        models::{TelegramChatHistory, TokenUsage},
        recap_logs, usage_metrics,
    },
    services::{
        message_capture::PrivateForwardedReplayChatHistory,
        openai::{CondensedGenerationError, CondensedResult, OpenAiClient, TokenUsageRecorder},
        rich_recap::{
            CondensedExecutionTrace, GenerationModelExecutionTrace, RecapExecutionTrace,
            resolve_rich_recap_references, sanitize_detailed_recap_markdown,
        },
    },
};

const GO_DETAILED_SLICE_ATTEMPTS: usize = 5;
const GO_DETAILED_SLICE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Persist OpenAI completion usage using Go's production metric shape.
#[derive(Clone)]
pub struct DatabaseTokenUsageRecorder {
    database: Database,
}

impl DatabaseTokenUsageRecorder {
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self { database }
    }
}

#[async_trait]
impl TokenUsageRecorder for DatabaseTokenUsageRecorder {
    async fn record(
        &self,
        prompt_operation: &str,
        usage: TokenUsage,
        model_name: &str,
    ) -> anyhow::Result<()> {
        usage_metrics::create(&self.database, prompt_operation, usage, model_name).await
    }
}

/// Detailed-generation failure with the trace Go returns beside the error.
#[derive(Debug)]
pub struct DetailedRecapGenerationError {
    pub source: anyhow::Error,
    pub usage: TokenUsage,
    pub trace: RecapExecutionTrace,
}

impl fmt::Display for DetailedRecapGenerationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for DetailedRecapGenerationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Go-compatible detailed recap orchestration over token slices and storage.
pub struct RecapGenerationService {
    database: Database,
    openai: OpenAiClient,
    token_budget: usize,
    summary_language: String,
    configured_model: String,
    retry_delay: Duration,
}

impl RecapGenerationService {
    pub fn new(
        database: Database,
        openai: OpenAiClient,
        config: &RecapOpenAiConfig,
    ) -> anyhow::Result<Self> {
        let token_budget = config
            .token_limit
            .checked_sub(config.recap_reserve)
            .filter(|budget| *budget > 0)
            .ok_or_else(|| anyhow::anyhow!("recap token budget must be greater than zero"))?;
        let token_budget = usize::try_from(token_budget)
            .map_err(|_| anyhow::anyhow!("recap token budget is too large"))?;

        Ok(Self {
            database,
            openai,
            token_budget,
            summary_language: config.summary_language.clone(),
            configured_model: config.primary_model.clone(),
            retry_delay: GO_DETAILED_SLICE_RETRY_DELAY,
        })
    }

    /// Override only the outer Go slice-retry delay, primarily for tests.
    #[must_use]
    pub fn with_retry_delay(mut self, retry_delay: Duration) -> Self {
        self.retry_delay = retry_delay;
        self
    }

    pub async fn generate_condensed(
        &self,
        chat_id: i64,
        histories: &[TelegramChatHistory],
    ) -> Result<CondensedResult, CondensedGenerationError> {
        if histories.is_empty() {
            return Err(CondensedGenerationError {
                source: anyhow::anyhow!("no chat histories"),
                trace: CondensedExecutionTrace::default(),
            });
        }

        self.generate_condensed_from_text(chat_id, &build_condensed_history(histories))
            .await
    }

    pub async fn generate_condensed_from_text(
        &self,
        _chat_id: i64,
        chat_history: &str,
    ) -> Result<CondensedResult, CondensedGenerationError> {
        if chat_history.trim().is_empty() {
            return Err(CondensedGenerationError {
                source: anyhow::anyhow!("no chat history text"),
                trace: CondensedExecutionTrace::default(),
            });
        }

        self.openai.sarcastic_condense_traced(chat_history).await
    }

    pub async fn summarize_group_histories(
        &self,
        chat_id: i64,
        chat_type: &str,
        histories: &[TelegramChatHistory],
    ) -> Result<GroupDetailedRecap, DetailedRecapGenerationError> {
        let (recap_inputs, virtual_to_real) = build_rich_recap_prompt(histories);
        let raw = self.summarize_detailed_inputs(&recap_inputs).await?;
        let resolved_summaries = raw
            .summaries
            .into_iter()
            .filter_map(|summary| {
                let sanitized = sanitize_detailed_recap_markdown(&summary);
                let resolved =
                    resolve_rich_recap_references(&sanitized, chat_id, chat_type, &virtual_to_real);
                let resolved = resolved.trim().to_owned();
                (!resolved.is_empty()).then_some(resolved)
            })
            .collect::<Vec<_>>();

        persist_group_detailed_recap(
            &self.database,
            chat_id,
            &recap_inputs,
            resolved_summaries,
            raw.usage,
            raw.trace.clone(),
            &self.configured_model,
        )
        .await
        .map_err(|source| DetailedRecapGenerationError {
            source,
            usage: raw.usage,
            trace: raw.trace,
        })
    }

    /// Generate and persist Go's private-forwarded detailed recap.
    pub async fn summarize_private_forwarded_histories(
        &self,
        user_id: i64,
        histories: &[PrivateForwardedReplayChatHistory],
    ) -> Result<PrivateForwardedDetailedRecap, DetailedRecapGenerationError> {
        let recap_inputs = build_private_forwarded_recap_prompt(histories);
        let raw = self.summarize_detailed_inputs(&recap_inputs).await?;
        let empty_mapping = HashMap::new();
        let summaries = raw
            .summaries
            .into_iter()
            .filter_map(|summary| {
                let sanitized = sanitize_detailed_recap_markdown(&summary);
                let resolved =
                    resolve_rich_recap_references(&sanitized, 0, "private", &empty_mapping);
                let resolved = resolved.trim().to_owned();
                (!resolved.is_empty()).then_some(resolved)
            })
            .collect::<Vec<_>>();
        let recap_outputs = summaries.join("\n");

        recap_logs::create_private_forwarded_recap(
            &self.database,
            user_id,
            &recap_inputs,
            &recap_outputs,
            raw.usage,
        )
        .await
        .map_err(|source| DetailedRecapGenerationError {
            source,
            usage: raw.usage,
            trace: raw.trace.clone(),
        })?;

        Ok(PrivateForwardedDetailedRecap {
            recap_inputs,
            summaries,
            usage: raw.usage,
            trace: raw.trace,
        })
    }

    async fn summarize_detailed_inputs(
        &self,
        recap_inputs: &str,
    ) -> Result<RawDetailedRecap, DetailedRecapGenerationError> {
        let slices = self
            .openai
            .split_content_by_token_limit(recap_inputs, self.token_budget)
            .map_err(|source| DetailedRecapGenerationError {
                source,
                usage: zero_usage(),
                trace: RecapExecutionTrace::default(),
            })?;
        let mut raw_summaries = Vec::with_capacity(slices.len());
        let mut total_usage = zero_usage();
        let mut aggregated_trace = RecapExecutionTrace::default();

        for content in slices {
            let mut successful_summary = None;
            let mut successful_trace = None;
            let mut last_trace = RecapExecutionTrace::default();
            let mut last_error = anyhow::anyhow!("recap model returned empty Rich Markdown");

            for attempt in 0..GO_DETAILED_SLICE_ATTEMPTS {
                if content.trim().is_empty() {
                    last_trace = RecapExecutionTrace::default();
                    last_error = anyhow::anyhow!("recap model returned empty Rich Markdown");
                } else {
                    match self
                        .openai
                        .summarize_chat_histories_raw(&content, &self.summary_language)
                        .await
                    {
                        Ok(generated) => {
                            add_usage(&mut total_usage, generated.usage);
                            last_trace = generated.trace.clone();
                            let summary = generated.content.trim().to_owned();
                            if summary.is_empty() {
                                last_error =
                                    anyhow::anyhow!("recap model returned empty Rich Markdown");
                            } else {
                                successful_summary = Some(summary);
                                successful_trace = Some(generated.trace);
                                break;
                            }
                        }
                        Err(error) => {
                            last_trace = error.trace;
                            last_error = error.source;
                        }
                    }
                }

                if attempt + 1 < GO_DETAILED_SLICE_ATTEMPTS {
                    tokio::time::sleep(self.retry_delay).await;
                }
            }

            match (successful_summary, successful_trace) {
                (Some(summary), Some(trace)) => {
                    merge_recap_execution_trace(&mut aggregated_trace, &trace);
                    raw_summaries.push(summary);
                }
                _ => {
                    merge_recap_execution_trace(&mut aggregated_trace, &last_trace);
                    return Err(DetailedRecapGenerationError {
                        source: last_error,
                        usage: total_usage,
                        trace: aggregated_trace,
                    });
                }
            }
        }

        Ok(RawDetailedRecap {
            summaries: raw_summaries,
            usage: total_usage,
            trace: aggregated_trace,
        })
    }
}

struct RawDetailedRecap {
    summaries: Vec<String>,
    usage: TokenUsage,
    trace: RecapExecutionTrace,
}

/// The detailed group recap after Go's mandatory recap-log write succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDetailedRecap {
    pub log_id: String,
    pub recap_inputs: String,
    pub summaries: Vec<String>,
    pub usage: TokenUsage,
    pub trace: RecapExecutionTrace,
}

/// The private-forwarded detailed recap after its mandatory log write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateForwardedDetailedRecap {
    pub recap_inputs: String,
    pub summaries: Vec<String>,
    pub usage: TokenUsage,
    pub trace: RecapExecutionTrace,
}

/// Build Go's detailed Rich recap input and its request-local virtual-ID map.
#[must_use]
pub fn build_rich_recap_prompt(histories: &[TelegramChatHistory]) -> (String, HashMap<i64, i64>) {
    let mut lines = Vec::with_capacity(histories.len());
    let mut virtual_to_real = HashMap::with_capacity(histories.len());
    let mut next_virtual_id = 1_i64;

    for history in histories {
        let message_virtual_id = next_virtual_id;
        virtual_to_real.insert(message_virtual_id, history.message_id);
        next_virtual_id += 1;

        let sender = format_full_name_and_username(&history.full_name, &history.username);
        if history.replied_to_message_id == 0 {
            lines.push(format!(
                "msgId:{message_virtual_id}: {sender} sent: {}",
                history.text
            ));
            continue;
        }

        let reply_virtual_id = next_virtual_id;
        virtual_to_real.insert(reply_virtual_id, history.replied_to_message_id);
        next_virtual_id += 1;
        let reply_sender = format_full_name_and_username(
            &history.replied_to_full_name,
            &history.replied_to_username,
        );
        lines.push(format!(
            "msgId:{message_virtual_id}: {sender} replying to \
             [{reply_sender} sent msgId:{reply_virtual_id}]: {}",
            history.text
        ));
    }

    (lines.join("\n"), virtual_to_real)
}

/// Build Go's private-forwarded prompt with Telegram's real message IDs.
#[must_use]
pub fn build_private_forwarded_recap_prompt(
    histories: &[PrivateForwardedReplayChatHistory],
) -> String {
    histories
        .iter()
        .map(|history| {
            format!(
                "msgId:{}: {} sent: {}",
                history.message_id,
                format_full_name_and_username(
                    &history.actor_display_name,
                    &history.actor_username,
                ),
                history.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build Go's condensed-generator transcript while retaining source indices.
#[must_use]
pub fn build_condensed_history(histories: &[TelegramChatHistory]) -> String {
    let mut content = String::new();

    for (index, history) in histories.iter().enumerate() {
        if history.text.is_empty() {
            continue;
        }
        let name = if !history.full_name.is_empty() {
            history.full_name.as_str()
        } else if !history.username.is_empty() {
            history.username.as_str()
        } else {
            "未知用戶"
        };
        content.push_str(&format!("[{}] {name}: {}\n", index + 1, history.text));
    }

    content
}

/// Merge one detailed-generation trace using Go's ordered unique text rules.
pub fn merge_recap_execution_trace(
    destination: &mut RecapExecutionTrace,
    source: &RecapExecutionTrace,
) {
    merge_generation_trace(&mut destination.generation, &source.generation);
}

/// Select the single model name stored in Go's group recap log.
#[must_use]
pub fn resolved_group_model_name(configured_model: &str, trace: &RecapExecutionTrace) -> String {
    if trace.generation.backup_used && trace.generation.backup_succeeded {
        if !trace.generation.backup_used_model.is_empty() {
            return trace.generation.backup_used_model.clone();
        }
        if !trace.generation.backup_model.is_empty() {
            return trace.generation.backup_model.clone();
        }
    } else if !trace.generation.primary_used_model.is_empty() {
        return trace.generation.primary_used_model.clone();
    }

    configured_model.to_owned()
}

/// Persist Go's group recap log and return the values needed by delivery.
pub async fn persist_group_detailed_recap(
    database: &Database,
    chat_id: i64,
    recap_inputs: &str,
    summaries: Vec<String>,
    usage: TokenUsage,
    trace: RecapExecutionTrace,
    configured_model: &str,
) -> anyhow::Result<GroupDetailedRecap> {
    let model_name = resolved_group_model_name(configured_model, &trace);
    let recap_outputs = summaries.join("\n\n");
    let log_id = recap_logs::create_group_recap(
        database,
        chat_id,
        recap_inputs,
        &recap_outputs,
        usage,
        &model_name,
    )
    .await?;

    Ok(GroupDetailedRecap {
        log_id,
        recap_inputs: recap_inputs.to_owned(),
        summaries,
        usage,
        trace,
    })
}

fn format_full_name_and_username(full_name: &str, username: &str) -> String {
    if full_name.chars().count() >= 10 && !username.is_empty() {
        return username.to_owned();
    }
    full_name.replace('#', "")
}

fn zero_usage() -> TokenUsage {
    TokenUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    }
}

fn add_usage(total: &mut TokenUsage, usage: TokenUsage) {
    total.prompt_tokens += usage.prompt_tokens;
    total.completion_tokens += usage.completion_tokens;
    total.total_tokens += usage.total_tokens;
}

fn merge_generation_trace(
    destination: &mut GenerationModelExecutionTrace,
    source: &GenerationModelExecutionTrace,
) {
    destination.primary_model =
        merge_unique_trace_text(&destination.primary_model, &source.primary_model);
    destination.primary_used_model =
        merge_unique_trace_text(&destination.primary_used_model, &source.primary_used_model);
    destination.primary_failed |= source.primary_failed;
    destination.primary_failure_reason = merge_unique_trace_text(
        &destination.primary_failure_reason,
        &source.primary_failure_reason,
    );
    destination.backup_model =
        merge_unique_trace_text(&destination.backup_model, &source.backup_model);
    destination.backup_used_model =
        merge_unique_trace_text(&destination.backup_used_model, &source.backup_used_model);
    destination.backup_used |= source.backup_used;
    destination.backup_succeeded |= source.backup_succeeded;
    destination.backup_failure_reason = merge_unique_trace_text(
        &destination.backup_failure_reason,
        &source.backup_failure_reason,
    );
}

fn merge_unique_trace_text(current: &str, incoming: &str) -> String {
    let incoming = incoming.trim();
    if incoming.is_empty() {
        return current.to_owned();
    }
    if current.trim().is_empty() {
        return incoming.to_owned();
    }
    if current.split(", ").any(|existing| existing == incoming) {
        return current.to_owned();
    }
    format!("{current}, {incoming}")
}
