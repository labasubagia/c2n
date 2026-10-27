//! Notion API adapter for file uploads and database pages.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderMap};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};

use crate::adapters::sqlite::{NotionCache, StoreError};
use crate::domain::models::{DataSource, NotionContext};
use crate::shared::constants::NOTION_BASE_URL;
use crate::shared::http::{HttpError, build_client, ensure_success, retry_http};

#[derive(Debug, thiserror::Error)]
pub enum NotionError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("file not found: {0}")]
    FileNotFound(String),
    #[error("Notion file upload response is missing an id")]
    MissingUploadId,
    #[error("Notion database {0} has no data sources; refusing to upload without duplicate checks")]
    NoDataSources(String),
    #[error("invalid file path: {0}")]
    InvalidPath(String),
}

#[async_trait]
pub trait NotionApi: Send + Sync {
    fn headers(&self) -> &HeaderMap;

    async fn is_page_exists_in_db(&self, db_id: &str, name: &str) -> Result<bool, NotionError>;

    async fn add_page_to_db(
        &self,
        db_id: &str,
        file_path: &Path,
        prompt: &str,
    ) -> Result<Value, NotionError>;
}

pub struct NotionClient {
    http: Client,
    headers: HeaderMap,
    account_name: String,
    cache: Arc<dyn NotionCache>,
    base_url: String,
}

impl std::fmt::Debug for NotionClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NotionClient")
            .field("base_url", &self.base_url)
            .field("account_name", &self.account_name)
            .finish_non_exhaustive()
    }
}

impl NotionClient {
    pub fn new(context: NotionContext, cache: Arc<dyn NotionCache>) -> Result<Self, NotionError> {
        Self::with_base_url(context, cache, NOTION_BASE_URL)
    }

    pub fn with_base_url(
        context: NotionContext,
        cache: Arc<dyn NotionCache>,
        base_url: impl Into<String>,
    ) -> Result<Self, NotionError> {
        Ok(Self {
            http: build_client()?,
            headers: context.headers,
            account_name: context.account_name,
            cache,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        })
    }

    async fn get_data_sources(&self, db_id: &str) -> Result<Vec<DataSource>, NotionError> {
        if let Some(cached) = self.cache.get_cached_data_sources(db_id)? {
            if cached.is_empty() {
                return Err(NotionError::NoDataSources(db_id.to_owned()));
            }
            return Ok(cached);
        }

        let url = format!("{}/v1/databases/{}", self.base_url, db_id);
        let data = retry_http(|| async {
            let response = self
                .http
                .get(&url)
                .headers(self.headers.clone())
                .send()
                .await?;
            let response = ensure_success(response, Method::GET).await?;
            Ok(response.json::<Value>().await?)
        })
        .await?;

        let sources = data
            .get("data_sources")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        item.get("id")
                            .and_then(Value::as_str)
                            .map(|id| DataSource { id: id.to_owned() })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if sources.is_empty() {
            return Err(NotionError::NoDataSources(db_id.to_owned()));
        }

        self.cache.set_cached_data_sources(db_id, &sources)?;
        Ok(sources)
    }

    async fn query_data_source(
        &self,
        data_source_id: &str,
        query: &str,
    ) -> Result<Value, NotionError> {
        let url = format!("{}/v1/data_sources/{}/query", self.base_url, data_source_id);
        retry_http(|| async {
            let response = self
                .http
                .post(&url)
                .headers(self.headers.clone())
                .json(&json!({
                    "filter": {
                        "and": [
                            {"property": "Name", "title": {"equals": query}}
                        ]
                    }
                }))
                .send()
                .await?;
            let response = ensure_success(response, Method::POST).await?;
            Ok(response.json::<Value>().await?)
        })
        .await
        .map_err(NotionError::from)
    }

