//! Go-compatible OpenAI preprocessing adapter behavior.
//!
//! All completions are served by a loopback WireMock server. The API key is a
//! fixed test-only value and no request leaves the machine.

use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use insights_bot_telegram_rs::{
    config::{CondensedPromptConfig, OpenAiConfig as OpenAiSettings, RecapOpenAiConfig},
    db::models::TokenUsage,
    services::{
        openai::{
            OpenAiClient, SUMMARIZE_ANY_OPERATION, SUMMARIZE_ONE_CHAT_HISTORY_OPERATION,
            TokenUsageRecorder,
        },
        rate_limit::GoRateLimiter,
    },
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct UsageRecord {
    operation: String,
    usage: TokenUsage,
    model_name: String,
}

#[derive(Debug, Default)]
struct RecordingUsage {
    records: Mutex<Vec<UsageRecord>>,
}

impl RecordingUsage {
    fn records(&self) -> Vec<UsageRecord> {
        self.records.lock().expect("usage records").clone()
    }
}

#[async_trait]
impl TokenUsageRecorder for RecordingUsage {
    async fn record(
        &self,
        prompt_operation: &str,
        usage: TokenUsage,
        model_name: &str,
    ) -> Result<()> {
        self.records
            .lock()
            .expect("usage records")
            .push(UsageRecord {
                operation: prompt_operation.to_string(),
                usage,
                model_name: model_name.to_string(),
            });
        Ok(())
    }
}

#[derive(Debug)]
struct RejectingUsage;

#[async_trait]
impl TokenUsageRecorder for RejectingUsage {
    async fn record(
        &self,
        _prompt_operation: &str,
        _usage: TokenUsage,
        _model_name: &str,
    ) -> Result<()> {
        Err(anyhow!("simulated metric write failure"))
    }
}

fn openai_settings(server: &MockServer) -> OpenAiSettings {
    OpenAiSettings {
        api_key: "task5b1-test-key".to_string(),
        api_base: Some(format!("{}/v1", server.uri())),
        model: "configured-primary".to_string(),
        token_limit: Some(4096),
        recap_token_limit: Some(2000),
    }
}

fn recap_settings() -> RecapOpenAiConfig {
    RecapOpenAiConfig {
        primary_model: "configured-primary".to_string(),
        primary_backups: vec!["unused-backup".to_string()],
        condensed_model: "configured-condensed".to_string(),
        condensed_backups: Vec::new(),
        check_model: None,
        check_backups: Vec::new(),
        token_limit: 4096,
        recap_reserve: 2000,
        summary_language: "Traditional Chinese".to_string(),
        force_check_failure: false,
        force_condensed_primary_failure: false,
        verbose_payload_logs: false,
    }
}

fn base_client(server: &MockServer) -> OpenAiClient {
    OpenAiClient::new(
        &openai_settings(server),
        &recap_settings(),
        &CondensedPromptConfig {
            system_prompt: None,
            user_prompt: None,
        },
    )
    .expect("OpenAI test client")
}

fn completion_response(response_model: &str, content: &str, usage: Option<TokenUsage>) -> Value {
    let mut response = json!({
        "id": "chatcmpl-task5b1",
        "object": "chat.completion",
        "created": 0,
        "model": response_model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
            },
            "finish_reason": "stop",
        }],
    });
    if let Some(usage) = usage {
        response["usage"] = json!({
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens,
        });
    }
    response
}

async fn mount_completion(server: &MockServer, body: Value, expected_calls: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(expected_calls)
        .mount(server)
        .await;
}

#[tokio::test]
async fn constructor_primes_the_shared_one_qps_limiter() {
    let server = MockServer::start().await;
    let client = base_client(&server);

    assert_eq!(client.rate_limiter().takes(), 1);
    assert_eq!(
        client.rate_limiter().per_request(),
        std::time::Duration::from_secs(1)
    );
}

