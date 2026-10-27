# Product Requirement Document (PRD): c2n — ChatGPT to Notion Sync Tool

## 1. Document Control

- **Product Name**: c2n — ChatGPT to Notion Image Archiver & Sync Tool
- **Status**: Approved
- **Target Release**: v1.0.0
- **Implementation**: Rust CLI (see [technical.md](technical.md))

---

## 2. Executive Summary & Vision

### 2.1 The Problem

ChatGPT is a powerful creative assistant, but using it for consistent,
professional, or heavy image generation is constrained by the platform:

1. **Low Daily Generation Quotas**: ChatGPT imposes strict limits on daily and
   hourly image generations. Free and basic tiers are especially restrictive,
   and premium subscriptions are not always affordable.
2. **Non-Deterministic Iteration**: Image generation is inherently
   non-deterministic. Producing a desired result often requires several
   attempts, prompt modifications, and visual variations. A user can exhaust a
   daily quota in minutes while iterating.
3. **Multi-Account Fragmentation**: Power users pool multiple free or basic
   accounts to increase capacity. This scatters history, images, and prompts
   across separate accounts and makes manual organization difficult.
4. **Context Decoupling**: Downloaded images do not retain prompt metadata
   automatically, separating the final artwork from the exact text that created
   it.
5. **Manual Sync Overhead**: Downloading, organizing, and cataloging dozens of
   images and prompt iterations across multiple account histories is tedious
   and error-prone.

### 2.2 The Solution

A lightweight, high-performance, local-first CLI that consolidates image
generation histories from multiple ChatGPT accounts into one Notion database.

#### Strategic Architecture Decisions

- **Why Notion?** Notion's free tier does not impose a quantity limit on image
  uploads (as of May 2026). The practical constraint is the per-file size limit
  of under 5 MB. ChatGPT outputs commonly average 1.5–2.0 MB, making Notion a
  suitable permanent, searchable gallery.
- **Strict CLI-Only Architecture**: The tool deliberately avoids GUIs and web
  dashboards. It remains lightweight, scriptable, easy to schedule with cron or
  systemd, and optimized for speed and a low memory footprint.
- **Rust for Safety**: The implementation uses Rust's type system and ownership
  model to make filesystem, database, and network side effects explicit. This is
  especially important because the tool handles credentials and destructive
  cleanup operations.

The tool crawls configured profiles, downloads localized high-resolution
images, injects the original prompt into PNG metadata, uploads images to Notion
as permanent native files, and safely cleans up completed conversations only
after verifying that the corresponding images are present in Notion.

---

## 3. Product Goals & Objectives

### 3.1 Business & User Goals

- **Context Preservation**: Every generated image remains linked to its
  generation prompt, both in local file metadata and in Notion.
- **Ease of Retrieval**: Centralize assets in Notion so prompts are searchable
  and images can be viewed in gallery or board layouts.
- **Workflow Efficiency**: Reduce synchronization to a single terminal
  invocation.
- **Resilience**: Never upload duplicate files and never crash permanently due
  to typical network instability.

### 3.2 Key Success Metrics

- **Zero Prompt Loss**: 100% of synced images carry prompt context in local file
  metadata and in Notion.
- **Zero Duplicates**: No duplicate Notion pages are created for the same image.
- **High Recovery Rate**: The system resumes immediately after mid-run
  interruptions.

---

## 4. User Personas

### 4.1 The Creative Director / AI Artist

- **Needs**: Generates dozens of images a day and needs a searchable database of
  prompt histories to iterate on visual concepts.
- **Pain**: Loses the exact wording of prompts that produced good results and
  dislikes manually organizing local folders.

### 4.2 The Budget-Conscious Creator / Limit Pooler

- **Needs**: Generates multiple high-quality images daily without expensive
  premium subscriptions, using several free or basic accounts to pool quotas.
- **Pain**: Non-deterministic generation requires many attempts and rapidly
  exhausts a single account. Managing history, downloads, and prompts across
  three or more accounts manually is exhausting.

### 4.3 The Automation Enthusiast / Productivity Hacker

- **Needs**: A hands-off, cron-ready tool that keeps a Notion workspace up to
  date in the background.
- **Pain**: Does not want a complex web app or third-party cloud hosting and
  prefers a private, local-first utility.

---

## 5. Scope & Feature Requirements

The product scope is divided into these functional pillars:

```text
┌────────────────────────────────────────────────────────┐
│             ChatGPT to Notion Sync Pillars             │
├─────────────┬─────────────┬─────────────┬──────────────┤
│ Multi-Acct  │ Local Asset │ Notion Sync │ Conversation │
│   Profile   │ Enrichment  │ Deduplication│  Cleanup &  │
│ Management  │ (Metadata)  │  Pipeline   │   Archiving  │
└─────────────┴─────────────┴─────────────┴──────────────┘
```

### 5.1 Multi-Account & Profile Configuration

- Support simultaneous configuration for multiple independent ChatGPT profiles.
- Allow global fallback values (folders, user agent, cookie) with per-profile
  database and directory overrides.
