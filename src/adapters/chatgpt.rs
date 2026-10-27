//! ChatGPT backend API adapter.

use std::path::Path;

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::header::HeaderMap;
use reqwest::{Client, Method, StatusCode};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::domain::models::ProviderContext;
use crate::shared::constants::CHATGPT_BASE_URL;
use crate::shared::http::{HttpError, build_client, ensure_success, retry_http};
use crate::shared::verbosity::Reporter;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteConversationResult {
    Deleted,
    AlreadyDeleted,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageGenItem {
    #[serde(deserialize_with = "deserialize_string")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub message_id: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub asset_pointer: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub conversation_id: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub url: String,
    #[serde(deserialize_with = "deserialize_f64")]
    pub created_at: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LibraryItem {
    #[serde(deserialize_with = "deserialize_string")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub file_id: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub parent_directory_id: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LibraryImagesResponse {
    #[serde(default)]
    pub items: Vec<LibraryItem>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ImageGenerationsResponse {
    #[serde(default)]
    items: Vec<ImageGenItem>,
}

#[async_trait]
pub trait ChatGptApi: Send + Sync {
    fn headers(&self) -> &HeaderMap;

    async fn get_conversation_details(&self, conversation_id: &str) -> Result<Value, HttpError>;

    async fn delete_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<DeleteConversationResult, HttpError>;

    async fn get_image_generations(&self, limit: usize) -> Result<Vec<ImageGenItem>, HttpError>;

    async fn download_image(&self, url: &str, destination: &Path) -> Result<(), HttpError>;

    async fn get_library_images(
        &self,
        query: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<LibraryImagesResponse, HttpError>;

    async fn delete_library_file(&self, item: &LibraryItem) -> Result<Vec<Value>, HttpError>;

    async fn count_library_images(&self) -> Result<usize, HttpError>;

    async fn count_library_images_by_query(&self, query: Option<&str>) -> Result<usize, HttpError>;

    async fn delete_library_images_by_query(
        &self,
        query: Option<&str>,
        max_concurrent: usize,
        reporter: &Reporter,
    ) -> Result<usize, HttpError>;
}

pub struct ChatGptClient {
    http: Client,
    headers: HeaderMap,
    base_url: String,
}

impl std::fmt::Debug for ChatGptClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChatGptClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl ChatGptClient {
    pub fn new(context: ProviderContext) -> Result<Self, HttpError> {
        Self::with_base_url(context, CHATGPT_BASE_URL)
    }

    pub fn with_base_url(
        context: ProviderContext,
        base_url: impl Into<String>,
    ) -> Result<Self, HttpError> {
        Ok(Self {
            http: build_client()?,
            headers: context.headers,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        })
    }

    fn conversation_url(&self, conversation_id: &str) -> String {
        format!("{}/conversation/{}", self.base_url, conversation_id)
    }
}

#[async_trait]
impl ChatGptApi for ChatGptClient {
    fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    async fn get_conversation_details(&self, conversation_id: &str) -> Result<Value, HttpError> {
        let url = self.conversation_url(conversation_id);
        retry_http(|| async {
            let response = self
                .http
                .get(&url)
                .headers(self.headers.clone())
                .send()
                .await?;
            let response = ensure_success(response, Method::GET).await?;
            Ok(response.json::<Value>().await?)
        })
        .await
    }

    async fn delete_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<DeleteConversationResult, HttpError> {
        let url = self.conversation_url(conversation_id);
        retry_http(|| async {
            let response = self
                .http
                .patch(&url)
                .headers(self.headers.clone())
                .json(&json!({"is_visible": false}))
                .send()
                .await?;

            if response.status() == StatusCode::NOT_FOUND {
                let body = response.text().await.unwrap_or_default();
                if body.contains("conversation_deleted") {
                    return Ok(DeleteConversationResult::AlreadyDeleted);
                }
                return Err(HttpError::status(
                    StatusCode::NOT_FOUND,
                    Method::PATCH,
                    &url,
                    body,
                    None,
                ));
            }

            let response = ensure_success(response, Method::PATCH).await?;
            let _: Value = response.json().await?;
            Ok(DeleteConversationResult::Deleted)
        })
        .await
    }

    async fn get_image_generations(&self, limit: usize) -> Result<Vec<ImageGenItem>, HttpError> {
        let url = format!("{}/my/recent/image_gen", self.base_url);
        retry_http(|| async {
            let response = self
                .http
                .get(&url)
                .headers(self.headers.clone())
                .query(&[("limit", limit)])
                .send()
                .await?;
            let response = ensure_success(response, Method::GET).await?;
            let parsed = response.json::<ImageGenerationsResponse>().await?;
            Ok(parsed.items)
        })
        .await
    }

    async fn download_image(&self, url: &str, destination: &Path) -> Result<(), HttpError> {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;

        retry_http(|| async {
            let response = self
                .http
                .get(url)
                .headers(self.headers.clone())
                .send()
                .await?;
            let mut response = ensure_success(response, Method::GET).await?;

            let mut temporary = tempfile::Builder::new()
                .prefix(".c2n-")
                .suffix(".tmp")
                .tempfile_in(parent)?;
            while let Some(chunk) = response.chunk().await? {
                std::io::Write::write_all(&mut temporary, &chunk)?;
            }
            temporary.as_file_mut().sync_all()?;
            temporary
                .persist(destination)
                .map_err(|error| HttpError::Io(error.error))?;
            Ok(())
        })
        .await
    }

    async fn get_library_images(
        &self,
        query: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<LibraryImagesResponse, HttpError> {
        let url = format!("{}/files/library/nodes", self.base_url);
        retry_http(|| async {
            let mut request = self
                .http
                .get(&url)
                .headers(self.headers.clone())
                .query(&[("categories", "image")]);
            if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
                request = request.query(&[("q", query)]);
            }
            if let Some(cursor) = cursor.filter(|value| !value.is_empty()) {
                request = request.query(&[("cursor", cursor)]);
            }
            let response = request.send().await?;
            let response = ensure_success(response, Method::GET).await?;
            Ok(response.json::<LibraryImagesResponse>().await?)
        })
        .await
    }

    async fn delete_library_file(&self, item: &LibraryItem) -> Result<Vec<Value>, HttpError> {
        let url = format!(
            "{}/files/library/files/{}/delete_stream",
            self.base_url, item.id
        );
        retry_http(|| async {
            let response = self
                .http
                .post(&url)
                .headers(self.headers.clone())
                .query(&[
                    ("file_id", item.file_id.as_str()),
                    ("parent_directory_id", item.parent_directory_id.as_str()),
                    ("file_name", item.name.as_str()),
                    ("soft_delete", "true"),
                ])
                .send()
                .await?;

            if response.status() == StatusCode::NOT_FOUND {
                return Ok(Vec::new());
            }

            let response = ensure_success(response, Method::POST).await?;
            let body = response.text().await?;
            Ok(parse_sse_events(&body))
        })
        .await
    }

    async fn count_library_images(&self) -> Result<usize, HttpError> {
        self.count_library_images_by_query(None).await
    }

    async fn count_library_images_by_query(&self, query: Option<&str>) -> Result<usize, HttpError> {
        let mut total = 0;
        let mut cursor: Option<String> = None;
        loop {
            let data = self.get_library_images(query, cursor.as_deref()).await?;
            total += data.items.len();
            cursor = data.cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(total)
    }

    async fn delete_library_images_by_query(
        &self,
        query: Option<&str>,
        max_concurrent: usize,
        reporter: &Reporter,
    ) -> Result<usize, HttpError> {
        let mut total = 0;
        let mut cursor: Option<String> = None;
        let concurrency = max_concurrent.max(1);

        loop {
            let data = self.get_library_images(query, cursor.as_deref()).await?;
            if data.items.is_empty() {
                break;
            }

            let deleted = stream::iter(data.items)
                .map(|item| async move { self.delete_library_file(&item).await.map(|_| 1usize) })
                .buffer_unordered(concurrency)
                .try_fold(0usize, |total, count| async move { Ok(total + count) })
                .await?;

            total += deleted;
            reporter.add_stage("library_deleted", deleted);
            cursor = data.cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(total)
    }
}

/// Find the mapping key that contains the given asset pointer.
pub fn get_conversation_mapping_key_by_asset_pointer(
    data: &Value,
    asset_pointer: &str,
) -> Option<String> {
    let mapping = data.get("mapping")?.as_object()?;
    for (key, node) in mapping {
        let Some(message) = node.get("message").and_then(Value::as_object) else {
            continue;
        };
        let Some(parts) = message
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        if parts
            .iter()
            .any(|part| part.get("asset_pointer").and_then(Value::as_str) == Some(asset_pointer))
        {
            return Some(key.clone());
        }
    }
    None
}

/// Walk parent links from an image node to the user message that produced it.
pub fn get_prompt_from_image_node_in_conversation(
    data: &Value,
    start_node_id: &str,
    asset_pointer: &str,
) -> Option<String> {
    let mapping = data.get("mapping")?.as_object()?;
    let mut current_id = if mapping.contains_key(start_node_id) {
        Some(start_node_id.to_owned())
    } else {
        get_conversation_mapping_key_by_asset_pointer(data, asset_pointer)
    };

    while let Some(current_id_value) = current_id {
        let node = mapping.get(&current_id_value)?;
        if let Some(message) = node.get("message").and_then(Value::as_object) {
            let author = message
                .get("author")
                .and_then(|author| author.get("role"))
                .and_then(Value::as_str);
            if author == Some("user")
                && let Some(parts) = message
                    .get("content")
                    .and_then(|content| content.get("parts"))
                    .and_then(Value::as_array)
            {
                for part in parts {
                    if let Some(text) = part.as_str() {
                        return Some(text.to_owned());
                    }
                }
            }
        }
        current_id = node
            .get("parent")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }

    None
}

fn parse_sse_events(body: &str) -> Vec<Value> {
    fn append(results: &mut Vec<Value>, raw: &str) {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            results.push(value);
        }
    }

    let mut results = Vec::new();
    for event in body.split("\n\n") {
        let mut data_lines = Vec::new();
        for line in event.trim().lines() {
            let line = line.trim_end_matches('\r');
            if let Some(rest) = line.strip_prefix("data: ") {
                data_lines.push(rest);
            } else if line.starts_with("event: ")
                || line.starts_with("id: ")
                || line.starts_with("retry: ")
            {
                continue;
            } else {
                data_lines.push(line);
            }
        }
        if !data_lines.is_empty() {
            append(&mut results, &data_lines.join(""));
        }
    }

    if results.is_empty() && !body.trim().is_empty() {
        for line in body.trim().lines() {
            append(&mut results, line);
        }
    }

    results
}

fn deserialize_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(value) => Ok(value),
        Value::Number(value) => Ok(value.to_string()),
        other => Err(D::Error::custom(format!(
            "expected a string or number, got {other}"
        ))),
    }
}

fn deserialize_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Number(value) => value
            .as_f64()
            .ok_or_else(|| D::Error::custom("number is not representable as f64")),
        Value::String(value) => value
            .parse::<f64>()
            .map_err(|_| D::Error::custom("expected a numeric timestamp")),
        other => Err(D::Error::custom(format!(
            "expected a numeric timestamp, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_prompt_by_walking_parents() {
        let detail = json!({
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
        });
        assert_eq!(
            get_prompt_from_image_node_in_conversation(&detail, "image", "pointer"),
            Some("draw a cat".to_owned())
        );
    }

    #[test]
    fn falls_back_to_asset_pointer_lookup() {
        let detail = json!({
            "mapping": {
                "user": {
                    "message": {
                        "author": {"role": "user"},
                        "content": {"parts": ["draw a dog"]}
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
        });
        assert_eq!(
            get_prompt_from_image_node_in_conversation(&detail, "missing", "pointer"),
            Some("draw a dog".to_owned())
        );
    }

    #[test]
    fn parses_sse_data_lines() {
        let body = "data: {\"a\":1}\n\ndata: [DONE]\n\n";
        let events = parse_sse_events(body);
        assert_eq!(events, vec![json!({"a": 1})]);
    }
}
