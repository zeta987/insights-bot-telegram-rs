# Go Rich Message Recap Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port every included Telegram Rich Message recap behavior from Go `v1.0.0` commit `02aee8ce260165592e2152eb5a024a602e4eced1` into Rust with equivalent Telegram, SQL, Redis, scheduling, generation, and failure behavior.

**Architecture:** Keep teloxide as the update dispatcher and SQLx as the database layer. Add an injected Redis state store, a raw Bot API `sendRichMessage` client, pure Rich Markdown composition modules, and a single delivery state machine shared by manual, forwarded, and automatic recaps. Replace the current Telegraph/HTML recap path only at the three recap delivery call sites; keep unrelated Telegram behavior outside this change.

**Tech Stack:** Rust 2024 on Rust 1.97.1, teloxide 0.12, SQLx 0.7, reqwest 0.12, redis 1.5 with Tokio and Rustls, async-trait 0.1, base64 0.23, sha2 0.11, tiktoken-rs 0.12, wiremock 0.6, PostgreSQL, SQLite, Redis, Telegram Bot API 10.1+.

## Global Constraints

- The binding specification is `docs/superpowers/specs/2026-08-14-rich-message-recap-parity-design.md`. It supersedes the older `openspec/specs/telegram-group-recap-core/spec.md` exclusions for Redis, subscriptions, forwarded recaps, feedback, private delivery, automatic fan-out, and sent-message tracking.
- The Go source repository is read-only. Do not edit it, stage it, commit it, push it, or read either repository's `.env` files.
- `/smr` generation and webpage summarization remain unavailable. Preserve only the explicitly included `smr/summarization/feedback/react` callback/table/edit compatibility path.
- Preserve all observable Go `v1.0.0` thresholds, wire values, TTLs, ordering, duplicate behavior, error branches, user-visible literals, and persistence effects. Preserve only the three documented Rust safety adapters for sensitive logging, invalid token budgets/templates, and missing options.
- Follow strict red-green-refactor. Add a behavior test first, run it, and record the expected failure before modifying matching production code. Tests assert literal outputs and real boundary effects rather than the behavior of mocks.
- Use `apply_patch` for every repository file edit. Use `sg` before `rg` for syntax-aware code searches. Never print secrets, Telegram token paths, production chat content, or complete upstream payloads.
- Keep PostgreSQL and SQLite migrations aligned. Add a new migration; do not rewrite migration files that may already be applied.
- Keep files focused. Rich content, Telegram transport, delivery, Redis state, repositories, handlers, and scheduler remain separate modules with narrow public interfaces.
- Each task is one module checkpoint. Before its commit: run focused tests, `cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`, `git diff --check`, and inspect `cargo llvm-cov` for changed recap modules. Changed recap modules must reach at least 95% line coverage.
- Before every commit, inspect exact staged paths and staged diff, scan tracked/staged content for credentials, API keys, bot tokens, private URLs, personal names, email addresses, phone numbers, and local absolute paths without displaying matched secret values, and confirm no `.env`, log, database, or Telegram capture is staged.
- Every task commit is a focused signed Conventional Commit with exactly one blank line before `Co-authored-by: Codex <noreply@openai.com>`. Verify `git verify-commit HEAD` and the exact trailer before pushing `feat/rich-recap-parity` to `origin`.
- Update only this task's rows in `docs/parity/go-v1.0.0-rich-recap-ledger.md`. A row becomes complete only after its characterization test, implementation, focused verification, security scan, signed commit, and push all succeed.
- One implementation agent works at a time. Every task receives a fresh implementation agent, a task-scoped spec/quality review, and fix-loop re-reviews before the next task begins.

---

### Task 1: Create the Go parity ledger

**Files:**

- Create: `docs/parity/go-v1.0.0-rich-recap-ledger.md`
- Read only: `docs/superpowers/specs/2026-08-14-rich-message-recap-parity-design.md`
- Read only: the included Go files named in the design's component mapping and persistence sections

**Required ledger schema:**

```markdown
| Go file and line | Go function or branch | Trigger and callback literal | Telegram effect | SQL effect | Redis effect | Rust symbol | Rust test | Status |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
```

- [ ] Enumerate all six recap commands, both `/start` continuations, `/cancel`, message/edit/chat-member/migration updates, all nine callback route literals, the three Rich delivery call sites, every Redis key lifecycle, and each SQL repository branch covered by the approved scope.
- [ ] Give every row a stable identifier such as `MANUAL-001`, `CALLBACK-001`, `FORWARDED-001`, `AUTO-001`, `DELIVERY-001`, `OPENAI-001`, `SQL-001`, and `CAPTURE-001` so later commits can update rows without renumbering them.
- [ ] Set every implementation status to `not-started`; use exact planned Rust symbols and exact planned test names from Tasks 2–14.
- [ ] Verify the ledger contains the Go delivery call sites `callback_query.go:757`, `recap_forwarded.go:126`, and `autorecap.go:374`, and contains no `/smr` production generation row.
- [ ] Run a structural documentation check that the table has nine columns and every row has a non-empty Go source, Rust symbol, Rust test, and status. Human prose needs no source-text unit test.
- [ ] Run the global formatting, compile, test, coverage-inspection, staged-diff, security, signature, trailer, and push gates.

