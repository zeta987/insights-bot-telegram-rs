// Prompt templates for OpenAI summarization, ported from Go version.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{CondensedPromptConfig, RecapOpenAiConfig, render_go_template};

// Default sarcastic condensed prompts (English fallback, can be overridden via environment variables or i18n).
pub const DEFAULT_SARCASTIC_SYSTEM_PROMPT: &str = "You are a concise chat history summarizer.\nSummarize the provided chat history into a single-sentence core summary, using 1-2 relevant emojis.\nBe concise and get straight to the point.\nRespond directly with the summary, no preamble or explanation.";

pub const DEFAULT_SARCASTIC_USER_PROMPT: &str = "Here is a chat history, please provide your summary:\n\nChat history:\"\"\"\n{{ .ChatHistory }}\n\"\"\"\n\nPlease provide the summary directly, no explanation.";

// Structured chat history summarization with JSON Schema output.
pub const CHAT_HISTORY_SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are an expert in summarizing refined outlines from documents and dialogues. Your task is to identify 1-20 distinct discussion topics from chat histories, focusing on key points and maintaining the conversation's essence.

Please format your response according to the following JSON Schema:
{"$schema":"http://json-schema.org/draft-07/schema#","title":"Chat Histories Summarization Schema","type":"array","items":{"type":"object","properties":{"topicName":{"type":"string","description":"The title, brief short title of the topic that talked, discussed in the chat history."},"sinceId":{"type":"number","description":"The id of the message from which the topic initially starts."},"participants":{"type":"array","description":"The list of the names of the participated users in the topic.","items":{"type":"string"}},"discussion":{"type":"array","description":"The list of the points that discussed during the topic.","items":{"type":"object","properties":{"point":{"type":"string","description":"The key point that talked, expressed, mentioned, or discussed during the topic."},"keyIds":{"type":"array","description":"The list of the ids of the messages that contain the key point.","items":{"type":"number"}}},"required":["point","keyIds"]},"minItems": 1,"maxItems": 5},"conclusion":{"type":"string","description":"The conclusion of the topic, optional."}},"required":["topicName","sinceId","participants","discussion"]}}

Example output:
[{"topicName":"Most Important Topic 1","sinceId":123456789,"participants":["John","Mary"],"discussion":[{"point":"Most relevant key point","keyIds":[123456789,987654321]}],"conclusion":"Optional brief conclusion"},{"topicName":"Most Important Topic 2","sinceId":987654321,"participants":["Bob","Alice"],"discussion":[{"point":"Most relevant key point","keyIds":[987654321]}],"conclusion":"Optional brief conclusion"}]"#;

pub const CHAT_HISTORY_SUMMARIZATION_USER_PROMPT: &str = r#"Please analyze the following chat history and provide a summary in {{ .Language }}:

Chat histories:"""
{{ .ChatHistory }}
"""

Note: Topics may be discussed in parallel, so consider relevant keywords across the chat histories. Be concise and focus on the key essence of each topic."#;

// Check model prompts for format verification / repair (ported from Go prompts.go:99-128).

pub const CHECK_SUMMARY_JSON_SYSTEM_PROMPT: &str = r#"You are a strict JSON repair validator.
Your task is to output a valid JSON array only.
The JSON MUST conform to this schema:
[{"topicName":"string","sinceId":123,"participants":["string"],"discussion":[{"point":"string","keyIds":[123]}],"conclusion":"string"}]
Rules:
1) Output valid JSON only.
2) Do not use markdown fences.
3) Do not include any explanation text.
4) Keep original meaning as much as possible.
5) Ensure each item has non-empty topicName, participants, and discussion.
6) Ensure each discussion item has non-empty point and keyIds.
7) If sinceId/keyIds are missing or unknown, use sinceId=1 and keyIds=[1]."#;

pub const CHECK_SUMMARY_JSON_USER_PROMPT: &str = "Please repair the following JSON payload into a valid JSON array that follows the schema:\n\n{{raw_json}}";

