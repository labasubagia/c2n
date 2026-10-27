//! Wiremock tests for the Notion upload and deduplication flow.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use c2n::adapters::notion::{NotionApi, NotionClient};
use c2n::adapters::sqlite::{GenerationStore, SqliteStore};
use c2n::domain::models::NotionContext;

const DB_ID: &str = "0123456789abcdef";

fn context() -> NotionContext {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
    headers.insert(
        HeaderName::from_static("notion-version"),
        HeaderValue::from_static("2025-09-03"),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    NotionContext {
        headers,
        database_id: Some(DB_ID.to_owned()),
        account_name: "user@example.com".to_owned(),
    }
}

fn png_file() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("image.png");
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")
        .unwrap();
    std::fs::write(&path, png).unwrap();
    (temp, path)
}

fn store() -> (tempfile::TempDir, Arc<SqliteStore>) {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::open(temp.path().join("chatgpt.db")).unwrap());
    store.init_db().unwrap();
    (temp, store)
}

#[tokio::test]
async fn uploads_file_and_creates_page_with_prompt_and_email() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/file_uploads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "upload-1"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/file_uploads/upload-1/send"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "upload-1", "status": "complete"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/pages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "page-1"})))
        .mount(&server)
        .await;

    let (_store_temp, store) = store();
    let client = NotionClient::with_base_url(context(), store, server.uri()).unwrap();
    let (_temp, file_path) = png_file();

    let response = client
        .add_page_to_db(DB_ID, &file_path, "draw a cat\r\nsecond line")
        .await
        .unwrap();
    assert_eq!(response["id"], "page-1");

    let requests = server.received_requests().await.unwrap();

    let create = requests
        .iter()
        .find(|request| request.url.path() == "/v1/file_uploads")
        .expect("create upload request");
    let create_body: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(create_body["mode"], "single_part");
    assert_eq!(create_body["filename"], "image.png");
    assert_eq!(create_body["content_type"], "image/png");

    let send = requests
        .iter()
        .find(|request| request.url.path() == "/v1/file_uploads/upload-1/send")
        .expect("send upload request");
    assert_eq!(
        send.headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer secret")
    );
    assert_eq!(
        send.headers
            .get("notion-version")
            .and_then(|value| value.to_str().ok()),
        Some("2025-09-03")
    );
    assert!(
        send.headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .starts_with("multipart/form-data")
    );

    let page = requests
        .iter()
        .find(|request| request.url.path() == "/v1/pages")
        .expect("page request");
    let page_body: Value = serde_json::from_slice(&page.body).unwrap();
    assert_eq!(page_body["parent"]["database_id"], DB_ID);
    assert_eq!(
        page_body["properties"]["Name"]["title"][0]["text"]["content"],
        "image.png"
    );
    assert_eq!(
        page_body["properties"]["Image"]["files"][0]["file_upload"]["id"],
        "upload-1"
    );
    let markdown = page_body["markdown"].as_str().unwrap();
    assert!(markdown.contains("**Prompt:**"));
    assert!(markdown.contains("draw a cat\nsecond line"));
    assert!(markdown.contains("**Email:** user@example.com"));
}

#[tokio::test]
async fn send_upload_400_is_treated_as_already_uploaded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/file_uploads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "upload-1"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/file_uploads/upload-1/send"))
        .respond_with(ResponseTemplate::new(400).set_body_string("already uploaded"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/pages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "page-1"})))
        .mount(&server)
        .await;

    let (_store_temp, store) = store();
    let client = NotionClient::with_base_url(context(), store, server.uri()).unwrap();
    let (_temp, file_path) = png_file();
    client
        .add_page_to_db(DB_ID, &file_path, "prompt")
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let page = requests
        .iter()
        .find(|request| request.url.path() == "/v1/pages")
        .expect("page request");
    let page_body: Value = serde_json::from_slice(&page.body).unwrap();
    assert_eq!(
        page_body["properties"]["Image"]["files"][0]["file_upload"]["id"],
        "upload-1"
    );
}

#[tokio::test]
async fn retries_page_creation_with_the_same_upload_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/file_uploads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "upload-1"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/file_uploads/upload-1/send"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "upload-1", "status": "complete"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/pages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("transient"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/pages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "page-1"})))
        .mount(&server)
        .await;

    let (_store_temp, store) = store();
    let client = NotionClient::with_base_url(context(), store, server.uri()).unwrap();
    let (_temp, file_path) = png_file();
    client
        .add_page_to_db(DB_ID, &file_path, "prompt")
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/v1/file_uploads")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/v1/file_uploads/upload-1/send")
            .count(),
        1
    );
    let page_requests = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/pages")
        .collect::<Vec<_>>();
    assert_eq!(page_requests.len(), 2);
    for request in page_requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body["properties"]["Image"]["files"][0]["file_upload"]["id"],
            "upload-1"
        );
    }
}

#[tokio::test]
async fn refuses_to_upload_when_database_has_no_data_sources() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/databases/{DB_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data_sources": []
        })))
        .mount(&server)
        .await;

    let (_store_temp, store) = store();
    let client = NotionClient::with_base_url(context(), store, server.uri()).unwrap();
    let error = client
        .is_page_exists_in_db(DB_ID, "image.png")
        .await
        .expect_err("empty data sources should fail closed");
    assert!(matches!(
        error,
        c2n::adapters::notion::NotionError::NoDataSources(_)
    ));
}

#[tokio::test]
async fn deduplicates_by_page_title_and_caches_data_sources() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/databases/{DB_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data_sources": [{"id": "ds1"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/data_sources/ds1/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "properties": {
                    "Name": {
                        "title": [{"text": {"content": "image.png"}}]
                    }
                }
            }]
        })))
        .mount(&server)
        .await;

    let (_store_temp, store) = store();
    let client = NotionClient::with_base_url(context(), store, server.uri()).unwrap();
    assert!(
        client
            .is_page_exists_in_db(DB_ID, "image.png")
            .await
            .unwrap()
    );
    assert!(
        client
            .is_page_exists_in_db(DB_ID, "image.png")
            .await
            .unwrap()
    );

    let database_path = format!("/v1/databases/{DB_ID}");
    let requests = server.received_requests().await.unwrap();
    let database_requests = requests
        .iter()
        .filter(|request| request.url.path() == database_path.as_str())
        .count();
    assert_eq!(database_requests, 1, "data sources should be cached");
}
