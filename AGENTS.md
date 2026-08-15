# Repository Guidelines

## Project Structure & Module Organization
`src/` contains the runtime code. Use `src/bot/` for Telegram commands, routing, handlers, and middleware; `src/services/` for recap, OpenAI, Telegraph, rate limiting, and auto-recap logic; `src/db/` for `sqlx::AnyPool` data access and models; and `src/http/` for the health endpoint. Keep locale bundles in `locales/`, SQL bootstrap files in `migrations/{postgres,sqlite}/`, integration tests in `tests/`, and behavior proposals or task tracking in `openspec/`.

## Build, Test, and Development Commands
Run `cargo fmt` to format code, `cargo check` for a fast compile pass, and `cargo clippy --all-targets --all-features -D warnings` before opening a PR. Use `cargo test` to run the current test suite. Start the bot locally with `cargo run` after configuring `.env`; the app will load env vars, run startup migrations, start `/health`, and then launch the Telegram dispatcher.

## Coding Style & Naming Conventions
Target Rust 2024 idioms and keep indentation at four spaces. Use `snake_case` for modules, files, and functions, `CamelCase` for structs and enums, and clear command-oriented names such as `handle_recap` or `set_rates_per_day`. Prefer `anyhow::Result` for fallible flows and `tracing` for logs. Keep SQL in `sqlx::query` or `query_as::<_, T>` form so Postgres and SQLite remain compatible.

## Testing Guidelines
Place integration-style tests in `tests/*_tests.rs` and keep helpers local to each file unless reused broadly. Prefer isolated fixtures such as `tempfile` directories or in-memory SQLite when possible. New work should cover handlers, middleware, and auto-recap edge cases, not only happy paths. If available, use `cargo llvm-cov` to inspect coverage; aim for strong coverage on changed logic, especially scheduling and config flows.

## Commit & Pull Request Guidelines
Follow Conventional Commits as seen in project history: `feat:`, `fix:`, `chore:`, `test:`. Keep each commit focused and descriptive, for example `feat: add Telegraph fallback for recap publishing`. PRs should include a short summary, test evidence, and any env, locale, or schema changes. When command behavior or Telegram output changes, include a sample message, screenshot, or log excerpt. If behavior changes materially, update `README.md` and the relevant `openspec` files in the same PR.

## Security & Configuration Tips
Start from `.env.example` and never commit real secrets. Document new environment variables in both `.env.example` and `README.md`. Treat migration changes carefully: keep Postgres and SQLite schema files aligned.

## Agent skills

### Issue tracker

Issues are tracked as GitHub Issues on `zeta987/insights-bot-telegram-rs` via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles use their default label strings (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
