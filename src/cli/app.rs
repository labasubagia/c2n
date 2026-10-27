//! Command-line interface and command handlers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use comfy_table::{Table, presets::UTF8_FULL};
use tracing_subscriber::EnvFilter;

use crate::adapters::chatgpt::{ChatGptApi, ChatGptClient};
use crate::adapters::filesystem::Filesystem;
use crate::adapters::notion::{NotionApi, NotionClient};
use crate::adapters::sqlite::{GenerationStore, SqliteStore};
use crate::config::Config;
use crate::domain::models::RequiredVar;
use crate::services::account_status::{ActivityRow, get_account_activity_statuses};
use crate::services::upload::{UploadOptions, upload_to_notion_sequential};
use crate::shared::constants::DEFAULT_CONFIG_PATH;
use crate::shared::time::resolve_timezone;
use crate::shared::verbosity::{Reporter, Verbosity};

#[derive(Debug, Parser)]
#[command(
    name = "c2n",
    version,
    about = "Download ChatGPT image generations, embed prompts, and archive them in Notion"
)]
pub struct Cli {
    /// Path to the TOML configuration file.
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        default_value = DEFAULT_CONFIG_PATH
    )]
    pub config: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Upload image generations to Notion.
    #[command(name = "upload-to-notion")]
    UploadToNotion(UploadArgs),

    /// Show which accounts are ready to generate new data.
    #[command(name = "account-status")]
    AccountStatus(AccountStatusArgs),

    /// Clean the output directory, preserving the database.
    #[command(name = "clean-output-path")]
    CleanOutputPath,

    /// Copy the local SQLite database to TARGET.
    #[command(name = "backup-db")]
    BackupDb {
        /// Destination path for the backup file.
        target: PathBuf,
    },

    /// Overwrite the local SQLite database with SOURCE.
    #[command(name = "restore-db")]
    RestoreDb {
        /// Path to the backup file to restore from.
        source: PathBuf,
    },

    /// Remove images from the ChatGPT library, optionally filtered by query.
    #[command(name = "delete-library")]
    DeleteLibrary(DeleteLibraryArgs),
}

#[derive(Debug, Args)]
pub struct UploadArgs {
    /// Path to a history folder (accepted for compatibility; SQLite is the source of truth).
    #[arg(long)]
    pub history_folder: Option<String>,

    /// Image output folder (absolute, or relative to output/).
    #[arg(long)]
    pub image_folder: Option<String>,

    /// Notion database ID (overrides the configured database).
    #[arg(long)]
    pub db_id: Option<String>,

    /// Upload images to Notion (default; accepted for CLI compatibility).
    #[arg(long, conflicts_with = "no_upload_to_notion")]
    pub upload_to_notion: bool,

    /// Download images and embed prompts without uploading them to Notion.
    #[arg(long = "no-upload-to-notion")]
    pub no_upload_to_notion: bool,

    /// Hide conversations in ChatGPT after all their images are verified uploaded.
    #[arg(long)]
    pub delete_conversation: bool,

    /// Delete images from the ChatGPT library after upload, using configured queries.
    #[arg(long)]
    pub delete_library: bool,

    /// Delete ALL images from the ChatGPT library after upload.
    #[arg(long)]
    pub delete_library_all: bool,

    /// Actually delete library images. Without this flag, library deletion is a dry run.
    #[arg(long)]
    pub yes: bool,

    /// Check the Notion API even when uploaded_at is already set (default).
    #[arg(long, conflicts_with = "no_check_notion_api")]
    pub check_notion_api: bool,

    /// Skip remote Notion checks even when uploaded_at is already set.
    #[arg(long = "no-check-notion-api")]
    pub no_check_notion_api: bool,

    /// Use local history data as the source instead of the live ChatGPT API.
    #[arg(long)]
    pub from_history: bool,

    /// Shortcut for --from-history --check-notion-api (includes uploaded items).
    #[arg(long)]
    pub verify_history: bool,

    /// Use all history data, not just the last three days (requires --from-history or --verify-history).
    #[arg(long)]
    pub all: bool,

    /// Bypass the SQLite cache and fetch fresh data from the ChatGPT API.
    #[arg(long)]
    pub no_cache: bool,

    /// Maximum number of recent image generations to fetch.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,

    /// IANA timezone name, e.g. Asia/Jakarta.
    #[arg(long)]
    pub timezone: Option<String>,

    /// Named account from the TOML config.
    #[arg(long)]
    pub account: Option<String>,

    /// Delay in seconds between conversations.
    #[arg(long)]
    pub conversation_delay: Option<f64>,

