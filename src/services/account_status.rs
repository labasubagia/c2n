//! Account readiness and cooldown reporting.

use chrono::{DateTime, Duration, Utc};

use crate::adapters::sqlite::{GenerationStore, StoreError};
use crate::config::{Config, ConfigError};
use crate::shared::time::{
    TimeError, format_duration, format_local_datetime, local_day_bounds_utc, parse_utc,
    resolve_timezone, utc_iso,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    pub account: String,
    pub next_wait: String,
    pub next_cooldown: String,
    pub fully_ready_in: String,
    pub total_wait: String,
    pub ready_generate: String,
}

#[derive(Debug, Clone)]
pub struct AccountStatusReport {
    pub today: Vec<ActivityRow>,
    pub yesterday: Vec<ActivityRow>,
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

    let mut today_rows = Vec::new();
    let mut yesterday_rows = Vec::new();
    let mut today_sortable = Vec::new();
    let mut yesterday_sortable = Vec::new();

    for account in config.account_names() {
        if !store.has_generations(&account)? {
            let ready = ready_row(&account);
            today_sortable.push((now, ready.clone()));
            yesterday_sortable.push((now, ready));
            continue;
        }

        let (today_key, today_row) = activity_for_date(store, &account, now, today, timezone)?;
        if let Some(row) = today_row {
            today_sortable.push((today_key, row));
        }

        let (yesterday_key, yesterday_row) =
            activity_for_date(store, &account, now, yesterday, timezone)?;
        if let Some(row) = yesterday_row {
            yesterday_sortable.push((yesterday_key, row));
        } else {
            yesterday_sortable.push((now, ready_row(&account)));
        }
    }

    today_sortable.sort_by_key(|(key, _)| *key);
    yesterday_sortable.sort_by_key(|(key, _)| *key);

    for (_, row) in today_sortable {
        today_rows.push(row);
    }
    for (_, row) in yesterday_sortable {
        yesterday_rows.push(row);
    }

    Ok(AccountStatusReport {
        today: today_rows,
        yesterday: yesterday_rows,
        total_accounts: config.account_names().len(),
    })
}

fn activity_for_date(
    store: &dyn GenerationStore,
    account: &str,
    now: DateTime<Utc>,
    date: chrono::NaiveDate,
    timezone: chrono_tz::Tz,
) -> Result<(DateTime<Utc>, Option<ActivityRow>), AccountStatusError> {
    let (date_start, date_end) = local_day_bounds_utc(date, timezone)?;
    let cooldown_threshold = now - Duration::days(1);

    let stats = store.get_activity_stats(
        account,
        &utc_iso(date_start),
        &utc_iso(date_end),
        &utc_iso(cooldown_threshold),
    )?;

    let Some(stats) = stats else {
        return Ok((now, None));
    };

    if stats.active_count == 0 {
        return Ok((now, Some(ready_row(account))));
    }

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

    let next_wait = first_active + Duration::days(1);
    let status = format!("{}/{} to wait", stats.active_count, stats.total);
    let ready_generate = if stats.active_count == stats.total {
        format!("❌  ({status})")
    } else {
        format!("⚠️  ({status})")
    };

    Ok((
        next_wait,
        Some(ActivityRow {
            account: account.to_owned(),
            next_wait: format_local_datetime(next_wait, timezone),
            next_cooldown: format_duration((next_wait - now).num_seconds() as f64),
            fully_ready_in: format_duration(
                (last_active + Duration::days(1) - now).num_seconds() as f64
            ),
            total_wait: format_duration((last_active - first_active).num_seconds() as f64),
            ready_generate,
        }),
    ))
}

fn ready_row(account: &str) -> ActivityRow {
    ActivityRow {
        account: account.to_owned(),
        next_wait: "Ready".to_owned(),
        next_cooldown: "0s".to_owned(),
        fully_ready_in: "0s".to_owned(),
        total_wait: "0s".to_owned(),
        ready_generate: "✅".to_owned(),
    }
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
        app.accounts.insert(
            account.to_owned(),
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
    fn reports_ready_for_accounts_without_data() {
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
        assert_eq!(report.today[0].next_wait, "Ready");
        assert_eq!(report.yesterday[0].next_wait, "Ready");
    }

    #[test]
    fn reports_partial_and_full_cooldowns() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(temp.path().join("chatgpt.db")).unwrap();
        store.init_db().unwrap();
        store
            .upsert_generations(
                "user@example.com",
                &[
                    generation("a", "2026-01-02T10:00:00+00:00"),
                    generation("b", "2026-01-02T11:00:00+00:00"),
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
        assert_eq!(report.today.len(), 1);
        assert_eq!(report.today[0].ready_generate, "❌  (2/2 to wait)");
        assert_eq!(report.today[0].total_wait, "1h");
    }
}
