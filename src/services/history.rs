//! History retrieval and download orchestration.

use std::collections::HashSet;
use std::path::Path;

use crate::adapters::chatgpt::{
    ChatGptApi, ImageGenItem, get_prompt_from_image_node_in_conversation,
};
use crate::adapters::sqlite::{GenerationStore, GetGenerationsOptions, StoreError};
use crate::domain::models::ChatGptImageGeneration;
use crate::shared::http::HttpError;
use crate::shared::time::{TimeError, timestamp_to_utc_iso};

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    Time(#[from] TimeError),
}

pub fn get_uploaded_generation_ids(
    store: &dyn GenerationStore,
    account: &str,
) -> Result<HashSet<String>, StoreError> {
    store.get_uploaded_ids(account)
}

pub fn mark_generations_uploaded(
    store: &dyn GenerationStore,
    account: &str,
    generation_ids: &HashSet<String>,
) -> Result<(), StoreError> {
    store.mark_uploaded(account, generation_ids)
}

pub fn load_image_generations(
    store: &dyn GenerationStore,
    account: &str,
    include_uploaded: bool,
    keep_days: Option<i64>,
) -> Result<Vec<ChatGptImageGeneration>, StoreError> {
    store.get_generations(
        account,
        &GetGenerationsOptions {
            include_uploaded,
            keep_days,
            ids_filter: None,
        },
    )
}

pub async fn download_image(
    api: &dyn ChatGptApi,
    url: &str,
    file_path: &Path,
) -> Result<(), HttpError> {
    api.download_image(url, file_path).await
}

/// Build local generation records for one conversation.
///
/// Conversation details are fetched at most once, and only when at least one
/// image is not already present in the local database.
pub async fn fetch_generations_for_conversation(
    api: &dyn ChatGptApi,
    conversation_id: &str,
    image_items: &[ImageGenItem],
    existing_ids: &HashSet<String>,
) -> Result<Vec<ChatGptImageGeneration>, HistoryError> {
    let new_items: Vec<&ImageGenItem> = image_items
        .iter()
        .filter(|item| !existing_ids.contains(&item.id))
        .collect();
    if new_items.is_empty() {
        return Ok(Vec::new());
    }

    let detail = api.get_conversation_details(conversation_id).await?;
    let mut generations = Vec::with_capacity(new_items.len());
    for item in new_items {
        let prompt = get_prompt_from_image_node_in_conversation(
            &detail,
            &item.message_id,
            &item.asset_pointer,
        )
        .unwrap_or_default();
        let created_at = timestamp_to_utc_iso(item.created_at)?;
        generations.push(ChatGptImageGeneration {
            created_at,
            id: item.id.clone(),
            conversation_id: conversation_id.to_owned(),
            message_id: item.message_id.clone(),
            asset_pointer: item.asset_pointer.clone(),
            url: item.url.clone(),
            prompt,
        });
    }

    Ok(generations)
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use reqwest::header::HeaderMap;
    use serde_json::json;

    use super::*;
    use crate::adapters::chatgpt::{DeleteConversationResult, LibraryImagesResponse};
    use crate::shared::verbosity::Reporter;

    struct MockApi {
        headers: HeaderMap,
        detail: serde_json::Value,
    }

    #[async_trait]
    impl ChatGptApi for MockApi {
        fn headers(&self) -> &HeaderMap {
            &self.headers
        }

        async fn get_conversation_details(
            &self,
            _conversation_id: &str,
        ) -> Result<serde_json::Value, HttpError> {
            Ok(self.detail.clone())
        }

        async fn delete_conversation(
            &self,
            _conversation_id: &str,
        ) -> Result<DeleteConversationResult, HttpError> {
            Ok(DeleteConversationResult::Deleted)
        }

        async fn get_image_generations(
            &self,
            _limit: usize,
        ) -> Result<Vec<ImageGenItem>, HttpError> {
            Ok(Vec::new())
        }

        async fn download_image(&self, _url: &str, _destination: &Path) -> Result<(), HttpError> {
            Ok(())
        }

        async fn get_library_images(
            &self,
            _query: Option<&str>,
            _cursor: Option<&str>,
        ) -> Result<LibraryImagesResponse, HttpError> {
            Ok(LibraryImagesResponse::default())
        }

        async fn delete_library_file(
            &self,
            _item: &crate::adapters::chatgpt::LibraryItem,
        ) -> Result<Vec<serde_json::Value>, HttpError> {
            Ok(Vec::new())
        }

        async fn count_library_images(&self) -> Result<usize, HttpError> {
            Ok(0)
        }

        async fn count_library_images_by_query(
            &self,
            _query: Option<&str>,
        ) -> Result<usize, HttpError> {
            Ok(0)
        }

        async fn delete_library_images_by_query(
            &self,
            _query: Option<&str>,
            _max_concurrent: usize,
            _reporter: &Reporter,
        ) -> Result<usize, HttpError> {
            Ok(0)
        }
    }

    fn item(id: &str, created_at: f64) -> ImageGenItem {
        ImageGenItem {
            id: id.to_owned(),
            message_id: "image".to_owned(),
            asset_pointer: "pointer".to_owned(),
            conversation_id: "conv".to_owned(),
            url: "https://example.test/image.png".to_owned(),
            created_at,
        }
    }

    #[tokio::test]
    async fn builds_generations_and_skips_existing() {
        let api = MockApi {
            headers: HeaderMap::new(),
            detail: json!({
                "mapping": {
                    "user": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"parts": ["draw a cat"]}
                        },
                        "parent": null
                    },
                    "image": {
                        "message": {
                            "author": {"role": "assistant"},
                            "content": {"parts": [{"asset_pointer": "pointer"}]}
                        },
                        "parent": "user"
                    }
                }
            }),
        };

        let existing = HashSet::new();
        let generations =
            fetch_generations_for_conversation(&api, "conv", &[item("a", 0.0)], &existing)
                .await
                .unwrap();
        assert_eq!(generations.len(), 1);
        assert_eq!(generations[0].prompt, "draw a cat");
        assert_eq!(generations[0].created_at, "1970-01-01T00:00:00.000000Z");

        let existing = HashSet::from(["a".to_owned()]);
        let generations =
            fetch_generations_for_conversation(&api, "conv", &[item("a", 0.0)], &existing)
                .await
                .unwrap();
        assert!(generations.is_empty());
    }
}
