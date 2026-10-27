//! Sequential upload pipeline: one conversation at a time, end to end.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

use crate::adapters::chatgpt::{ChatGptApi, DeleteConversationResult, ImageGenItem};
use crate::adapters::filesystem::{Filesystem, FsError};
use crate::adapters::notion::{NotionApi, NotionError};
use crate::adapters::sqlite::{GenerationStore, GetGenerationsOptions, StoreError};
use crate::config::{Config, ConfigError};
use crate::domain::models::{ChatGptImageGeneration, ResolvedConfig};
use crate::services::history::{
    HistoryError, fetch_generations_for_conversation, get_uploaded_generation_ids,
    load_image_generations,
};
use crate::services::image::{ImageOutcome, add_prompt_to_image};
use crate::shared::constants::image_ext_from_url;
use crate::shared::http::HttpError;
use crate::shared::verbosity::{FailLogEntry, Reporter};

#[derive(Debug, Clone)]
pub struct UploadOptions {
    pub account: String,
    pub db_id: String,
    pub image_folder: Option<String>,
    pub upload_to_notion: bool,
    pub delete_conversation: bool,
    pub delete_library: bool,
    pub delete_library_all: bool,
    pub confirm_delete_library: bool,
    pub check_notion_api: bool,
    pub from_history: bool,
    pub include_uploaded: bool,
    pub limit: usize,
    pub keep_days: Option<i64>,
    pub no_cache: bool,
    pub conversation_delay: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    Notion(#[from] NotionError),
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("database id must not be empty")]
    MissingDatabaseId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessOutcome {
    Uploaded,
    AlreadyUploaded,
    DownloadedOnly,
}

impl ProcessOutcome {
    fn is_uploaded(self) -> bool {
        matches!(self, Self::Uploaded | Self::AlreadyUploaded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessStage {
    NotionCheck,
    Download,
    Prompt,
    Upload,
}

impl ProcessStage {
    fn label(self) -> &'static str {
        match self {
            Self::NotionCheck => "notion_check",
            Self::Download => "download",
            Self::Prompt => "prompt",
            Self::Upload => "upload",
        }
    }
}

#[derive(Debug)]
struct ProcessFailure {
    stage: ProcessStage,
    error: String,
}

impl ProcessFailure {
    fn new(stage: ProcessStage, error: impl std::fmt::Display) -> Self {
        Self {
            stage,
            error: error.to_string(),
        }
    }
}

enum ConversationPayload {
    History(Vec<ChatGptImageGeneration>),
    Api(Vec<ImageGenItem>),
}

/// Clears an active progress bar when the upload pipeline exits, including on
/// early errors, so a failed account cannot leave a stale bar behind.
struct ProgressGuard<'a> {
    reporter: &'a Reporter,
}

impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        self.reporter.clear_progress();
    }
}

