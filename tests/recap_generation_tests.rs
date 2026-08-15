use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use insights_bot_telegram_rs::{
    config::{CondensedPromptConfig, OpenAiConfig as OpenAiSettings, RecapOpenAiConfig},
    db::models::{TelegramChatHistory, TokenUsage},
    services::{
        message_capture::PrivateForwardedReplayChatHistory,
        openai::{OpenAiClient, TokenUsageRecorder},
        rate_limit::GoRateLimiter,
        recap_generation::{
            DatabaseTokenUsageRecorder, RecapGenerationService, build_condensed_history,
            build_private_forwarded_recap_prompt, build_rich_recap_prompt,
            merge_recap_execution_trace, persist_group_detailed_recap, resolved_group_model_name,
        },
        rich_recap::{GenerationModelExecutionTrace, RecapExecutionTrace},
    },
};
use serde_json::json;
use sqlx::Row;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

mod support;
use support::sqlite_fixture::SchemaFixture;

fn history(message_id: i64, text: &str) -> TelegramChatHistory {
    TelegramChatHistory {
        id: message_id,
        chat_id: -100_123,
        chat_type: "supergroup".to_owned(),
        chat_title: "Group".to_owned(),
        message_id,
        user_id: message_id,
        username: String::new(),
        full_name: String::new(),
        text: text.to_owned(),
        replied_to_message_id: 0,
        replied_to_user_id: 0,
        replied_to_full_name: String::new(),
        replied_to_username: String::new(),
        replied_to_text: String::new(),
        replied_to_chat_type: String::new(),
        chatted_at: 1_700_000_000_000,
        embedded: false,
        from_platform: 0,
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
    }
}

fn recap_settings() -> RecapOpenAiConfig {
    RecapOpenAiConfig {
        primary_model: "recap-primary".to_owned(),
        primary_backups: Vec::new(),
        condensed_model: "condensed-primary".to_owned(),
        condensed_backups: Vec::new(),
        check_model: None,
        check_backups: Vec::new(),
        token_limit: 4_096,
        recap_reserve: 2_000,
        summary_language: "Traditional Chinese".to_owned(),
        force_check_failure: false,
        force_condensed_primary_failure: false,
        verbose_payload_logs: false,
    }
}

fn openai_client(server: &MockServer, recap: &RecapOpenAiConfig) -> OpenAiClient {
    OpenAiClient::new(
        &OpenAiSettings {
            api_key: "recap-generation-test-key".to_owned(),
            api_base: Some(format!("{}/v1", server.uri())),
            model: "unused".to_owned(),
            token_limit: Some(4_096),
            recap_token_limit: Some(2_000),
        },
        recap,
        &CondensedPromptConfig {
            system_prompt: None,
            user_prompt: None,
        },
    )
    .expect("OpenAI test client")
    .with_rate_limiter(Arc::new(GoRateLimiter::per_second(1_000)))
}

#[test]
fn rich_prompt_allocates_a_second_virtual_id_for_reply_targets() {
    let mut first = history(101, "first");
    first.full_name = "Alice".to_owned();
    first.replied_to_message_id = 50;
    first.replied_to_full_name = "Original".to_owned();

    let mut second = history(102, "second");
    second.full_name = "Bob".to_owned();

    let (prompt, mapping) = build_rich_recap_prompt(&[first, second]);

    assert_eq!(
        prompt,
        "msgId:1: Alice replying to [Original sent msgId:2]: first\n\
         msgId:3: Bob sent: second"
    );
    assert_eq!(
        mapping,
        HashMap::from([(1_i64, 101_i64), (2, 50), (3, 102)])
    );
}

#[test]
fn rich_prompt_preserves_go_name_selection_and_empty_text_rows() {
    let mut long_name = history(1, "");
    long_name.full_name = "一二三四五六七八九十".to_owned();
    long_name.username = "preferred_username".to_owned();

    let mut short_name = history(2, "text");
    short_name.full_name = "A#B##".to_owned();
    short_name.username = "ignored".to_owned();

    let (prompt, _) = build_rich_recap_prompt(&[long_name, short_name]);

    assert_eq!(
        prompt,
        "msgId:1: preferred_username sent: \nmsgId:2: AB sent: text"
    );
}

#[test]
fn condensed_history_skips_empty_text_without_renumbering_following_rows() {
    let empty = history(1, "");
    let mut username_fallback = history(2, "hi");
    username_fallback.username = "bob".to_owned();
    let unknown = history(3, "yo");

    assert_eq!(
        build_condensed_history(&[empty, username_fallback, unknown]),
        "[2] bob: hi\n[3] 未知用戶: yo\n"
    );
}

