use std::env;

use anyhow::{Context, Result, bail};
use teloxide::Bot;
use url::Url;

const OFFICIAL_TELEGRAM_API_ENDPOINT: &str = "https://api.telegram.org";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Locale {
    En,
    ZhHans,
    ZhHant,
}

impl Locale {
    fn from_lookup<F>(lookup: &F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        match lookup("INSIGHTS_LANG").as_deref() {
            Some("zh-Hans") => Self::ZhHans,
            Some("zh-Hant") => Self::ZhHant,
            _ => Self::En,
        }
    }

    pub fn from_env() -> Self {
        Self::from_lookup(&|key| env::var(key).ok())
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhHans => "zh-Hans",
            Self::ZhHant => "zh-Hant",
        }
    }
}

#[derive(Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub api_endpoint: String,
    pub webhook_url: Option<String>,
    pub webhook_port: Option<u16>,
}

impl TelegramConfig {
    pub fn bot(&self) -> Bot {
        let api_url = Url::parse(&self.api_endpoint)
            .expect("Telegram API endpoint was validated during configuration loading");
        Bot::new(&self.bot_token).set_api_url(api_url)
    }
}

#[derive(Clone)]
pub struct DbConfig {
    pub postgres_url: Option<String>,
    pub sqlite_file: Option<String>,
}

#[derive(Clone)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub api_base: Option<String>,
    pub model: String,
    pub token_limit: Option<u32>,
    #[allow(dead_code)]
    pub recap_token_limit: Option<u32>,
}

#[derive(Clone)]
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    pub tls_enabled: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    pub database: u32,
    pub client_cache_enabled: bool,
}

#[derive(Clone, Debug)]
pub struct RecapOpenAiConfig {
    pub primary_model: String,
    pub primary_backups: Vec<String>,
    pub condensed_model: String,
    pub condensed_backups: Vec<String>,
    pub check_model: Option<String>,
    pub check_backups: Vec<String>,
    pub token_limit: i64,
    pub recap_reserve: i64,
    pub summary_language: String,
    pub force_check_failure: bool,
    pub force_condensed_primary_failure: bool,
    pub verbose_payload_logs: bool,
}