**Commit:** `docs: add rich recap parity ledger`

### Task 2: Add exact configuration parsing and dependencies

**Files:**

- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/config.rs`
- Modify: `src/main.rs`
- Modify: `src/bot/context.rs`
- Modify: `.env.example`
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `tests/config_tests.rs`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

**Public configuration boundary:**

```rust
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    pub tls_enabled: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    pub database: u32,
    pub client_cache_enabled: bool,
}

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

impl AppConfig {
    pub fn from_lookup<F>(lookup: F) -> anyhow::Result<Self>
    where
        F: Fn(&str) -> Option<String>;
}
```

- [ ] Add dependencies with the verified major/minor versions from the plan header and enable Redis `connection-manager`, `cache-aio`, `tokio-rustls-comp`, and `tls-rustls-webpki-roots`. Add `wiremock` only as a development dependency.
- [ ] Add failing table-driven tests for every exact-case variable in the design, `OPENAI_API_SECRET` precedence over the legacy key alias, Go defaults, `true`/`1` parsing, negative/invalid Redis DB fallback, manual interval parsing, ordered backup normalization, primary removal, and Check backups disabled without Check.
- [ ] Add failing tests for `TELEGRAM_BOT_API_ENDPOINT` normalization, fixed timezone shift, immediate auto-recap test settings, strictly positive detailed input budget, and malformed custom condensed user templates.
- [ ] Implement `from_lookup` and make `from_env` delegate to it so tests do not mutate process-global environment state.
- [ ] Ensure `TelegramConfig` carries the parsed API endpoint and every teloxide `Bot` is created through one helper using `set_api_url`; never log the token-bearing method URL.
- [ ] Add construction seams to `AppContext` for Redis state and raw Telegram delivery without instantiating production Redis in unit tests; concrete construction occurs in Task 3.
- [ ] Add every new variable with non-secret example values to `.env.example` and document exact precedence/default behavior in both READMEs in the same module commit.
- [ ] Confirm no configuration debug output exposes passwords, API keys, tokens, custom prompt bodies, or webhook token paths.
- [ ] Run focused `cargo test --test config_tests`, then the global gates and push.

**Commit:** `feat: add recap parity configuration`

### Task 3: Implement Redis recap state and callback codecs

**Files:**

- Create: `src/redis/mod.rs`
- Create: `src/redis/keys.rs`
- Create: `src/redis/recap_state.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Modify: `src/bot/context.rs`
- Create: `tests/recap_redis_state_tests.rs`
- Create: `tests/support/mod.rs`
- Create: `tests/support/redis_fixture.rs`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

**State interface:**

```rust
#[async_trait::async_trait]
pub trait RecapStateStore: Send + Sync {
    async fn put_callback(
        &self,
        route: &str,
        payload_json: &str,
    ) -> anyhow::Result<String>;
    async fn get_callback(&self, route: &str, action_hash: &str)
        -> anyhow::Result<Option<String>>;
    async fn put_start_context(&self, domain: StartContextDomain, token: &str, json: &str)
        -> anyhow::Result<()>;
    async fn get_start_context(&self, domain: StartContextDomain, token: &str)
        -> anyhow::Result<Option<String>>;
    async fn forwarded_active(&self, user_id: i64) -> anyhow::Result<bool>;
    async fn start_forwarded(&self, user_id: i64) -> anyhow::Result<()>;
    async fn append_forwarded(&self, user_id: i64, score_ms: i64, json: &str)
        -> anyhow::Result<()>;
    async fn forwarded_batch(&self, user_id: i64) -> anyhow::Result<Vec<String>>;
    async fn cancel_forwarded(&self, user_id: i64) -> anyhow::Result<bool>;
    async fn push_delete_later(&self, user_id: i64, chat_id: i64, message_id: i32)
        -> anyhow::Result<()>;
    async fn drain_delete_later(&self, user_id: i64) -> anyhow::Result<Vec<(i64, i32)>>;
}
```

