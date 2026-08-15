//! Go-compatible Rich Markdown OpenAI transport behavior.

use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use insights_bot_telegram_rs::{
    config::{CondensedPromptConfig, OpenAiConfig as OpenAiSettings, RecapOpenAiConfig},
    db::models::TokenUsage,
    services::{
        openai::{
            OpenAiClient, SARCASTIC_CONDENSE_OPERATION, SUMMARIZE_CHAT_HISTORIES_OPERATION,
            TokenUsageRecorder,
        },
        rate_limit::GoRateLimiter,
    },
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

fn openai_settings(server: &MockServer) -> OpenAiSettings {
    OpenAiSettings {
        api_key: "task9-test-key".to_string(),
        api_base: Some(format!("{}/v1", server.uri())),
        model: "unused-legacy-model".to_string(),
        token_limit: Some(4096),
        recap_token_limit: Some(2000),
    }
}

fn recap_settings() -> RecapOpenAiConfig {
    RecapOpenAiConfig {
        primary_model: "recap-primary".to_string(),
        primary_backups: vec!["recap-backup-1".to_string(), "recap-backup-2".to_string()],
        condensed_model: "condensed-primary".to_string(),
        condensed_backups: vec!["condensed-backup".to_string()],
        check_model: Some("check-primary".to_string()),
        check_backups: vec!["check-backup".to_string()],
        token_limit: 4096,
        recap_reserve: 2000,
        summary_language: "Traditional Chinese".to_string(),
        force_check_failure: false,
        force_condensed_primary_failure: false,
        verbose_payload_logs: false,
    }
}

fn client(server: &MockServer, recap: &RecapOpenAiConfig) -> OpenAiClient {
    OpenAiClient::new(
        &openai_settings(server),
        recap,
        &CondensedPromptConfig {
            system_prompt: None,
            user_prompt: None,
        },
    )
    .expect("OpenAI test client")
    .with_rate_limiter(Arc::new(GoRateLimiter::per_second(1000)))
}

fn completion_response(model: &str, content: &str, usage: TokenUsage) -> Value {
    json!({
        "id": "chatcmpl-task9",
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens
        }
    })
}

async fn mount_model_response(server: &MockServer, requested_model: &str, response: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({ "model": requested_model })))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_model_error(server: &MockServer, requested_model: &str, message: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({ "model": requested_model })))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {
                "message": message,
                "type": "server_error",
                "param": null,
                "code": null
            }
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UsageRecord {
    operation: String,
    usage: TokenUsage,
    model: String,
}

#[derive(Debug, Default)]
struct RecordingUsage(Mutex<Vec<UsageRecord>>);

#[async_trait]
impl TokenUsageRecorder for RecordingUsage {
    async fn record(&self, operation: &str, usage: TokenUsage, model: &str) -> Result<()> {
        self.0.lock().expect("usage records").push(UsageRecord {
            operation: operation.to_string(),
            usage,
            model: model.to_string(),
        });
        Ok(())
    }
}

#[derive(Debug)]
struct RejectingUsage;

#[async_trait]
impl TokenUsageRecorder for RejectingUsage {
    async fn record(&self, _operation: &str, _usage: TokenUsage, _model: &str) -> Result<()> {
        Err(anyhow!("metric datastore unavailable"))
    }
}

#[tokio::test]
async fn detailed_recap_uses_raw_rich_markdown_prompt_and_response_model_trace() {
    let server = MockServer::start().await;
    let usage = TokenUsage {
        prompt_tokens: 13,
        completion_tokens: 8,
        total_tokens: 21,
    };
    mount_model_response(
        &server,
        "recap-primary",
        completion_response(
            "actual-primary-version",
            "## Topic\n- Detail {{tg-ref:7}}",
            usage,
        ),
    )
    .await;

    let result = client(&server, &recap_settings())
        .summarize_chat_histories_raw("msgId:7: Ada sent: hello", "English")
        .await
        .expect("detailed recap");

    assert_eq!(result.content, "## Topic\n- Detail {{tg-ref:7}}");
    assert_eq!(result.usage, usage);
    assert_eq!(result.trace.generation.primary_model, "recap-primary");
    assert_eq!(
        result.trace.generation.primary_used_model,
        "actual-primary-version"
    );

    let requests = server.received_requests().await.expect("recorded request");
    let request: Value = serde_json::from_slice(&requests[0].body).expect("request JSON");
    assert_eq!(request["model"], "recap-primary");
    assert!(request.get("max_tokens").is_none());
    assert!(request.get("temperature").is_none());
    let system = request["messages"][0]["content"]
        .as_str()
        .expect("system prompt");
    assert!(system.contains("detailed Telegram chat recaps as Rich Markdown"));
    assert!(system.contains("{{tg-ref:1,2}}"));
    assert!(!system.contains("JSON Schema"));
    assert!(!system.contains("topicName"));
    assert_eq!(
        request["messages"][1]["content"],
        "Summarize the following Telegram chat history into a detailed Rich Markdown recap.\nThe output language should be English.\nFollow the Rich Markdown and controlled source-marker rules from the system instructions.\n\nChat histories:\nmsgId:7: Ada sent: hello\n"
    );
}