#[derive(Clone)]
pub struct CondensedPromptConfig {
    pub system_prompt: Option<String>,
    pub user_prompt: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoRecapTestConfig {
    pub enabled: bool,
    pub chat_id: i64,
}

#[derive(Clone)]
pub struct AppConfig {
    pub locale: Locale,
    pub telegram: TelegramConfig,
    pub db: DbConfig,
    pub redis: RedisConfig,
    pub openai: OpenAiConfig,
    pub recap_openai: RecapOpenAiConfig,
    pub condensed_prompts: CondensedPromptConfig,
    pub manual_recap_rate_per_seconds: i64,
    pub timezone_shift_seconds: i64,
    pub auto_recap_test: AutoRecapTestConfig,
    pub log_level: String,
    pub log_file_path: Option<String>,
    pub locales_dir: String,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    pub fn from_lookup<F>(lookup: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let locale = Locale::from_lookup(&lookup);
        let telegram = TelegramConfig {
            bot_token: required(&lookup, "TELEGRAM_BOT_TOKEN")?,
            api_endpoint: telegram_api_endpoint(optional_non_empty(
                &lookup,
                "TELEGRAM_BOT_API_ENDPOINT",
            ))?,
            webhook_url: optional_non_empty(&lookup, "TELEGRAM_BOT_WEBHOOK_URL"),
            webhook_port: optional_non_empty(&lookup, "TELEGRAM_BOT_WEBHOOK_PORT")
                .and_then(|value| value.parse::<u16>().ok()),
        };

        let postgres_url = optional_non_empty(&lookup, "DATABASE_URL")
            .or_else(|| optional_non_empty(&lookup, "DB_CONNECTION_STR"));
        let db = DbConfig {
            sqlite_file: optional_non_empty(&lookup, "SQLITE_PATH").or_else(|| {
                if postgres_url.is_none() {
                    Some("data/dev.db".to_owned())
                } else {
                    None
                }
            }),
            postgres_url,
        };

        let redis = RedisConfig {
            host: optional_non_empty(&lookup, "REDIS_HOST")
                .unwrap_or_else(|| "localhost".to_owned()),
            port: redis_port(optional_non_empty(&lookup, "REDIS_PORT"))?,
            tls_enabled: bool_switch(&lookup, "REDIS_TLS_ENABLED"),
            username: optional_non_empty(&lookup, "REDIS_USERNAME"),
            password: optional_non_empty(&lookup, "REDIS_PASSWORD"),
            database: nonnegative_u32_or_zero(optional_non_empty(&lookup, "REDIS_DB")),
            client_cache_enabled: bool_switch(&lookup, "REDIS_CLIENT_CACHE_ENABLED"),
        };

        let api_key = match lookup("OPENAI_API_SECRET") {
            Some(value) => value,
            None => required(&lookup, "OPENAI_API_KEY")?,
        };
        if api_key.trim().is_empty() {
            bail!("OPENAI_API_SECRET is required");
        }

        let primary_model = optional_non_empty(&lookup, "OPENAI_API_MODEL_NAME")
            .unwrap_or_else(|| "gpt-3.5-turbo".to_owned());
        let primary_backups = normalized_backups(
            optional_non_empty(&lookup, "OPENAI_API_MODEL_NAME_backup"),
            &primary_model,
        );
        let condensed_model = optional_non_empty(&lookup, "SARCASTIC_CONDENSED_MODEL_NAME")
            .unwrap_or_else(|| primary_model.clone());
        let condensed_backups = normalized_backups(
            optional_non_empty(&lookup, "SARCASTIC_CONDENSED_MODEL_NAME_backup"),
            &condensed_model,
        );
        let check_model = optional_non_empty(&lookup, "CHECK_MODEL");
        let check_backups = check_model.as_ref().map_or_else(Vec::new, |primary| {
            normalized_backups(optional_non_empty(&lookup, "CHECK_MODEL_backup"), primary)
        });
        let token_limit =
            positive_i64_or_default(optional_non_empty(&lookup, "OPENAI_API_TOKEN_LIMIT"), 4096);
        let recap_reserve = positive_i64_or_default(
            optional_non_empty(&lookup, "OPENAI_API_CHAT_HISTORIES_RECAP_TOKEN_LIMIT"),
            2000,
        );
        if token_limit - recap_reserve <= 0 {
            bail!(
                "OPENAI_API_TOKEN_LIMIT minus OPENAI_API_CHAT_HISTORIES_RECAP_TOKEN_LIMIT must be strictly positive"
            );
        }
        let recap_openai = RecapOpenAiConfig {
            primary_model: primary_model.clone(),
            primary_backups,
            condensed_model,
            condensed_backups,
            check_model,
            check_backups,
            token_limit,
            recap_reserve,
            summary_language: optional_non_empty(&lookup, "CHAT_HISTORIES_SUMMARIZATION_LANGUAGE")
                .unwrap_or_else(|| "Simplified Chinese".to_owned()),
            force_check_failure: bool_switch(&lookup, "OPENAI_FORCE_CHECK_MODEL_FAILURE"),
            force_condensed_primary_failure: bool_switch(
                &lookup,
                "OPENAI_FORCE_CONDENSED_PRIMARY_FAILURE_FOR_TEST",
            ),
            verbose_payload_logs: bool_switch(&lookup, "OPENAI_VERBOSE_PAYLOAD_LOGS"),
        };
        let user_prompt = optional_non_empty(&lookup, "SARCASTIC_CONDENSED_USER_PROMPT");
        if let Some(template) = user_prompt.as_deref() {
            validate_go_template(template)?;
        }
        let condensed_prompts = CondensedPromptConfig {
            system_prompt: optional_non_empty(&lookup, "SARCASTIC_CONDENSED_SYSTEM_PROMPT"),
            user_prompt,
        };
        let openai = OpenAiConfig {
            api_key,
            api_base: openai_api_base(optional_non_empty(&lookup, "OPENAI_API_HOST")),
            model: primary_model,
            token_limit: u32::try_from(token_limit).ok(),
            recap_token_limit: u32::try_from(recap_reserve).ok(),
        };

        Ok(Self {
            locale,
            telegram,
            db,
            redis,
            openai,
            recap_openai,
            condensed_prompts,
            manual_recap_rate_per_seconds: nonnegative_i64_or_zero(optional_non_empty(
                &lookup,
                "HARD_LIMIT_MANUAL_RECAP_RATE_PER_SECONDS",
            )),
            timezone_shift_seconds: optional_non_empty(&lookup, "TIMEZONE_SHIFT_SECONDS")
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0),
            auto_recap_test: AutoRecapTestConfig {
                enabled: bool_switch(&lookup, "AUTO_RECAP_TEST_ENABLED"),
                chat_id: optional_non_empty(&lookup, "AUTO_RECAP_TEST_CHAT_ID")
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap_or(0),
            },
            log_level: optional_non_empty(&lookup, "LOG_LEVEL")
                .unwrap_or_else(|| "info".to_owned()),
            log_file_path: optional_non_empty(&lookup, "LOG_FILE_PATH"),
            locales_dir: optional_non_empty(&lookup, "LOCALES_DIR")
                .unwrap_or_else(|| "./locales".to_owned()),
        })
    }
}

