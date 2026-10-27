# User Playbook & Deployment Guide

Welcome to the **c2n** user playbook. This guide covers Notion setup, ChatGPT
credential harvesting, configuration, command usage, and scheduled runs.

---

## 1. Notion Workspace Connection & Database Setup

To sync images, create a private Notion integration and share a target database
with it.

### Step 1: Create a Notion Integration

1. Open the [Notion Integrations Portal](https://www.notion.so/my-integrations).
2. Click **+ New Integration**.
3. Choose the target workspace, name it **"ChatGPT Sync Tool"**, and grant:
   - **Read Content**
   - **Update Content**
   - **Insert Content**
4. Save the integration and copy the **Internal Integration Token**
   (for example, `secret_...`).

### Step 2: Create or Choose a Database

The database must expose two properties used by `c2n`:

| Property | Type | Purpose |
| :--- | :--- | :--- |
| `Name` | Title | Page title; set to the image file name. |
| `Image` | Files | Receives the native Notion file upload. |

If your title property is not named `Name`, rename it or create a compatible
database. The integration must be able to read and write both properties.

### Step 3: Link the Integration to the Database

1. Open the target database in Notion.
2. Click the three dots `...` in the top-right corner.
3. Click **Add connections** (or **Connect to**) and select your integration.
4. Confirm access.
5. Copy the **Database ID** from the browser URL:
   - URL format: `https://www.notion.so/<long_hash_1>?v=<long_hash_2>`
   - The database ID is the 32-character string in `<long_hash_1>` before the
     `?v=` view parameter.

See the [official Notion connection guide](https://developers.notion.com/guides/get-started/overview#connection-types)
for more detail on connection types.

---

## 2. Acquiring ChatGPT Credentials

`c2n` fetches generation records from your ChatGPT account history and therefore
needs your private session credentials.

### 2.1 Authorization Token

1. Open your browser and log in to [https://chatgpt.com](https://chatgpt.com).
2. Open Developer Tools (`F12`, or `Ctrl + Shift + I` / `Cmd + Opt + I`).
3. Go to the **Network** tab and filter by **Fetch/XHR**.
4. Click an authenticated request such as `conversations?offset=0&limit=20` or
   `bootstrap`.
5. In **Request Headers**, locate `Authorization`.
6. Copy the token **without** the leading `Bearer `.

Example header:

```text
Authorization: Bearer eyJhbGciOiJSUzI1...
```

Value to copy:

```text
eyJhbGciOiJSUzI1...
```

### 2.2 User-Agent

In the same request headers, copy the `User-Agent` value:

```text
Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36...
```

The user agent is normally configured once under `[shared]` and reused by every
account.

### 2.3 Session Cookie (Base64)

ChatGPT's backend also expects the session cookie. Copy the raw `Cookie` header
value and encode it as Base64.

Raw cookie example:

```text
oai-did=...; __Secure-next-auth.session-token=...
```

Linux / macOS:

```bash
printf '%s' 'YOUR_RAW_COOKIE_STRING' | base64 -w 0
```

macOS without `-w`:

```bash
printf '%s' 'YOUR_RAW_COOKIE_STRING' | base64
```

Windows PowerShell:

```powershell
[Convert]::ToBase64String(
  [System.Text.Encoding]::UTF8.GetBytes("YOUR_RAW_COOKIE_STRING")
)
```

Store the encoded value in `cookie_string_base64`. Keep it secret.

---

## 3. Build and Configure

### 3.1 Build

```bash
cargo build --release
```

The binary is `target/release/c2n`. Optionally copy it to a directory on your
`PATH`:

```bash
install -m 755 target/release/c2n ~/.local/bin/c2n
```

### 3.2 Create the Configuration

```bash
cp config.toml.example config.toml
```

`config.toml` contains secrets and is ignored by git. Do not commit it.

```toml
[shared]
user_agent = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36..."
cookie_string_base64 = "b2FpLWRpZD0..."
# Optional:
# image_folder = "~/Pictures/chatgpt"
# delete_library_queries = ["old prompt", "scratch"]
# remove_all_library_images = false
# conversation_delay = 2.0

[notion]
api_key = "secret_yourNotionIntegrationTokenHere"
database_id = "yourDefaultNotionDatabaseIDHere"
# version = "2025-09-03"

# Account keys must be valid email addresses. The key is written into each
# uploaded Notion page body as the **Email:** line.
[accounts."personal@example.com"]
authorization_token = "eyJhbGciOiJSUzI1..."

[accounts."work@example.com"]
authorization_token = "eyJhbGciOiJSUzI1_other..."
# cookie_string_base64 = "b3RoZXJfY29va2ll..."
# notion_database_id = "optional_override_database_id"
```

### 3.3 Resolution Rules

- `[shared]` values are fallbacks.
- Per-account `user_agent`, `cookie_string_base64`, `notion_database_id`,
  `history_folder`, `image_folder`, `delete_library_queries`,
  `remove_all_library_images`, and `conversation_delay` override the shared
  values. Blank per-account strings fall back to the shared value.
- `notion.api_key` is required for `sync`.
- `notion.database_id` is required for `sync` unless `--db-id` or
  `[shared].notion_database_id` supplies one.
- Every account key must be a valid email address.
- `--config <PATH>` selects a different configuration file.

---

## 4. Command Line Reference

All examples assume `c2n` is on your `PATH`. During development, replace `c2n`
with `cargo run --`.

```bash
c2n --help
c2n sync --help
c2n account-status --help
c2n delete-library --help
```

### 4.1 `sync`

Synchronizes images from one or more ChatGPT accounts into Notion.

```bash
c2n sync [OPTIONS]
```

Options:

- `--account <EMAIL>`: Process only this configured account. Defaults to all
  accounts, sequentially.
- `--limit <INT>`: Maximum number of recent image generations to fetch from the
  live API. Default `100`.
- `--image-folder <PATH>`: Image output folder. A relative path is resolved
  under `output/`; an absolute path is used as-is.
- `--db-id <ID>`: Override the configured Notion database ID.
- `--delete-conversation`: Hide a conversation in ChatGPT after every image in
  it is verified uploaded, then remove that conversation's local image files.
  Ignored in history replay modes.
- `--delete-library`: After upload, delete library images matching
  `delete_library_queries` from the config. Without `--yes`, this only counts
  and reports the matching images.
- `--delete-library-all`: After upload, delete all library images. Without
  `--yes`, this only counts and reports them.
- `--yes`: Confirm post-run library deletion. Required for both
  `--delete-library` and `--delete-library-all` to actually delete anything.
- `--check-notion-api`: Accepted for CLI compatibility; remote checks are
  enabled by default.
- `--no-check-notion-api`: Trust the local `uploaded_at` marker and skip remote
  Notion checks. By default, `c2n` checks Notion to prevent duplicates.
- `--from-history`: Use local SQLite rows that are not marked uploaded, instead
  of fetching live data.
- `--verify-history`: Include uploaded rows and audit/repair them against
  Notion. Implies `--from-history` behavior and remote checks.
- `--all`: Use all history, not just the last three days. Requires
  `--from-history` or `--verify-history`.
- `--no-cache`: Fetch fresh live data and skip the local existence cache for the
  current discovery batch.
- `--timezone <IANA>`: Accepted for compatibility. `account-status` uses it for
  date boundaries; history filtering uses absolute time.
- `--conversation-delay <SECONDS>`: Delay between conversations. Defaults to the
  account/shared config value, or `2.0`.
- `--fail-log <PATH>`: Append failed images to a JSONL log.
- `-v`, `--verbose`: Show per-file decisions and skips.
- `-q`, `--quiet`: Show only warnings, errors, and the final summary.
- `--history-folder <PATH>`: Accepted for compatibility; SQLite is the source of
  truth.

Examples:

```bash
# One account, recent 100 items
c2n sync --account personal@example.com --limit 100

# Every configured account
c2n sync

# Replay non-uploaded history from the last 3 days
c2n sync --from-history

# Replay all non-uploaded history
c2n sync --from-history --all

# Audit and repair uploaded history against Notion
c2n sync --verify-history --all

# Hide conversations only after verified uploads
c2n sync --delete-conversation

# Preview post-run library deletion (dry run)
c2n sync --delete-library

# Actually delete configured library queries after upload
c2n sync --delete-library --yes

# Log failures for later inspection
c2n sync --fail-log output/failures.jsonl
```

### 4.2 `account-status`

Shows daily usage and 24-hour cooldown state for each account.

```bash
c2n account-status --timezone Asia/Singapore
```

Options:

- `--timezone <IANA>`: Timezone used for “today” and “yesterday” boundaries.
  Defaults to the system timezone, then UTC.

Output columns:

- **Account**
- **Next Wait**: when the oldest active generation leaves the 24-hour window.
- **Next Cooldown**: time until `Next Wait`.
- **Fully Ready In**: when the newest active generation leaves the window.
- **Total Wait**: span between the first and last active generation.
- **Ready Generate?**: `✅`, `⚠️ (x/y to wait)`, or `❌ (x/y to wait)`.

### 4.3 `clean-output-path`

Removes downloaded images and other temporary entries under `output/` while
preserving:

- `output/chatgpt.db`
- `output/chatgpt.db-wal`
- `output/chatgpt.db-shm`
- `output/chatgpt.db-journal`
- `output/.gitkeep`

```bash
c2n clean-output-path
```

### 4.4 `backup-db`

Copies the local database to a target path using the SQLite backup API. This
produces a consistent snapshot even while another process writes to the
database.

```bash
c2n backup-db ./backups/chatgpt.db
c2n backup-db /home/user/backups/chatgpt.db
```

Parent directories are created automatically.

### 4.5 `restore-db`

Restores the local database from a backup using the SQLite backup API into the
live connection. The target database is not replaced underneath open handles.

```bash
c2n restore-db ./backups/chatgpt.db
c2n restore-db /home/user/backups/chatgpt.db
```

Stop scheduled sync jobs before restoring to avoid interleaving writes.

### 4.6 `delete-library`

Removes images from the ChatGPT image library.

```bash
c2n delete-library [QUERY] [OPTIONS]
```

Arguments and options:

- `QUERY`: Optional search query. Omit to target all images. A blank query is
  rejected; use no query when you intentionally want all images.
- `--account <EMAIL>`: Process one account. Defaults to all accounts.
- `--max-concurrent <INT>`: Concurrent delete requests. Default `10`.
- `--yes`: Actually delete. Without this flag, the command only counts matching
  images and exits.

Actual deletion shows a live progress indicator with the number of images
deleted so far.

Examples:

```bash
# Dry run: count images matching the query
c2n delete-library "old prompt"

# Delete images matching the query
c2n delete-library "old prompt" --yes

# Delete all library images
c2n delete-library --yes
```

---

## 5. Rate Limiting & 429 Handling

ChatGPT may return HTTP 429 (Too Many Requests). `c2n` combines sequential
processing with automatic retries.

### 5.1 Sequential Processing

The only execution mode processes conversations one at a time, end to end:

1. Fetch conversation details once per conversation.
2. Group images from the same conversation.
3. Download, embed the prompt, and upload each image.
4. Wait `--conversation-delay` seconds before the next conversation.

```bash
c2n sync --conversation-delay 2.0
```

### 5.2 Retry Policy

- Up to 10 attempts per request.
- Exponential backoff with a 2-second minimum and 60-second maximum delay.
- Retries HTTP 429, HTTP 5xx, timeouts, connection errors, and request/body
  errors.
- Honors `Retry-After` (seconds or HTTP date) and caps it at 60 seconds.

Retry diagnostics are emitted through `tracing`. Set `RUST_LOG` to override the
default filter:

```bash
RUST_LOG=c2n=debug c2n sync
```

`--verbose` enables per-file output; `--quiet` suppresses non-warning output.

---

## 6. Automated Scheduling

### 6.1 Cron

Run a sync every hour:

```cron
0 * * * * cd /home/john/tmp/c2n && /home/john/.local/bin/c2n sync >> /home/john/tmp/c2n/output/cron.log 2>&1
```

Use absolute paths in cron entries. Ensure the user running cron can read
`config.toml` and write to `output/`.

### 6.2 systemd User Timer

Create `~/.config/systemd/user/c2n-sync.service`:

```ini
[Unit]
Description=c2n ChatGPT to Notion Sync
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
WorkingDirectory=/home/john/tmp/c2n
ExecStart=/home/john/.local/bin/c2n sync
```

Create `~/.config/systemd/user/c2n-sync.timer`:

```ini
[Unit]
Description=Run c2n sync every hour

[Timer]
OnCalendar=hourly
Persistent=true

[Install]
WantedBy=timers.target
```

Enable it:

```bash
systemctl --user daemon-reload
systemctl --user enable --now c2n-sync.timer
systemctl --user list-timers c2n-sync.timer
```

Inspect logs:

```bash
journalctl --user -u c2n-sync.service -f
```

---

## 7. Safety Checklist

- Keep `config.toml` private. It contains ChatGPT cookies and Notion tokens.
- Use `c2n sync --from-history` for a local replay trial run.
- Use `--verify-history` after restoring a database or changing Notion
  databases.
- Leave `--check-notion-api` enabled unless you intentionally trust local state.
- Test library deletion with the default dry run before adding `--yes`.
- Back up `output/chatgpt.db` before large restores or database migrations.
- Use `--fail-log` in unattended jobs so failures can be reviewed later.
