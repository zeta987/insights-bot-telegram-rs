# Claude Code Handoff — Go Rich Recap Port

Updated: 2026-08-16

## Objective

Continue the 1:1 port of Telegram Rich recap behavior from the production Go
v1.0.0 source into Rust. The pinned Go source revision is
`02aee8ce260165592e2152eb5a024a602e4eced1`.

`/smr` generation is excluded. Keep only the `/smr` compatibility feedback
route/buttons required by Rich recap messages.

## Repositories

- Rust target and current working directory:
  `D:\Data\Coding_Github\Projects\_TG_BOT\insights-bot-workspace\insights-bot-telegram-rs`
- Go source:
  `D:\Data\Coding_Github\Projects\_TG_BOT\insights-bot-workspace\insights-bot-go`
- Rust branch: `feat/rich-recap-parity`
- Behavior ledger:
  `docs/parity/go-v1.0.0-rich-recap-ledger.md`

Always treat the latest signed local `HEAD`, `git status --short`, this file,
and the current Codex session as the progress source of truth. The session may
contain work completed after this file's timestamp.

## Completed checkpoints

The branch already contains signed local commits for message capture, Rich
formatting and transport, Rich delivery and generation, public manual recap,
subscriptions, and forwarded recap. Run `git log --oneline -12` after the
required lock preflight to see the exact current list.

The latest completed automatic-recap commit is `be73d17` (`feat: port automatic
rich recaps`). The next signed checkpoint `f47f2dc` added the presentation half
of `/configure_recap`:

- `src/bot/handlers/recap_configure.rs` exposes `build_configure_keyboard`.
- `tests/recap_configure_tests.rs` fixes the disabled five-row and enabled
  nine-row keyboards, exact Simplified Chinese labels, `nop` callback payload,
  compact camel-case JSON, hashed callback wires, and the pin payload without
  `fromId`.
- `src/bot/handlers/mod.rs` exports the new module.
- The focused keyboard test passed before the checkpoint was committed.

Signed commit `27c2ea7` completes the production `/configure_recap` runtime:

- The command checks bot-admin status first, then creator/administrator or the
  exact Group Anonymous Bot, replies to the command, and keeps missing options
  as an ephemeral public/4-times/no-pin view.
- The five Go routes are registered and dispatched through opaque Redis callback
  wires: toggle, assign mode, complete, daily rate, and pin.
- Toggle enable creates usable first-enable options, enables the parity feature
  flag, and queues one deterministic TimeCapsule member. Toggle disable does not
  remove an existing member.
- Mode is creator-only and never queues. Daily rate is creator-only and always
  adds or rescores the deterministic member. Complete checks only the actor and
  best-effort deletes the settings message plus original command.
- Pin intentionally has no payload chat/actor guard and preserves Go's visible
  bug where requested pin status is reused as recap-enabled UI state.
- The legacy production `cfg:*` dispatcher and keyboard were removed. The old
  Rust-only `db::recap_config` module remains for existing schema compatibility
  and tests but has no production callsite.
- `tests/recap_configure_tests.rs` has seven command, keyboard,
  callback, persistence, queue, deletion, and pin-quirk tests.

The checkpoint after `27c2ea7` ports Go's callback permission branching that a
success-only audit originally missed:

- An ordinary member's toggle, mode, rate, and pin mutations are silently
  ignored without editing the configuration message.
- Mode, rate, and pin perform separate creator and administrator Telegram
  membership lookups, matching Go's two `IsUserMemberStatus` calls.
- Administrators and the Group Anonymous Bot receive Go's same creator-only
  mode error for all three operations, including the duplicated
  `抱歉，此操作无法进行` text. Bot-admin denial edits now include the configuration
  header exactly as Go does.
- Command, toggle, and complete now query GroupAnonymousBot membership before
  applying the exception, so a Bot API error follows Go's error path instead of
  continuing the mutation.
- Configuration edits now preserve Go's parse-mode split: toggle and pin
  success plus ordinary errors omit `parse_mode`; mode, rate, and HTML
  permission errors use `HTML`.