    /// Show per-file output.
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// Show only summary counts.
    #[arg(long, short = 'q')]
    pub quiet: bool,

    /// Append failed items to a JSONL log file.
    #[arg(long)]
    pub fail_log: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct AccountStatusArgs {
    /// IANA timezone name, e.g. Asia/Singapore.
    #[arg(long)]
    pub timezone: Option<String>,
}

#[derive(Debug, Args)]
pub struct DeleteLibraryArgs {
    /// Search query used to filter library images (omit to target all images).
    #[arg(value_name = "QUERY")]
    pub query: Option<String>,

    /// Maximum number of concurrent delete requests.
    #[arg(long, default_value_t = 10)]
    pub max_concurrent: usize,

    /// Named account from the TOML config.
    #[arg(long)]
    pub account: Option<String>,

    /// Actually delete images. Without this flag, only count matching images.
    #[arg(long)]
    pub yes: bool,
}

pub async fn run(cli: Cli) -> Result<()> {
    let Cli { config, command } = cli;
    init_tracing(verbosity_for(&command));

    match command {
        Command::UploadToNotion(args) => run_upload(&config, args).await,
        Command::AccountStatus(args) => run_account_status(&config, args),
        Command::CleanOutputPath => run_clean_output(),
        Command::BackupDb { target } => run_backup(&target),
        Command::RestoreDb { source } => run_restore(&source),
        Command::DeleteLibrary(args) => run_delete_library(&config, args).await,
    }
}

async fn run_upload(config_path: &Path, args: UploadArgs) -> Result<()> {
    validate_upload_args(&args)?;

    let config = Config::load(Some(config_path))?;
    let filesystem = Filesystem::new();
    let store = Arc::new(SqliteStore::open(filesystem.db_path())?);
    store.init_db()?;

    let verbosity = if args.quiet {
        Verbosity::Quiet
    } else if args.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Simple
    };
    let reporter = Reporter::new(verbosity, args.fail_log.clone());
    let upload_enabled = !args.no_upload_to_notion;

    if args.history_folder.is_some() {
        reporter.verbose(
            "--history-folder is accepted for compatibility; the SQLite database is the source of truth",
        );
    }
    if args.timezone.is_some() {
        reporter.verbose(
            "--timezone is accepted for compatibility; keep-days filtering uses absolute time",
        );
    }
    if args.no_upload_to_notion && args.delete_conversation {
        reporter.warn("--delete-conversation is ignored when --no-upload-to-notion is used");
    }

    let target_accounts = if let Some(account) = args.account.as_deref() {
        config.account(account)?;
        vec![account.to_owned()]
    } else {
        let names = config.account_names();
        if names.is_empty() {
            bail!("No accounts found in config.");
        }
        names
    };

    let total_accounts = target_accounts.len();
    let mut had_error = false;

    for (index, account_name) in target_accounts.iter().enumerate() {
        let resolved = config.account(account_name)?;
        let configured_db_id = args
            .db_id
            .clone()
            .or_else(|| resolved.notion.database_id.clone());

        let db_id = if upload_enabled {
            let Some(db_id) = configured_db_id else {
                bail!("db_id must be provided");
            };
            validate_db_id(&db_id)?;
            db_id
        } else {
            if configured_db_id.is_some() {
                reporter.verbose(
                    "--db-id and notion.database_id are ignored when uploads are disabled",
                );
            }
            configured_db_id.unwrap_or_default()
        };

        let mut required = vec![
            RequiredVar::ChatGptAuthorizationToken,
            RequiredVar::ChatGptUserAgent,
            RequiredVar::ChatGptCookie,
        ];
        if upload_enabled {
            required.push(RequiredVar::NotionApiKey);
            if args.db_id.is_none() {
                required.push(RequiredVar::NotionDatabaseId);
            }
        }
        resolved.validate_runtime(&required)?;

        print_account_header(
            &reporter,
            "Upload To Notion",
            account_name,
            index + 1,
            total_accounts,
        );

        let chatgpt = ChatGptClient::new(resolved.provider_context()?)?;
        let notion = if upload_enabled {
            Some(NotionClient::new(
                resolved.notion_context()?,
                store.clone(),
            )?)
        } else {
            None
        };
        let notion_api: Option<&dyn NotionApi> =
            notion.as_ref().map(|client| client as &dyn NotionApi);

        let effective_from_history = args.from_history || args.verify_history;
        let effective_check_notion_api =
            args.check_notion_api || !args.no_check_notion_api || args.verify_history;
        let effective_keep_days = if effective_from_history && !args.all {
            Some(3)
        } else {
            None
        };
        let conversation_delay = args
            .conversation_delay
            .or(resolved.conversation_delay)
            .unwrap_or(2.0);
        if !conversation_delay.is_finite() || conversation_delay < 0.0 {
            bail!("--conversation-delay must be a non-negative number");
        }

        let options = UploadOptions {
            account: account_name.clone(),
            db_id: db_id.clone(),
            image_folder: args.image_folder.clone(),
            upload_to_notion: upload_enabled,
            delete_conversation: args.delete_conversation,
            delete_library: args.delete_library,
            delete_library_all: args.delete_library_all,
            confirm_delete_library: args.yes,
            check_notion_api: effective_check_notion_api,
            from_history: effective_from_history,
            include_uploaded: args.verify_history,
            limit: args.limit,
            keep_days: effective_keep_days,
            no_cache: args.no_cache,
            conversation_delay,
        };

        let result = upload_to_notion_sequential(
            &options,
            &config,
            store.as_ref(),
            &filesystem,
            &chatgpt,
            notion_api,
            &reporter,
        )
        .await;

        match result {
            Ok(()) => print_account_footer(
                &reporter,
                "Upload To Notion",
                account_name,
                index + 1,
                total_accounts,
            ),
            Err(error) => {
                reporter.error(format!("[{account_name}] {error:#}"));
                had_error = true;
            }
        }
    }