- [ ] Add failing tests for the exact callback route/action SHA-256 prefixes, `<route-hash>;<action-hash>` wire value, literal-route Redis key, JSON storage, 86,400-second TTL, GET-only reuse, malformed wire, unknown route hash, known route without handler, and expired payload returning an empty handler payload. The registered literals are `recap/select-hour`, `recap/configure/toggle`, `recap/configure/assign_mode`, `recap/configure/complete`, `recap/unsubscribe_recap`, `recap/recap/feedback/react`, `recap/configure/auto_recap_rates_per_day`, `recap/configure/pin`, and `smr/summarization/feedback/react`.
- [ ] Add failing tests for first-eight-lowercase-hex SHA-256 over `recap/private_subscription_mode/start_command_context/<decimal-chat-id>` and `recap/subscribe_recap/start_command_context/<decimal-chat-id>`, their distinct Redis domains, 86,400-second reusable GET-only state, and no TTL refresh.
- [ ] Add failing tests for forwarded control/batch keys, `1` control value, 7,200-second TTL refresh, JSON ZSET members, Unix-millisecond scores, `ZREVRANGE` plus reversal ordering, restart deletion, active-only cancellation, and success retention.
- [ ] Add failing tests for the delete-later `LPUSH` member `<chat-id>;<message-id>`, per-user shared list, TTL refresh, delete-before-Telegram semantics, and malformed member filtering.
- [ ] Implement the production Redis connection from `RedisConfig`, an in-memory test double with Redis-equivalent ordering/TTL semantics, and an isolated real-Redis fixture for codec/lifecycle integration tests. The fixture must never read a production Redis address.
- [ ] Keep automatic queue methods out of this trait until Task 13 so handler state can be reviewed independently.
- [ ] Run focused `cargo test --test recap_redis_state_tests`, then the global gates and push.

**Commit:** `feat: add recap Redis state`

### Task 4: Add recap-domain SQL migrations and repositories

**Files:**

- Create: `migrations/postgres/0003_rich_recap_parity.sql`
- Create: `migrations/sqlite/0003_rich_recap_parity.sql`
- Modify: `src/db/mod.rs`
- Modify: `src/db/models.rs`
- Modify: `src/db/chat_history.rs`
- Modify: `src/db/recap_config.rs`
- Modify: `src/db/logs.rs`
- Modify: `src/db/migration.rs`
- Create: `src/db/chats.rs`
- Create: `src/db/feature_flags.rs`
- Create: `src/db/recap_options.rs`
- Create: `src/db/subscribers.rs`
- Create: `src/db/feedback.rs`
- Create: `src/db/sent_messages.rs`
- Create: `src/db/usage_metrics.rs`
- Create: `tests/recap_persistence_tests.rs`
- Create: `tests/support/sqlite_fixture.rs`
- Update: `tests/recap_scope_tests.rs`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

**Repository contracts:**

```rust
pub enum AutoRecapSendMode { Public = 0, PrivateSubscribers = 1 }

pub struct RecapOptions {
    pub chat_id: i64,
    pub auto_recap_send_mode: i32,
    pub manual_recap_rate_per_seconds: i64,
    pub auto_recap_rates_per_day: i32,
    pub pin_auto_recap_message: bool,
}

pub struct SentTelegramMessage {
    pub id: uuid::Uuid,
    pub chat_id: i64,
    pub message_id: i64,
    pub text: String,
    pub is_pinned: bool,
    pub from_platform: i32,
    pub message_type: i32,
    pub created_at: i64,
    pub updated_at: i64,
}
```

- [ ] Add failing SQLite migration tests for all required columns/tables, defaults, indexes, nullable fields, and the intentional absence of uniqueness/foreign-key constraints on subscribers, both reaction tables, and `(chat_id, message_id)` sent-message pairs.
- [ ] Add a PostgreSQL schema test path that runs against an explicitly isolated test URL; keep the SQL statements aligned even when PostgreSQL is unavailable locally.
- [ ] Extend chat histories with chat type/title, reply snapshot fields, millisecond `chatted_at`, platform, and embedding state. Characterize caption/reply consumers through repository DTOs rather than nullable SQLx `Any` surprises.
- [ ] Replace the old combined config behavior with feature-flag and options repositories. Schema daily rate defaults to `0`; only first-enable materialization writes public mode, `4` per day, and pin false.
- [ ] Implement subscriber insertion precheck without a database uniqueness constraint, unordered/undeduplicated listing, idempotent sequential deletion, and explicit duplicate-row tests.
- [ ] Implement both non-transactional reaction repositories: delete same type to toggle off; otherwise delete all tuple rows then insert; preserve the no-reaction state when insert fails after deletion; count every duplicate row.
- [ ] Implement recap-log input/output/type/usage/model/sent-count fields, automatic-only sent-message inserts, last-pinned lookup by `created_at DESC`, bulk clearing by selected `(chat_id, message_id)`, and OpenAI usage metrics.
- [ ] Update the legacy PostgreSQL guard to detect incompatible Go schema shapes instead of rejecting table names that Rust now owns.
- [ ] Implement exact migration and bot-left subsets: never migrate chats, sent messages, reactions, metrics, or Redis; never delete retained log rows, sent messages, reactions, metrics, or Redis on bot-left.
- [ ] Run focused `cargo test --test recap_persistence_tests --test recap_scope_tests`, then the global gates and push.

**Commit:** `feat: add recap persistence domain`

### Task 5: Port prompt rows, sanitizers, references, and model trace

**Files:**

