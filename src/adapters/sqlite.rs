//! SQLite persistence for image generations and Notion data-source caching.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use rusqlite::backup::Backup;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::domain::models::{ActivityStats, ChatGptImageGeneration, DataSource};
use crate::shared::constants::db_path;
use crate::shared::time::{TimeError, now_utc_iso, parse_utc, utc_iso};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS image_generations (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    asset_pointer TEXT NOT NULL,
    url TEXT NOT NULL,
    prompt TEXT DEFAULT '',
    uploaded_at TEXT DEFAULT '',
    account TEXT DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_generations_account
    ON image_generations(account);
CREATE INDEX IF NOT EXISTS idx_generations_created_at
    ON image_generations(created_at);
CREATE INDEX IF NOT EXISTS idx_generations_uploaded_at
    ON image_generations(uploaded_at);

CREATE TABLE IF NOT EXISTS cache_data_sources (
    db_id TEXT PRIMARY KEY,
    data_sources_json TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
"#;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Time(#[from] TimeError),
    #[error("database lock poisoned")]
    LockPoisoned,
    #[error("backup file not found: {0}")]
    BackupNotFound(String),
    #[error("invalid database backup: {0}")]
    InvalidBackup(String),
}

#[derive(Debug, Clone, Default)]
pub struct GetGenerationsOptions {
    pub include_uploaded: bool,
    pub keep_days: Option<i64>,
    pub ids_filter: Option<HashSet<String>>,
}

pub trait GenerationStore: Send + Sync {
    fn init_db(&self) -> Result<(), StoreError>;
    fn upsert_generations(
        &self,
        account: &str,
        generations: &[ChatGptImageGeneration],
    ) -> Result<(), StoreError>;
    fn get_generations(
        &self,
        account: &str,
        options: &GetGenerationsOptions,
    ) -> Result<Vec<ChatGptImageGeneration>, StoreError>;
    fn get_uploaded_ids(&self, account: &str) -> Result<HashSet<String>, StoreError>;
    fn get_existing_ids(
        &self,
        account: &str,
        ids: &HashSet<String>,
    ) -> Result<HashSet<String>, StoreError>;
    fn mark_uploaded(&self, account: &str, ids: &HashSet<String>) -> Result<(), StoreError>;
    fn has_generations(&self, account: &str) -> Result<bool, StoreError>;
    fn get_activity_stats(
        &self,
        account: &str,
        date_start: &str,
        date_end: &str,
    ) -> Result<Option<ActivityStats>, StoreError>;
    fn is_uploaded(&self, account: &str, id: &str) -> Result<bool, StoreError>;
    fn backup(&self, target: &Path) -> Result<PathBuf, StoreError>;
    fn restore(&self, source: &Path) -> Result<PathBuf, StoreError>;
}

pub trait NotionCache: Send + Sync {
    fn get_cached_data_sources(&self, db_id: &str) -> Result<Option<Vec<DataSource>>, StoreError>;
    fn set_cached_data_sources(
        &self,
        db_id: &str,
        sources: &[DataSource],
    ) -> Result<(), StoreError>;
}

pub struct SqliteStore {
    path: PathBuf,
    connection: Mutex<Connection>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SqliteStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&path)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=10000;")?;
        Ok(Self {
            path,
            connection: Mutex::new(connection),
        })
    }

    pub fn open_default() -> Result<Self, StoreError> {
        Self::open(db_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.connection.lock().map_err(|_| StoreError::LockPoisoned)
    }
}

impl GenerationStore for SqliteStore {
    fn init_db(&self) -> Result<(), StoreError> {
        let connection = self.conn()?;
        connection.execute_batch(SCHEMA)?;
        Ok(())
    }