fn required<F>(lookup: &F, key: &str) -> Result<String>
where
    F: Fn(&str) -> Option<String>,
{
    optional_non_empty(lookup, key).with_context(|| format!("{key} is required"))
}

fn optional_non_empty<F>(lookup: &F, key: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(key).filter(|value| !value.trim().is_empty())
}

fn bool_switch<F>(lookup: &F, key: &str) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    matches!(lookup(key).as_deref(), Some("true") | Some("1"))
}

fn redis_port(value: Option<String>) -> Result<u16> {
    let Some(value) = value else {
        bail!("REDIS_PORT is required and must be in 1..=65535");
    };
    let port = value
        .parse::<u16>()
        .with_context(|| "REDIS_PORT must be in 1..=65535")?;
    if port == 0 {
        bail!("REDIS_PORT must be in 1..=65535");
    }
    Ok(port)
}

fn nonnegative_u32_or_zero(value: Option<String>) -> u32 {
    value
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
}

fn nonnegative_i64_or_zero(value: Option<String>) -> i64 {
    value
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or(0)
}

fn positive_i64_or_default(value: Option<String>, default: i64) -> i64 {
    value
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn normalized_backups(raw: Option<String>, primary: &str) -> Vec<String> {
    let mut normalized = Vec::new();
    if let Some(raw) = raw {
        for candidate in raw
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if candidate != primary && !normalized.iter().any(|existing| existing == candidate) {
                normalized.push(candidate.to_owned());
            }
        }
    }
    normalized
}

fn telegram_api_endpoint(value: Option<String>) -> Result<String> {
    let endpoint = value.unwrap_or_else(|| OFFICIAL_TELEGRAM_API_ENDPOINT.to_owned());
    let normalized = endpoint.trim_end_matches('/');
    let parsed = Url::parse(normalized)
        .with_context(|| "TELEGRAM_BOT_API_ENDPOINT must be an absolute HTTP(S) base URL")?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("TELEGRAM_BOT_API_ENDPOINT must be an absolute HTTP(S) base URL");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("TELEGRAM_BOT_API_ENDPOINT must not contain a query or fragment");
    }
    Ok(normalized.to_owned())
}

fn openai_api_base(value: Option<String>) -> Option<String> {
    value.map(|value| {
        let normalized = value.trim_end_matches('/');
        if normalized.ends_with("/v1") {
            normalized.to_owned()
        } else {
            format!("{normalized}/v1")
        }
    })
}

fn validate_go_template(template: &str) -> Result<()> {
    let mut remainder = template;
    let mut controls = Vec::new();
    while let Some(open) = remainder.find("{{") {
        remainder = &remainder[open + 2..];
        let Some(close) = remainder.find("}}") else {
            bail!("SARCASTIC_CONDENSED_USER_PROMPT contains an unclosed template action");
        };
        let action = remainder[..close].trim();
        if action.is_empty() {
            bail!("SARCASTIC_CONDENSED_USER_PROMPT contains an empty template action");
        }
        match action.split_whitespace().next() {
            Some("if" | "range" | "with" | "define" | "block") => controls.push(action),
            Some("end") if controls.pop().is_none() => {
                bail!("SARCASTIC_CONDENSED_USER_PROMPT contains an unexpected template end")
            }
            Some("else") if controls.is_empty() => {
                bail!("SARCASTIC_CONDENSED_USER_PROMPT contains an unexpected template else")
            }
            _ => {}
        }
        remainder = &remainder[close + 2..];
    }
    if remainder.contains("}}") {
        bail!("SARCASTIC_CONDENSED_USER_PROMPT contains an unexpected template close")
    }
    if !controls.is_empty() {
        bail!("SARCASTIC_CONDENSED_USER_PROMPT contains an unclosed template control")
    }
    Ok(())
}
