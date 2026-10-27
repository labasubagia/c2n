//! Loading and validating the TOML configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use email_address::EmailAddress;

use crate::domain::models::{AccountConfig, AppConfig, NotionConfig, ResolvedConfig, SharedConfig};
use crate::shared::constants::config_path;

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
    /// Load `config.toml` from `$C2N_CONFIG_DIR` or the platform config dir.
    pub fn load_default() -> Result<Self, ConfigError> {
        let path = config_path();
        Self::load(Some(&path))
    }

    /// Load configuration from `path`.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let default_path;
        let path = match path {
            Some(path) => path,
            None => {
                default_path = config_path();
                default_path.as_path()
            }
        };
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
            .filter_map(|name| match name.parse::<EmailAddress>() {
                Ok(_) => None,
                Err(e) => Some(format!("{name:?} ({e})")),
            })
            .collect();
        if !invalid.is_empty() {
            return Err(ConfigError::InvalidAccountKeys(invalid.join(", ")));
        }
        let accounts = app
            .accounts
            .iter()
            .map(|(name, account)| {
                resolve_account(name, account, &app.shared, &app.notion)
                    .map(|resolved| (name.clone(), resolved))
            })
            .collect::<Result<_, _>>()?;

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
) -> Result<ResolvedConfig, ConfigError> {
    let mut missing = Vec::new();

    let authorization_token = account.authorization_token.trim().to_owned();
    if authorization_token.is_empty() {
        missing.push("CHATGPT_AUTHORIZATION_TOKEN");
    }

    let user_agent = nonempty(&account.user_agent).or_else(|| nonempty(&shared.user_agent));
    let cookie_string_base64 =
        nonempty(&account.cookie_string_base64).or_else(|| nonempty(&shared.cookie_string_base64));
    let notion_api_key = nonempty(&notion.api_key);
    let notion_database_id = nonempty(&account.notion_database_id)
        .or_else(|| nonempty(&shared.notion_database_id))
        .or_else(|| nonempty(&notion.database_id));

    for (label, present) in [
        ("CHATGPT_USER_AGENT", user_agent.is_some()),
        ("CHATGPT_COOKIE_STRING", cookie_string_base64.is_some()),
        ("NOTION_API_KEY", notion_api_key.is_some()),
        ("NOTION_DATABASE_ID", notion_database_id.is_some()),
    ] {
        if !present {
            missing.push(label);
        }
    }
    if !missing.is_empty() {
        return Err(ConfigError::MissingRequired(missing.join(", ")));
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
    let conversation_delay = account.conversation_delay.or(shared.conversation_delay);

    Ok(ResolvedConfig {
        account_name: name.to_owned(),
        authorization_token,
        user_agent: user_agent.unwrap(),
        cookie_string_base64: cookie_string_base64.unwrap(),
        notion_api_key: notion_api_key.unwrap(),
        notion_version: notion.version.clone(),
        notion_database_id: notion_database_id.unwrap(),
        history_folder,
        image_folder,
        conversation_delay,
    })
}

fn nonempty(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(account_name: &str) -> AppConfig {
        let mut app = AppConfig::default();
        app.notion.api_key = Some("key".to_owned());
        app.notion.database_id = Some("db".to_owned());
        app.accounts.insert(
            account_name.to_owned(),
            AccountConfig {
                authorization_token: "token".to_owned(),
                user_agent: Some("agent".to_owned()),
                cookie_string_base64: Some("cookie".to_owned()),
                notion_database_id: None,
                history_folder: None,
                image_folder: None,
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
        let config = Config::from_app(PathBuf::from("config.toml"), app).unwrap();
        let resolved = config.account("user@example.com").unwrap();
        assert_eq!(resolved.user_agent, "agent");
        assert_eq!(resolved.image_folder.as_deref(), Some("images"));
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
        assert_eq!(resolved.user_agent, "shared-agent");
        assert_eq!(resolved.cookie_string_base64, "shared-cookie");
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
        assert_eq!(resolved.notion_database_id, "account");
    }

    #[test]
    fn shared_database_id_is_a_fallback() {
        let mut app = app_with("user@example.com");
        app.notion.database_id = Some("global".to_owned());
        app.shared.notion_database_id = Some("shared".to_owned());
        let config = Config::from_app(PathBuf::from("config.toml"), app).unwrap();
        let resolved = config.account("user@example.com").unwrap();
        assert_eq!(resolved.notion_database_id, "shared");
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