fn forwarded_history(
    message_id: i64,
    actor_display_name: &str,
    actor_username: &str,
    text: &str,
) -> PrivateForwardedReplayChatHistory {
    PrivateForwardedReplayChatHistory {
        chat_id: 42,
        chat_type: "private".to_owned(),
        chat_title: "Ada".to_owned(),
        message_id,
        actor_id: 0,
        actor_username: actor_username.to_owned(),
        actor_display_name: actor_display_name.to_owned(),
        text: text.to_owned(),
        chatted_at: 1_700_000_000_000 + message_id,
    }
}

#[test]
fn private_forwarded_prompt_uses_real_message_ids_and_go_name_selection() {
    let histories = [
        forwarded_history(91, "一二三四五六七八九十", "long_name", "first"),
        forwarded_history(17, "A#B##", "ignored", "second"),
        forwarded_history(105, "", "orphan_username", "third"),
    ];

    assert_eq!(
        build_private_forwarded_recap_prompt(&histories),
        "msgId:91: long_name sent: first\n\
         msgId:17: AB sent: second\n\
         msgId:105:  sent: third"
    );
}

#[tokio::test]
async fn private_forwarded_generation_removes_references_and_persists_go_log_shape() -> Result<()> {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let server = MockServer::start().await;
    let recap = recap_settings();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({ "model": "recap-primary" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-private-forwarded",
            "object": "chat.completion",
            "created": 0,
            "model": "resolved-forwarded-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "<b>Forwarded</b> {{tg-ref:91}}"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 8,
                "completion_tokens": 3,
                "total_tokens": 11
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let service =
        RecapGenerationService::new(database.clone(), openai_client(&server, &recap), &recap)?;
    let histories = [forwarded_history(91, "Ada", "ada", "hello")];

    let generated = service
        .summarize_private_forwarded_histories(42, &histories)
        .await?;

    assert_eq!(generated.recap_inputs, "msgId:91: Ada sent: hello");
    assert_eq!(generated.summaries, ["Forwarded"]);
    assert_eq!(
        generated.usage,
        TokenUsage {
            prompt_tokens: 8,
            completion_tokens: 3,
            total_tokens: 11,
        }
    );
    assert_eq!(
        generated.trace.generation.primary_used_model,
        "resolved-forwarded-model"
    );

    let row = sqlx::query(
        "SELECT chat_id, recap_inputs, recap_outputs, recap_type, model_name, \
         prompt_token_usage, completion_token_usage, total_token_usage \
         FROM log_chat_histories_recaps",
    )
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(row.try_get::<i64, _>(0)?, 42);
    assert_eq!(row.try_get::<String, _>(1)?, generated.recap_inputs);
    assert_eq!(row.try_get::<String, _>(2)?, "Forwarded");
    assert_eq!(row.try_get::<i64, _>(3)?, 1);
    assert_eq!(row.try_get::<String, _>(4)?, "");
    assert_eq!(row.try_get::<i64, _>(5)?, 8);
    assert_eq!(row.try_get::<i64, _>(6)?, 3);
    assert_eq!(row.try_get::<i64, _>(7)?, 11);

    Ok(())
}

#[test]
fn recap_trace_merge_preserves_go_composite_backup_list_quirk_and_ors_flags() {
    let mut aggregate = RecapExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_model: "primary".to_owned(),
            primary_used_model: "resolved-a".to_owned(),
            primary_failed: true,
            primary_failure_reason: "first failure".to_owned(),
            ..Default::default()
        },
    };
    let incoming = RecapExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_model: "primary".to_owned(),
            primary_used_model: "resolved-b".to_owned(),
            primary_failure_reason: "first failure".to_owned(),
            backup_model: "backup-a, backup-b".to_owned(),
            backup_used: true,
            backup_used_model: "resolved-backup".to_owned(),
            backup_succeeded: true,
            ..Default::default()
        },
    };

    merge_recap_execution_trace(&mut aggregate, &incoming);
    merge_recap_execution_trace(&mut aggregate, &incoming);

    assert_eq!(aggregate.generation.primary_model, "primary");
    assert_eq!(
        aggregate.generation.primary_used_model,
        "resolved-a, resolved-b"
    );
    assert_eq!(aggregate.generation.primary_failure_reason, "first failure");
    assert!(aggregate.generation.primary_failed);
    assert!(aggregate.generation.backup_used);
    assert!(aggregate.generation.backup_succeeded);
    assert_eq!(
        aggregate.generation.backup_model,
        "backup-a, backup-b, backup-a, backup-b"
    );
    assert_eq!(aggregate.generation.backup_used_model, "resolved-backup");
}