#[tokio::test]
async fn token_split_matches_go_cl100k_vectors_without_broken_utf8() {
    let server = MockServer::start().await;
    let client = client(&server, &recap_settings());
    let chinese = "小溪河水清澈见底，沿岸芦苇丛生。远处山峰耸立，白云飘渺。一只黄鹂停在枝头，唱起了优美的歌曲，引来了不少路人驻足欣赏。";

    assert_eq!(
        client
            .split_content_by_token_limit(chinese, 20)
            .expect("token split"),
        vec![
            "小溪河水清澈见底，沿岸芦",
            "苇丛生。远处山峰耸立，白",
            "云飘渺。一只黄鹂停在枝头，",
            "唱起了优美的歌曲，引来了不少路人",
            "驻足欣赏。",
        ]
    );
    assert_eq!(
        client
            .split_content_by_token_limit("小溪河水清澈见底", 4)
            .expect("partial UTF-8 token boundary"),
        vec!["小溪", "河水清", "澈见", "底"]
    );
    assert_eq!(
        client
            .split_content_by_token_limit("", 20)
            .expect("empty Go vector"),
        vec![""]
    );
    assert!(client.split_content_by_token_limit("content", 0).is_err());
}

#[tokio::test]
async fn detailed_recap_tries_error_then_empty_backups_in_order_and_records_final_model() {
    let server = MockServer::start().await;
    mount_model_error(&server, "recap-primary", "primary failed").await;
    mount_model_response(
        &server,
        "recap-backup-1",
        completion_response(
            "empty-backup-version",
            "   ",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
        ),
    )
    .await;
    let final_usage = TokenUsage {
        prompt_tokens: 5,
        completion_tokens: 7,
        total_tokens: 12,
    };
    mount_model_response(
        &server,
        "recap-backup-2",
        completion_response("resolved-detail-backup", "## 備援\n- 成功", final_usage),
    )
    .await;

    let recorder = Arc::new(RecordingUsage::default());
    let result = client(&server, &recap_settings())
        .with_token_usage_recorder(recorder.clone())
        .summarize_chat_histories_raw("history", "")
        .await
        .expect("final backup succeeds");

    assert_eq!(result.content, "## 備援\n- 成功");
    assert!(result.trace.generation.primary_failed);
    assert!(result.trace.generation.backup_used);
    assert!(result.trace.generation.backup_succeeded);
    assert_eq!(
        result.trace.generation.backup_used_model,
        "resolved-detail-backup"
    );
    assert_eq!(
        recorder.0.lock().expect("usage records").as_slice(),
        &[UsageRecord {
            operation: SUMMARIZE_CHAT_HISTORIES_OPERATION.to_string(),
            usage: final_usage,
            model: "resolved-detail-backup".to_string(),
        }]
    );
    let requests = server.received_requests().await.expect("recorded requests");
    let models = requests
        .iter()
        .map(|request| {
            serde_json::from_slice::<Value>(&request.body).expect("request JSON")["model"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        models,
        ["recap-primary", "recap-backup-1", "recap-backup-2"]
    );
}

#[tokio::test]
async fn detailed_recap_returns_last_failure_with_complete_trace() {
    let server = MockServer::start().await;
    mount_model_error(&server, "recap-primary", "primary failed").await;
    mount_model_error(&server, "recap-backup-1", "backup one failed").await;
    mount_model_response(
        &server,
        "recap-backup-2",
        completion_response(
            "backup-two",
            "",
            TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
        ),
    )
    .await;

    let error = client(&server, &recap_settings())
        .summarize_chat_histories_raw("history", "English")
        .await
        .expect_err("all models fail");

    assert_eq!(
        error.to_string(),
        "backup model returned empty recap content"
    );
    assert!(error.trace.generation.primary_failed);
    assert!(error.trace.generation.backup_used);
    assert!(!error.trace.generation.backup_succeeded);
    assert_eq!(
        error.trace.generation.backup_failure_reason,
        "backup model returned empty recap content"
    );
}

#[tokio::test]
async fn detailed_empty_primary_keeps_resolved_primary_model_when_backups_fail() {
    let server = MockServer::start().await;
    mount_model_response(
        &server,
        "recap-primary",
        completion_response(
            "resolved-empty-primary",
            "",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 0,
                total_tokens: 1,
            },
        ),
    )
    .await;
    mount_model_error(&server, "recap-backup-1", "backup one failed").await;
    mount_model_error(&server, "recap-backup-2", "backup two failed").await;

    let error = client(&server, &recap_settings())
        .summarize_chat_histories_raw("history", "English")
        .await
        .expect_err("empty primary and failed backups");
    assert_eq!(
        error.trace.generation.primary_used_model,
        "resolved-empty-primary"
    );
    assert!(error.trace.generation.primary_failed);
    assert!(error.trace.generation.backup_used);
    assert!(!error.trace.generation.backup_succeeded);
}

#[tokio::test]
async fn detailed_metric_failure_is_nonfatal() {
    let server = MockServer::start().await;
    mount_model_response(
        &server,
        "recap-primary",
        completion_response(
            "resolved-primary",
            "## still returned",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
            },
        ),
    )
    .await;
    let result = client(&server, &recap_settings())
        .with_token_usage_recorder(Arc::new(RejectingUsage))
        .summarize_chat_histories_raw("history", "English")
        .await
        .expect("metric error is non-fatal");
    assert_eq!(result.content, "## still returned");
}