/// Process conversations sequentially to minimize rate limits.
#[allow(clippy::too_many_arguments)]
pub async fn upload_to_notion_sequential(
    options: &UploadOptions,
    config: &Config,
    store: &dyn GenerationStore,
    filesystem: &Filesystem,
    chatgpt: &dyn ChatGptApi,
    notion: Option<&dyn NotionApi>,
    reporter: &Reporter,
) -> Result<(), UploadError> {
    if options.upload_to_notion && options.db_id.trim().is_empty() {
        return Err(UploadError::MissingDatabaseId);
    }

    let resolved = config.account(&options.account)?;
    store.init_db()?;

    let image_folder = filesystem.resolve_image_folder(
        options.image_folder.as_deref(),
        resolved.image_folder.as_deref(),
    )?;
    let mut uploaded_ids = get_uploaded_generation_ids(store, &options.account)?;

    let (conversations, mut existing_ids) = if options.from_history {
        let generations = load_image_generations(
            store,
            &options.account,
            options.include_uploaded,
            options.keep_days,
        )?;
        (group_history(generations), HashSet::new())
    } else {
        let items = chatgpt.get_image_generations(options.limit).await?;
        let existing_ids = if options.no_cache {
            HashSet::new()
        } else {
            let ids: HashSet<String> = items.iter().map(|item| item.id.clone()).collect();
            store.get_existing_ids(&options.account, &ids)?
        };
        (group_api(items), existing_ids)
    };

    if conversations.is_empty() {
        reporter.info("No generations found.");
        cleanup_library(options, resolved, chatgpt, reporter).await?;
        return Ok(());
    }

    let progress = ProgressBar::new(conversations.len() as u64);
    progress.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
        )
        .expect("valid progress template")
        .progress_chars("#>-"),
    );
    progress.set_message("processing conversations");
    reporter.set_progress(progress.clone());
    let _progress_guard = ProgressGuard { reporter };

    let total = conversations.len();
    for (index, (conversation_id, payload)) in conversations.into_iter().enumerate() {
        let generations = match payload {
            ConversationPayload::History(generations) => generations,
            ConversationPayload::Api(items) => {
                let new_generations = fetch_generations_for_conversation(
                    chatgpt,
                    &conversation_id,
                    &items,
                    &existing_ids,
                )
                .await?;
                if !new_generations.is_empty() {
                    store.upsert_generations(&options.account, &new_generations)?;
                    existing_ids.extend(
                        new_generations
                            .iter()
                            .map(|generation| generation.id.clone()),
                    );
                }

                let conversation_ids: HashSet<String> =
                    items.iter().map(|item| item.id.clone()).collect();
                let mut generations = store.get_generations(
                    &options.account,
                    &GetGenerationsOptions {
                        include_uploaded: true,
                        keep_days: None,
                        ids_filter: Some(conversation_ids),
                    },
                )?;
                generations.sort_by(|left, right| left.created_at.cmp(&right.created_at));
                generations
            }
        };

        if generations.is_empty() {
            reporter.verbose(format!(
                "⚠️  Conversation {conversation_id} has no indexed images; skipping"
            ));
            progress.inc(1);
            if options.conversation_delay > 0.0 && index + 1 < total {
                tokio::time::sleep(Duration::from_secs_f64(options.conversation_delay)).await;
            }
            continue;
        }

        let mut all_ok = true;
        let mut all_uploaded = true;
        let mut failed_count = 0;

        for generation in &generations {
            match process_one(
                generation,
                options,
                &image_folder,
                store,
                notion,
                chatgpt,
                &mut uploaded_ids,
                reporter,
            )
            .await
            {
                Ok(outcome) => {
                    if !outcome.is_uploaded() {
                        all_uploaded = false;
                    }
                }
                Err(failure) => {
                    all_ok = false;
                    all_uploaded = false;
                    failed_count += 1;
                    let file_name = file_name_for(generation);
                    reporter.error(format!("Failed to process {file_name}: {}", failure.error));
                    reporter.write_fail_log(&FailLogEntry::new(
                        &generation.id,
                        &generation.conversation_id,
                        &generation.url,
                        &generation.prompt,
                        failure.stage.label(),
                        file_name,
                        failure.error,
                    ));
                }
            }
        }

        if all_ok {
            reporter.add_stage("success", 1);
            if options.delete_conversation {
                if !options.from_history && options.upload_to_notion && all_uploaded {
                    match chatgpt.delete_conversation(&conversation_id).await {
                        Ok(result) => {
                            if reporter.is_verbose() {
                                let suffix = if result == DeleteConversationResult::AlreadyDeleted {
                                    " (already deleted)"
                                } else {
                                    ""
                                };
                                reporter.verbose(format!(
                                    "✅ Conversation {conversation_id} deleted{suffix}"
                                ));
                            }
                        }
                        Err(error) => reporter.error(format!(
                            "Failed to delete conversation {conversation_id}: {error}"
                        )),
                    }
                } else if reporter.is_verbose() {
                    reporter.verbose(format!(
                        "⚠️  Conversation {conversation_id} kept; not all images were verified uploaded"
                    ));
                }
            }
        } else {
            reporter.add_stage("failed", 1);
            if reporter.is_verbose() {
                reporter.verbose(format!(
                    "⚠️  Conversation {conversation_id} skipped, {failed_count} image(s) failed"
                ));
            }
        }

        progress.inc(1);
        if options.conversation_delay > 0.0 && index + 1 < total {
            tokio::time::sleep(Duration::from_secs_f64(options.conversation_delay)).await;
        }
    }

    reporter.clear_progress();
    if !reporter.is_verbose() {
        reporter.println(reporter.summary_line());
    }

    cleanup_library(options, resolved, chatgpt, reporter).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn process_one(
    generation: &ChatGptImageGeneration,
    options: &UploadOptions,
    image_folder: &Path,
    store: &dyn GenerationStore,
    notion: Option<&dyn NotionApi>,
    chatgpt: &dyn ChatGptApi,
    uploaded_ids: &mut HashSet<String>,
    reporter: &Reporter,
) -> Result<ProcessOutcome, ProcessFailure> {
    let file_name = file_name_for(generation);
    let file_path = image_folder.join(&file_name);
    let notion = if options.upload_to_notion {
        Some(notion.ok_or_else(|| {
            ProcessFailure::new(ProcessStage::Upload, "Notion client is unavailable")
        })?)
    } else {
        None
    };

    if let Some(notion) = notion {
        if uploaded_ids.contains(&generation.id) && !options.check_notion_api {
            reporter.verbose(format!("⏭️  {file_name} skipped, already uploaded"));
            return Ok(ProcessOutcome::AlreadyUploaded);
        }

        match notion
            .is_page_exists_in_db(&options.db_id, &file_name)
            .await
        {
            Ok(true) => {
                let ids = HashSet::from([generation.id.clone()]);
                store
                    .mark_uploaded(&options.account, &ids)
                    .map_err(|error| ProcessFailure::new(ProcessStage::NotionCheck, error))?;
                uploaded_ids.insert(generation.id.clone());
                reporter.verbose(format!("⏭️  {file_name} skipped, already in Notion"));
                return Ok(ProcessOutcome::AlreadyUploaded);
            }
            Ok(false) => {}
            Err(error) => {
                return Err(ProcessFailure::new(ProcessStage::NotionCheck, error));
            }
        }
    }

    if !file_path.exists() {
        chatgpt
            .download_image(&generation.url, &file_path)
            .await
            .map_err(|error| ProcessFailure::new(ProcessStage::Download, error))?;
    }

    match add_prompt_to_image(&file_path, &generation.prompt) {
        Ok(ImageOutcome::SkippedNotPng) => reporter.verbose(format!(
            "⚠️  {file_name} is not a PNG; prompt metadata was not embedded"
        )),
        Ok(_) => {}
        Err(error) => {
            return Err(ProcessFailure::new(ProcessStage::Prompt, error));
        }
    }

    if let Some(notion) = notion {
        notion
            .add_page_to_db(&options.db_id, &file_path, &generation.prompt)
            .await
            .map_err(|error| ProcessFailure::new(ProcessStage::Upload, error))?;
        let ids = HashSet::from([generation.id.clone()]);
        store
            .mark_uploaded(&options.account, &ids)
            .map_err(|error| ProcessFailure::new(ProcessStage::Upload, error))?;
        uploaded_ids.insert(generation.id.clone());
        reporter.verbose(format!("✅ {file_name} uploaded"));
        Ok(ProcessOutcome::Uploaded)
    } else {
        reporter.verbose(format!("⬇️  {file_name} downloaded; upload disabled"));
        Ok(ProcessOutcome::DownloadedOnly)
    }
}

