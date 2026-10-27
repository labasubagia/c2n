# Technical Requirements Document (TRD): c2n — ChatGPT to Notion Sync Tool

## 1. Introduction & Architectural Overview

This document translates the product goals in [requirement.md](requirement.md)
into the concrete architecture of the Rust implementation.

`c2n` is a local-first, rate-limit-resilient, scriptable CLI that acts as a
secure intermediary between ChatGPT generation history and a Notion database.

### Core Architecture Goals

- **Local Sovereignty**: Credentials, configuration, downloaded assets, and
  execution state live on the user's machine. No intermediary service is used.
- **Strict CLI Design**: No GUI. The binary is designed for terminal use, cron,
  and systemd timers.
- **Transactional Integrity**: Every state transition (discover → download →
  metadata → upload → cleanup) is tracked so interruptions can be resumed
  without duplicate uploads, resource leaks, or premature conversation deletion.
- **Safety First**: Destructive operations are gated by explicit verification
  and, for standalone library deletion, by `--yes`.

---

## 2. Technology Stack & Key Libraries

```text
┌────────────────────────────────────────────────────────┐
│                   Rust CLI Runtime                     │
├───────────────────┬───────────────────┬────────────────┤
│       Core        │    Networking     │  Persistence   │
├───────────────────┼───────────────────┼────────────────┤
│ Rust 2024 edition │ reqwest + rustls  │ SQLite (local) │
│ Tokio             │ backon retries    │ rusqlite       │
│ serde + toml      │ multipart/stream  │ backup API     │
└───────────────────┴───────────────────┴────────────────┘
```

1. **Language & Runtime**: Rust 2024 edition, Tokio multi-thread runtime.
2. **CLI**: `clap` derive with subcommands and typed options.
3. **HTTP**: `reqwest` with `rustls`, JSON, multipart, streaming, and query
   features. No OpenSSL dependency.
4. **Retries**: `backon::ExponentialBuilder`, up to 10 attempts, 2 s minimum
   delay, 60 s maximum delay. `Retry-After` is honored and capped at 60 s.
5. **Configuration**: `serde` + `toml`, with `deny_unknown_fields` and typed
   resolution of shared/account fallbacks.
6. **Persistence**: `rusqlite` with bundled SQLite and the backup API. WAL mode
   and a 10 s busy timeout are enabled.
7. **PNG metadata**: `img-parts` for `tEXt`/`iTXt` chunk editing; `tempfile` for
   atomic replacement.
8. **Presentation**: `comfy-table` for account-status tables, `indicatif` for
   progress, `tracing` for diagnostics, and a typed `Reporter` for user output.
9. **Email validation**: `email_address` for account-key validation.
10. **Time**: `chrono` + `chrono-tz` + `iana-time-zone` for timezone-aware
    account reporting and UTC-normalized timestamps.

### Module Layout

```text
src/
├── adapters/
│   ├── chatgpt.rs      # ChatGptApi trait + reqwest implementation
│   ├── filesystem.rs   # data dir, path resolution, cleaning
│   ├── notion.rs       # NotionApi trait + upload/page implementation
│   └── sqlite.rs       # GenerationStore/NotionCache + rusqlite implementation
├── cli/app.rs          # clap definitions and handlers
├── config.rs           # TOML loading, validation, account resolution
├── domain/models.rs    # config/data models and resolved contexts
├── services/
│   ├── account_status.rs
│   ├── history.rs
│   ├── image.rs
│   └── sync.rs
└── shared/
    ├── constants.rs
    ├── http.rs
    ├── time.rs
    └── verbosity.rs
```

Traits are defined at external boundaries so services can be tested with fakes:

- `ChatGptApi`
- `NotionApi`
- `GenerationStore`
- `NotionCache`

---

## 3. Local Datastore & State Machine

### 3.1 `image_generations`