#[test]
fn group_log_model_prefers_successful_backup_then_primary_then_configured() {
    let mut trace = RecapExecutionTrace::default();
    assert_eq!(
        resolved_group_model_name("configured", &trace),
        "configured"
    );

    trace.generation.primary_used_model = "resolved-primary".to_owned();
    assert_eq!(
        resolved_group_model_name("configured", &trace),
        "resolved-primary"
    );

    trace.generation.backup_used = true;
    trace.generation.backup_succeeded = true;
    trace.generation.backup_model = "configured-backup".to_owned();
    assert_eq!(
        resolved_group_model_name("configured", &trace),
        "configured-backup"
    );

    trace.generation.backup_used_model = "resolved-backup".to_owned();
    assert_eq!(
        resolved_group_model_name("configured", &trace),
        "resolved-backup"
    );
}

#[tokio::test]
async fn group_detailed_persistence_stores_raw_prompt_resolved_output_usage_and_model() -> Result<()>
{
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let trace = RecapExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_model: "configured".to_owned(),
            primary_failed: true,
            backup_model: "configured-backup".to_owned(),
            backup_used: true,
            backup_used_model: "resolved-backup".to_owned(),
            backup_succeeded: true,
            ..Default::default()
        },
    };
    let usage = TokenUsage {
        prompt_tokens: 11,
        completion_tokens: 22,
        total_tokens: 33,
    };

    let generated = persist_group_detailed_recap(
        &database,
        -100_456,
        "msgId:1: Alice sent: hi",
        vec!["topic one".to_owned(), "topic two".to_owned()],
        usage,
        trace.clone(),
        "configured",
    )
    .await?;

    let row = sqlx::query(
        "SELECT recap_inputs, recap_outputs, prompt_token_usage, completion_token_usage, \
         total_token_usage, model_name FROM log_chat_histories_recaps WHERE id = $1",
    )
    .bind(&generated.log_id)
    .fetch_one(&database.pool)
    .await?;

    assert_eq!(generated.recap_inputs, "msgId:1: Alice sent: hi");
    assert_eq!(generated.summaries, vec!["topic one", "topic two"]);
    assert_eq!(generated.usage, usage);
    assert_eq!(generated.trace, trace);
    assert_eq!(row.try_get::<String, _>(0)?, generated.recap_inputs);
    assert_eq!(row.try_get::<String, _>(1)?, "topic one\n\ntopic two");
    assert_eq!(row.try_get::<i64, _>(2)?, 11);
    assert_eq!(row.try_get::<i64, _>(3)?, 22);
    assert_eq!(row.try_get::<i64, _>(4)?, 33);
    assert_eq!(row.try_get::<String, _>(5)?, "resolved-backup");

    Ok(())
}

#[tokio::test]
async fn database_usage_recorder_writes_the_production_metric_shape() -> Result<()> {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let recorder = DatabaseTokenUsageRecorder::new(database.clone());

    recorder
        .record(
            "Summarize Chat Histories",
            TokenUsage {
                prompt_tokens: 101,
                completion_tokens: 202,
                total_tokens: 303,
            },
            "resolved-model",
        )
        .await?;

    let row = sqlx::query(
        "SELECT prompt_operation, prompt_token_usage, completion_token_usage, \
         total_token_usage, model_name FROM metric_open_ai_chat_completion_token_usages",
    )
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(row.try_get::<String, _>(0)?, "Summarize Chat Histories");
    assert_eq!(row.try_get::<i64, _>(1)?, 101);
    assert_eq!(row.try_get::<i64, _>(2)?, 202);
    assert_eq!(row.try_get::<i64, _>(3)?, 303);
    assert_eq!(row.try_get::<String, _>(4)?, "resolved-model");

    Ok(())
}