#[tokio::test]
async fn condensed_validator_matches_go_rules_exactly() {
    let cases = [
        ("", "no content generated"),
        (
            "```text\nhello\n```",
            "condensed output contains code fence",
        ),
        (r#"{"summary":"hello"}"#, "condensed output is json-like"),
        ("[\"hello\"]", "condensed output is json-like"),
        (
            "first line\nsecond line",
            "condensed output is not a single line",
        ),
        ("### heading", "condensed output contains a heading"),
        (
            "- list item",
            "condensed output contains an unordered-list item",
        ),
        (
            "* list item",
            "condensed output contains an unordered-list item",
        ),
    ];

    for (content, reason) in cases {
        let server = MockServer::start().await;
        let mut recap = recap_settings();
        recap.condensed_backups.clear();
        recap.check_model = None;
        recap.check_backups.clear();
        mount_model_response(
            &server,
            "condensed-primary",
            completion_response(
                "resolved-condensed",
                content,
                TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                },
            ),
        )
        .await;

        let error = client(&server, &recap)
            .sarcastic_condense_traced("history")
            .await
            .expect_err("invalid condensed output");
        assert_eq!(error.to_string(), reason, "content: {content:?}");
        assert!(error.trace.generation.primary_failed);
        assert!(error.trace.generation.primary_used_model.is_empty());
    }

    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.condensed_backups.clear();
    recap.check_model = None;
    recap.check_backups.clear();
    mount_model_response(
        &server,
        "condensed-primary",
        completion_response(
            "resolved-condensed",
            "  **一句話**——可以有 inline Markdown。  ",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
        ),
    )
    .await;
    let result = client(&server, &recap)
        .sarcastic_condense_traced("history")
        .await
        .expect("valid one-line output");
    assert_eq!(result.content, "**一句話**——可以有 inline Markdown。");
}

