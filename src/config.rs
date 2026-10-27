//! Loading and validating the TOML configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use email_address::EmailAddress;

use crate::domain::models::{AccountConfig, AppConfig, NotionConfig, ResolvedConfig, SharedConfig};
use crate::shared::constants::DEFAULT_CONFIG_PATH;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing configuration: {0} not found")]
    MissingFile(String),
    #[error("failed to read configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("account keys must be valid email addresses, got: {0}")]
    InvalidAccountKeys(String),
    #[error("missing configuration: create config.toml with at least one account")]
    NoAccounts,
    #[error("unknown account: {0}")]
    UnknownAccount(String),
    #[error("invalid base64 in cookie_string_base64: {0}")]
    InvalidCookie(String),
    #[error("invalid HTTP header {name}: {source}")]
    InvalidHeader {
        name: String,
        #[source]
        source: reqwest::header::InvalidHeaderValue,
    },
    #[error("missing required configuration values: {0}")]
    MissingRequired(String),
}

/// Immutable, per-account resolved configuration snapshot.
#[derive(Debug, Clone)]
pub struct Config {
    path: PathBuf,
    accounts: BTreeMap<String, ResolvedConfig>,
}

impl Config {
    /// Load `config.toml` from the current working directory.
    pub fn load_default() -> Result<Self, ConfigError> {
        Self::load(Some(Path::new(DEFAULT_CONFIG_PATH)))
    }

    /// Load configuration from `path`.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = path.unwrap_or_else(|| Path::new(DEFAULT_CONFIG_PATH));
        if !path.exists() {
            return Err(ConfigError::MissingFile(path.display().to_string()));
        }
        let contents = std::fs::read_to_string(path)?;
        let app: AppConfig = toml::from_str(&contents)?;
        Self::from_app(path.to_path_buf(), app)
    }

    /// Validate and resolve an already-parsed configuration.
    pub fn from_app(path: PathBuf, app: AppConfig) -> Result<Self, ConfigError> {
        if app.accounts.is_empty() {
            return Err(ConfigError::NoAccounts);
        }

        let invalid: Vec<String> = app
            .accounts
            .keys()
            .filter(|name| !EmailAddress::is_valid(name))
            .map(|name| format!("{name:?}"))
            .collect();
        if !invalid.is_empty() {
            return Err(ConfigError::InvalidAccountKeys(invalid.join(", ")));
        }

        let accounts = app
            .accounts
            .iter()
            .map(|(name, account)| {
                (
                    name.clone(),
                    resolve_account(name, account, &app.shared, &app.notion),
                )
            })
            .collect();

        Ok(Self { path, accounts })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn accounts(&self) -> &BTreeMap<String, ResolvedConfig> {
        &self.accounts
    }

    pub fn account(&self, name: &str) -> Result<&ResolvedConfig, ConfigError> {
        self.accounts
            .get(name)
            .ok_or_else(|| ConfigError::UnknownAccount(name.to_owned()))
    }

    pub fn account_names(&self) -> Vec<String> {
        self.accounts.keys().cloned().collect()
    }
}