- Create: `src/services/rich_recap/mod.rs`
- Create: `src/services/rich_recap/prompt_rows.rs`
- Create: `src/services/rich_recap/sanitize.rs`
- Create: `src/services/rich_recap/references.rs`
- Create: `src/services/rich_recap/trace.rs`
- Modify: `src/services/mod.rs`
- Create: `tests/rich_recap_content_tests.rs`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

**Pure content interface:**

```rust
pub struct RichRecapReplySnapshot {
    pub message_id: i64,
    pub actor_full_name: String,
    pub actor_username: String,
    pub text: String,
}

pub struct RichRecapHistoryRow {
    pub message_id: i64,
    pub actor_full_name: String,
    pub actor_username: String,
    pub text: String,
    pub reply: Option<RichRecapReplySnapshot>,
}

pub fn build_group_detailed_rows(rows: &[RichRecapHistoryRow])
    -> (String, std::collections::BTreeMap<i64, i64>);
pub fn build_group_condensed_rows(rows: &[RichRecapHistoryRow]) -> String;
pub fn build_forwarded_detailed_rows(rows: &[RichRecapHistoryRow]) -> String;
pub fn build_forwarded_condensed_rows(rows: &[RichRecapHistoryRow]) -> String;
pub fn sanitize_detailed_recap_markdown(input: &str) -> String;
pub fn sanitize_condensed_recap_markdown(input: &str) -> String;
pub fn resolve_rich_recap_references(
    markdown: &str,
    chat_id: i64,
    chat_type: &str,
    virtual_to_real: &std::collections::BTreeMap<i64, i64>,
) -> String;
```

- [ ] Add failing literal tests for group virtual IDs, interleaved reply IDs, detailed actor-name transformation, condensed actor fallback, skipped empty rows without renumbering, forwarded real message IDs, and forwarded condensed actor formatting.
- [ ] Add failing sanitizer tests for HTML removal with visible text, inline link/image labels, bare URLs, fence delimiters, live mention prefixes versus email addresses, blockquotes, list markers, heading normalization, and detailed-only table-pipe escaping.
- [ ] Add failing reference tests for marker order, positive IDs, per-marker deduplication by real ID, five-reference cap, private/group removal, ordinary-group notice, and supergroup `https://t.me/c/...` conversion.
- [ ] Add failing model-trace tests for the exact five quote lines, response-model precedence, unique detail model merge, candidate generation source, Check repair source, configured-unattempted Check, absent Check, and unavailable state.
- [ ] Implement only pure deterministic functions; no network, SQL, Redis, environment access, or Telegram sends are permitted in this module.
- [ ] Run focused `cargo test --test rich_recap_content_tests`, then the global gates and push.

**Commit:** `feat: port rich recap content`

### Task 6: Port Rich composition and plain conversion

**Files:**

- Create: `src/services/rich_recap/compose.rs`
- Create: `src/services/rich_recap/plain.rs`
- Create: `src/services/rich_recap/summary.rs`
- Modify: `src/services/rich_recap/mod.rs`
- Create: `tests/rich_recap_composition_tests.rs`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

**Composition interface:**

```rust
pub const RICH_MESSAGE_UTF16_LIMIT: usize = 32_768;
pub const PLAIN_MESSAGE_UTF16_LIMIT: usize = 4_096;

pub struct RichRecapSummaryConfig<'a> {
    pub title: &'a str,
    pub hours: Option<u32>,
    pub automatic: bool,
    pub initiator: Option<(&'a str, i64)>,
    pub condensed_summary: &'a str,
    pub ordinary_group_notice: bool,
    pub subscription_chat_title: Option<&'a str>,
    pub model_trace: &'a str,
}

pub fn build_rich_recap_summary(config: RichRecapSummaryConfig<'_>) -> String;
pub fn fallback_condensed_summary(details: &[String], default_text: &str) -> String;
pub fn compose_rich_recap_messages(prefix: &str, detailed: &[String]) -> Vec<String>;
pub fn rich_markdown_to_plain_text(markdown: &str) -> String;
pub fn split_plain_text(text: &str, utf16_limit: usize) -> Vec<String>;
```

- [ ] Add failing tests for title/metadata/notice order, initiator mention escaping, condensed label removal, empty-label fallback, deterministic 120-UTF-16 condensed fallback, exact details wrapper, and empty-input zero parts.
- [ ] Add failing UTF-16 tests using BMP and supplementary characters, block/sentence boundary preference, protected Markdown links/spans, oversized prefix stand-alone parts, and a complete details wrapper on every detailed part.
- [ ] Add failing plain-conversion tests for details summary replacement, `tg://user` visible labels, HTTP(S) labels plus URLs, heading/quote/fence removal, inline formatting unwrap, reversal of application escapes, and trimming.
- [ ] Add failing plain split tests for 4,096 UTF-16 limits, boundary preference, protected spans, and guaranteed progress on one oversized scalar sequence.
- [ ] Implement the pure composer and converter without Telegram-specific request types.
- [ ] Run focused `cargo test --test rich_recap_composition_tests`, then the global gates and push.