- Keep authorization tokens and account secrets local to the host machine.
- Require every `[accounts.<key>]` key to be a valid email address. Loading a
  config with a non-email key must fail with a clear validation error.
- Locate `config.toml` in the platform config dir, overridable via
  `C2N_CONFIG_DIR` (development). Store data in the platform data dir
  (overridable via `C2N_DATA_DIR`).

### 5.2 Local Asset Download & Context Enrichment

- Fetch recent image generation records from ChatGPT.
- Download full-resolution image assets to a structured local directory.
- Inject the generation prompt directly into PNG metadata (`tEXt` for Latin-1
  prompts, `iTXt` for UTF-8 prompts).
- Make metadata injection idempotent: if the file already contains the correct
  prompt, skip disk writes.
- Write metadata atomically so an interrupted run cannot leave a truncated PNG.
- Always synchronize enriched images to Notion; there is no download-only mode.

### 5.3 Notion Synchronization Pipeline

- Publish downloaded images to a user-defined Notion database as new pages.
- Set the page title to the file name.
- Render the prompt as a clean, readable Markdown block in the page body.
- Always append a labeled `**Email:**` line using the account key. There is no
  toggle to disable this.
- Use safe multi-part file uploads so images are hosted natively on Notion
  rather than referencing transient URLs.
- Reuse a Notion `file_upload` id when retrying page creation to avoid
  re-uploading the same file.

### 5.4 Deduplication & Resiliency

- Verify whether an asset already exists in the local database or in Notion
  before initiating a new upload.
- Check Notion directly by default to prevent duplicates when local state is
  stale.
- Provide `--no-check-notion-api` to trust the local `uploaded_at` marker and
  skip remote checks.
- Support offline history replays from the local SQLite index.
- Retry rate limits, 5xx responses, timeouts, and connection failures with
  exponential backoff. Honor `Retry-After`, capped at 60 seconds.

### 5.5 Post-Sync Cleanup

- Provide `--delete-conversation` to hide processed conversations from ChatGPT.
- Conversation deletion must be **fail-safe**: it is blocked unless every image
  in the conversation has been uploaded or verified already uploaded.
- Conversation deletion is only available on `sync`.
- Deletion is ignored during history replay (`--from-history` or
  `--verify-history`).
- A 404 response containing `conversation_deleted` is treated as success.
- After ChatGPT confirms deletion, remove that conversation's local image files.
  Keep the files if the delete request fails; ignore missing files and warn on
  other removal errors.

### 5.6 Library Maintenance

- Provide `library delete [QUERY]` to remove images from the ChatGPT library.
- The standalone command is a dry run by default and requires `--yes` for actual
  deletion.
- A blank query must be rejected rather than treated as “all images”.
- `--max-concurrent` controls concurrent delete requests.

### 5.7 Database Maintenance

- Provide `db backup <TARGET>` using the SQLite backup API for a consistent
  snapshot even while other processes write to the database.
- Provide `db restore <SOURCE>` using the SQLite backup API into the live
  connection rather than replacing the database file underneath open handles.
- Provide `img clear` to remove images under `output/images/` while preserving
  the database, WAL/SHM/journal files, `.gitkeep`, and all other output.

---

## 6. Non-Functional Requirements

### 6.1 Reliability & Resilience

- **Exponential Backoff**: Automatically retry failed network requests for both
  ChatGPT and Notion with progressive delays.
- **Transaction Safety**: Update local state incrementally so an interrupted run
  can resume at the failed item without corrupting the database.
- **Atomic File Writes**: Download images and rewrite PNG metadata through
  temporary files that are persisted only after a complete write.

### 6.2 Configuration Validation

- **Account Key Format**: Every account key must be a valid email address.
- **Required Credentials**: Fail with a clear error when required values are
  missing, before making network requests.
- **Database ID Validation**: Reject empty or implausibly short Notion database
  IDs.

### 6.3 Privacy & Data Sovereignty

- **Local-First Design**: No intermediary servers or cloud proxies. Downloads,
  credential handling, and API requests occur directly between the user's
  machine and ChatGPT or Notion.
- **Configuration Safety**: Sensitive credentials must never be exported,
  committed, or logged.
- **Log Redaction**: HTTP error messages strip query strings and fragments.

### 6.4 Performance & Modes

- **Sequential Sync Mode**: Process conversations one at a time end-to-end with
  a configurable delay. This is the only mode and is designed to avoid
  ChatGPT's strict rate limits (HTTP 429).
- **Low Memory Footprint**: Stream downloads and multipart uploads rather than
  loading entire images into memory.

---

## 7. Future Considerations (Out of Scope)

- **Web UI Dashboards / Desktop GUIs**: Strictly out of scope. The product must
  remain a pure command-line utility.
- **Browser Extensions or GUI Integrations**: Strictly out of scope.
  Authorization and sync must remain terminal-driven.
- **Multi-Engine CLI Support**: Archiving generations from other platforms
  (Midjourney, direct DALL·E API pipelines, local Stable Diffusion libraries)
  via additional commands.
