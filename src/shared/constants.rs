//! Constants and small path helpers shared across the crate.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use url::Url;

pub const MAX_HTTP_ATTEMPTS: usize = 10;
pub const HTTP_TIMEOUT_SECONDS: u64 = 120;
pub const MIN_RETRY_DELAY_SECONDS: u64 = 2;
pub const MAX_RETRY_DELAY_SECONDS: u64 = 60;
pub const OUTPUT_PATH: &str = "output";
pub const DB_FILE_NAME: &str = "chatgpt.db";
pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub const NOTION_BASE_URL: &str = "https://api.notion.com";

const CONFIG_DIR_ENV: &str = "C2N_CONFIG_DIR";
const DATA_DIR_ENV: &str = "C2N_DATA_DIR";

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "c2n")
}

/// Configuration directory: `$C2N_CONFIG_DIR`, else the platform config dir.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(CONFIG_DIR_ENV) {
        return PathBuf::from(dir);
    }
    project_dirs()
        .map(|dirs| dirs.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(OUTPUT_PATH))
}

/// Data directory: `$C2N_DATA_DIR`, else the platform data dir.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(DATA_DIR_ENV) {
        return PathBuf::from(dir);
    }
    project_dirs()
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(OUTPUT_PATH))
}

pub fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE_NAME)
}

/// Create the config, data, and default image directories if they are absent.
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    std::fs::create_dir_all(data_dir())?;
    std::fs::create_dir_all(data_dir().join("images"))?;
    Ok(())
}

const IMAGE_EXTENSIONS: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".webp"];

/// Return the image extension from a URL, defaulting to `.png`.
pub fn image_ext_from_url(url: &str) -> String {
    let path = Url::parse(url)
        .map(|parsed| parsed.path().to_owned())
        .unwrap_or_else(|_| url.split(['?', '#']).next().unwrap_or(url).to_owned());

    let extension = Path::new(&path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{}", value.to_ascii_lowercase()));

    extension
        .filter(|value| IMAGE_EXTENSIONS.contains(&value.as_str()))
        .unwrap_or_else(|| ".png".to_owned())
}

/// Default database path, relative to the current working directory.
pub fn db_path() -> PathBuf {
    PathBuf::from(OUTPUT_PATH).join(DB_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_known_extensions() {
        assert_eq!(image_ext_from_url("https://x.test/a.PNG?x=1"), ".png");
        assert_eq!(image_ext_from_url("https://x.test/a.webp"), ".webp");
        assert_eq!(image_ext_from_url("https://x.test/a.txt"), ".png");
        assert_eq!(image_ext_from_url("not a url"), ".png");
    }
}