**Commit:** `feat: port rich recap composition`

### Task 7: Implement raw Telegram Rich Message transport

**Files:**

- Create: `src/services/telegram_rich.rs`
- Modify: `src/services/mod.rs`
- Modify: `src/bot/context.rs`
- Create: `tests/telegram_rich_tests.rs`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

**Transport interface:**

```rust
pub struct RichMessageReplyParameters {
    pub message_id: i32,
    pub chat_id: Option<i64>,
    pub allow_sending_without_reply: bool,
}

pub struct RichMessageRequest {
    pub chat_id: i64,
    pub markdown: String,
    pub reply_parameters: Option<RichMessageReplyParameters>,
    pub reply_markup: Option<teloxide::types::InlineKeyboardMarkup>,
    pub disable_notification: bool,
}

pub struct TelegramRequestError {
    pub status: Option<reqwest::StatusCode>,
    pub error_code: Option<i64>,
    pub description: String,
}

pub async fn send_rich_message(
    &self,
    request: RichMessageRequest,
) -> Result<teloxide::types::Message, TelegramRequestError>;
```

- [ ] Add failing wiremock tests for POST `/botTOKEN/sendRichMessage`, form content type, decimal chat ID, exact `{"markdown":"..."}` rich JSON, optional JSON objects, `disable_notification=true`, and omission of every absent/false optional field.
- [ ] Add failing tests for cross-chat reply parameters, nonzero optional chat ID, conditional `allow_sending_without_reply`, keyboard JSON, success decoding, returned message ID/text/chat, non-2xx HTTP, and Telegram `{ok:false,error_code,description}` responses.
- [ ] Implement a reqwest client using the shared API base and token. Error display/logging may include status/code/redacted description but never the method URL containing the token or complete response body.
- [ ] Verify both ordinary teloxide requests and this client use the same configured API base.
- [ ] Run focused `cargo test --test telegram_rich_tests`, then the global gates and push.

**Commit:** `feat: add Telegram rich transport`

### Task 8: Implement recap delivery and fallback state machine

**Files:**

- Create: `src/services/recap_delivery.rs`
- Modify: `src/services/mod.rs`
- Create: `tests/recap_delivery_tests.rs`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

**Delivery interface:**

```rust
#[async_trait::async_trait]
pub trait RecapSender: Send + Sync {
    async fn send_rich(&self, request: RichMessageRequest)
        -> Result<teloxide::types::Message, TelegramRequestError>;
    async fn send_plain(&self, request: PlainMessageRequest)
        -> Result<teloxide::types::Message, TelegramRequestError>;
}

#[async_trait::async_trait]
pub trait SendAttemptGate: Send + Sync {
    async fn before_send(&self);
}

pub struct RecapDeliveryError {
    pub source: TelegramRequestError,
    pub delivered: Vec<teloxide::types::Message>,
}

pub async fn send_rich_recap_parts(
    sender: &dyn RecapSender,
    request: RecapDeliveryRequest,
) -> Result<Vec<teloxide::types::Message>, RecapDeliveryError>;
```

- [ ] Add failing fake-sender tests for keyboard only on logical part zero, continuation parts replying to the first delivered message, original reply on part zero, notifications, and `before_send` before every actual attempt.
- [ ] Characterize the exact Go 400 signatures that activate plain fallback. Add negative tests proving reply-markup/reply-parameter errors, network errors, 5xx, and unrelated 400 errors do not activate fallback.
- [ ] Add failing tests for sticky plain mode after the first qualifying Rich error, Rich-to-plain conversion, 4,096-unit chunks, recursive half-limit retries on plain too-long responses, and a gate call for every recursive retry.
- [ ] Add failing partial-delivery tests for Rich failure, fallback failure, zero plain chunks, and preservation of every already delivered Telegram message.
- [ ] Implement one delivery state machine shared by all three production callers; handlers may not duplicate fallback or splitting logic.
- [ ] Run focused `cargo test --test recap_delivery_tests`, then the global gates and push.

**Commit:** `feat: add rich recap delivery`

### Task 9: Port OpenAI recap generation, traces, logs, and metrics

**Files:**

- Modify: `src/services/openai.rs`
- Modify: `src/services/prompts.rs`
- Modify: `src/services/recap.rs`
- Create: `src/services/recap_generation.rs`
- Modify: `src/services/mod.rs`
- Create: `tests/openai_rich_recap_tests.rs`
- Create: `tests/recap_generation_tests.rs`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

**Generation interface:**