- An expired configure callback receives the fixed plain error edit and the
  existing inline keyboard is sent back unchanged.
- Toggle, mode, rate, and pin callbacks now perform Go's group/supergroup gate
  after the bot-admin lookup and before actor lookup or persistence. Crafted
  private callbacks receive the exact HTML error and retain their keyboard.
- A callback from an ordinary owner may bypass the payload actor binding when
  the original `/configure_recap` command came from GroupAnonymousBot, matching
  Go's reply-to-message exception; the actor membership lookup still occurs.
- The toggle integration now proves disable leaves the existing deterministic
  queue member untouched, and the rate integration proves a disabled chat is
  still rescored without being enabled.
- The focused configuration suite now contains thirteen tests.

The current automatic-recap checkpoint adds:

- Go TimeCapsule Redis ZSET key and deterministic padded standard-Base64 member.
- One-second poll cadence, no-TTL ZADD rescore, due pop-before-handler behavior,
  fixed-offset 2/3/4-per-day scheduling, and 12/8/6-hour history windows.
- Ten-attempt feature/options/subscriber reads, disabled/private-only decisions,
  pre-generation requeue, and the bounded Rust replacement for Go's nil-options
  dereference.
- Startup seeding uses Go's two phases: load or create options for every enabled
  chat first, then queue only the successful chats in a second pass.
- Production startup is wired from `main` and retains Go's order: synchronous
  seed, optional nonzero direct test capsule task, then the one-second poller.
- Rich recap generation from the parity history/log repositories, public-first
  delivery, duplicate subscriber fan-out, shared five-per-second send limiter,
  subscriber unsubscribe keyboards, partial-send persistence, pin replacement,
  and `sent_messages` tracking.

Relevant files:

- `src/services/autorecap.rs`
- `src/services/autorecap_queue.rs`
- `src/services/autorecap_delivery.rs`
- `src/redis/keys.rs`
- `src/redis/recap_state.rs`
- `tests/autorecap_worker_tests.rs`
- `tests/autorecap_queue_tests.rs`
- `tests/autorecap_delivery_tests.rs`

The current configuration-error checkpoint ports Go's stage-specific
`/configure_recap` failure behavior (`callback_query.go:95-641`):

- Six post-permission failures call `WithEdit(c.Update.Message)` inside a
  callback update, where that message is `nil`; `WithEdit(nil)` keeps no edit
  target and `processExceptionError` falls through to a brand-new plain
  `SendMessage` without keyboard, reply target, or parse mode. The branches
  are toggle options lookup (`:149`), toggle keyboard rebuild (`:200`), mode
  options reload (`:291`), rates options reload (`:478`), pin options lookup
  (`:583`), and pin keyboard rebuild (`:625`).
- The rates options reload keeps the configuration header plus
  `每天自动创建回顾频率次数设定失败，请稍后再试！` on that new message; the toggle
  and mode branches use the bare `暂时无法配置聊天记录回顾功能，请稍后再试！`; the
  pin branches use the bare
  `暂时无法配置聊天记录回顾消息置顶功能，请稍后再试！`. The earlier five-branch
  note omitted the rates branch; the pinned Go source is authoritative.
- Every other post-permission failure stays an `EditMessageText` with its
  stage text: toggle enable and disable, mode mutation (general text), mode
  feature lookup (`模式设定失败`), every remaining rate failure, and pin
  enable/disable. The mode keyboard rebuild failure stays an edit because Go
  passes the callback message there (`:313`).
- The approved `find_one_or_create` replacement for Go's missing-options
  nil-pointer crash is retained on toggle and pin.
- `tests/recap_configure_tests.rs` now holds twenty-six tests; the thirteen
  new ones inject failures with a closed pool, dropped tables, SQLite
  corruption/RAISE triggers, and a `put_callback`-failing state store
  wrapper.

The current bot-left wiring checkpoint connects the production router to the
existing cleanup:

- `src/bot/router.rs` adds an `Update::filter_my_chat_member()` branch ending
  in `src/bot/handlers/chat_member.rs::handle_my_chat_member`.