    if had_error {
        bail!("one or more accounts failed");
    }
    Ok(())
}

fn run_account_status(config_path: &Path, args: AccountStatusArgs) -> Result<()> {
    let config = Config::load(Some(config_path))?;
    let filesystem = Filesystem::new();
    let store = SqliteStore::open(filesystem.db_path())?;
    store.init_db()?;

    let report = get_account_activity_statuses(&config, &store, args.timezone.as_deref(), None)?;
    let timezone = resolve_timezone(args.timezone.as_deref())?;
    let current_time = Utc::now()
        .with_timezone(&timezone)
        .format("%Y-%m-%d %H:%M:%S");
    println!("Current Time: {current_time} ({})", timezone.name());

    print_activity_table(
        &report.today,
        &format!("Today ({}/{})", report.today.len(), report.total_accounts),
    );
    print_activity_table(
        &report.yesterday,
        &format!(
            "Yesterday ({}/{})",
            report.yesterday.len(),
            report.total_accounts
        ),
    );

    Ok(())
}

fn run_clean_output() -> Result<()> {
    println!("Cleaning output path...");
    Filesystem::new().clean_output()?;
    println!("Output path cleaned.");
    Ok(())
}

fn run_backup(target: &Path) -> Result<()> {
    println!("Backing up database to {}...", target.display());
    let store = SqliteStore::open(Filesystem::new().db_path())?;
    store.init_db()?;
    let result = store.backup(target)?;
    println!("Backup saved to {}", result.display());
    Ok(())
}

fn run_restore(source: &Path) -> Result<()> {
    println!("Restoring database from {}...", source.display());
    let store = SqliteStore::open(Filesystem::new().db_path())?;
    let result = store.restore(source)?;
    println!("Database restored from {}", result.display());
    Ok(())
}

async fn run_delete_library(config_path: &Path, args: DeleteLibraryArgs) -> Result<()> {
    if args.max_concurrent == 0 {
        bail!("--max-concurrent must be at least 1");
    }
    let query = match args.query.as_deref() {
        Some(query) if query.trim().is_empty() => {
            bail!("QUERY must not be blank; omit it to target all images")
        }
        Some(query) => Some(query.trim()),
        None => None,
    };

    let config = Config::load(Some(config_path))?;
    let target_accounts = if let Some(account) = args.account.as_deref() {
        config.account(account)?;
        vec![account.to_owned()]
    } else {
        let names = config.account_names();
        if names.is_empty() {
            bail!("No accounts found in config.");
        }
        names
    };

    let reporter = Reporter::new(Verbosity::Simple, None);
    let label = query
        .map(|query| format!("'{query}'"))
        .unwrap_or_else(|| "(all images)".to_owned());

    for account in target_accounts {
        let resolved = config.account(&account)?;
        resolved.validate_runtime(&[
            RequiredVar::ChatGptAuthorizationToken,
            RequiredVar::ChatGptUserAgent,
            RequiredVar::ChatGptCookie,
        ])?;
        let chatgpt = ChatGptClient::new(resolved.provider_context()?)?;

        if !args.yes {
            let count = chatgpt.count_library_images_by_query(query).await?;
            reporter.println(format!(
                "[{account}] Dry run: {count} image(s) would be deleted matching {label}. Pass --yes to delete."
            ));
            continue;
        }

        let deleted = chatgpt
            .delete_library_images_by_query(query, args.max_concurrent, &reporter)
            .await?;
        reporter.println(format!(
            "[{account}] Deleted {deleted} images matching {label}"
        ));

        let remaining = chatgpt.count_library_images().await?;
        reporter.println(format!(
            "[{account}] Total images remaining in library: {remaining}"
        ));
    }

    Ok(())
}