#[tokio::test]
async fn group_generation_sanitizes_before_resolving_controlled_references() -> Result<()> {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let server = MockServer::start().await;
    let recap = recap_settings();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({ "model": "recap-primary" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-recap-generation",
            "object": "chat.completion",
            "created": 0,
            "model": "resolved-primary",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "<b>Topic</b>\n- [external](https://evil.test) {{tg-ref:1}}"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut source = history(101, "hello");
    source.full_name = "Alice".to_owned();
    let service = RecapGenerationService::new(database, openai_client(&server, &recap), &recap)?
        .with_retry_delay(Duration::ZERO);

    let generated = service
        .summarize_group_histories(-100_456, "supergroup", &[source])
        .await?;

    assert_eq!(
        generated.summaries,
        vec!["Topic\n- external [1](https://t.me/c/456/101)"]
    );
    assert_eq!(
        generated.trace.generation.primary_used_model,
        "resolved-primary"
    );
    assert_eq!(
        generated.usage,
        TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        }
    );

    Ok(())
}

#[tokio::test]
async fn group_generation_retries_a_failed_slice_exactly_five_times() -> Result<()> {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.token_limit = 105;
    recap.recap_reserve = 100;
    let source = history(101, "a long enough message to create later token slices");
    let (prompt, _) = build_rich_recap_prompt(std::slice::from_ref(&source));
    let client = openai_client(&server, &recap);
    assert!(client.split_content_by_token_limit(&prompt, 5)?.len() > 1);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({ "model": "recap-primary" })))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {
                "message": "temporary failure",
                "type": "server_error",
                "param": null,
                "code": null
            }
        })))
        .expect(5)
        .mount(&server)
        .await;
    let service = RecapGenerationService::new(database.clone(), client, &recap)?
        .with_retry_delay(Duration::ZERO);

    let error = service
        .summarize_group_histories(-100_456, "supergroup", &[source])
        .await
        .expect_err("five failed attempts must abort the detailed recap");

    assert!(error.trace.generation.primary_failed);
    assert_eq!(error.trace.generation.primary_model, "recap-primary");
    let stored_count = sqlx::query("SELECT COUNT(*) FROM log_chat_histories_recaps")
        .fetch_one(&database.pool)
        .await?
        .try_get::<i64, _>(0)?;
    assert_eq!(stored_count, 0);
    server.verify().await;

    Ok(())
}

#[tokio::test]
async fn group_generation_aggregates_every_successful_token_slice() -> Result<()> {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let server = MockServer::start().await;
    let mut recap = recap_settings();
    recap.token_limit = 105;
    recap.recap_reserve = 100;
    let source = history(101, "a long enough message to require several token slices");
    let (prompt, _) = build_rich_recap_prompt(std::slice::from_ref(&source));
    let client = openai_client(&server, &recap)
        .with_token_usage_recorder(Arc::new(DatabaseTokenUsageRecorder::new(database.clone())));
    let slice_count = client.split_content_by_token_limit(&prompt, 5)?.len();
    assert!(slice_count > 1);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({ "model": "recap-primary" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-recap-slice",
            "object": "chat.completion",
            "created": 0,
            "model": "resolved-primary",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "slice summary" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 2,
                "completion_tokens": 3,
                "total_tokens": 5
            }
        })))
        .expect(slice_count as u64)
        .mount(&server)
        .await;
    let service = RecapGenerationService::new(database.clone(), client, &recap)?
        .with_retry_delay(Duration::ZERO);

    let generated = service
        .summarize_group_histories(-100_456, "supergroup", &[source])
        .await?;

    assert_eq!(generated.summaries.len(), slice_count);
    assert!(
        generated
            .summaries
            .iter()
            .all(|summary| summary == "slice summary")
    );
    assert_eq!(
        generated.usage,
        TokenUsage {
            prompt_tokens: 2 * slice_count as i64,
            completion_tokens: 3 * slice_count as i64,
            total_tokens: 5 * slice_count as i64,
        }
    );
    assert_eq!(generated.trace.generation.primary_model, "recap-primary");
    assert_eq!(
        generated.trace.generation.primary_used_model,
        "resolved-primary"
    );
    let metric_count = sqlx::query(
        "SELECT COUNT(*) FROM metric_open_ai_chat_completion_token_usages \
         WHERE prompt_operation = $1 AND model_name = $2",
    )
    .bind("Summarize Chat Histories")
    .bind("resolved-primary")
    .fetch_one(&database.pool)
    .await?
    .try_get::<i64, _>(0)?;
    assert_eq!(metric_count, slice_count as i64);
    server.verify().await;

    Ok(())
}

