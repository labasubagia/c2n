//! Integration tests for the sequential sync pipeline's safety guarantees.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use c2n::adapters::chatgpt::{
    ChatGptApi, DeleteConversationResult, ImageGenItem, LibraryImagesResponse,
};
use c2n::adapters::filesystem::Filesystem;
use c2n::adapters::notion::{NotionApi, NotionError};
use c2n::adapters::sqlite::{GenerationStore, SqliteStore};
use c2n::config::Config;
use c2n::domain::models::{AccountConfig, AppConfig, ChatGptImageGeneration};
use c2n::services::sync::{SyncOptions, sync};
use c2n::shared::http::HttpError;
use c2n::shared::verbosity::{Reporter, Verbosity};

const ACCOUNT: &str = "user@example.com";
const DB_ID: &str = "0123456789abcdef";

struct MockChatGpt {
    headers: HeaderMap,
    detail: Value,
    png: Vec<u8>,
    deletes: Mutex<Vec<String>>,
    library_deletes: Mutex<Vec<Option<String>>>,
    library_images: usize,
    delete_should_fail: AtomicBool,
    already_deleted: AtomicBool,
}

#[async_trait]
impl ChatGptApi for MockChatGpt {
    fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    async fn get_conversation_details(&self, _conversation_id: &str) -> Result<Value, HttpError> {
        Ok(self.detail.clone())
    }

    async fn delete_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<DeleteConversationResult, HttpError> {
        self.deletes
            .lock()
            .unwrap()
            .push(conversation_id.to_owned());
        if self.delete_should_fail.load(Ordering::SeqCst) {
            return Err(HttpError::Io(std::io::Error::other("delete failed")));
        }
        if self.already_deleted.load(Ordering::SeqCst) {
            return Ok(DeleteConversationResult::AlreadyDeleted);
        }
        Ok(DeleteConversationResult::Deleted)
    }

    async fn get_image_generations(&self, _limit: usize) -> Result<Vec<ImageGenItem>, HttpError> {
        Ok(vec![ImageGenItem {
            id: "img1".to_owned(),
            message_id: "image-node".to_owned(),
            asset_pointer: "pointer".to_owned(),
            conversation_id: "conv1".to_owned(),
            url: "https://example.test/img1.png".to_owned(),
            created_at: 0.0,
        }])
    }

    async fn download_image(&self, _url: &str, destination: &Path) -> Result<(), HttpError> {
        std::fs::write(destination, &self.png)?;
        Ok(())
    }

    async fn get_library_images(
        &self,
        _query: Option<&str>,
        _cursor: Option<&str>,
    ) -> Result<LibraryImagesResponse, HttpError> {
        Ok(LibraryImagesResponse::default())
    }

    async fn delete_library_file(
        &self,
        _item: &c2n::adapters::chatgpt::LibraryItem,
    ) -> Result<Vec<Value>, HttpError> {
        Ok(Vec::new())
    }

    async fn count_library_images(&self) -> Result<usize, HttpError> {
        Ok(self.library_images)
    }

    async fn count_library_images_by_query(
        &self,
        _query: Option<&str>,
    ) -> Result<usize, HttpError> {
        Ok(self.library_images)
    }

    async fn delete_library_images_by_query(
        &self,
        query: Option<&str>,
        _max_concurrent: usize,
        _reporter: &Reporter,
    ) -> Result<usize, HttpError> {
        self.library_deletes
            .lock()
            .unwrap()
            .push(query.map(str::to_owned));
        Ok(self.library_images)
    }
}

struct MockNotion {
    headers: HeaderMap,
    adds: Mutex<Vec<(String, String)>>,
    add_should_fail: AtomicBool,
}

#[async_trait]
impl NotionApi for MockNotion {
    fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    async fn is_page_exists_in_db(&self, _db_id: &str, _name: &str) -> Result<bool, NotionError> {
        Ok(false)
    }

    async fn add_page_to_db(
        &self,
        db_id: &str,
        file_path: &Path,
        _prompt: &str,
    ) -> Result<Value, NotionError> {
        if self.add_should_fail.load(Ordering::SeqCst) {
            return Err(NotionError::Io(std::io::Error::other("add failed")));
        }
        self.adds.lock().unwrap().push((
            db_id.to_owned(),
            file_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned(),
        ));
        Ok(json!({"id": "page"}))
    }
}

struct TestContext {
    _temp: tempfile::TempDir,
    config: Config,
    store: Arc<SqliteStore>,
    filesystem: Filesystem,
    chatgpt: Arc<MockChatGpt>,
    notion: Arc<MockNotion>,
}