```rust
pub struct RichRecapOutput {
    pub condensed_summary: String,
    pub detailed_summaries: Vec<String>,
    pub condensed_trace: CondensedExecutionTrace,
    pub detailed_trace: RecapExecutionTrace,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub created_at_ms: i64,
}

#[async_trait::async_trait]
pub trait ChatCompletionGateway: Send + Sync {
    async fn complete(&self, request: ChatCompletionRequest)
        -> anyhow::Result<ChatCompletionResult>;
}

pub async fn generate_rich_recap(
    &self,
    history: &[RichRecapHistoryRow],
    source: RecapSource,
) -> anyhow::Result<RichRecapOutput>;
```

- [ ] Add failing tests for the `gpt-3.5-turbo` tokenizer, `token_limit - recap_reserve`, bounded invalid budget, detailed slice prompts, no JSON schema, no explicit max tokens, no detailed/Check temperature, and condensed temperature `0.7`.
- [ ] Add failing deterministic-clock tests for five detailed attempts one second apart, primary cancellation skipping only that attempt's backups, ordered backup behavior, empty choices, blank content, and exhaustion.
- [ ] Add failing condensed/Check tests for the exact single-line validator, primary and ordered backups, last invalid candidate, conditional Check, Check backups, already-valid zero Check requests, and deterministic 120-unit fallback.
- [ ] Add failing trace tests for actual response model names, candidate versus repair source, unique detail model merge, failed/unattempted states, recap usage aggregation, detailed successful-response metrics, condensed pre-Check metrics, and zero Check metrics.
- [ ] Replace complete prompt/body logging with count/model/status/redacted metadata only. Add a capture test proving verbose mode never contains a supplied secret marker from prompts or completions.
- [ ] Persist group recap logs with resolved detail model and forwarded recap logs with Go's empty model default. Persist the already-created empty-output log before callers reject all-sanitized-empty results.
- [ ] Remove the Rich recap production dependency on structured JSON and Telegraph while keeping unrelated services compilable.
- [ ] Run focused `cargo test --test openai_rich_recap_tests --test recap_generation_tests`, then the global gates and push.

**Commit:** `feat: port rich recap generation`

### Task 10: Port message capture, edit, migration, and bot-left behavior

**Files:**

- Modify: `src/bot/middleware.rs`
- Modify: `src/bot/router.rs`
- Modify: `src/bot/handlers/migration.rs`
- Modify: `src/db/chat_history.rs`
- Modify: `src/db/migration.rs`
- Create: `src/bot/handlers/chat_member.rs`
- Modify: `src/bot/handlers/mod.rs`
- Create: `src/services/message_extract.rs`
- Create: `tests/message_capture_tests.rs`
- Create: `tests/chat_lifecycle_tests.rs`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

- [ ] Add failing tests for enabled group/supergroup capture before command dispatch, private capture only during an active forwarded session, caption precedence, and non-empty filtering.
- [ ] Add failing tests proving only `message.entities` drive URL/text-link rewriting, URL previews use a ten-second timeout, titles over 200 Unicode scalars use `SummarizeAny` with a one-minute timeout, and extracted text of at least 300 scalars uses `SummarizeOneChatHistory`.
- [ ] Add failing tests for the same reply-snapshot extraction, Telegram seconds converted to Unix milliseconds, Go forwarding prefixes, strict `chatted_at > cutoff`, and `message_id ASC` ordering.
- [ ] Add failing edit tests proving only text changes for `(chat_id,message_id)` and every other stored field remains unchanged.
- [ ] Add failing lifecycle tests for the exact group-to-supergroup migration table subset and forced supergroup history type, plus bot-left deletion/blanking/retention subsets.
- [ ] Implement deterministic extraction interfaces so HTTP and summarizer calls are fakeable without weakening repository behavior assertions.
- [ ] Ensure middleware sequencing does not spawn capture in a way that lets command handling overtake required persistence ordering.
- [ ] Run focused `cargo test --test message_capture_tests --test chat_lifecycle_tests`, then the global gates and push.

**Commit:** `feat: port recap message capture`

### Task 11: Port callback router, manual recap, and configuration

**Files:**

- Create: `src/bot/callbacks.rs`
- Create: `src/bot/handlers/recap_manual.rs`
- Create: `src/bot/handlers/recap_config.rs`
- Modify: `src/bot/handlers/recap.rs`
- Modify: `src/bot/handlers/mod.rs`
- Modify: `src/bot/commands.rs`
- Modify: `src/bot/router.rs`
- Modify: `src/services/rate_limit.rs`
- Create: `tests/recap_callback_tests.rs`
- Create: `tests/recap_manual_tests.rs`
- Create: `tests/recap_config_handler_tests.rs`
- Update: `tests/recap_scope_tests.rs`
- Modify: `locales/en.yml`
- Modify: `locales/zh-Hans.yml`
- Modify: `locales/zh-Hant.yml`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