#[tokio::test]
async fn summarize_any_takes_first_and_records_requested_model_usage() {
    let server = MockServer::start().await;
    let usage = TokenUsage {
        prompt_tokens: 11,
        completion_tokens: 7,
        total_tokens: 18,
    };
    mount_completion(
        &server,
        completion_response("response-side-model", "short summary", Some(usage)),
        1,
    )
    .await;

    let limiter = Arc::new(GoRateLimiter::per_second(1000));
    let recorder = Arc::new(RecordingUsage::default());
    let client = base_client(&server)
        .with_rate_limiter(limiter.clone())
        .with_token_usage_recorder(recorder.clone());

    let choices = client
        .summarize_any("A long title")
        .await
        .expect("summarize any");

    assert_eq!(choices, vec!["short summary"]);
    assert_eq!(limiter.takes(), 1);
    assert_eq!(
        recorder.records(),
        vec![UsageRecord {
            operation: SUMMARIZE_ANY_OPERATION.to_string(),
            usage,
            model_name: "configured-primary".to_string(),
        }]
    );

    let requests = server.received_requests().await.expect("recorded request");
    let request: Value = serde_json::from_slice(&requests[0].body).expect("request JSON");
    assert_eq!(request["model"], "configured-primary");
    assert_eq!(request["messages"][0]["role"], "system");
    assert_eq!(request["messages"][1]["role"], "user");
}

#[tokio::test]
async fn summarize_one_history_uses_its_operation_and_zero_usage_default() {
    let server = MockServer::start().await;
    mount_completion(
        &server,
        completion_response("configured-primary", "one line", None),
        1,
    )
    .await;

    let recorder = Arc::new(RecordingUsage::default());
    let client = base_client(&server)
        .with_rate_limiter(Arc::new(GoRateLimiter::per_second(1000)))
        .with_token_usage_recorder(recorder.clone());

    let choices = client
        .summarize_one_chat_history("Alice: an overly long message")
        .await
        .expect("summarize one history");

    assert_eq!(choices, vec!["one line"]);
    assert_eq!(
        recorder.records(),
        vec![UsageRecord {
            operation: SUMMARIZE_ONE_CHAT_HISTORY_OPERATION.to_string(),
            usage: TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
            model_name: "configured-primary".to_string(),
        }]
    );
}

#[tokio::test]
async fn api_failure_still_consumes_permission_and_records_no_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {
                "message": "simulated upstream failure",
                "type": "server_error",
                "param": null,
                "code": null,
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let limiter = Arc::new(GoRateLimiter::per_second(1000));
    let recorder = Arc::new(RecordingUsage::default());
    let client = base_client(&server)
        .with_rate_limiter(limiter.clone())
        .with_token_usage_recorder(recorder.clone());

    client
        .summarize_any("content")
        .await
        .expect_err("500 must fail");

    assert_eq!(limiter.takes(), 1);
    assert!(recorder.records().is_empty());
}

#[tokio::test]
async fn metric_failure_does_not_fail_a_successful_completion() {
    let server = MockServer::start().await;
    mount_completion(
        &server,
        completion_response(
            "configured-primary",
            "still returned",
            Some(TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            }),
        ),
        1,
    )
    .await;

    let client = base_client(&server)
        .with_rate_limiter(Arc::new(GoRateLimiter::per_second(1000)))
        .with_token_usage_recorder(Arc::new(RejectingUsage));

    assert_eq!(
        client
            .summarize_any("content")
            .await
            .expect("metric failure is non-fatal"),
        vec!["still returned"]
    );
}

#[tokio::test]
async fn clones_share_one_limiter_across_both_preprocessing_helpers() {
    let server = MockServer::start().await;
    mount_completion(
        &server,
        completion_response("configured-primary", "answer", None),
        2,
    )
    .await;

    let limiter = Arc::new(GoRateLimiter::per_second(1000));
    let client = base_client(&server).with_rate_limiter(limiter.clone());
    let clone = client.clone();

    let (any, one) = tokio::join!(
        client.summarize_any("title"),
        clone.summarize_one_chat_history("Alice: message"),
    );

    assert_eq!(any.expect("summarize any"), vec!["answer"]);
    assert_eq!(one.expect("summarize one"), vec!["answer"]);
    assert_eq!(limiter.takes(), 2);
}