fn test_context() -> TestContext {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("output");
    let filesystem = Filesystem::with_output_root(&output);
    let store = Arc::new(SqliteStore::open(filesystem.db_path()).unwrap());
    store.init_db().unwrap();

    let mut app = AppConfig::default();
    app.notion.api_key = Some("secret".to_owned());
    app.notion.database_id = Some(DB_ID.to_owned());
    app.accounts.insert(
        ACCOUNT.to_owned(),
        AccountConfig {
            authorization_token: "token".to_owned(),
            user_agent: Some("agent".to_owned()),
            cookie_string_base64: Some("Y29va2ll".to_owned()),
            notion_database_id: None,
            history_folder: None,
            image_folder: None,
            conversation_delay: None,
        },
    );
    let config = Config::from_app(PathBuf::from("config.toml"), app).unwrap();

    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")
        .unwrap();
    let detail = json!({
        "mapping": {
            "user": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["draw a cat"]}
                },
                "parent": null
            },
            "image-node": {
                "message": {
                    "author": {"role": "assistant"},
                    "content": {"parts": [{"asset_pointer": "pointer"}]}
                },
                "parent": "user"
            }
        }
    });

    TestContext {
        _temp: temp,
        config,
        store,
        filesystem,
        chatgpt: Arc::new(MockChatGpt {
            headers: HeaderMap::new(),
            detail,
            png,
            deletes: Mutex::new(Vec::new()),
            library_deletes: Mutex::new(Vec::new()),
            library_images: 3,
            delete_should_fail: AtomicBool::new(false),
            already_deleted: AtomicBool::new(false),
        }),
        notion: Arc::new(MockNotion {
            headers: HeaderMap::new(),
            adds: Mutex::new(Vec::new()),
            add_should_fail: AtomicBool::new(false),
        }),
    }
}

fn options() -> SyncOptions {
    SyncOptions {
        account: ACCOUNT.to_owned(),
        db_id: DB_ID.to_owned(),
        delete_conversation: true,
        check_notion_api: false,
        from_history: false,
        include_uploaded: false,
        limit: 100,
        keep_days: None,
        no_cache: false,
        conversation_delay: 0.0,
    }
}

fn image_path(context: &TestContext) -> PathBuf {
    context
        .filesystem
        .resolve_image_folder(None)
        .unwrap()
        .join("img1.png")
}

#[tokio::test]
async fn sync_then_delete_conversation_removes_local_images() {
    let context = test_context();
    let reporter = Reporter::new(Verbosity::Quiet, None);

    sync(
        &options(),
        &context.config,
        context.store.as_ref(),
        &context.filesystem,
        context.chatgpt.as_ref(),
        context.notion.as_ref(),
        &reporter,
    )
    .await
    .unwrap();

    assert_eq!(
        context.notion.adds.lock().unwrap().as_slice(),
        &[(DB_ID.to_owned(), "img1.png".to_owned())]
    );
    assert!(context.store.is_uploaded(ACCOUNT, "img1").unwrap());
    assert_eq!(
        context.chatgpt.deletes.lock().unwrap().as_slice(),
        &["conv1".to_owned()]
    );
    assert!(
        !image_path(&context).exists(),
        "local images must be removed after a successful conversation delete"
    );
}

#[tokio::test]
async fn already_deleted_conversation_removes_local_images() {
    let context = test_context();
    let reporter = Reporter::new(Verbosity::Quiet, None);
    context
        .chatgpt
        .already_deleted
        .store(true, Ordering::SeqCst);

    sync(
        &options(),
        &context.config,
        context.store.as_ref(),
        &context.filesystem,
        context.chatgpt.as_ref(),
        context.notion.as_ref(),
        &reporter,
    )
    .await
    .unwrap();

    assert_eq!(
        context.chatgpt.deletes.lock().unwrap().as_slice(),
        &["conv1".to_owned()]
    );
    assert!(!image_path(&context).exists());
}

#[tokio::test]
async fn failed_conversation_delete_keeps_local_images() {
    let context = test_context();
    let reporter = Reporter::new(Verbosity::Quiet, None);
    context
        .chatgpt
        .delete_should_fail
        .store(true, Ordering::SeqCst);

    sync(
        &options(),
        &context.config,
        context.store.as_ref(),
        &context.filesystem,
        context.chatgpt.as_ref(),
        context.notion.as_ref(),
        &reporter,
    )
    .await
    .unwrap();

    assert!(image_path(&context).exists());
    assert_eq!(
        context.chatgpt.deletes.lock().unwrap().as_slice(),
        &["conv1".to_owned()]
    );
}

#[tokio::test]
async fn failed_upload_keeps_local_images() {
    let context = test_context();
    let reporter = Reporter::new(Verbosity::Quiet, None);
    context.notion.add_should_fail.store(true, Ordering::SeqCst);

    sync(
        &options(),
        &context.config,
        context.store.as_ref(),
        &context.filesystem,
        context.chatgpt.as_ref(),
        context.notion.as_ref(),
        &reporter,
    )
    .await
    .unwrap();

    assert!(image_path(&context).exists());
    assert!(!context.store.is_uploaded(ACCOUNT, "img1").unwrap());
    assert!(context.chatgpt.deletes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn history_replay_keeps_local_images() {
    let context = test_context();
    let reporter = Reporter::new(Verbosity::Quiet, None);
    let mut options = options();
    options.from_history = true;

    context
        .store
        .upsert_generations(
            ACCOUNT,
            &[ChatGptImageGeneration {
                created_at: "2026-01-01T00:00:00.000000Z".to_owned(),
                id: "img1".to_owned(),
                conversation_id: "conv1".to_owned(),
                message_id: "image-node".to_owned(),
                asset_pointer: "pointer".to_owned(),
                url: "https://example.test/img1.png".to_owned(),
                prompt: "draw a cat".to_owned(),
            }],
        )
        .unwrap();

    let path = image_path(&context);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &context.chatgpt.png).unwrap();

    sync(
        &options,
        &context.config,
        context.store.as_ref(),
        &context.filesystem,
        context.chatgpt.as_ref(),
        context.notion.as_ref(),
        &reporter,
    )
    .await
    .unwrap();

    assert!(path.exists());
    assert!(context.chatgpt.deletes.lock().unwrap().is_empty());
}