- Only `update.new_chat_member.is_left()` triggers
  `chat_cleanup::prune_chat_data_after_bot_left(&db, update.chat.id.0)`;
  teloxide-core 0.11.2 `is_left()` matches only the `Left` variant, so a ban
  (`kicked`) falls through exactly like Go's unmatched status branch, and no
  Telegram request is sent on either path.
- The ordinary `left_chat_member` branch that removes one subscriber is
  untouched.
- `tests/chat_member_tests.rs` proves the left update performs the five-step
  cleanup (subscribers, feature flags, recap options, chat histories, blanked
  recap log text with the row retained) with zero Telegram requests, and that
  a banned update changes nothing.

A seven-module read-only parity audit (manual recap, subscriptions, forwarded
recap, feedback callbacks, middleware/lifecycle, Rich delivery/transport,
autorecap/startup/health/config) then compared every non-`/smr` Go Telegram
callsite against Rust. Subscriptions, forwarded recap, feedback callbacks,
Rich delivery, and `/cancel` came back with zero missing behavior and zero
deviations. The four checkpoints that followed close everything the audit
confirmed as an in-scope Telegram gap:

- `test: cover subscription gate and failure paths` adds the seven previously
  untested `/subscribe_recap`, `/unsubscribe_recap`, and `/start` continuation
  branches using SQLite `RAISE(ABORT)` trigger injection.
- `fix: yield during auto recap restore retry` replaces a
  `std::thread::sleep` inside the Redis zpop restore loop with
  `tokio::time::sleep`, matching Go's cooperative goroutine park.
- `fix: match go select-hour callback failures` removes the port's only
  `answer_callback_query` call (Go never answers callback queries anywhere)
  and splits the select-hour payload errors: a bind failure keeps the
  generation-failure text while an out-of-set hour sends Go's framework
  default `发生了一些错误，请稍后再试` as a plain reply-or-message.
- `fix: match go chat migration trigger and notice` moves the migration
  filter to the new supergroup's `migrate_from_chat_id` service message and
  adds Go's best-effort HTML migration notification with locale entries. Go
  registers every locale bundle under the zh-CN tag; that upstream i18n bug
  is documented, not reproduced.

Latest verification before this handoff:

- `cargo fmt --check` reports only the known `src/db/recap_config.rs` and
  `src/services/recap.rs` baseline differences.
- `cargo check --all-targets` and
  `cargo clippy --all-targets --all-features -- -D warnings` passed.
- Full `cargo test` passed.
- `/configure_recap` focused tests passed: 26.
- Bot-left chat member focused tests passed: 2.
- Chat migration focused tests passed: 3.
- Manual recap focused tests passed: 19.
- Subscription focused tests passed: 31.
- Automatic-recap focused tests passed: worker 10, queue 11, delivery 8.

## Deferred parity decisions

Two parity questions surfaced by the configuration-error checkpoint are
deliberately deferred; get an explicit decision before acting on either:

- Go `processExceptionError` never reads `replyMarkup` and its edit is a bare
  `NewEditMessageText` without parse mode or keyboard, so every
  `ExceptionError` edit in Go drops the keyboard from the wire payload
  (`pkg/bots/tgbot/handler.go:117-156`, `response.go`). Rust's
  `edit_configuration` re-attaches the existing keyboard, and the committed
  expired-callback checkpoint asserts that retention. Do not switch those
  edits to bare payloads without approval.
- Go treats a failed `QueueOneSendChatHistoriesRecapTaskForChatID` as the
  toggle-enable or rate stage error, but Rust's `queue_next_auto_recap`
  swallows queue errors after logging, so those two Go branches are currently
  unreachable. Surfacing them changes a shared signature used by the worker
  and the startup seed.

The parity audit added these further deferred items; none is an in-scope
Telegram callsite, so none was acted on without a decision:

- `set_my_commands` runs on every Rust startup (`src/bot/mod.rs:36-61`); Go
  v1.0.0 never calls `setMyCommands`. Removing it is a visible UX change.
- A Telegraph client is still constructed and held on `AppContext`
  (`src/services/telegraph.rs`, `src/main.rs`) although Go v1.0.0 removed the
  Telegraph integration. Cleanup candidate.
