use std::collections::BTreeMap;

use insights_bot_telegram_rs::config::AppConfig;
use teloxide::prelude::*;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

#[tokio::test]
async fn teloxide_bot_preserves_the_configured_endpoint_path_prefix() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"ok":true,"result":{"id":1,"is_bot":true,"first_name":"Test Bot","can_join_groups":true,"can_read_all_group_messages":false,"supports_inline_queries":false}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let values = BTreeMap::from([
        ("TELEGRAM_BOT_TOKEN".to_owned(), "test-token".to_owned()),
        (
            "TELEGRAM_BOT_API_ENDPOINT".to_owned(),
            format!("{}/telegram-proxy/", server.uri()),
        ),
        (
            "OPENAI_API_SECRET".to_owned(),
            "test-openai-secret".to_owned(),
        ),
        ("REDIS_PORT".to_owned(), "6379".to_owned()),
    ]);
    let config =
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("configuration should load");

    config
        .telegram
        .bot()
        .get_me()
        .send()
        .await
        .expect("teloxide should call the prefixed endpoint");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain the Telegram request");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.path(),
        "/telegram-proxy/bottest-token/GetMe"
    );
}