| Column | Type | Key | Description |
| :--- | :--- | :--- | :--- |
| `id` | TEXT | Primary Key | Unique ChatGPT image generation identifier. |
| `created_at` | TEXT | - | RFC 3339 UTC timestamp with microseconds. |
| `conversation_id` | TEXT | - | ChatGPT conversation identifier. |
| `message_id` | TEXT | - | Generative node ID where the image was returned. |
| `asset_pointer` | TEXT | - | ChatGPT asset pointer used for prompt lookup. |
| `url` | TEXT | - | Remote image URL. |
| `prompt` | TEXT | - | Original prompt injected into the image and Notion. |
| `uploaded_at` | TEXT | - | RFC 3339 UTC timestamp when Notion upload was committed. Empty means pending. |
| `account` | TEXT | - | Account email key that owns the row. |

Indexes:

- `idx_generations_account(account)`
- `idx_generations_created_at(created_at)`
- `idx_generations_uploaded_at(uploaded_at)`

### 3.2 `cache_data_sources`

| Column | Type | Key | Description |
| :--- | :--- | :--- | :--- |
| `db_id` | TEXT | Primary Key | Notion database ID. |
| `data_sources_json` | TEXT | - | JSON array of `{ "id": "..." }` data sources. |
| `expires_at` | TEXT | - | RFC 3339 UTC expiry (24 hours). |

The cache prevents repeated `GET /v1/databases/{id}` calls during duplicate
checks. Expired rows are deleted lazily.

### 3.3 State Invariants

1. A row is marked uploaded **only** after a successful Notion page creation or
   a positive remote Notion duplicate check.
2. A conversation is hidden only when every image in it is in the uploaded or
   already-uploaded state.
3. Local image files are removed only after ChatGPT confirms the conversation
   was deleted.
4. Library deletion is explicit: both the standalone command and post-run
   cleanup require `--yes`.
5. All timestamps written by the tool are UTC RFC 3339 strings. Local time is
   used only for display and date-boundary calculations.

---

## 4. Feature Flows

### 4.1 Image Sync Pipeline (`sync`)

#### 4.1.1 Discovery

Two sources are supported:

- **Live API** (`default`): `GET /backend-api/my/recent/image_gen?limit=N`.
  Items are grouped by `conversation_id`. Conversation details are fetched at
  most once per conversation, and only when at least one image is not already
  indexed locally.
- **History replay** (`--from-history` or `--verify-history`): rows are read
  from SQLite. `--from-history` selects non-uploaded rows. `--verify-history`
  also includes uploaded rows so they can be audited against Notion. The default
  window is the last 3 days; `--all` removes it.

Prompt extraction walks the conversation mapping from the image node to the
nearest `user` message and takes its first text part. If the start node is
missing, the tool falls back to locating the node by `asset_pointer`.

#### 4.1.2 Per-Image Processing

```text
for each generation in conversation:
    if uploaded_at set and remote checks disabled:
        skip as already uploaded
    if Notion page with title == file name exists:
        mark uploaded_at
        skip as already uploaded

    if local image file is missing:
        download to a temporary file
        persist atomically

    inject prompt into PNG metadata (idempotent, atomic)

    create Notion file upload
    send file bytes
    create page with title, image, prompt Markdown, email
    mark uploaded_at
```

File names are `<generation id><extension>`, where the extension comes from the
source URL and defaults to `.png`.

#### 4.1.3 Conversation Outcome and Cleanup

Each conversation tracks `all_ok`, which is true when no image failed.
`--delete-conversation` sends
`PATCH /backend-api/conversation/{id}` with `{"is_visible": false}` only when:

```text
all_ok
&& !from_history
```

If any image fails, the conversation is kept. A 404 response containing
`conversation_deleted` is treated as success. After a successful delete, the
conversation's local image files are removed. If the delete request fails, the
files are kept. Missing files are ignored during removal; other filesystem
errors produce a warning and do not fail the run. SQLite rows are retained.

#### 4.1.4 Failure Logging