#[tokio::test]
async fn condensed_uses_last_invalid_backup_then_check_backup_and_traces_resolved_models() {
    let server = MockServer::start().await;
    mount_model_response(
        &server,
        "condensed-primary",
        completion_response(
            "resolved-invalid-primary",
            r#"{"summary":"primary invalid"}"#,
            TokenUsage {
                prompt_tokens: 2,
                completion_tokens: 3,
                total_tokens: 5,
            },
        ),
    )
    .await;
    mount_model_response(
        &server,
        "condensed-backup",
        completion_response(
            "resolved-invalid-backup",
            "- backup invalid",
            TokenUsage {
                prompt_tokens: 4,
                completion_tokens: 5,
                total_tokens: 9,
            },
        ),
    )
    .await;
    mount_model_response(
        &server,
        "check-primary",
        completion_response(
            "resolved-invalid-check",
            "### still invalid\n- item",
            TokenUsage {
                prompt_tokens: 20,
                completion_tokens: 20,
                total_tokens: 40,
            },
        ),
    )
    .await;
    mount_model_response(
        &server,
        "check-backup",
        completion_response(
            "resolved-check-backup",
            "**修復完成**——一句話把所有話題串起來。",
            TokenUsage {
                prompt_tokens: 30,
                completion_tokens: 30,
                total_tokens: 60,
            },
        ),
    )
    .await;

    let recorder = Arc::new(RecordingUsage::default());
    let result = client(&server, &recap_settings())
        .with_token_usage_recorder(recorder.clone())
        .sarcastic_condense_traced("history")
        .await
        .expect("check backup repairs output");

    assert_eq!(result.content, "**修復完成**——一句話把所有話題串起來。");
    assert!(result.trace.generation.primary_failed);
    assert!(result.trace.generation.backup_used);
    assert!(!result.trace.generation.backup_succeeded);
    assert!(result.trace.generation.primary_used_model.is_empty());
    assert_eq!(
        result.trace.generation.backup_used_model,
        "resolved-invalid-backup"
    );
    assert!(result.trace.check.attempted);
    assert!(result.trace.check.succeeded);
    assert!(result.trace.check.generation.primary_failed);
    assert!(result.trace.check.generation.backup_used);
    assert!(result.trace.check.generation.backup_succeeded);
    assert_eq!(
        result.trace.check.generation.backup_used_model,
        "resolved-check-backup"
    );
    assert_eq!(
        recorder.0.lock().expect("usage records").as_slice(),
        &[UsageRecord {
            operation: SARCASTIC_CONDENSE_OPERATION.to_string(),
            usage: TokenUsage {
                prompt_tokens: 4,
                completion_tokens: 5,
                total_tokens: 9,
            },
            model: "resolved-invalid-backup".to_string(),
        }]
    );

    let requests = server.received_requests().await.expect("recorded requests");
    let bodies = requests
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).expect("request JSON"))
        .collect::<Vec<_>>();
    let models = bodies
        .iter()
        .map(|body| body["model"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        models,
        [
            "condensed-primary",
            "condensed-backup",
            "check-primary",
            "check-backup"
        ]
    );
    assert_eq!(bodies[0]["temperature"], 0.7);
    assert_eq!(bodies[1]["temperature"], 0.7);
    assert!(bodies[2].get("temperature").is_none());
    assert!(bodies.iter().all(|body| body.get("max_tokens").is_none()));
    assert!(
        bodies[2]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("- backup invalid")
    );
}

#[tokio::test]
async fn condensed_all_generation_failures_return_trace_without_check_attempt() {
    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.condensed_backups = vec!["condensed-backup-1".into(), "condensed-backup-2".into()];
    mount_model_error(&server, "condensed-primary", "primary failed").await;
    mount_model_error(&server, "condensed-backup-1", "backup one failed").await;
    mount_model_response(
        &server,
        "condensed-backup-2",
        completion_response(
            "backup-two",
            "",
            TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
        ),
    )
    .await;

    let error = client(&server, &recap)
        .sarcastic_condense_traced("history")
        .await
        .expect_err("all generation models fail");
    assert!(error.trace.generation.primary_failed);
    assert!(error.trace.generation.backup_used);
    assert!(!error.trace.generation.backup_succeeded);
    assert!(error.trace.check.generation.primary_model == "check-primary");
    assert!(!error.trace.check.attempted);
    assert!(error.trace.check.failure_reason.is_empty());
}

