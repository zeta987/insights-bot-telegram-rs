use std::collections::BTreeMap;

use insights_bot_telegram_rs::config::AppConfig;

fn config(values: &[(&str, &str)]) -> anyhow::Result<AppConfig> {
    let values = values
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect::<BTreeMap<_, _>>();
    AppConfig::from_lookup(|key| values.get(key).cloned())
}

fn required_values() -> Vec<(&'static str, &'static str)> {
    vec![
        ("TELEGRAM_BOT_TOKEN", "test-token"),
        ("OPENAI_API_SECRET", "canonical-test-secret"),
        ("REDIS_PORT", "6379"),
    ]
}

#[test]
fn config_parses_every_recap_variable_with_exact_casing() {
    let mut values = required_values();
    values.extend([
        ("TELEGRAM_BOT_API_ENDPOINT", "https://telegram.example///"),
        ("REDIS_HOST", "cache.internal"),
        ("REDIS_PORT", "6380"),
        ("REDIS_TLS_ENABLED", "true"),
        ("REDIS_USERNAME", "recap-user"),
        ("REDIS_PASSWORD", "recap-password"),
        ("REDIS_DB", "4"),
        ("REDIS_CLIENT_CACHE_ENABLED", "1"),
        ("HARD_LIMIT_MANUAL_RECAP_RATE_PER_SECONDS", "300"),
        ("OPENAI_API_HOST", "https://openai.example/"),
        ("OPENAI_API_MODEL_NAME", "detailed-primary"),
        (
            "OPENAI_API_MODEL_NAME_backup",
            " detailed-primary, backup-a, , backup-b, backup-a ",
        ),
        ("OPENAI_API_TOKEN_LIMIT", "8192"),
        ("OPENAI_API_CHAT_HISTORIES_RECAP_TOKEN_LIMIT", "2048"),
        ("SARCASTIC_CONDENSED_MODEL_NAME", "condensed-primary"),
        (
            "SARCASTIC_CONDENSED_MODEL_NAME_backup",
            "condensed-primary, condensed-backup, condensed-backup",
        ),
        ("SARCASTIC_CONDENSED_SYSTEM_PROMPT", "custom system"),
        (
            "SARCASTIC_CONDENSED_USER_PROMPT",
            "History: {{ .ChatHistory }}",
        ),
        ("CHECK_MODEL", "check-primary"),
        (
            "CHECK_MODEL_backup",
            "check-primary, check-backup, check-backup",
        ),
        (
            "CHAT_HISTORIES_SUMMARIZATION_LANGUAGE",
            "Traditional Chinese",
        ),
        ("OPENAI_FORCE_CHECK_MODEL_FAILURE", "true"),
        ("OPENAI_FORCE_CONDENSED_PRIMARY_FAILURE_FOR_TEST", "1"),
        ("OPENAI_VERBOSE_PAYLOAD_LOGS", "true"),
        ("TIMEZONE_SHIFT_SECONDS", "28800"),
        ("AUTO_RECAP_TEST_ENABLED", "1"),
        ("AUTO_RECAP_TEST_CHAT_ID", "-100123"),
    ]);

    let cfg = config(&values).expect("configuration should load");

    assert_eq!(cfg.telegram.api_endpoint, "https://telegram.example");
    assert_eq!(cfg.redis.host, "cache.internal");
    assert_eq!(cfg.redis.port, 6380);
    assert!(cfg.redis.tls_enabled);
    assert_eq!(cfg.redis.username.as_deref(), Some("recap-user"));
    assert_eq!(cfg.redis.password.as_deref(), Some("recap-password"));
    assert_eq!(cfg.redis.database, 4);
    assert!(cfg.redis.client_cache_enabled);
    assert_eq!(cfg.manual_recap_rate_per_seconds, 300);
    assert_eq!(cfg.openai.api_key, "canonical-test-secret");
    assert_eq!(
        cfg.openai.api_base.as_deref(),
        Some("https://openai.example/v1")
    );
    assert_eq!(cfg.recap_openai.primary_model, "detailed-primary");
    assert_eq!(cfg.recap_openai.primary_backups, ["backup-a", "backup-b"]);
    assert_eq!(cfg.recap_openai.condensed_model, "condensed-primary");
    assert_eq!(cfg.recap_openai.condensed_backups, ["condensed-backup"]);
    assert_eq!(
        cfg.recap_openai.check_model.as_deref(),
        Some("check-primary")
    );
    assert_eq!(cfg.recap_openai.check_backups, ["check-backup"]);
    assert_eq!(cfg.recap_openai.token_limit, 8192);
    assert_eq!(cfg.recap_openai.recap_reserve, 2048);
    assert_eq!(cfg.recap_openai.summary_language, "Traditional Chinese");
    assert!(cfg.recap_openai.force_check_failure);
    assert!(cfg.recap_openai.force_condensed_primary_failure);
    assert!(cfg.recap_openai.verbose_payload_logs);
    assert_eq!(cfg.timezone_shift_seconds, 28800);
    assert!(cfg.auto_recap_test.enabled);
    assert_eq!(cfg.auto_recap_test.chat_id, -100123);
}