fn resolve_account(
    name: &str,
    account: &AccountConfig,
    shared: &SharedConfig,
    notion: &NotionConfig,
) -> ResolvedConfig {
    let mut notion = notion.clone();
    let account_database_id = account
        .notion_database_id
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let shared_database_id = shared
        .notion_database_id
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    if let Some(database_id) = account_database_id.or(shared_database_id) {
        notion.database_id = Some(database_id.to_owned());
    }

    let history_folder = account
        .history_folder
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            shared
                .history_folder
                .clone()
                .filter(|value| !value.trim().is_empty())
        });
    let image_folder = account
        .image_folder
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            shared
                .image_folder
                .clone()
                .filter(|value| !value.trim().is_empty())
        });

    let mut account = account.clone();
    if account
        .user_agent
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        account.user_agent = shared.user_agent.clone();
    }
    if account
        .cookie_string_base64
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        account.cookie_string_base64 = shared.cookie_string_base64.clone();
    }

    let delete_library_queries = if account.delete_library_queries.is_empty() {
        shared.delete_library_queries.clone()
    } else {
        account.delete_library_queries.clone()
    };

    let remove_all_library_images =
        account.remove_all_library_images || shared.remove_all_library_images;

    let conversation_delay = account.conversation_delay.or(shared.conversation_delay);

    ResolvedConfig {
        account_name: name.to_owned(),
        account,
        notion,
        history_folder,
        image_folder,
        delete_library_queries,
        remove_all_library_images,
        conversation_delay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(account_name: &str) -> AppConfig {
        let mut app = AppConfig::default();
        app.accounts.insert(
            account_name.to_owned(),
            AccountConfig {
                authorization_token: "token".to_owned(),
                user_agent: None,
                cookie_string_base64: None,
                notion_database_id: None,
                history_folder: None,
                image_folder: None,
                delete_library_queries: Vec::new(),
                remove_all_library_images: false,
                conversation_delay: None,
            },
        );
        app
    }

    #[test]
    fn rejects_non_email_account_keys() {
        let error = Config::from_app(PathBuf::from("config.toml"), app_with("personal"))
            .expect_err("non-email account should be rejected");
        assert!(matches!(error, ConfigError::InvalidAccountKeys(_)));
    }

    #[test]
    fn resolves_shared_fallbacks() {
        let mut app = app_with("user@example.com");
        app.shared.user_agent = Some("agent".to_owned());
        app.shared.image_folder = Some("images".to_owned());
        app.shared.delete_library_queries = vec!["old".to_owned()];
        let config = Config::from_app(PathBuf::from("config.toml"), app).unwrap();
        let resolved = config.account("user@example.com").unwrap();
        assert_eq!(resolved.account.user_agent.as_deref(), Some("agent"));
        assert_eq!(resolved.image_folder.as_deref(), Some("images"));
        assert_eq!(resolved.delete_library_queries, vec!["old"]);
    }

    #[test]
    fn blank_account_values_fall_back_to_shared() {
        let mut app = app_with("user@example.com");
        app.shared.user_agent = Some("shared-agent".to_owned());
        app.shared.cookie_string_base64 = Some("shared-cookie".to_owned());
        app.shared.history_folder = Some("shared-history".to_owned());
        app.shared.image_folder = Some("shared-images".to_owned());
        {
            let account = app.accounts.get_mut("user@example.com").unwrap();
            account.user_agent = Some("   ".to_owned());
            account.cookie_string_base64 = Some(String::new());
            account.history_folder = Some(String::new());
            account.image_folder = Some("  ".to_owned());
        }
        let config = Config::from_app(PathBuf::from("config.toml"), app).unwrap();
        let resolved = config.account("user@example.com").unwrap();
        assert_eq!(resolved.account.user_agent.as_deref(), Some("shared-agent"));
        assert_eq!(
            resolved.account.cookie_string_base64.as_deref(),
            Some("shared-cookie")
        );
        assert_eq!(resolved.history_folder.as_deref(), Some("shared-history"));
        assert_eq!(resolved.image_folder.as_deref(), Some("shared-images"));
    }

    #[test]
    fn account_database_id_overrides_global() {
        let mut app = app_with("user@example.com");
        app.notion.database_id = Some("global".to_owned());
        app.accounts
            .get_mut("user@example.com")
            .unwrap()
            .notion_database_id = Some("account".to_owned());
        let config = Config::from_app(PathBuf::from("config.toml"), app).unwrap();
        let resolved = config.account("user@example.com").unwrap();
        assert_eq!(resolved.notion.database_id.as_deref(), Some("account"));
    }

    #[test]
    fn shared_database_id_is_a_fallback() {
        let mut app = app_with("user@example.com");
        app.notion.database_id = Some("global".to_owned());
        app.shared.notion_database_id = Some("shared".to_owned());
        let config = Config::from_app(PathBuf::from("config.toml"), app).unwrap();
        let resolved = config.account("user@example.com").unwrap();
        assert_eq!(resolved.notion.database_id.as_deref(), Some("shared"));
    }

    #[test]
    fn account_folders_override_shared_folders() {
        let mut app = app_with("user@example.com");
        app.shared.history_folder = Some("shared-history".to_owned());
        app.shared.image_folder = Some("shared-images".to_owned());
        {
            let account = app.accounts.get_mut("user@example.com").unwrap();
            account.history_folder = Some("account-history".to_owned());
            account.image_folder = Some("account-images".to_owned());
        }
        let config = Config::from_app(PathBuf::from("config.toml"), app).unwrap();
        let resolved = config.account("user@example.com").unwrap();
        assert_eq!(resolved.history_folder.as_deref(), Some("account-history"));
        assert_eq!(resolved.image_folder.as_deref(), Some("account-images"));
    }
}