async fn cleanup_library(
    options: &UploadOptions,
    resolved: &ResolvedConfig,
    chatgpt: &dyn ChatGptApi,
    reporter: &Reporter,
) -> Result<(), HttpError> {
    if !options.delete_library && !options.delete_library_all {
        return Ok(());
    }

    let remove_all = options.delete_library_all || resolved.remove_all_library_images;
    let queries: Vec<&str> = resolved
        .delete_library_queries
        .iter()
        .map(|query| query.trim())
        .filter(|query| !query.is_empty())
        .collect();
    if !remove_all && queries.is_empty() {
        reporter
            .warn("library deletion was requested, but no delete_library_queries are configured");
        return Ok(());
    }

    if !options.confirm_delete_library {
        if remove_all {
            let count = chatgpt.count_library_images().await?;
            reporter.println(format!(
                "[{}] Dry run: {count} library image(s) would be deleted. Pass --yes to delete.",
                options.account
            ));
        } else {
            for query in queries {
                let count = chatgpt.count_library_images_by_query(Some(query)).await?;
                reporter.println(format!(
                    "[{}] Dry run: {count} library image(s) would be deleted matching '{query}'. Pass --yes to delete.",
                    options.account
                ));
            }
        }
        return Ok(());
    }

    if remove_all {
        let deleted = chatgpt
            .delete_library_images_by_query(None, 10, reporter)
            .await?;
        reporter.info(format!("Deleted {deleted} library images"));
        return Ok(());
    }

    for query in queries {
        let deleted = chatgpt
            .delete_library_images_by_query(Some(query), 10, reporter)
            .await?;
        reporter.info(format!(
            "Deleted {deleted} library images matching '{query}'"
        ));
    }

    Ok(())
}