fn validate_upload_args(args: &UploadArgs) -> Result<()> {
    if args.verbose && args.quiet {
        bail!("Cannot use --verbose and --quiet together");
    }
    if args.delete_library && args.delete_library_all {
        bail!("Use either --delete-library or --delete-library-all, not both");
    }
    if args.all && !(args.from_history || args.verify_history) {
        bail!("--all only applies with --from-history or --verify-history");
    }
    Ok(())
}

fn validate_db_id(db_id: &str) -> Result<()> {
    if db_id.trim().len() < 10 {
        bail!("Notion database ID must be a valid ID");
    }
    Ok(())
}

fn print_account_header(
    reporter: &Reporter,
    action: &str,
    account: &str,
    position: usize,
    total: usize,
) {
    let rule = "-".repeat(60);
    reporter.println(format!(
        "\n{rule}\n[{position}/{total}] {action}\nAccount: {account}\n{rule}"
    ));
}

fn print_account_footer(
    reporter: &Reporter,
    action: &str,
    account: &str,
    position: usize,
    total: usize,
) {
    let rule = "-".repeat(60);
    reporter.println(format!(
        "Finished [{position}/{total}] {action} for {account}\n{rule}"
    ));
}

fn print_activity_table(rows: &[ActivityRow], title: &str) {
    if rows.is_empty() {
        println!("\n{title}");
        println!("No accounts have data for this period.");
        return;
    }

    let mut table = Table::new();
    table.load_style(UTF8_FULL);
    table.set_header(vec![
        "Account",
        "Next Wait",
        "Next Cooldown",
        "Fully Ready In",
        "Total Wait",
        "Ready Generate?",
    ]);
    for row in rows {
        table.add_row(vec![
            row.account.clone(),
            row.next_wait.clone(),
            row.next_cooldown.clone(),
            row.fully_ready_in.clone(),
            row.total_wait.clone(),
            row.ready_generate.clone(),
        ]);
    }

    println!("\n{title}");
    println!("{table}");
}

fn verbosity_for(command: &Command) -> Verbosity {
    match command {
        Command::UploadToNotion(args) => {
            if args.quiet {
                Verbosity::Quiet
            } else if args.verbose {
                Verbosity::Verbose
            } else {
                Verbosity::Simple
            }
        }
        _ => Verbosity::Simple,
    }
}

fn init_tracing(verbosity: Verbosity) {
    let default_filter = match verbosity {
        Verbosity::Quiet => "c2n=warn",
        Verbosity::Simple => "c2n=info",
        Verbosity::Verbose => "c2n=debug",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload_args() -> UploadArgs {
        UploadArgs {
            history_folder: None,
            image_folder: None,
            db_id: None,
            upload_to_notion: false,
            no_upload_to_notion: false,
            delete_conversation: false,
            delete_library: false,
            delete_library_all: false,
            yes: false,
            check_notion_api: false,
            no_check_notion_api: false,
            from_history: false,
            verify_history: false,
            all: false,
            no_cache: false,
            limit: 100,
            timezone: None,
            account: None,
            conversation_delay: None,
            verbose: false,
            quiet: false,
            fail_log: None,
        }
    }

    #[test]
    fn rejects_conflicting_verbosity() {
        let mut args = upload_args();
        args.verbose = true;
        args.quiet = true;
        assert!(validate_upload_args(&args).is_err());
    }

    #[test]
    fn rejects_all_without_history_mode() {
        let mut args = upload_args();
        args.all = true;
        assert!(validate_upload_args(&args).is_err());
    }

    #[test]
    fn accepts_all_with_history_mode() {
        let mut args = upload_args();
        args.all = true;
        args.from_history = true;
        assert!(validate_upload_args(&args).is_ok());
    }

    #[test]
    fn rejects_short_database_ids() {
        assert!(validate_db_id("short").is_err());
        assert!(validate_db_id("0123456789abcdef").is_ok());
    }
}