- [ ] Reverse the old command-surface characterization so every included recap command is registered and `/smr` remains absent.
- [ ] Add failing callback dispatcher tests for all exact route literals, hash lookup, malformed wire generic edit, unknown hash silence, known route without handler generic edit, and expired handler-specific JSON-bind branches.
- [ ] Add failing `/recap` tests for group/supergroup validation, enablement, public versus private mode, public Redis rate state written before six callback allocations, persisted interval override only when larger, six exact hour choices, private deep-link continuation, and waiting-message cleanup branches.
- [ ] Add failing selection tests for the hour allowlist, no initiator binding, clicking-user attribution, `>5` group-history threshold, pre-delivery placeholder retention, Rich delivery cleanup, feedback keyboard, recap log sent-count update, and zero `sent_messages` rows.
- [ ] Add failing configuration tests for first-enable option materialization, toggle creator/admin rules, creator-only mode/rate/pin rules, complete rules, bot-admin checks, anonymous-admin exception, queue transition output, disable queue retention, and the pin keyboard-state bug. Return an explicit `QueueMutation::Rescore(chat_id)` after enable/rate changes and `QueueMutation::None` after disable; Task 14 connects that transition to Redis.
- [ ] Preserve UI behavior for daily rate `0` displaying `4`, other invalid rates selecting nothing, and scheduler-only in-memory normalization.
- [ ] Route manual Rich delivery exclusively through `send_rich_recap_parts`; remove manual Telegraph/HTML delivery.
- [ ] Run the three focused handler tests plus `recap_scope_tests`, then the global gates and push.

**Commit:** `feat: port manual recap controls`

### Task 12: Port subscriptions and feedback compatibility

**Files:**

- Create: `src/bot/handlers/recap_subscription.rs`
- Create: `src/bot/handlers/recap_feedback.rs`
- Modify: `src/bot/handlers/system.rs`
- Modify: `src/bot/router.rs`
- Create: `tests/recap_subscription_tests.rs`
- Create: `tests/recap_feedback_tests.rs`
- Modify: `locales/en.yml`
- Modify: `locales/zh-Hans.yml`
- Modify: `locales/zh-Hant.yml`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

- [ ] Add failing deep-link tests for exact token sources, eight lowercase hex characters, reusable GET-only contexts, no feature/member/original-actor recheck, and the two distinct `/start` domains.
- [ ] Add failing subscribe tests for private success message before database insertion, repeated success message with sequentially idempotent insert, insertion failure after visible success, and no uniqueness constraint.
- [ ] Add failing unsubscribe tests for ordinary idempotence, GroupAnonymousBot command deletion without SQL change, inline payload `FromID` binding, button removal, and private confirmation.
- [ ] Add failing member-left tests proving only the departed member's subscriber row is removed.
- [ ] Add failing feedback tests proving keyboards initially count recap reactions while `smr/summarization/feedback/react` clicks write summarization reactions, toggle/replace non-transactionally, recount that table, and edit the payload source-group chat ID even for subscriber DMs.
- [ ] Add the included `recap/recap/feedback/react` route without exposing the `/smr` command or webpage summarization generation.
- [ ] Run focused subscription/feedback tests, then the global gates and push.

**Commit:** `feat: port recap subscriptions`

### Task 13: Port forwarded recap sessions

**Files:**

- Create: `src/bot/handlers/recap_forwarded.rs`
- Modify: `src/bot/handlers/system.rs`
- Modify: `src/bot/router.rs`
- Create: `tests/recap_forwarded_tests.rs`
- Modify: `locales/en.yml`
- Modify: `locales/zh-Hans.yml`
- Modify: `locales/zh-Hant.yml`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

- [ ] Add failing `/recap_forwarded_start` tests for batch deletion, control creation, two-hour TTL, instruction message, and delete-later tracking.
- [ ] Add failing active-session capture tests for every non-empty private text or caption, including messages without Telegram forwarding metadata, TTL refresh, second-resolution Telegram date multiplied to milliseconds, and equal-score lexicographic ordering after reversal.
- [ ] Add failing `/recap_forwarded` tests for no control-key requirement, command-message capture before handling, `>=5` threshold, waiting placeholder retention on every pre-delivery failure, forwarded prompt actor/message-ID rules, Rich delivery, completion message, placeholder deletion after delivery attempt, and batch/session retention after success.
- [ ] Add failing `/cancel` tests for active control value `1`, both-key deletion, orphan-batch retention, generic already-cancelled branch, reply chat, and the group administrator bare-command guard.
- [ ] Route forwarded delivery only through `send_rich_recap_parts`, update log sent count, preserve the forwarded empty model field, and create zero `sent_messages` rows.
- [ ] Run focused `cargo test --test recap_forwarded_tests`, then the global gates and push.

**Commit:** `feat: port forwarded recaps`

### Task 14: Port automatic queue, fan-out, and pinning

**Files:**

- Create: `src/services/autorecap_queue.rs`
- Modify: `src/services/autorecap.rs`
- Modify: `src/services/mod.rs`
- Modify: `src/main.rs`
- Modify: `src/bot/handlers/recap_config.rs`
- Create: `tests/autorecap_queue_tests.rs`
- Create: `tests/autorecap_worker_tests.rs`
- Create: `tests/autorecap_delivery_tests.rs`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

