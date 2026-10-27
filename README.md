# c2n — ChatGPT to Notion

A local-first Rust CLI that downloads ChatGPT image generations, embeds the
original prompt in PNG metadata, and archives each image in a Notion database.

This is a Rust port of the original Python `chatgpt-to-notion` tool. It keeps
the useful CLI surface and safety properties while using typed Rust APIs,
`reqwest`, `backon`, `rusqlite`, and `img-parts`.

## Features

- Sync recent ChatGPT image generations for one or more accounts.
- Store image generations in a local SQLite database (`output/chatgpt.db`).
- Embed the prompt in PNG `tEXt`/`iTXt` metadata without rewriting files that
  already contain the correct prompt.
- Upload images to Notion as native file uploads and create one page per image.
- Write the prompt as a Markdown block and append the account email to the page.
- Deduplicate against the local database and, by default, the Notion API.
- Retry rate limits, server errors, timeouts, and connection failures with
  exponential backoff and `Retry-After` support.
- Optionally hide processed ChatGPT conversations with a fail-safe verification
  step.
- Optionally remove images from the ChatGPT library, with dry-run defaults for
  both the standalone `delete-library` command and post-run library cleanup.
- Back up and restore the SQLite database with the SQLite backup API.

## Requirements

- Rust 1.85 or newer (the crate uses edition 2024).
- ChatGPT session credentials for each configured account.
- A Notion integration token and a database shared with that integration.

## Build

```bash
cargo build --release
```

The binary is `target/release/c2n`. During development you can use
`cargo run -- <command>`.

## Setup

```bash
cp config.toml.example config.toml
```

Edit `config.toml` with your Notion credentials and one or more ChatGPT
accounts. Account keys must be valid email addresses; the key is written into
each Notion page body as the `**Email:**` line.

```toml
[shared]
user_agent = "Mozilla/5.0 ..."
cookie_string_base64 = "b2FpLWRpZD0..."
# Optional overrides:
# image_folder = "~/Pictures/chatgpt"
# delete_library_queries = ["old prompt", "scratch"]
# remove_all_library_images = false
# conversation_delay = 2.0

[notion]
api_key = "secret_xxx"
database_id = "your_default_database_id"
# version = "2025-09-03"

[accounts."personal@example.com"]
authorization_token = "eyJhbGciOi..."
# notion_database_id = "optional_account_specific_database_id"

[accounts."work@example.com"]
authorization_token = "eyJhbGciOi..."
# cookie_string_base64 = "per-account-override"
```

### Credential notes

- `authorization_token` is the ChatGPT `Authorization` header value **without**
  the leading `Bearer `.
- `cookie_string_base64` is the raw `Cookie` header value encoded as Base64.
- The global `[shared]` values are fallbacks. Per-account values override them.
- `notion.database_id` is the default. `[shared].notion_database_id` can provide
  a shared fallback, and a per-account `notion_database_id` overrides both.
- `--config <PATH>` selects a different configuration file.

See [docs/user_guide.md](docs/user_guide.md) for step-by-step instructions on
creating the Notion integration and harvesting browser credentials.

## Usage

Show help:

```bash
c2n --help
c2n sync --help
```

Upload recent generations for one account:

```bash
c2n sync --account personal@example.com --limit 100
```

Process every configured account sequentially:

```bash
c2n sync
```

Replay non-uploaded items from the local database (last 3 days):

```bash
c2n sync --from-history
```

Replay all non-uploaded history:

```bash
c2n sync --from-history --all
```

Audit and repair uploaded items against Notion (last 3 days):

```bash
c2n sync --verify-history
```

Audit all uploaded history:

```bash
c2n sync --verify-history --all
```

Hide conversations after every image in them is verified uploaded:

```bash
c2n sync --delete-conversation
```

Preview configured library-query deletion after upload:

```bash
c2n sync --delete-library
```

Delete configured library queries after upload:

```bash
c2n sync --delete-library --yes
```

Preview deletion of the entire ChatGPT image library after upload:

```bash
c2n sync --delete-library-all
```

Delete the entire ChatGPT image library after upload:

```bash
c2n sync --delete-library-all --yes
```

Check account cooldowns:

```bash
c2n account-status --timezone Asia/Singapore
```

Clean the output directory while preserving the database:

```bash
c2n clean-output-path
```

Back up and restore the database:

