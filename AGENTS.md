# AGENTS.md

## Overview

`c2n` is a Rust CLI that downloads ChatGPT image generations, embeds the prompt in PNG metadata, and archives images in Notion. `src/lib.rs` exposes the library; `src/main.rs` only parses arguments and calls `cli::app::run`.

Requires Rust 1.85+ and edition 2024. The package and binary are both named `c2n`.

## Commands

Run from the repository root:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

The release binary is `target/release/c2n`; use `cargo run -- <command>` during development. Run a single integration test with:

```bash
cargo test --test sync_pipeline sync_then_delete_conversation_removes_local_images
```

CI runs the four commands above in that order. Do not use `cargo test` alone as the final check; CI uses `--all-targets`.

## Architecture

- `src/cli/app.rs`: clap definitions and command handlers.
- `src/config.rs`: TOML loading, validation, and shared/account fallback resolution. Raw config models live in `src/domain/models.rs`.
- `src/services/`: orchestration (`sync`, `history`, `image`, `account_status`).
- `src/adapters/`: external boundaries (`chatgpt`, `notion`, `sqlite`, `filesystem`).
- Traits `ChatGptApi`, `NotionApi`, `GenerationStore`, and `NotionCache` exist so services can be tested with fakes.
- `src/shared/http.rs`: the only retry path. All HTTP requests should go through `retry_http`/`backon`.
- SQLite schema is the `SCHEMA` constant in `src/adapters/sqlite.rs`. There is no migration framework; `init_db` only runs idempotent `CREATE IF NOT EXISTS`. Add explicit migration logic before changing existing tables.
- `docs/technical.md` is the authoritative architecture and safety document. Update it when behavior or invariants change.

## Safety Invariants

- Set `uploaded_at` only after a successful Notion page creation or a positive remote duplicate check.
- Delete a conversation only when every image is uploaded or verified uploaded and the run is not history replay. Empty conversation lists must never trigger deletion.
- Remove local images for a conversation only after ChatGPT confirms the conversation was deleted. If the delete request fails, keep the files. Ignore missing files, and warn and continue on other removal errors.
- `library delete`, and library cleanup during `sync`, are dry runs unless `--yes` is supplied. `img clear` preserves `chatgpt.db`, its WAL/SHM/journal files, and `.gitkeep`; `db restore` uses the SQLite backup API rather than replacing the live database file.
- Reuse the original Notion file-upload ID when retrying page creation. Treat a `400` on the upload-send step as "already uploaded". Duplicate checks fail closed when the database exposes no data sources.
- Keep retry behavior centralized: 10 total attempts, 2–60 second delays, retry 429/5xx/transport errors, and cap `Retry-After` at 60 seconds.
- PNG prompt injection is idempotent and atomic; non-PNG files are skipped. Relative image paths resolve under `output/`, and `..` or symlink escapes must be rejected. Absolute paths remain allowed.
- Store timestamps as UTC RFC 3339; use local time only for display and calendar-day calculations.

## Config and State

- `config.toml` and `output/` are gitignored and may contain secrets or runtime state. Never commit them or print secrets from them; use `config.toml.example` for structure.
- Account keys must be valid email addresses. Resolution precedence is CLI override, then per-account value, then shared value; blank per-account strings fall back to shared.
- `--check-notion-api` is a compatibility flag; remote checks are enabled by default. `--no-check-notion-api` is the actual opt-out.
- History replay defaults to the last 3 days; `--all` requires `--from-history` or `--verify-history`. `--timezone` is accepted for compatibility; SQLite is the source of truth and history filtering uses absolute time. Config/data dirs are platform defaults, overridable via `C2N_CONFIG_DIR`/`C2N_DATA_DIR` for development. Example dev invocation: `C2N_CONFIG_DIR=./ C2N_DATA_DIR=./output c2n sync`.
- `account-status` reads `output/chatgpt.db` and needs a valid config with accounts, but does not use ChatGPT or Notion credentials.
- Do not run live upload, library-delete, restore, or cleanup commands against real data unless the user explicitly asks. Use temporary directories and mocks in tests.

## Testing

- Unit tests are inline under `#[cfg(test)]`; integration tests live in `tests/`.
- Integration tests use `wiremock` for ChatGPT/Notion, `assert_cmd` for CLI behavior, and `tempfile` for SQLite/filesystem isolation.
- Clients expose `with_base_url` so tests point at mock servers. Keep new tests deterministic and credential-free; no live API calls.
- `tests/cli.rs` checks argument validation before config loading. Preserve that separation.

## Git and PRs

- `main` is protected: work on a branch and open a PR; only squash merges are allowed. Direct pushes, force pushes, and deletion are blocked; there are no bypass actors.
- **Do not merge any pull request without explicit permission from the user.** Creating or updating a PR is not permission to merge it.
- The required `PR Template Compliance` check enforces `.github/PULL_REQUEST_TEMPLATE.md`: use the exact headings `### Summary`, `### Changes Made`, and `### Testing`, replace all placeholder text, and check at least one `- [x]` testing item. Do not paste test logs into the PR body.
- The `CI` workflow runs formatting, clippy, tests, and a release build; make it pass before merging.
- Releases must update `CHANGELOG.md` (Keep a Changelog format) and bump the `version` in `Cargo.toml` in the same release commit; the changelog heading must match the tag version.