#[tokio::test]
async fn condensed_generation_rejects_missing_or_textless_histories_without_http() -> Result<()> {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let server = MockServer::start().await;
    let recap = recap_settings();
    let service = RecapGenerationService::new(database, openai_client(&server, &recap), &recap)?;

    let missing = service
        .generate_condensed(-100_456, &[])
        .await
        .expect_err("empty histories must fail");
    assert_eq!(missing.source.to_string(), "no chat histories");
    assert_eq!(missing.trace, Default::default());

    let textless = service
        .generate_condensed(-100_456, &[history(101, "")])
        .await
        .expect_err("histories without text must fail");
    assert_eq!(textless.source.to_string(), "no chat history text");
    assert_eq!(textless.trace, Default::default());
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );

    Ok(())
}

#[tokio::test]
async fn condensed_generation_sends_the_go_numbered_transcript() -> Result<()> {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let server = MockServer::start().await;
    let recap = recap_settings();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({ "model": "condensed-primary" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-condensed-generation",
            "object": "chat.completion",
            "created": 0,
            "model": "resolved-condensed",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "**一行銳評**" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 4,
                "completion_tokens": 2,
                "total_tokens": 6
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = openai_client(&server, &recap)
        .with_token_usage_recorder(Arc::new(DatabaseTokenUsageRecorder::new(database.clone())));
    let service = RecapGenerationService::new(database.clone(), client, &recap)?;
    let first = history(100, "");
    let mut second = history(101, "hello");
    second.full_name = "Bob".to_owned();

    let generated = service
        .generate_condensed(-100_456, &[first, second])
        .await?;

    assert_eq!(generated.content, "**一行銳評**");
    assert_eq!(
        generated.trace.generation.primary_used_model,
        "resolved-condensed"
    );
    server.verify().await;
    let requests = server.received_requests().await.expect("recorded requests");
    let request: serde_json::Value = serde_json::from_slice(&requests[0].body)?;
    assert!(
        request["messages"][1]["content"]
            .as_str()
            .expect("condensed user prompt")
            .contains("[2] Bob: hello\n")
    );
    let metric = sqlx::query(
        "SELECT prompt_operation, prompt_token_usage, completion_token_usage, \
         total_token_usage, model_name \
         FROM metric_open_ai_chat_completion_token_usages",
    )
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(metric.try_get::<String, _>(0)?, "Sarcastic Condense");
    assert_eq!(metric.try_get::<i64, _>(1)?, 4);
    assert_eq!(metric.try_get::<i64, _>(2)?, 2);
    assert_eq!(metric.try_get::<i64, _>(3)?, 6);
    assert_eq!(metric.try_get::<String, _>(4)?, "resolved-condensed");

    Ok(())
}

#[tokio::test]
async fn group_generation_persists_a_log_when_sanitization_removes_every_summary() -> Result<()> {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let server = MockServer::start().await;
    let recap = recap_settings();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({ "model": "recap-primary" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-empty-after-sanitize",
            "object": "chat.completion",
            "created": 0,
            "model": "resolved-primary",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "<b></b>" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let service =
        RecapGenerationService::new(database.clone(), openai_client(&server, &recap), &recap)?;

    let generated = service
        .summarize_group_histories(-100_456, "supergroup", &[history(101, "hello")])
        .await?;

    assert!(generated.summaries.is_empty());
    let stored_output =
        sqlx::query("SELECT recap_outputs FROM log_chat_histories_recaps WHERE id = $1")
            .bind(&generated.log_id)
            .fetch_one(&database.pool)
            .await?
            .try_get::<String, _>(0)?;
    assert_eq!(stored_output, "");

    Ok(())
}

#[tokio::test]
async fn group_generation_returns_the_trace_when_recap_log_storage_fails() -> Result<()> {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    sqlx::query("DROP TABLE log_chat_histories_recaps")
        .execute(&database.pool)
        .await?;
    let server = MockServer::start().await;
    let recap = recap_settings();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({ "model": "recap-primary" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-storage-failure",
            "object": "chat.completion",
            "created": 0,
            "model": "resolved-primary",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "summary" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 7,
                "completion_tokens": 3,
                "total_tokens": 10
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let service = RecapGenerationService::new(database, openai_client(&server, &recap), &recap)?;

    let error = service
        .summarize_group_histories(-100_456, "supergroup", &[history(101, "hello")])
        .await
        .expect_err("recap log write failure must fail detailed generation");

    assert_eq!(error.trace.generation.primary_model, "recap-primary");
    assert_eq!(
        error.trace.generation.primary_used_model,
        "resolved-primary"
    );
    assert_eq!(
        error.usage,
        TokenUsage {
            prompt_tokens: 7,
            completion_tokens: 3,
            total_tokens: 10,
        }
    );
    assert!(
        error
            .source
            .to_string()
            .contains("log_chat_histories_recaps")
    );

    Ok(())
}