**Queue interface:**

```rust
pub const AUTO_RECAP_QUEUE_KEY: &str = "time_capsule/auto_recap_capsules";

#[derive(serde::Serialize, serde::Deserialize)]
pub struct AutoRecapCapsule {
    pub chat_id: i64,
}

pub fn encode_auto_recap_member(chat_id: i64) -> String;
pub fn next_auto_recap_at_ms(
    now_utc_ms: i64,
    timezone_shift_seconds: i64,
    rates_per_day: i32,
) -> i64;
```

- [ ] Add failing queue tests for standard-Base64 compact `{"payload":{"chat_id":<decimal>}}`, deterministic ZADD rescore, no TTL, one-second polling, pop-before-handler, consumed failure with no automatic redelivery, and swallowed one-minute-context enqueue failure.
- [ ] Add failing schedule tests for exact 2/3/4 slot sets, fixed offset without DST, hour-only strict `<`, exact `HH:00:00` treated as passed, next-day wrap, invalid-rate in-memory `4`, and 12/8/6-hour message windows.
- [ ] Add failing worker tests for ten immediate enablement/options/subscriber reads, error-side requeue without automatic return, normal pre-generation requeue, missing-options bounded adapter, disabled no-requeue, private-no-subscriber requeue then skip, and immediate startup test trigger.
- [ ] Add failing fan-out tests for public-first source target, exact private mode, invalid modes, unordered/undeduplicated subscribers, shared reaction-count/keyboard failure abort, per-subscriber keyboard failure skip, one five-attempts-per-second gate shared by actual sends, target failure continuation, and no generation retry.
- [ ] Add failing pin/persistence tests for public target only, prior lookup by newest `created_at`, unpin failure followed by bulk SQL clear, missing/failed lookup still offering the new pin, pin success marking only part zero, partial delivery all false, one insert failure not stopping later inserts, and no manual/forwarded rows.
- [ ] Replace the old 60-second SQL scanner and Telegraph/HTML send with the Redis digger, shared Rich generator, shared delivery, subscriber fan-out, pinning, and automatic-only persistence.
- [ ] Consume Task 11's `QueueMutation::Rescore` after enable/rate changes; disabling does not remove the existing member.
- [ ] Run all focused automatic tests, then the global gates and push.

**Commit:** `feat: port automatic recaps`

### Task 15: Complete documentation and live parity acceptance

**Files:**

- Modify: `.env.example`
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `locales/en.yml`
- Modify: `locales/zh-Hans.yml`
- Modify: `locales/zh-Hant.yml`
- Create: `docs/parity/ayugram-rich-recap-acceptance.md`
- Update: `docs/parity/go-v1.0.0-rich-recap-ledger.md`

- [ ] Compare every command, button, waiting, completion, limit, permission, and error literal against Go `v1.0.0` byte-for-byte and correct only parity mismatches in the locale bundles.
- [ ] Verify `.env.example` contains every exact-case variable with fake values and both READMEs describe Redis, Rich delivery, subscriptions, forwarded sessions, feedback, automatic fan-out, pinning, safety adapters, and local test startup without exposing production data.
- [ ] Run the complete automated gate: `cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`, `git diff --check`, and `cargo llvm-cov` with the changed recap module denominator at or above 95%.
- [ ] Run a final parity-ledger audit: every included row complete, exactly three Rust Rich delivery production call sites, no `/smr` generation route, no unmapped Go branch, and every permitted delta identified as a documented safety adapter.
- [ ] Start an isolated test stack, bind webhook port `9487`, use the bore public base without printing the token path, and record AyuGram results for manual, forwarded, automatic public, automatic subscriber, continuation replies, feedback, unsubscribe, and pinning. Use a local mock Telegram endpoint for fallback, recursive split, partial-send, and upstream-error scenarios.
- [ ] Record visible text/buttons, first and continuation message IDs, reply relationships, test SQL state, and test Redis state in `docs/parity/ayugram-rich-recap-acceptance.md` using only isolated identifiers and redacted endpoint data.
- [ ] Perform the final staged security/PII scan, create the signed acceptance commit, verify signature/trailer, and push.

**Commit:** `test: verify rich recap parity`

## Final branch review and delivery

- [ ] Generate one whole-branch review package from the branch merge base through `HEAD` and dispatch the most capable available read-only reviewer using `superpowers:requesting-code-review`.
- [ ] If the review reports Critical or Important findings, dispatch one fix agent with the complete findings list, run covering tests, create signed fix commits, push, and run one scoped re-review.
- [ ] Use `superpowers:finishing-a-development-branch` only after the final review, full automated gates, security scan, signature/trailer checks, remote-ref equality, parity ledger, coverage, and AyuGram evidence all pass.
- [ ] Prepare the GitHub PR title, bilingual summary, implementation details, executed tests and measured coverage, risks/breaking changes, and required checklist without merging to the default branch.
