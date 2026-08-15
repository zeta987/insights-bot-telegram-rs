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
- The toggle integration now proves disable leaves the existing deterministic
  queue member untouched, and the rate integration proves a disabled chat is
  still rescored without being enabled.
- Four RED-to-GREEN transport tests raise the focused configuration total to
  eleven.

The current automatic-recap checkpoint adds:

- Go TimeCapsule Redis ZSET key and deterministic padded standard-Base64 member.
- One-second poll cadence, no-TTL ZADD rescore, due pop-before-handler behavior,
  fixed-offset 2/3/4-per-day scheduling, and 12/8/6-hour history windows.
- Ten-attempt feature/options/subscriber reads, disabled/private-only decisions,
  pre-generation requeue, and the bounded Rust replacement for Go's nil-options
  dereference.
- Startup seeding uses Go's two phases: load or create options for every enabled
  chat first, then queue only the successful chats in a second pass.
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

Latest verification before this handoff:

- `cargo clippy --all-targets --all-features -- -D warnings` passed.
- Full `cargo test` passed.
- `/configure_recap` focused tests passed: 11.
- Automatic-recap focused tests passed: worker 10, queue 11, delivery 8.

## Next required slice

Continue the read-only pinned-Go versus Rust audit of the completed automatic
worker and configuration runtime; passing tests do not prove every error branch.
The ordinary-member, creator-only, GroupAnonymousBot actor lookup, expired
payload, toggle-off queue retention, disabled-rate-rescore, and two-phase
startup seed branches are complete. Next verify production worker invocation
and direct test-capsule startup behavior, then check original-message
anonymous-origin exceptions plus the callback group-only and remaining Bot API
failure branches.

When that audit is clean, use `docs/parity/go-v1.0.0-rich-recap-ledger.md` and a
fresh structural callsite inventory to select the next non-`/smr` Telegram gap.
Continue module by module until every in-scope Go command, middleware, callback,
persistence side effect, Redis lifecycle, and Telegram delivery callsite has a
Rust equivalent or a documented approved deviation. Port 9487 and AyuGram are
available for a later live Telegram test after local verification.

## Mandatory repository rules

- Read `AGENTS.md` before changing files.
- Never read or stage `.env` or `.env.golang1`.
- Use `apply_patch` for every manual file edit; formatters may modify formatting.
- Before every Git command, run `Test-Path .git\index.lock` and proceed only when
  it prints `False`. Never delete a lock that may belong to another process.
- Local commits only until the user explicitly asks to push.
- Commit with `git commit -S` and the exact trailer:
  `Co-authored-by: Codex <noreply@openai.com>`
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
above. Do not redo completed modules merely because their implementation is
unfamiliar: compare production callsites and tests first. Use TDD, keep edits
scoped, run focused plus full Rust verification, perform a staged secrets/PII
scan, and create signed local commits with the exact Codex co-author trailer.
Never push unless the user explicitly authorizes that push.
