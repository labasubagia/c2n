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
    pub history_folder: Option<String>,
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
    pub history_folder: Option<String>,
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
    pub account: AccountConfig,
    pub notion: NotionConfig,
    pub history_folder: Option<String>,
    pub image_folder: Option<String>,
    pub conversation_delay: Option<f64>,
}

/// Configuration values that a command requires before it can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredVar {
    NotionApiKey,
    NotionDatabaseId,
    ChatGptAuthorizationToken,
    ChatGptUserAgent,
    ChatGptCookie,
}

impl RequiredVar {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotionApiKey => "NOTION_API_KEY",
            Self::NotionDatabaseId => "NOTION_DATABASE_ID",
            Self::ChatGptAuthorizationToken => "CHATGPT_AUTHORIZATION_TOKEN",
            Self::ChatGptUserAgent => "CHATGPT_USER_AGENT",
            Self::ChatGptCookie => "CHATGPT_COOKIE_STRING",
        }
    }
}

impl ResolvedConfig {
    /// Build the headers and defaults used for ChatGPT requests.
    pub fn provider_context(&self) -> Result<ProviderContext, ConfigError> {
        let mut headers = HeaderMap::new();
        let token = self.account.authorization_token.trim();
        insert_header(
            &mut headers,
            HeaderName::from_static("authorization"),
            &format!("Bearer {token}"),
        )?;
        let user_agent = self.account.user_agent.as_deref().unwrap_or("").trim();
        insert_header(
            &mut headers,
            HeaderName::from_static("user-agent"),
            user_agent,
        )?;
        insert_header(
            &mut headers,
            HeaderName::from_static("content-type"),
            "application/json",
        )?;

        if let Some(encoded) = self
            .account
            .cookie_string_base64
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| ConfigError::InvalidCookie(error.to_string()))?;
            let cookie = String::from_utf8(decoded)
                .map_err(|error| ConfigError::InvalidCookie(error.to_string()))?;
            insert_header(&mut headers, HeaderName::from_static("cookie"), &cookie)?;
        }

        Ok(ProviderContext {
            headers,
            notion: self.notion.clone(),
            account_name: self.account_name.clone(),
        })
    }

    /// Build the headers and database selection used for Notion requests.
    pub fn notion_context(&self) -> Result<NotionContext, ConfigError> {
        let mut headers = HeaderMap::new();
        let api_key = self.notion.api_key.as_deref().unwrap_or("").trim();
        insert_header(
            &mut headers,
            HeaderName::from_static("authorization"),
            &format!("Bearer {api_key}"),
        )?;
        insert_header(
            &mut headers,
            HeaderName::from_static("notion-version"),
            &self.notion.version,
        )?;
        insert_header(
            &mut headers,
            HeaderName::from_static("content-type"),
            "application/json",
        )?;

        Ok(NotionContext {
            headers,
            database_id: self.notion.database_id.clone(),
            account_name: self.account_name.clone(),
        })
    }

    /// Fail if any required configuration value is missing.
    pub fn validate_runtime(&self, required: &[RequiredVar]) -> Result<(), ConfigError> {
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|key| match key {
                RequiredVar::NotionApiKey => self
                    .notion
                    .api_key
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty()),
                RequiredVar::NotionDatabaseId => self
                    .notion
                    .database_id
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty()),
                RequiredVar::ChatGptAuthorizationToken => {
                    self.account.authorization_token.trim().is_empty()
                }
                RequiredVar::ChatGptUserAgent => self
                    .account
                    .user_agent
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty()),
                RequiredVar::ChatGptCookie => self
                    .account
                    .cookie_string_base64
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty()),
            })
            .map(RequiredVar::label)
            .collect();

        if missing.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::MissingRequired(missing.join(", ")))
        }
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

/// Resolved headers and defaults for ChatGPT API calls.
#[derive(Debug, Clone)]
pub struct ProviderContext {
    pub headers: HeaderMap,
    pub notion: NotionConfig,
    pub account_name: String,
}

/// Resolved headers and database selection for Notion API calls.
#[derive(Debug, Clone)]
pub struct NotionContext {
    pub headers: HeaderMap,
    pub database_id: Option<String>,
    pub account_name: String,
}