    fn upsert_generations(
        &self,
        account: &str,
        generations: &[ChatGptImageGeneration],
    ) -> Result<(), StoreError> {
        if generations.is_empty() {
            return Ok(());
        }
        let mut connection = self.conn()?;
        let transaction = connection.transaction()?;
        {
            let mut statement = transaction.prepare_cached(
                r#"
                INSERT INTO image_generations (
                    id, created_at, conversation_id, message_id,
                    asset_pointer, url, prompt, account
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(id) DO UPDATE SET
                    created_at=excluded.created_at,
                    conversation_id=excluded.conversation_id,
                    message_id=excluded.message_id,
                    asset_pointer=excluded.asset_pointer,
                    url=excluded.url,
                    prompt=excluded.prompt,
                    account=excluded.account
                "#,
            )?;
            for generation in generations {
                statement.execute(params![
                    generation.id,
                    generation.created_at,
                    generation.conversation_id,
                    generation.message_id,
                    generation.asset_pointer,
                    generation.url,
                    generation.prompt,
                    account,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    fn get_generations(
        &self,
        account: &str,
        options: &GetGenerationsOptions,
    ) -> Result<Vec<ChatGptImageGeneration>, StoreError> {
        let connection = self.conn()?;
        let mut sql = String::from(
            "SELECT id, created_at, conversation_id, message_id, asset_pointer, url, prompt \
             FROM image_generations WHERE account = ?",
        );
        let mut values: Vec<Value> = vec![account.to_owned().into()];

        if let Some(ids) = options.ids_filter.as_ref() {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            sql.push_str(&format!(" AND id IN ({})", placeholders(ids.len())));
            values.extend(ids.iter().cloned().map(Value::from));
        }

        if !options.include_uploaded {
            sql.push_str(" AND uploaded_at = ''");
        }

        if let Some(days) = options.keep_days {
            let cutoff = utc_iso(Utc::now() - ChronoDuration::days(days));
            // Use SQLite's date parser so timestamps written by older versions
            // (`+00:00`) and current versions (`Z`) compare correctly.
            sql.push_str(" AND julianday(created_at) >= julianday(?)");
            values.push(cutoff.into());
        }

        sql.push_str(" ORDER BY created_at ASC");

        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| {
            Ok(ChatGptImageGeneration {
                id: row.get(0)?,
                created_at: row.get(1)?,
                conversation_id: row.get(2)?,
                message_id: row.get(3)?,
                asset_pointer: row.get(4)?,
                url: row.get(5)?,
                prompt: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn get_uploaded_ids(&self, account: &str) -> Result<HashSet<String>, StoreError> {
        let connection = self.conn()?;
        let mut statement = connection
            .prepare("SELECT id FROM image_generations WHERE account = ? AND uploaded_at != ''")?;
        let rows = statement.query_map(params![account], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<HashSet<_>, _>>()?)
    }

    fn get_existing_ids(
        &self,
        account: &str,
        ids: &HashSet<String>,
    ) -> Result<HashSet<String>, StoreError> {
        if ids.is_empty() {
            return Ok(HashSet::new());
        }
        let connection = self.conn()?;
        let sql = format!(
            "SELECT id FROM image_generations WHERE account = ? AND id IN ({})",
            placeholders(ids.len())
        );
        let mut values: Vec<Value> = vec![account.to_owned().into()];
        values.extend(ids.iter().cloned().map(Value::from));
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<HashSet<_>, _>>()?)
    }

    fn mark_uploaded(&self, account: &str, ids: &HashSet<String>) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let connection = self.conn()?;
        let sql = format!(
            "UPDATE image_generations SET uploaded_at = ? WHERE account = ? AND id IN ({})",
            placeholders(ids.len())
        );
        let mut values: Vec<Value> = vec![now_utc_iso().into(), account.to_owned().into()];
        values.extend(ids.iter().cloned().map(Value::from));
        connection.execute(&sql, params_from_iter(values))?;
        Ok(())
    }

    fn has_generations(&self, account: &str) -> Result<bool, StoreError> {
        let connection = self.conn()?;
        let found = connection
            .query_row(
                "SELECT 1 FROM image_generations WHERE account = ? LIMIT 1",
                params![account],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    fn get_activity_stats(
        &self,
        account: &str,
        date_start: &str,
        date_end: &str,
    ) -> Result<Option<ActivityStats>, StoreError> {
        let connection = self.conn()?;
        let stats = connection.query_row(
            r#"
            SELECT
                MIN(created_at) as first_active,
                MAX(created_at) as last_active
            FROM image_generations
            WHERE account = ?
              AND julianday(created_at) >= julianday(?)
              AND julianday(created_at) < julianday(?)
            "#,
            params![account, date_start, date_end],
            |row| {
                Ok(ActivityStats {
                    first_active: row.get(0)?,
                    last_active: row.get(1)?,
                })
            },
        )?;

        if stats.first_active.is_none() {
            Ok(None)
        } else {
            Ok(Some(stats))
        }
    }
    fn is_uploaded(&self, account: &str, id: &str) -> Result<bool, StoreError> {
        let connection = self.conn()?;
        let found = connection
            .query_row(
                "SELECT 1 FROM image_generations \
                 WHERE account = ? AND id = ? AND uploaded_at != '' LIMIT 1",
                params![account, id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    fn backup(&self, target: &Path) -> Result<PathBuf, StoreError> {
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        if same_file(&self.path, target) {
            return Ok(target.to_path_buf());
        }

        let source = self.conn()?;
        let parent = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let temporary = tempfile::Builder::new()
            .prefix(".c2n-backup-")
            .suffix(".db")
            .tempfile_in(parent)?;
        let temporary_path = temporary.into_temp_path();
        {
            let mut destination = Connection::open(&temporary_path)?;
            let backup = Backup::new(&source, &mut destination)?;
            backup.run_to_completion(5, Duration::from_millis(50), None)?;
        }

        // A stale WAL from an older target must not be applied to the new
        // backup after it is moved into place.
        remove_sidecars(target)?;
        temporary_path
            .persist(target)
            .map_err(|error| StoreError::Io(error.error))?;
        Ok(target.to_path_buf())
    }

    fn restore(&self, source: &Path) -> Result<PathBuf, StoreError> {
        if !source.exists() {
            return Err(StoreError::BackupNotFound(source.display().to_string()));
        }
        if same_file(&self.path, source) {
            return Ok(self.path.clone());
        }

        let source_connection =
            Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        validate_backup(&source_connection, source)?;

        let mut destination = self.conn()?;
        {
            let backup = Backup::new(&source_connection, &mut destination)?;
            backup.run_to_completion(5, Duration::from_millis(50), None)?;
        }
        destination.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(self.path.clone())
    }
}

impl NotionCache for SqliteStore {
    fn get_cached_data_sources(&self, db_id: &str) -> Result<Option<Vec<DataSource>>, StoreError> {
        let connection = self.conn()?;
        let row: Option<(String, String)> = connection
            .query_row(
                "SELECT data_sources_json, expires_at FROM cache_data_sources WHERE db_id = ?",
                params![db_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let Some((json, expires_at)) = row else {
            return Ok(None);
        };

        let expires_at = match parse_utc(&expires_at) {
            Ok(value) => value,
            Err(_) => {
                connection.execute(
                    "DELETE FROM cache_data_sources WHERE db_id = ?",
                    params![db_id],
                )?;
                return Ok(None);
            }
        };

        if Utc::now() > expires_at {
            connection.execute(
                "DELETE FROM cache_data_sources WHERE db_id = ?",
                params![db_id],
            )?;
            return Ok(None);
        }

        let sources: Vec<DataSource> = match serde_json::from_str(&json) {
            Ok(sources) => sources,
            Err(error) => {
                tracing::warn!(
                    db_id,
                    %error,
                    "discarding corrupt cached Notion data sources"
                );
                connection.execute(
                    "DELETE FROM cache_data_sources WHERE db_id = ?",
                    params![db_id],
                )?;
                return Ok(None);
            }
        };
        Ok(Some(sources))
    }

    fn set_cached_data_sources(
        &self,
        db_id: &str,
        sources: &[DataSource],
    ) -> Result<(), StoreError> {
        let connection = self.conn()?;
        let json = serde_json::to_string(sources)?;
        let expires_at = utc_iso(Utc::now() + ChronoDuration::days(1));
        connection.execute(
            r#"
            INSERT INTO cache_data_sources (db_id, data_sources_json, expires_at)
            VALUES (?, ?, ?)
            ON CONFLICT(db_id) DO UPDATE SET
                data_sources_json=excluded.data_sources_json,
                expires_at=excluded.expires_at
            "#,
            params![db_id, json, expires_at],
        )?;
        Ok(())
    }
}

fn placeholders(count: usize) -> String {
    vec!["?"; count].join(",")
}

fn validate_backup(connection: &Connection, path: &Path) -> Result<(), StoreError> {
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(StoreError::InvalidBackup(format!(
            "{} failed SQLite integrity check: {integrity}",
            path.display()
        )));
    }

    let has_generations: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'image_generations')",
        [],
        |row| row.get(0),
    )?;
    if !has_generations {
        return Err(StoreError::InvalidBackup(format!(
            "{} does not contain an image_generations table",
            path.display()
        )));
    }
    connection
        .prepare(
            "SELECT id, created_at, conversation_id, message_id, asset_pointer, url, prompt, uploaded_at, account FROM image_generations LIMIT 0",
        )
        .map_err(|error| {
            StoreError::InvalidBackup(format!(
                "{} has an incompatible image_generations table: {error}",
                path.display()
            ))
        })?;
    Ok(())
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn remove_sidecars(path: &Path) -> Result<(), StoreError> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        match std::fs::remove_file(PathBuf::from(sidecar)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(StoreError::Io(error)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation(id: &str, created_at: &str) -> ChatGptImageGeneration {
        ChatGptImageGeneration {
            created_at: created_at.to_owned(),
            id: id.to_owned(),
            conversation_id: "conv".to_owned(),
            message_id: "msg".to_owned(),
            asset_pointer: "pointer".to_owned(),
            url: "https://example.test/image.png".to_owned(),
            prompt: "a prompt".to_owned(),
        }
    }

    #[test]
    fn upserts_and_filters_generations() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(temp.path().join("chatgpt.db")).unwrap();
        store.init_db().unwrap();
        store
            .upsert_generations(
                "user@example.com",
                &[
                    generation("b", "2026-01-02T00:00:00+00:00"),
                    generation("a", "2026-01-01T00:00:00+00:00"),
                ],
            )
            .unwrap();

        let all = store
            .get_generations("user@example.com", &GetGenerationsOptions::default())
            .unwrap();
        assert_eq!(
            all.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );

        let ids = HashSet::from(["b".to_owned()]);
        store.mark_uploaded("user@example.com", &ids).unwrap();
        assert!(store.is_uploaded("user@example.com", "b").unwrap());
        assert!(!store.is_uploaded("user@example.com", "a").unwrap());

        let pending = store
            .get_generations("user@example.com", &GetGenerationsOptions::default())
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "a");

        let uploaded = store
            .get_generations(
                "user@example.com",
                &GetGenerationsOptions {
                    include_uploaded: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(uploaded.len(), 2);
    }

    #[test]
    fn caches_data_sources() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(temp.path().join("chatgpt.db")).unwrap();
        store.init_db().unwrap();
        let sources = vec![DataSource {
            id: "source".to_owned(),
        }];
        store.set_cached_data_sources("db", &sources).unwrap();
        assert_eq!(store.get_cached_data_sources("db").unwrap(), Some(sources));
    }

    #[test]
    fn discards_corrupt_cached_data_sources() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("chatgpt.db");
        let store = SqliteStore::open(&path).unwrap();
        store.init_db().unwrap();
        store
            .set_cached_data_sources(
                "db",
                &[DataSource {
                    id: "source".to_owned(),
                }],
            )
            .unwrap();

        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE cache_data_sources SET data_sources_json = 'not json' WHERE db_id = 'db'",
                [],
            )
            .unwrap();
        drop(connection);

        assert_eq!(store.get_cached_data_sources("db").unwrap(), None);
        assert_eq!(store.get_cached_data_sources("db").unwrap(), None);
    }

    #[test]
    fn backs_up_and_restores_through_the_live_connection() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(temp.path().join("chatgpt.db")).unwrap();
        store.init_db().unwrap();
        store
            .upsert_generations(
                "user@example.com",
                &[generation("a", "2026-01-01T00:00:00+00:00")],
            )
            .unwrap();

        let backup_path = temp.path().join("backups").join("chatgpt.db");
        {
            let stale = SqliteStore::open(backup_path.as_path()).unwrap();
            stale.init_db().unwrap();
            stale
                .upsert_generations(
                    "user@example.com",
                    &[generation("stale", "2025-01-01T00:00:00+00:00")],
                )
                .unwrap();
        }
        let mut stale_wal = backup_path.as_os_str().to_os_string();
        stale_wal.push("-wal");
        std::fs::write(PathBuf::from(stale_wal), b"stale wal").unwrap();

        assert_eq!(store.backup(&backup_path).unwrap(), backup_path);
        assert!(backup_path.exists());
        let mut backup_wal = backup_path.as_os_str().to_os_string();
        backup_wal.push("-wal");
        assert!(!PathBuf::from(backup_wal).exists());

        store
            .upsert_generations(
                "user@example.com",
                &[generation("b", "2026-01-02T00:00:00+00:00")],
            )
            .unwrap();
        assert_eq!(
            store
                .get_generations(
                    "user@example.com",
                    &GetGenerationsOptions {
                        include_uploaded: true,
                        ..Default::default()
                    }
                )
                .unwrap()
                .len(),
            2
        );

        store.restore(&backup_path).unwrap();
        let restored = store
            .get_generations(
                "user@example.com",
                &GetGenerationsOptions {
                    include_uploaded: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, "a");

        let invalid = temp.path().join("empty.db");
        Connection::open(&invalid).unwrap();
        assert!(matches!(
            store.restore(&invalid),
            Err(StoreError::InvalidBackup(_))
        ));
        let incompatible = temp.path().join("incompatible.db");
        Connection::open(&incompatible)
            .unwrap()
            .execute_batch("CREATE TABLE image_generations (id TEXT PRIMARY KEY);")
            .unwrap();
        assert!(matches!(
            store.restore(&incompatible),
            Err(StoreError::InvalidBackup(_))
        ));
        let unchanged = store
            .get_generations(
                "user@example.com",
                &GetGenerationsOptions {
                    include_uploaded: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(unchanged.len(), 1);
        assert_eq!(unchanged[0].id, "a");

        let missing = temp.path().join("missing.db");
        assert!(matches!(
            store.restore(&missing),
            Err(StoreError::BackupNotFound(_))
        ));
    }

    #[test]
    fn activity_stats_compare_mixed_utc_formats_at_day_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(temp.path().join("chatgpt.db")).unwrap();
        store.init_db().unwrap();
        store
            .upsert_generations(
                "user@example.com",
                &[
                    generation("before", "2026-01-01T23:59:59.999999+00:00"),
                    generation("start", "2026-01-02T00:00:00+00:00"),
                    generation("inside", "2026-01-02T12:00:00.000000Z"),
                    generation("end", "2026-01-03T00:00:00+00:00"),
                ],
            )
            .unwrap();

        let stats = store
            .get_activity_stats(
                "user@example.com",
                "2026-01-02T00:00:00.000000Z",
                "2026-01-03T00:00:00.000000Z",
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            stats.first_active.as_deref(),
            Some("2026-01-02T00:00:00+00:00")
        );
        assert_eq!(
            stats.last_active.as_deref(),
            Some("2026-01-02T12:00:00.000000Z")
        );
    }

    #[test]
    fn computes_activity_stats() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(temp.path().join("chatgpt.db")).unwrap();
        store.init_db().unwrap();
        store
            .upsert_generations(
                "user@example.com",
                &[
                    generation("a", "2026-01-01T10:00:00+00:00"),
                    generation("b", "2026-01-01T12:00:00+00:00"),
                ],
            )
            .unwrap();

        let stats = store
            .get_activity_stats(
                "user@example.com",
                "2026-01-01T00:00:00+00:00",
                "2026-01-02T00:00:00+00:00",
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            stats.first_active.as_deref(),
            Some("2026-01-01T10:00:00+00:00")
        );
        assert_eq!(
            stats.last_active.as_deref(),
            Some("2026-01-01T12:00:00+00:00")
        );
    }
}