pub const CHECK_CONDENSED_OUTPUT_SYSTEM_PROMPT: &str = r#"You are a strict output rewriter for condensed summaries.
Your task is to rewrite the provided text into one natural sentence only.
Rules:
1) Output exactly one single-line sentence.
2) Do not use markdown code fences.
3) Do not output JSON, arrays, objects, or key-value format.
4) Do not add explanations or prefixes.
5) Keep the original meaning as much as possible.
6) Preserve emoji when appropriate."#;

pub const CHECK_CONDENSED_OUTPUT_USER_PROMPT: &str = "Please rewrite the following invalid condensed summary into one natural sentence:\n\n{{raw_output}}";

// Message preprocessing prompts, ported verbatim from Go v1.0.0
// `internal/thirdparty/openai/prompts.go` and `openai.go`.

/// Go's `AnySummarizationSystemPrompt`, used when a link title exceeds 200 runes.
pub const ANY_SUMMARIZATION_SYSTEM_PROMPT: &str = "你是我的总结助手。我将为你提供一段话，我需要你在不丢失原文主旨和情感、不做更多的解释和说明的情况下帮我用不超过100字总结一下这段话说了什么。";

/// Go's `AnySummarizationUserPrompt`, whose template body is `內容：\n{{ .Content }}`.
pub fn render_any_summarization_user_prompt(content: &str) -> String {
    format!("內容：\n{content}")
}

/// Go's inline system prompt in `SummarizeOneChatHistory`.
pub const ONE_CHAT_HISTORY_SUMMARIZATION_SYSTEM_PROMPT: &str = "你是我的聊天消息总结助手。我将为你提供一条包含了人物名称、人物用户名、消息发送时间、消息内容等信息的消息，因为这条聊天消息有些过长了，我需要你帮我总结一下这条消息说了什么。最好一句话概括，如果这条消息有标题的话你可以直接返回标题。";

/// Go's inline user prompt in `SummarizeOneChatHistory`.
pub fn render_one_chat_history_user_prompt(chat_history: &str) -> String {
    format!("消息：\n{chat_history}\n请你帮我总结一下。")
}

/// Condensed prompts sourced from the validated application configuration.
#[derive(Clone)]
pub struct PromptConfig {
    pub sarcastic_system_prompt: String,
    pub sarcastic_user_prompt: String,
    pub summarization_language: String,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            sarcastic_system_prompt: DEFAULT_SARCASTIC_SYSTEM_PROMPT.to_string(),
            sarcastic_user_prompt: DEFAULT_SARCASTIC_USER_PROMPT.to_string(),
            summarization_language: "English".to_string(),
        }
    }
}

impl PromptConfig {
    pub fn from_config(recap: &RecapOpenAiConfig, condensed: &CondensedPromptConfig) -> Self {
        Self {
            sarcastic_system_prompt: condensed
                .system_prompt
                .clone()
                .unwrap_or_else(|| DEFAULT_SARCASTIC_SYSTEM_PROMPT.to_owned()),
            sarcastic_user_prompt: condensed
                .user_prompt
                .clone()
                .unwrap_or_else(|| DEFAULT_SARCASTIC_USER_PROMPT.to_owned()),
            summarization_language: recap.summary_language.clone(),
        }
    }

    /// Render the sarcastic user prompt with chat history substitution.
    pub fn render_sarcastic_user_prompt(&self, chat_history: &str) -> Result<String> {
        render_go_template(&self.sarcastic_user_prompt, [("ChatHistory", chat_history)])
    }
}

pub fn render_structured_summary_user_prompt(language: &str, chat_history: &str) -> Result<String> {
    render_go_template(
        CHAT_HISTORY_SUMMARIZATION_USER_PROMPT,
        [("Language", language), ("ChatHistory", chat_history)],
    )
}

// Structured output types for JSON summarization mode.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscussionPoint {
    pub point: String,
    #[serde(rename = "keyIds")]
    pub key_ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicSummary {
    #[serde(rename = "topicName")]
    pub topic_name: String,
    #[serde(rename = "sinceId")]
    pub since_id: i64,
    pub participants: Vec<String>,
    pub discussion: Vec<DiscussionPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
}

pub type StructuredSummary = Vec<TopicSummary>;