    async fn query_data_source_for_page(
        &self,
        data_source_id: &str,
        query: &str,
    ) -> Result<bool, NotionError> {
        let data = self.query_data_source(data_source_id, query).await?;
        let Some(results) = data.get("results").and_then(Value::as_array) else {
            return Ok(false);
        };

        for page in results {
            let name = page
                .get("properties")
                .and_then(|properties| properties.get("Name"))
                .and_then(|name| name.get("title"))
                .and_then(Value::as_array)
                .and_then(|titles| titles.first())
                .and_then(|title| title.get("text"))
                .and_then(|text| text.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name == query {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn create_upload(
        &self,
        file_path: &Path,
        content_type: &str,
    ) -> Result<Value, NotionError> {
        let file_name = file_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| NotionError::InvalidPath(file_path.display().to_string()))?;
        let url = format!("{}/v1/file_uploads", self.base_url);

        retry_http(|| async {
            let response = self
                .http
                .post(&url)
                .headers(self.headers.clone())
                .json(&json!({
                    "mode": "single_part",
                    "filename": file_name,
                    "content_type": content_type,
                }))
                .send()
                .await?;
            let response = ensure_success(response, Method::POST).await?;
            Ok(response.json::<Value>().await?)
        })
        .await
        .map_err(NotionError::from)
    }

    async fn send_upload(
        &self,
        file_upload_id: &str,
        file_path: &Path,
        content_type: &str,
    ) -> Result<Value, NotionError> {
        let file_name = file_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| NotionError::InvalidPath(file_path.display().to_string()))?
            .to_owned();
        let url = format!("{}/v1/file_uploads/{}/send", self.base_url, file_upload_id);
        let mut headers = self.headers.clone();
        headers.remove(CONTENT_TYPE);

        retry_http(|| async {
            let part = reqwest::multipart::Part::file(file_path)
                .await?
                .file_name(file_name.clone())
                .mime_str(content_type)?;
            let form = reqwest::multipart::Form::new().part("file", part);

            let response = self
                .http
                .post(&url)
                .headers(headers.clone())
                .multipart(form)
                .send()
                .await?;

            if response.status() == StatusCode::BAD_REQUEST {
                let body = response.text().await.unwrap_or_default();
                tracing::info!(
                    file_upload_id,
                    body,
                    "file upload returned 400; assuming it was already uploaded"
                );
                return Ok(json!({"id": file_upload_id, "status": "complete"}));
            }

            let response = ensure_success(response, Method::POST).await?;
            Ok(response.json::<Value>().await?)
        })
        .await
        .map_err(NotionError::from)
    }
}

#[async_trait]
impl NotionApi for NotionClient {
    fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    async fn is_page_exists_in_db(&self, db_id: &str, name: &str) -> Result<bool, NotionError> {
        let sources = self.get_data_sources(db_id).await?;
        for source in sources {
            if self.query_data_source_for_page(&source.id, name).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn add_page_to_db(
        &self,
        db_id: &str,
        file_path: &Path,
        prompt: &str,
    ) -> Result<Value, NotionError> {
        if !file_path.exists() {
            return Err(NotionError::FileNotFound(file_path.display().to_string()));
        }

        let file_name = file_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| NotionError::InvalidPath(file_path.display().to_string()))?
            .to_owned();
        let content_type = content_type_for_path(file_path);

        let create = self.create_upload(file_path, &content_type).await?;
        let upload_id = create
            .get("id")
            .and_then(Value::as_str)
            .ok_or(NotionError::MissingUploadId)?
            .to_owned();
        let send = self
            .send_upload(&upload_id, file_path, &content_type)
            .await?;
        let final_upload_id = send
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(upload_id.as_str());

        let normalized_prompt = prompt.trim().replace("\r\n", "\n").replace('\r', "\n");
        let mut body = format!("**Prompt:**\n\n```\n{normalized_prompt}\n```");
        if !self.account_name.is_empty() {
            body.push_str(&format!("\n\n**Email:** {}", self.account_name));
        }

        let payload = json!({
            "parent": {"database_id": db_id},
            "properties": {
                "Name": {"title": [{"text": {"content": file_name}}]},
                "Image": {
                    "files": [
                        {
                            "type": "file_upload",
                            "file_upload": {"id": final_upload_id}
                        }
                    ]
                }
            },
            "markdown": body,
        });
        let url = format!("{}/v1/pages", self.base_url);

        retry_http(|| async {
            let response = self
                .http
                .post(&url)
                .headers(self.headers.clone())
                .json(&payload)
                .send()
                .await?;
            let response = ensure_success(response, Method::POST).await?;
            Ok(response.json::<Value>().await?)
        })
        .await
        .map_err(NotionError::from)
    }
}

fn content_type_for_path(path: &Path) -> String {
    let guessed = mime_guess::from_path(path).first_or_octet_stream();
    if guessed.to_string().starts_with("image/") {
        guessed.to_string()
    } else {
        "image/png".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_defaults_to_png_for_unknown_extensions() {
        assert_eq!(
            content_type_for_path(Path::new("image.unknown")),
            "image/png"
        );
        assert_eq!(content_type_for_path(Path::new("image.png")), "image/png");
        assert_eq!(content_type_for_path(Path::new("image.webp")), "image/webp");
    }
}