With `--fail-log <PATH>`, each failed image appends one JSON object per line:

```json
{
  "id": "...",
  "conversation_id": "...",
  "url": "...",
  "prompt": "...",
  "stage": "notion_check|download|prompt|upload",
  "file": "....png",
  "error": "...",
  "timestamp": "2026-..."
}
```

A failed image does not abort the whole run; the conversation is marked failed
and processing continues with the next conversation. A failure to fetch
conversation details or persist the database aborts the run.

#### 4.1.5 System Flowchart

```mermaid
flowchart TD
    Start([Start Sync]) --> Config[Load and validate config]
    Config --> Source{Live API or history?}
    Source -- Live --> Fetch[Fetch recent generations]
    Source -- History --> Load[Load rows from SQLite]
    Fetch --> Group[Group by conversation]
    Load --> Group

    Group --> Conv[Next conversation]
    Conv --> Details[Fetch conversation details if new images]
    Details --> Item[Next image]

    Item --> Local{uploaded_at set and no remote check?}
    Local -- Yes --> Skip[Skip as already uploaded]
    Local -- No --> Remote{Page exists in Notion?}
    Remote -- Yes --> Mark[Mark uploaded_at]
    Remote -- No --> Download{File exists?}

    Download -- No --> Get[Download atomically]
    Download -- Yes --> Prompt
    Get --> Prompt[Inject prompt into PNG]

    Prompt --> Create[Create upload, send file, create page]
    Create --> Mark

    Skip --> Outcome
    Mark --> Outcome
    Outcome{all_ok?}
    Outcome -- Yes --> Delete{Delete conversations enabled and not history?}
    Delete -- Yes --> Hide[Hide conversation and remove local images]
    Delete -- No --> Next
    Outcome -- No --> Keep[Keep conversation and log failure]
    Hide --> Next
    Keep --> Next
    Next --> Conv
    Conv -- Done --> Summary[Print summary and finish]
```

### 4.2 Notion Upload Details

1. `POST /v1/file_uploads` with `mode = single_part`, `filename`, and
   `content_type`.
2. `POST /v1/file_uploads/{id}/send` with multipart file bytes.
   - A `400` response is treated as “already uploaded”; the original upload ID
     is reused. This makes retries safe when the server committed the upload but
     the response was lost.
3. `POST /v1/pages` with:
   - `parent.database_id`
   - `properties.Name.title[0].text.content = <file name>`
   - `properties.Image.files[0] = { type: "file_upload", file_upload: { id } }`
   - `markdown` body (prompt normalized to LF):

     ~~~text
     **Prompt:**

     ```
     <normalized prompt>
     ```

     **Email:** <account email>
     ~~~

   - CRLF and CR are normalized to LF before building the body.
4. The page request is retried with the same upload ID, never by re-running the
   entire upload sequence.

Duplicate checks query each Notion data source:

```json
{
  "filter": {
    "and": [
      { "property": "Name", "rich_text": { "equals": "<file name>" } }
    ]
  }
}
```

A page is considered a duplicate when its `Name` title text exactly matches the
file name. If the database exposes no data sources, the duplicate check fails
closed instead of uploading without protection. Corrupt cached data-source JSON
is discarded and refreshed.

### 4.3 Account Status (`status`)

1. Resolve the requested IANA timezone, then fall back to the system timezone and
   finally UTC.
2. For each account, check whether any generations exist in SQLite.
   - No rows: the account is immediately `Ready`.
3. For today and yesterday:
   - Compute UTC bounds for the local calendar day.
   - `cooldown_threshold = now - 24h`.
   - Count total rows in the day and “active” rows created after the threshold.
4. Status:
   - `active_count == 0`: `Ready`.
   - `active_count == total`: `❌ (x/y to wait)`.
   - Otherwise: `⚠️ (x/y to wait)`.
