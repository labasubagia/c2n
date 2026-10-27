//! Domain types shared across adapters and services.

use std::collections::BTreeMap;

use base64::Engine as _;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::config::ConfigError;

/// A single image generation discovered through the ChatGPT API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptImageGeneration {
    pub created_at: String,
    pub id: String,
    pub conversation_id: String,
    pub message_id: String,
    pub asset_pointer: String,
    pub url: String,
    pub prompt: String,
}

/// A Notion data source belonging to a database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSource {
    pub id: String,
}

/// Aggregate activity for one account on one local calendar day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityStats {
    pub total: i64,
    pub active_count: i64,
    pub first_active: Option<String>,
    pub last_active: Option<String>,
}

/// Notion credentials and defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotionConfig {
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub database_id: Option<String>,
    #[serde(default = "default_notion_version")]
    pub version: String,
}

impl Default for NotionConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            database_id: None,
            version: default_notion_version(),
        }
    }
}

fn default_notion_version() -> String {
    "2025-09-03".to_owned()
}

/// Shared browser/session defaults across accounts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedConfig {
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub cookie_string_base64: Option<String>,
    #[serde(default)]
    pub notion_database_id: Option<String>,
    #[serde(default)]
    pub image_folder: Option<String>,
    #[serde(default)]
    pub conversation_delay: Option<f64>,
}

/// Per-account ChatGPT credentials and overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub authorization_token: String,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub cookie_string_base64: Option<String>,
    #[serde(default)]
    pub notion_database_id: Option<String>,
    #[serde(default)]
    pub image_folder: Option<String>,
    #[serde(default)]
    pub conversation_delay: Option<f64>,
}

/// The TOML configuration file as written by the user.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub notion: NotionConfig,
    #[serde(default)]
    pub shared: SharedConfig,
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountConfig>,
}

/// Fully resolved configuration for one account.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub account_name: String,
    pub authorization_token: String,
    pub user_agent: String,
    pub cookie_string_base64: String,
    pub notion_api_key: String,
    pub notion_version: String,
    pub notion_database_id: String,
    pub image_folder: Option<String>,
    pub conversation_delay: Option<f64>,
}

impl ResolvedConfig {
    /// Build the headers and defaults used for ChatGPT requests.
    pub fn provider_context(&self) -> Result<ProviderContext, ConfigError> {
        let mut headers = HeaderMap::new();
        insert_header(
            &mut headers,
            HeaderName::from_static("authorization"),
            &format!("Bearer {}", self.authorization_token),
        )?;
        insert_header(
            &mut headers,
            HeaderName::from_static("user-agent"),
            &self.user_agent,
        )?;
        insert_header(
            &mut headers,
            HeaderName::from_static("content-type"),
            "application/json",
        )?;

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&self.cookie_string_base64)
            .map_err(|error| ConfigError::InvalidCookie(error.to_string()))?;
        let cookie = String::from_utf8(decoded)
            .map_err(|error| ConfigError::InvalidCookie(error.to_string()))?;
        insert_header(&mut headers, HeaderName::from_static("cookie"), &cookie)?;

        Ok(ProviderContext {
            headers,
            account_name: self.account_name.clone(),
        })
    }

    /// Build the headers and defaults used for Notion requests.
    pub fn notion_context(&self) -> Result<NotionContext, ConfigError> {
        let mut headers = HeaderMap::new();
        insert_header(
            &mut headers,
            HeaderName::from_static("authorization"),
            &format!("Bearer {}", self.notion_api_key),
        )?;
        insert_header(
            &mut headers,
            HeaderName::from_static("notion-version"),
            &self.notion_version,
        )?;
        insert_header(
            &mut headers,
            HeaderName::from_static("content-type"),
            "application/json",
        )?;

        Ok(NotionContext {
            headers,
            account_name: self.account_name.clone(),
        })
    }
}

fn insert_header(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &str,
) -> Result<(), ConfigError> {
    let value = HeaderValue::from_str(value).map_err(|source| ConfigError::InvalidHeader {
        name: name.as_str().to_owned(),
        source,
    })?;
    headers.insert(name, value);
    Ok(())
}

/// Resolved headers for ChatGPT API calls.
#[derive(Debug, Clone)]
pub struct ProviderContext {
    pub headers: HeaderMap,
    pub account_name: String,
}

/// Resolved headers for Notion API calls.
#[derive(Debug, Clone)]
pub struct NotionContext {
    pub headers: HeaderMap,
    pub account_name: String,
}