- Startup order is inverted versus Go (Rust arms the auto-recap poller, then
  spawns health without waiting for the bind, then starts the dispatcher; Go
  binds health, starts the dispatcher, then starts the digger) and the health
  port differs: Go hardcodes `7069`, Rust `3000`, with no env override.
- Go's `/health` is a composite checker (`telegram_bot`, digger, auto-recap
  readiness as JSON); Rust's is a bare `SELECT 1`.
- Rust has no SIGINT/SIGTERM graceful shutdown; Go stops every component in
  reverse order via fx lifecycle hooks.
- `REDIS_CLIENT_CACHE_ENABLED` is parsed but unwired; the `redis` crate has
  no client-side-cache seam, so this likely becomes a documented deviation.
- `services/rate_limit.rs::CommandRateLimiter` is dead scaffolding: built in
  `main.rs`, stored on `AppContext`, never checked by the router.
- Kept Rust hardenings, recorded as intentional: fail-fast
  `TELEGRAM_BOT_API_ENDPOINT` validation, the missing-`error_code` HTTP-status
  fallback in the Rich transport, and fail-fast `REDIS_PORT` parsing.

## Next required slice

The last confirmed in-scope Telegram gap is the bot-join welcome branch
(`welcome.go:57-74,137-184`): a `my_chat_member` transition to exactly
`member` should run the `HasJoinedGroupsBefore` guard, set the group language
from the adder's `language_code`, and send the HTML welcome message
best-effort. The DB primitives (`has_joined_before`, `set_language`,
`find_language`) already exist unwired in `src/db/feature_flags.rs`.

After that, the remaining work is the audit's test-backfill pool (largest
first): the autorecap production wiring (`spawn_autorecap`,
`handle_auto_recap_capsule`, `generate_and_deliver_auto_recap`, the concrete
startup seeder, the double-requeue assertion), the dispatcher fallback and
expired-feedback-payload end-to-end cases, the live-Redis
`check_manual_recap_rate` and orphan-batch cancel cases, the duplicate
reaction-count and recap-table un-toggle cases, the endpoint query/fragment
rejection branch, and one composition-through-delivery integration test.
Then resolve the deferred parity decisions above with the user. Port 9487 and
AyuGram are available for a later live Telegram test after local
verification.

## Mandatory repository rules

- Read `AGENTS.md` before changing files.
- Never read or stage `.env` or `.env.golang1`.
- Use `apply_patch` for every manual file edit; formatters may modify formatting.
- Before every Git command, run `Test-Path .git\index.lock` and proceed only when
  it prints `False`. Never delete a lock that may belong to another process.
- Local commits only until the user explicitly asks to push.
- Commit with `git commit -S` and the exact trailer:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
  The old Codex co-author trailer is retired; do not add it to new commits.
- Verify the signature with `git verify-commit HEAD` and verify the exact trailer.
- Scan staged changes for API keys, tokens, passwords, private endpoints, email
  addresses, and personal data before committing.
- Preserve unrelated user changes and stage only the intended module files.
- `cargo fmt --check` has a known unrelated baseline difference in
  `src/db/recap_config.rs` and `src/services/recap.rs`; use scoped rustfmt for
  touched Rust files and do not reformat those two files as collateral change.

## Paste-ready continuation prompt

Read `AGENTS.md`, this handoff file, and
`docs/parity/go-v1.0.0-rich-recap-ledger.md` completely. If the Codex session is
available, read its latest messages too. Then check `.git/index.lock`, current
branch, latest signed commits, and the unstaged/staged diff without reading any
`.env*` file. Preserve the completed automatic-recap checkpoint and continue
the pinned Go v1.0.0 1:1 port from the audit and next-gap procedure described
above. Begin with the bot-join welcome slice under `Next required slice`; do not
broadly refactor existing handlers. Do not redo
completed modules merely because their implementation is
unfamiliar: compare production callsites and tests first. Use TDD, keep edits
scoped, run focused plus full Rust verification, perform a staged secrets/PII
scan, and create signed local commits with the exact Claude co-author trailer.
Never push unless the user explicitly authorizes that push.