```bash
c2n backup-db ./backups/chatgpt.db
c2n restore-db ./backups/chatgpt.db
```

Dry-run a library deletion, then actually delete:

```bash
c2n delete-library "old prompt"
c2n delete-library "old prompt" --yes
c2n delete-library --yes          # all library images
```

## Behavior and safety notes

### Local state

- The SQLite database lives at `output/chatgpt.db`.
- Images default to `output/images/`.
- A relative `--image-folder` is resolved under `output/`; absolute paths are
  allowed.
- `uploaded_at` is only written after a successful Notion page creation, or
  after a remote Notion check confirms the page already exists.
- `--check-notion-api` is accepted for CLI compatibility; remote checks are
  enabled by default.
- `--no-check-notion-api` trusts the local `uploaded_at` marker. The default is
  to query Notion to prevent duplicates when local state is stale.
- `sync` always uploads and requires Notion credentials and a database ID.

### Conversation deletion

`--delete-conversation` is deliberately conservative:

- It is only available on `sync`.
- It is ignored in history replay mode.
- A conversation is hidden only when every image in it was uploaded or verified
  already uploaded.
- If any image fails, the conversation is kept.
- A 404 response containing `conversation_deleted` is treated as success.
- After a confirmed deletion, that conversation's local image files are
  removed. If deletion fails, the files are kept.

### Library deletion

- `delete-library` is a dry run unless `--yes` is supplied.
- `--max-concurrent` controls concurrent delete requests (default 10).
- `sync --delete-library` uses `delete_library_queries` from the config.
  `--delete-library-all` targets every library image.
- Post-run library cleanup is also a dry run unless `--yes` is supplied.
- `remove_all_library_images = true` makes `--delete-library` behave like
  `--delete-library-all`.
- Blank configured queries are ignored; delete all images explicitly with
  `--delete-library-all` or `remove_all_library_images = true`.

### Prompt metadata

- PNG files receive a `Prompt` `tEXt` chunk when the prompt is Latin-1-safe,
  otherwise an `iTXt` chunk for UTF-8.
- Existing `Prompt` chunks are replaced only when the prompt differs.
- Writes are atomic: a temporary file is persisted over the original only after
  the complete PNG has been encoded.
- Non-PNG files are still uploaded, but prompt metadata is skipped with a
  warning.

### Networking

- All HTTP requests use a 120-second timeout.
- Retries use `backon::ExponentialBuilder`: up to 10 attempts, 2 seconds minimum
  delay, 60 seconds maximum delay.
- 429 responses, 5xx responses, timeouts, connection errors, request errors,
  body errors, and decode errors are retried.
- `Retry-After` is honored and capped at 60 seconds.
- Notion file uploads are retried at the upload/page step. If Notion reports
  that a file was already uploaded (HTTP 400), the existing upload ID is reused.

### History modes

- `--from-history` uses local rows that are not marked uploaded.
- `--verify-history` includes uploaded rows and checks Notion directly; it is a
  shortcut for `--from-history --check-notion-api` in intent.
- `--all` removes the 3-day history window and requires `--from-history` or
  `--verify-history`.
- `--history-folder` is accepted for compatibility, but SQLite is the source of
  truth.
- `--timezone` is accepted for compatibility. `account-status` uses it for date
  boundaries; history filtering uses absolute time.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

The test suite includes unit tests and wiremock integration tests for the
ChatGPT and Notion adapters and the sync pipeline's safety guarantees. CI runs
formatting, clippy, tests, and a release build.

## Project structure

```text
src/
├── adapters/     # filesystem, SQLite, ChatGPT, Notion
├── cli/          # clap definitions and command handlers
├── domain/       # shared data and config models
├── services/     # sync, history, image, account status
├── shared/       # constants, HTTP retry, time, reporting
├── config.rs
├── lib.rs
└── main.rs
tests/
├── chatgpt_api.rs
├── cli.rs
├── notion_api.rs
└── sync_pipeline.rs
docs/
├── requirement.md
├── technical.md
└── user_guide.md
```

## Documentation

- [Product requirements](docs/requirement.md): goals, users, and functional
  pillars.
- [Technical requirements](docs/technical.md): architecture, state machine,
  pipeline flow, and account cooldown logic.
- [User playbook](docs/user_guide.md): Notion setup, credential harvesting,
  configuration, commands, and scheduling.

## License

MIT