5. Durations:
   - **Next Wait** = earliest active generation + 24h, displayed in local time.
   - **Next Cooldown** = next wait − now.
   - **Fully Ready In** = latest active generation + 24h − now.
   - **Total Wait** = latest active − earliest active.

Accounts without any data appear as ready in both tables. Accounts with data
but no activity on a given day appear only in the yesterday fallback row, which
is marked ready, matching the original tool's behavior.

### 4.4 Maintenance Commands

- **`img clear`**: removes entries under `output/` except the database,
  `chatgpt.db-wal`, `chatgpt.db-shm`, `chatgpt.db-journal`, and `.gitkeep`.
- **`db backup <TARGET>`**: uses `rusqlite::backup::Backup` to produce a
  consistent snapshot. Parent directories are created.
- **`db restore <SOURCE>`**: uses the SQLite backup API into the live
  connection, then runs `PRAGMA wal_checkpoint(TRUNCATE)`. This avoids replacing
  the database file underneath open handles.
- **`library delete [QUERY]`**: dry-runs by default and prints a count. With
  `--yes`, deletes matching images concurrently and prints the remaining count.
  A blank query is rejected instead of being treated as “all images”. Deletes
  use the ChatGPT `delete_stream` endpoint, parse SSE `data:` events, and show a
  live progress indicator.

---

## 5. Non-Functional Requirements & Design Patterns

### 5.1 Idempotency & Replay

- SQLite is the local source of truth.
- A crash at any point resumes from the failed item on the next invocation.
- A local row can be marked uploaded after a positive Notion duplicate check,
  repairing lost state.
- PNG prompt injection is idempotent and skips disk writes when the prompt is
  already present.

### 5.2 Network Rate-Limiting Resiliency

- `retry_http` wraps every API operation with `backon`.
- Retryable conditions:
  - HTTP 429
  - HTTP 5xx
  - Timeouts
  - Connection errors
  - Request/body/decode errors
- `Retry-After` is parsed from either seconds or an RFC 2822 date and capped at
  60 seconds. It cannot extend the retry budget past 10 total attempts.
- Retries are logged at `warn` level through `tracing`.

### 5.3 Memory and Disk Footprint

- Downloads stream response chunks into a temporary file.
- Notion multipart uploads use `Part::file`, avoiding loading the whole image
  into memory.
- PNG metadata edits parse only chunk headers and write through a temporary
  file.
- `img clear` preserves the database and WAL/SHM/journal files.

### 5.4 Path Safety

- The data dir is the platform data dir (`$C2N_DATA_DIR` overrides it).
- Images are written to `<data_dir>/images`.
- `..` components that would escape the data dir are rejected.
- Symlinks inside the data dir are resolved and rejected if they escape it,
  including dangling symlinks whose target does not exist yet.
- Downloaded filenames sanitize API-provided IDs so they cannot contain path
  separators.

### 5.5 Testing Strategy

Tests target public behavior:

- Unit tests cover configuration validation, SQLite state transitions, timestamp
  formatting, PNG metadata round-trips, prompt extraction, and retry
  classification.
- Integration tests use `wiremock` for the ChatGPT and Notion adapters:
  - mixed string/number IDs
  - conversation deletion 404 semantics
  - atomic downloads
  - library pagination and SSE deletion
  - `Retry-After` handling and the 10-attempt cap
  - multipart upload and page payload shape
  - page-creation retry with the same upload ID
  - 400 “already uploaded” fallback
  - data-source cache reuse and fail-closed empty data sources
- The sync pipeline is tested with injected `ChatGptApi`/`NotionApi` fakes and
  a real temporary SQLite database:
  - a verified upload marks the row, deletes the conversation, and removes the
    local image
  - a failed conversation delete keeps the local image
  - a failed upload keeps the local image
  - history replay keeps the local image and never deletes conversations
- SQLite tests cover corrupt cache recovery and backup/restore through the live
  connection.
- CLI tests cover argument conflicts without loading configuration.
- The suite must remain deterministic and green under `cargo test`.