#[tokio::test]
async fn condensed_all_check_failures_return_complete_trace() {
    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.condensed_backups.clear();
    recap.check_backups = vec!["check-backup-1".into(), "check-backup-2".into()];
    mount_model_response(
        &server,
        "condensed-primary",
        completion_response(
            "resolved-invalid-primary",
            r#"{"summary":"invalid"}"#,
            TokenUsage {
                prompt_tokens: 2,
                completion_tokens: 3,
                total_tokens: 5,
            },
        ),
    )
    .await;
    mount_model_error(&server, "check-primary", "check primary failed").await;
    mount_model_response(
        &server,
        "check-backup-1",
        completion_response(
            "invalid-check-backup",
            "- still invalid",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
        ),
    )
    .await;
    mount_model_error(&server, "check-backup-2", "last check failed").await;

    let error = client(&server, &recap)
        .sarcastic_condense_traced("history")
        .await
        .expect_err("all check models fail");
    assert!(error.trace.check.attempted);
    assert!(!error.trace.check.succeeded);
    assert!(error.trace.check.generation.primary_failed);
    assert!(error.trace.check.generation.backup_used);
    assert!(!error.trace.check.generation.backup_succeeded);
    assert!(
        error
            .trace
            .check
            .generation
            .backup_failure_reason
            .contains("last check failed")
    );
    assert_eq!(
        error.trace.check.failure_reason,
        error.trace.check.generation.backup_failure_reason
    );
}

#[tokio::test]
async fn condensed_keeps_primary_validation_error_after_later_backup_transport_failure() {
    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.condensed_backups = vec!["condensed-backup-1".into(), "condensed-backup-2".into()];
    recap.check_model = None;
    recap.check_backups.clear();
    mount_model_response(
        &server,
        "condensed-primary",
        completion_response(
            "invalid-primary",
            "# invalid primary",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
        ),
    )
    .await;
    mount_model_response(
        &server,
        "condensed-backup-1",
        completion_response(
            "invalid-backup",
            "- invalid backup",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
        ),
    )
    .await;
    mount_model_error(
        &server,
        "condensed-backup-2",
        "later backup transport failed",
    )
    .await;

    let error = client(&server, &recap)
        .sarcastic_condense_traced("history")
        .await
        .expect_err("invalid generation without Check must fail");

    assert_eq!(
        error.source.to_string(),
        "condensed output contains a heading"
    );
    assert!(
        error
            .trace
            .generation
            .backup_failure_reason
            .contains("later backup transport failed")
    );
    assert_eq!(error.trace.generation.backup_used_model, "invalid-backup");
}

#[tokio::test]
async fn condensed_primary_transport_failure_returns_retained_backup_validation_error() {
    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.condensed_backups = vec!["condensed-backup-1".into(), "condensed-backup-2".into()];
    recap.check_model = None;
    recap.check_backups.clear();
    mount_model_error(&server, "condensed-primary", "primary transport failed").await;
    mount_model_response(
        &server,
        "condensed-backup-1",
        completion_response(
            "invalid-backup",
            "- invalid backup",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
        ),
    )
    .await;
    mount_model_error(
        &server,
        "condensed-backup-2",
        "later backup transport failed",
    )
    .await;

    let error = client(&server, &recap)
        .sarcastic_condense_traced("history")
        .await
        .expect_err("invalid retained backup without Check must fail");

    assert_eq!(
        error.source.to_string(),
        "condensed output contains an unordered-list item"
    );
    assert!(
        error
            .trace
            .generation
            .backup_failure_reason
            .contains("later backup transport failed")
    );
    assert_eq!(error.trace.generation.backup_used_model, "invalid-backup");
}

#[tokio::test]
async fn condensed_empty_primary_uses_go_trace_reason_and_returns_last_backup_failure() {
    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.condensed_backups = vec!["condensed-backup".into()];
    recap.check_model = None;
    recap.check_backups.clear();
    mount_model_response(
        &server,
        "condensed-primary",
        json!({
            "id": "chatcmpl-empty-condensed",
            "object": "chat.completion",
            "created": 0,
            "model": "empty-primary",
            "choices": [],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 0,
                "total_tokens": 1
            }
        }),
    )
    .await;
    mount_model_error(&server, "condensed-backup", "backup unavailable").await;

    let error = client(&server, &recap)
        .sarcastic_condense_traced("history")
        .await
        .expect_err("empty primary and failed backups must fail");

    assert_eq!(
        error.trace.generation.primary_failure_reason,
        "no content generated from primary model"
    );
    assert!(error.source.to_string().contains("backup unavailable"));
    assert!(
        error
            .trace
            .generation
            .backup_failure_reason
            .contains("backup unavailable")
    );
}