fn group_history(
    generations: Vec<ChatGptImageGeneration>,
) -> BTreeMap<String, ConversationPayload> {
    let mut conversations: BTreeMap<String, Vec<ChatGptImageGeneration>> = BTreeMap::new();
    for generation in generations {
        conversations
            .entry(generation.conversation_id.clone())
            .or_default()
            .push(generation);
    }
    conversations
        .into_iter()
        .map(|(conversation_id, generations)| {
            (conversation_id, ConversationPayload::History(generations))
        })
        .collect()
}

fn group_api(items: Vec<ImageGenItem>) -> BTreeMap<String, ConversationPayload> {
    let mut conversations: BTreeMap<String, Vec<ImageGenItem>> = BTreeMap::new();
    for item in items {
        conversations
            .entry(item.conversation_id.clone())
            .or_default()
            .push(item);
    }
    conversations
        .into_iter()
        .map(|(conversation_id, items)| (conversation_id, ConversationPayload::Api(items)))
        .collect()
}

fn file_name_for(generation: &ChatGptImageGeneration) -> String {
    format!(
        "{}{}",
        sanitize_file_stem(&generation.id),
        image_ext_from_url(&generation.url)
    )
}

/// Keep API-provided ids from escaping the image directory or containing
/// characters that are awkward in filenames and Notion page titles.
fn sanitize_file_stem(id: &str) -> String {
    let mut stem = String::with_capacity(id.len());
    for character in id.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            stem.push(character);
        } else {
            stem.push('_');
        }
    }
    if stem.is_empty() || stem == "." || stem == ".." {
        "_".to_owned()
    } else {
        stem
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation_with_id(id: &str) -> ChatGptImageGeneration {
        ChatGptImageGeneration {
            created_at: "2026-01-01T00:00:00.000000Z".to_owned(),
            id: id.to_owned(),
            conversation_id: "conversation".to_owned(),
            message_id: "message".to_owned(),
            asset_pointer: "pointer".to_owned(),
            url: "https://example.test/image.png".to_owned(),
            prompt: "prompt".to_owned(),
        }
    }

    #[test]
    fn file_names_cannot_escape_the_image_folder() {
        assert_eq!(
            file_name_for(&generation_with_id("../../etc/passwd")),
            ".._.._etc_passwd.png"
        );
        assert_eq!(file_name_for(&generation_with_id("..")), "_.png");
        assert_eq!(
            file_name_for(&generation_with_id("normal-id_1")),
            "normal-id_1.png"
        );
    }
}