#[test]
fn config_uses_canonical_secret_and_go_defaults() {
    let mut values = required_values();
    values.extend([("OPENAI_API_KEY", "legacy-test-key"), ("REDIS_HOST", "")]);

    let cfg = config(&values).expect("configuration should load");

    assert_eq!(cfg.openai.api_key, "canonical-test-secret");
    assert_eq!(cfg.telegram.api_endpoint, "https://api.telegram.org");
    assert_eq!(cfg.redis.host, "localhost");
    assert_eq!(cfg.recap_openai.primary_model, "gpt-3.5-turbo");
    assert_eq!(cfg.recap_openai.condensed_model, "gpt-3.5-turbo");
    assert_eq!(cfg.recap_openai.token_limit, 4096);
    assert_eq!(cfg.recap_openai.recap_reserve, 2000);
    assert_eq!(cfg.recap_openai.summary_language, "Simplified Chinese");
}

#[test]
fn config_uses_legacy_secret_only_when_canonical_secret_is_absent() {
    let cfg = config(&[
        ("TELEGRAM_BOT_TOKEN", "test-token"),
        ("OPENAI_API_KEY", "legacy-test-key"),
        ("REDIS_PORT", "6379"),
    ])
    .expect("legacy secret alias should remain supported");

    assert_eq!(cfg.openai.api_key, "legacy-test-key");
}

#[test]
fn config_normalizes_backups_and_disables_check_backups_without_check_model() {
    let mut values = required_values();
    values.extend([
        ("OPENAI_API_MODEL_NAME", "primary"),
        ("OPENAI_API_MODEL_NAME_backup", "primary, next, next, final"),
        ("CHECK_MODEL_backup", "unused-check"),
    ]);

    let cfg = config(&values).expect("configuration should load");

    assert_eq!(cfg.recap_openai.primary_backups, ["next", "final"]);
    assert_eq!(cfg.recap_openai.check_model, None);
    assert!(cfg.recap_openai.check_backups.is_empty());
}

#[test]
fn config_accepts_only_true_and_one_for_boolean_switches() {
    for (raw, expected) in [("true", true), ("1", true), ("TRUE", false), ("yes", false)] {
        let mut values = required_values();
        values.extend([
            ("REDIS_TLS_ENABLED", raw),
            ("REDIS_CLIENT_CACHE_ENABLED", raw),
            ("OPENAI_FORCE_CHECK_MODEL_FAILURE", raw),
            ("OPENAI_FORCE_CONDENSED_PRIMARY_FAILURE_FOR_TEST", raw),
            ("OPENAI_VERBOSE_PAYLOAD_LOGS", raw),
            ("AUTO_RECAP_TEST_ENABLED", raw),
        ]);

        let cfg = config(&values).expect("configuration should load");
        assert_eq!(cfg.redis.tls_enabled, expected, "REDIS_TLS_ENABLED={raw}");
        assert_eq!(
            cfg.redis.client_cache_enabled, expected,
            "REDIS_CLIENT_CACHE_ENABLED={raw}"
        );
        assert_eq!(cfg.recap_openai.force_check_failure, expected);
        assert_eq!(cfg.recap_openai.force_condensed_primary_failure, expected);
        assert_eq!(cfg.recap_openai.verbose_payload_logs, expected);
        assert_eq!(cfg.auto_recap_test.enabled, expected);
    }
}

#[test]
fn config_falls_back_for_invalid_redis_database_and_manual_interval() {
    for raw in ["", "broken", "-1"] {
        let mut values = required_values();
        values.extend([
            ("REDIS_DB", raw),
            ("HARD_LIMIT_MANUAL_RECAP_RATE_PER_SECONDS", raw),
        ]);
        let cfg = config(&values).expect("invalid values should be bounded");
        assert_eq!(cfg.redis.database, 0, "REDIS_DB={raw}");
        assert_eq!(
            cfg.manual_recap_rate_per_seconds, 0,
            "manual interval={raw}"
        );
    }
}

#[test]
fn config_rejects_invalid_redis_ports() {
    for raw in ["", "0", "broken", "65536"] {
        let mut values = required_values();
        values.push(("REDIS_PORT", raw));
        let err = config(&values)
            .err()
            .expect("invalid Redis port must be rejected");
        assert!(err.to_string().contains("REDIS_PORT"), "REDIS_PORT={raw}");
    }

    let err = config(&[
        ("TELEGRAM_BOT_TOKEN", "test-token"),
        ("OPENAI_API_SECRET", "canonical-test-secret"),
    ])
    .err()
    .expect("a missing Redis port must be rejected");
    assert!(err.to_string().contains("REDIS_PORT"));
}

#[test]
fn config_rejects_non_positive_detailed_input_budget() {
    let mut values = required_values();
    values.extend([
        ("OPENAI_API_TOKEN_LIMIT", "2000"),
        ("OPENAI_API_CHAT_HISTORIES_RECAP_TOKEN_LIMIT", "2000"),
    ]);

    let err = config(&values)
        .err()
        .expect("zero detailed input budget must be rejected");
    assert!(err.to_string().contains("strictly positive"));
}

#[test]
fn config_rejects_malformed_condensed_user_template() {
    let mut values = required_values();
    values.push(("SARCASTIC_CONDENSED_USER_PROMPT", "{{ .ChatHistory"));

    let err = config(&values)
        .err()
        .expect("malformed template must be rejected");
    assert!(err.to_string().contains("SARCASTIC_CONDENSED_USER_PROMPT"));
}

#[test]
fn config_rejects_invalid_telegram_api_endpoints() {
    for raw in ["telegram.example", "ftp://telegram.example", "https://"] {
        let mut values = required_values();
        values.push(("TELEGRAM_BOT_API_ENDPOINT", raw));
        let err = config(&values)
            .err()
            .expect("endpoint must be an absolute HTTP base URL");
        assert!(err.to_string().contains("TELEGRAM_BOT_API_ENDPOINT"));
    }
}