#[tokio::test]
async fn condensed_check_zero_choices_uses_go_fixed_error() {
    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.condensed_backups.clear();
    recap.check_backups.clear();
    mount_model_response(
        &server,
        "condensed-primary",
        completion_response(
            "invalid-primary",
            "# invalid primary",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
        ),
    )
    .await;
    mount_model_response(
        &server,
        "check-primary",
        json!({
            "id": "chatcmpl-empty-check",
            "object": "chat.completion",
            "created": 0,
            "model": "check-primary",
            "choices": [],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 0,
                "total_tokens": 1
            }
        }),
    )
    .await;

    let error = client(&server, &recap)
        .sarcastic_condense_traced("history")
        .await
        .expect_err("empty Check choices must fail");

    assert_eq!(
        error.source.to_string(),
        "check model returned empty choices"
    );
    assert_eq!(
        error.trace.check.generation.primary_failure_reason,
        "check model returned empty choices"
    );
    assert_eq!(
        error.trace.check.failure_reason,
        "check model returned empty choices"
    );
}

#[tokio::test]
async fn condensed_force_switch_uses_generation_backup_after_a_valid_primary() {
    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.force_condensed_primary_failure = true;
    recap.condensed_backups = vec!["condensed-backup".into()];
    mount_model_response(
        &server,
        "condensed-primary",
        completion_response(
            "resolved-primary",
            "valid primary",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
        ),
    )
    .await;
    mount_model_response(
        &server,
        "condensed-backup",
        completion_response(
            "resolved-backup",
            "valid backup",
            TokenUsage {
                prompt_tokens: 2,
                completion_tokens: 2,
                total_tokens: 4,
            },
        ),
    )
    .await;

    let generated = client(&server, &recap)
        .sarcastic_condense_traced("history")
        .await
        .expect("forced generation backup");

    assert_eq!(generated.content, "valid backup");
    assert!(generated.trace.generation.primary_failed);
    assert_eq!(
        generated.trace.generation.primary_failure_reason,
        "forced condensed primary failure via env switch"
    );
    assert!(generated.trace.generation.backup_used);
    assert!(generated.trace.generation.backup_succeeded);
    assert!(generated.trace.generation.primary_used_model.is_empty());
    assert_eq!(
        generated.trace.generation.backup_used_model,
        "resolved-backup"
    );
}

#[tokio::test]
async fn condensed_force_check_failure_skips_primary_check_http_and_uses_backup() {
    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.condensed_backups.clear();
    recap.force_check_failure = true;
    recap.check_backups = vec!["check-backup".into()];
    mount_model_response(
        &server,
        "condensed-primary",
        completion_response(
            "invalid-primary",
            "# invalid primary",
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
        ),
    )
    .await;
    mount_model_response(
        &server,
        "check-backup",
        completion_response(
            "resolved-check-backup",
            "repaired line",
            TokenUsage {
                prompt_tokens: 2,
                completion_tokens: 1,
                total_tokens: 3,
            },
        ),
    )
    .await;

    let generated = client(&server, &recap)
        .sarcastic_condense_traced("history")
        .await
        .expect("forced Check backup");

    assert_eq!(generated.content, "repaired line");
    assert!(generated.trace.check.generation.primary_failed);
    assert_eq!(
        generated.trace.check.generation.primary_failure_reason,
        "check model forced failure via env switch"
    );
    assert!(generated.trace.check.generation.backup_succeeded);
    assert_eq!(
        generated.trace.check.generation.backup_used_model,
        "resolved-check-backup"
    );
    let requests = server.received_requests().await.expect("recorded requests");
    let models = requests
        .iter()
        .map(|request| {
            serde_json::from_slice::<Value>(&request.body).expect("request JSON")["model"]
                .as_str()
                .expect("request model")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(models, ["condensed-primary", "check-backup"]);
}
