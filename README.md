# insights-bot-telegram-rs

Telegram-only rewrite of the recap bot inspired by the original Go implementation [`insights-bot`](https://github.com/nekomeowww/insights-bot) (MIT). This Rust version focuses on recap features and drops Slack/Discord and `/smr` web crawling.

## Features (parity target from Go version)
- `/start`, `/help`, `/cancel`
- `/recap`: summarize recent group messages
- `/configure_recap`: configure recap enablement and auto-recap delivery
- Auto recap worker: periodic recap per group based on config
- Callback handling for recap configuration

## Tech stack
- Runtime: `tokio`
- Telegram: `teloxide` (rustls)
- DB: `sqlx::AnyPool` (Postgres preferred, fallback SQLite)
- OpenAI client: `async-openai` (recap logic stubbed; ready for integration)
- Rate limiting: in-crate Go-parity leaky bucket (`GoRateLimiter`) plus the Redis manual-recap counter
- Logging: `tracing` / `tracing-subscriber`
- i18n: YAML bundles via `serde_yaml`

## Setup
1. Copy `.env` from project template and set:
   - `TELEGRAM_BOT_TOKEN`
   - `REDIS_PORT` (`1..=65535`; Redis host defaults to `localhost`)
   - `DATABASE_URL` (Postgres) or `SQLITE_PATH` (fallback)
   - `OPENAI_API_SECRET` (or `OPENAI_API_KEY`)
   - `INSIGHTS_LANG` (`en` / `zh-Hans` / `zh-Hant`)
   - `LOCALES_DIR` (defaults `./locales`)
2. Run:
   ```bash
   cargo fmt
   cargo check
   ```
3. (Optional) create DB schema via `sqlx` migrations (not included yet).

### Recap parity configuration

`OPENAI_API_SECRET` is canonical; `OPENAI_API_KEY` is used only when the
canonical variable is absent. Detailed models default to `gpt-3.5-turbo`, total
recap tokens default to `4096`, recap reserve defaults to `2000`, and summary
language defaults to `Simplified Chinese`. The total minus reserve must be
strictly positive. Backup-model variables are comma-separated, trim empty
entries, preserve order, remove duplicates and the primary model; Check backups
are ignored when `CHECK_MODEL` is unset.

`REDIS_PORT` is mandatory and accepts only `1..=65535`. Missing or empty Redis
hosts normalize to `localhost`; TLS and client-cache switches accept exactly
`true` or `1`; an invalid or negative Redis DB selects database `0`. The manual
recap interval is in seconds and invalid or negative values become `0`.

`TELEGRAM_BOT_API_ENDPOINT` defaults to `https://api.telegram.org`; a custom
endpoint must be an absolute HTTP(S) base URL and trailing slashes are removed.
The same base is used for ordinary teloxide calls and Rich Message transport.
`SARCASTIC_CONDENSED_USER_PROMPT` is validated at startup with the supported
Go-template syntax used by recap, including `{{ .ChatHistory }}`, `printf`,
and comments. `AUTO_RECAP_TEST_ENABLED=true` or `1` with a nonzero
`AUTO_RECAP_TEST_CHAT_ID` schedules one immediate automatic recap in addition
to the normal schedule.

## Database behavior
- Attempts Postgres first; on failure logs warning and connects to SQLite (`sqlite://{SQLITE_PATH}`).
- Models prepared for chat histories, recap configs/subscriptions, recap logs.

### Running migrations
- Postgres: `DATABASE_URL=postgres://... sqlx migrate run`
- SQLite: `DATABASE_URL=sqlite://data/dev.db sqlx migrate run`
- This service only supports the Rust schema defined in `migrations/postgres/0001_init.sql` and `migrations/sqlite/0001_init.sql`.
- Existing PostgreSQL databases created by the Go bot are rejected at startup and must be migrated into the Rust schema before use.

### Message recording
- Middleware records incoming group text/caption messages into `chat_histories` for recap generation.

### Important: Group Permissions
For the bot to receive and record all messages in a group (not just commands), you must do **ONE** of the following:
1. **Disable Privacy Mode** (recommended): Contact [@BotFather](https://t.me/BotFather), send `/setprivacy`, select your bot, then choose `Disable`.
2. **Make the bot a group admin**: Add the bot as an administrator in the group settings.

Without this, the bot will only receive messages that directly mention it (e.g., `/recap@your_bot`) or are replies to the bot's messages.

### Recap configuration
- `/configure_recap` shows inline buttons to toggle recap on/off and auto recap on/off; settings are stored in `recap_configs`.

### Auto recap
- Background worker finds chats due for auto recap on a fixed 6-hour cadence, generates recap, sends it to the originating group, then updates `last_recap_at`.

### Health & lifecycle

Startup order mirrors Go: the `/health` listener binds first (`HEALTH_HTTP_PORT`,
default `7069`, must be `1..=65535`), then a one-second pause, then the
Telegram dispatcher starts, then the automatic-recap poller is armed.

`/health` returns Go's composite readiness JSON instead of a bare `SELECT 1`:

```json
{
  "status": "up",
  "details": {
    "telegram_bot": { "status": "up" },
    "auto recap timecapsule digger": { "status": "up" },
    "auto_recap": { "status": "up" }
  }
}
```

Each named check flips from `down` to `up` exactly once (Telegram bot
authorized, the automatic-recap poller started, the automatic-recap
subsystem started) and never reverts; the aggregate `status` is `up` (HTTP
200) only once all three are up, and `down` (HTTP 503) otherwise.

On SIGINT (all platforms) or SIGTERM (Unix), the process shuts down in Go's
reverse startup order: the Telegram dispatcher stops, the database pool
closes, the `/health` HTTP server shuts down gracefully with a ten-second
timeout, then the automatic-recap poller stops.

### Migration from the Go bot

The Rust service does not run directly on the legacy Go PostgreSQL schema. Migration from the Go bot is a one-time transform into the Rust-owned schema.

#### Retained domains
- `telegram_chats` can be mapped into `chats`.
- Group `chat_histories` can be transformed into the Rust `chat_histories` table.
- Recap enablement and auto recap enablement can be derived from `telegram_chat_feature_flags` and `telegram_chat_recaps_options` and stored in `recap_configs`.
- `last_recap_at` can be carried forward into `recap_configs` when the legacy value is safe to reuse.
- `log_chat_histories_recaps` can be imported into `recap_logs` on a best-effort basis by mapping supported fields only.

#### Dropped domains
- Private recap subscriptions from `telegram_chat_auto_recaps_subscribers`
- Forwarded recap state stored outside PostgreSQL in the Go bot
- Recap feedback reactions
- Sent-message pin tracking
- Legacy recap delivery mode and per-chat recap frequency selection

#### One-time migration order
1. Stop the Go bot and take a database snapshot.
2. Start with a fresh database for the Rust service and let the Rust migrations create the supported schema.
3. Export retained entities from the Go database.
4. Transform Go records into the Rust table shapes:
   - `telegram_chats` -> `chats`
   - supported group `chat_histories` -> `chat_histories`
   - feature flags and recap options -> `recap_configs`
   - optional recap logs -> `recap_logs`
5. Skip the dropped domains listed above.
6. Start the Rust service against the migrated Rust schema and verify recap generation before decommissioning the Go deployment.

### Known warnings
- Webhook configuration and media/whisper placeholders remain outside the recap core scope.

### Rate limiting
- Public `/recap` follows Go's per-chat Redis counter (`HARD_LIMIT_MANUAL_RECAP_RATE_PER_SECONDS`,
  loosened per chat only by a larger stored override); exceeding it replies with Go's rate-limit
  notice. Automatic recap delivery is throttled at five sends per second.

## Status
- Bot/handlers/services/db scaffolding is complete.
- Recap generation fully functional with locale-aware prompts (en/zh-Hans/zh-Hant).
- Auto-migrations run on startup (no separate migration step needed).
- `/recap` now shows time selection buttons (1h, 2h, 4h, 6h, 12h, 24h) matching Go version.
- Processing indicator shown during recap generation.

## License
MIT. Based on MIT-licensed upstream `insights-bot`; this rewrite remains MIT-compatible.
