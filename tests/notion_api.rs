//! Wiremock tests for the Notion upload and deduplication flow.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, method, path};
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
async fn reconciles_an_ambiguous_page_creation_error() {
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
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/databases/{DB_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data_sources": [{"id": "ds1"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/data_sources/ds1/query"))
        .and(body_json(json!({
            "filter": {
                "and": [{"property": "Name", "title": {"equals": "image.png"}}]
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "properties": {"Name": {"title": [{"plain_text": "other.png"}]}}
            }],
            "has_more": true,
            "next_cursor": "page-2"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/data_sources/ds1/query"))
        .and(body_json(json!({
            "filter": {
                "and": [{"property": "Name", "title": {"equals": "image.png"}}]
            },
            "start_cursor": "page-2"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "properties": {
                    "Name": {"title": [
                        {"plain_text": "image"},
                        {"text": {"content": ".png"}}
                    ]}
                }
            }],
            "has_more": false,
            "next_cursor": null
        })))
        .mount(&server)
        .await;

    let (_store_temp, store) = store();
    let client = NotionClient::with_base_url(context(), store, server.uri()).unwrap();
    let (_temp, file_path) = png_file();
    let response = client
        .add_page_to_db(DB_ID, &file_path, "prompt")
        .await
        .expect("existing page should reconcile the ambiguous response");
    assert_eq!(response["reconciled"], true);
    assert_eq!(response["name"], "image.png");

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
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/v1/pages")
            .count(),
        1
    );
    let queries: Vec<Value> = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/data_sources/ds1/query")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(queries.len(), 2, "reconciliation should traverse all pages");
    assert_eq!(queries[1]["start_cursor"], "page-2");
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
            }],
            "has_more": false,
            "next_cursor": null
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

    let query_request = requests
        .iter()
        .find(|request| request.url.path() == "/v1/data_sources/ds1/query")
        .expect("data source query request");
    let query_body: Value = serde_json::from_slice(&query_request.body).unwrap();
    assert_eq!(query_body["filter"]["and"][0]["property"], "Name");
    assert_eq!(
        query_body["filter"]["and"][0]["title"]["equals"],
        "image.png"
    );
    assert!(
        query_body["filter"]["and"][0].get("rich_text").is_none(),
        "title properties must use a title filter"
    );
}
