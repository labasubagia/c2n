//! Account activity reporting.

use chrono::{DateTime, Duration, Utc};

use crate::adapters::sqlite::{GenerationStore, StoreError};
use crate::config::{Config, ConfigError};
use crate::shared::time::{
    TimeError, format_local_datetime, local_day_bounds_utc, parse_utc, resolve_timezone, utc_iso,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    pub account: String,
    pub first_img_yesterday: String,
    pub last_img_yesterday: String,
    pub first_img_today: String,
    pub last_img_today: String,
}

#[derive(Debug, Clone)]
pub struct AccountStatusReport {
    pub rows: Vec<ActivityRow>,
    pub total_accounts: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum AccountStatusError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Time(#[from] TimeError),
    #[error("missing activity timestamps for account {0}")]
    MissingActivity(String),
}

pub fn get_account_activity_statuses(
    config: &Config,
    store: &dyn GenerationStore,
    timezone_name: Option<&str>,
    now: Option<DateTime<Utc>>,
) -> Result<AccountStatusReport, AccountStatusError> {
    let timezone = resolve_timezone(timezone_name)?;
    let now = now.unwrap_or_else(Utc::now);
    let now_local = now.with_timezone(&timezone);
    let today = now_local.date_naive();
    let yesterday = today - Duration::days(1);

    let mut rows = Vec::new();

    for account in config.account_names() {
        if !store.has_generations(&account)? {
            rows.push(ActivityRow {
                account: account.to_owned(),
                first_img_yesterday: "-".to_owned(),
                last_img_yesterday: "-".to_owned(),
                first_img_today: "-".to_owned(),
                last_img_today: "-".to_owned(),
            });
            continue;
        }

        let (y_first, y_last) = bounds_for_date(store, &account, yesterday, timezone)?;
        let (t_first, t_last) = bounds_for_date(store, &account, today, timezone)?;

        rows.push(ActivityRow {
            account: account.to_owned(),
            first_img_yesterday: y_first,
            last_img_yesterday: y_last,
            first_img_today: t_first,
            last_img_today: t_last,
        });
    }

    Ok(AccountStatusReport {
        rows,
        total_accounts: config.account_names().len(),
    })
}

fn bounds_for_date(
    store: &dyn GenerationStore,
    account: &str,
    date: chrono::NaiveDate,
    timezone: chrono_tz::Tz,
) -> Result<(String, String), AccountStatusError> {
    let (date_start, date_end) = local_day_bounds_utc(date, timezone)?;

    let stats = store.get_activity_stats(account, &utc_iso(date_start), &utc_iso(date_end))?;

    let Some(stats) = stats else {
        return Ok(("-".to_owned(), "-".to_owned()));
    };

    let first_active = parse_utc(
        stats
            .first_active
            .as_deref()
            .ok_or_else(|| AccountStatusError::MissingActivity(account.to_owned()))?,
    )?;
    let last_active = parse_utc(
        stats
            .last_active
            .as_deref()
            .ok_or_else(|| AccountStatusError::MissingActivity(account.to_owned()))?,
    )?;

    Ok((
        format_local_datetime(first_active, timezone),
        format_local_datetime(last_active, timezone),
    ))
}
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::TimeZone;

    use super::*;
    use crate::adapters::sqlite::{GenerationStore, SqliteStore};
    use crate::domain::models::{AccountConfig, AppConfig, ChatGptImageGeneration};

    fn config(account: &str) -> Config {
        let mut app = AppConfig::default();
        app.shared.user_agent = Some("shared-agent".to_owned());
        app.shared.cookie_string_base64 = Some("shared-cookie".to_owned());
        app.notion.api_key = Some("key".to_owned());
        app.notion.database_id = Some("db".to_owned());
        app.accounts.insert(
            account.to_owned(),
            AccountConfig {
                authorization_token: "token".to_owned(),
                user_agent: None,
                cookie_string_base64: None,
                notion_database_id: None,
            },
        );
        Config::from_app(PathBuf::from("config.toml"), app).unwrap()
    }

    fn generation(id: &str, created_at: &str) -> ChatGptImageGeneration {
        ChatGptImageGeneration {
            created_at: created_at.to_owned(),
            id: id.to_owned(),
            conversation_id: "conv".to_owned(),
            message_id: "msg".to_owned(),
            asset_pointer: "pointer".to_owned(),
            url: "https://example.test/image.png".to_owned(),
            prompt: "prompt".to_owned(),
        }
    }

    #[test]
    fn reports_dashes_for_accounts_without_data() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(temp.path().join("chatgpt.db")).unwrap();
        store.init_db().unwrap();
        let report = get_account_activity_statuses(
            &config("user@example.com"),
            &store,
            Some("UTC"),
            Some(Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap()),
        )
        .unwrap();
        assert_eq!(report.total_accounts, 1);
        assert_eq!(
            report.rows[0],
            ActivityRow {
                account: "user@example.com".to_owned(),
                first_img_yesterday: "-".to_owned(),
                last_img_yesterday: "-".to_owned(),
                first_img_today: "-".to_owned(),
                last_img_today: "-".to_owned(),
            }
        );
    }

    #[test]
    fn reports_first_and_last_image_timestamps() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(temp.path().join("chatgpt.db")).unwrap();
        store.init_db().unwrap();
        store
            .upsert_generations(
                "user@example.com",
                &[
                    generation("a", "2026-01-01T08:00:00+00:00"),
                    generation("b", "2026-01-01T09:00:00+00:00"),
                    generation("c", "2026-01-02T10:00:00+00:00"),
                    generation("d", "2026-01-02T11:00:00+00:00"),
                ],
            )
            .unwrap();

        let report = get_account_activity_statuses(
            &config("user@example.com"),
            &store,
            Some("UTC"),
            Some(Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap()),
        )
        .unwrap();
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].first_img_yesterday, "2026-01-01 08:00:00");
        assert_eq!(report.rows[0].last_img_yesterday, "2026-01-01 09:00:00");
        assert_eq!(report.rows[0].first_img_today, "2026-01-02 10:00:00");
        assert_eq!(report.rows[0].last_img_today, "2026-01-02 11:00:00");
    }
}
