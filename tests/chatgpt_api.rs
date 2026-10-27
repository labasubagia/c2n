//! Wiremock tests for the ChatGPT backend adapter.

use std::path::PathBuf;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use c2n::adapters::chatgpt::{ChatGptApi, ChatGptClient, DeleteConversationResult};
use c2n::domain::models::{NotionConfig, ProviderContext};
use c2n::shared::verbosity::{Reporter, Verbosity};

const ACCOUNT: &str = "user@example.com";

fn context() -> ProviderContext {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer token"));
    headers.insert(USER_AGENT, HeaderValue::from_static("agent"));
    ProviderContext {
        headers,
        notion: NotionConfig::default(),
        account_name: ACCOUNT.to_owned(),
    }
}

fn client(server: &MockServer) -> ChatGptClient {
    ChatGptClient::with_base_url(context(), server.uri()).unwrap()
}

#[tokio::test]
async fn parses_image_generations_with_mixed_id_types() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/my/recent/image_gen"))
        .and(query_param("limit", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                {
                    "id": "img1",
                    "message_id": "msg1",
                    "asset_pointer": "pointer1",
                    "conversation_id": "conv1",
                    "url": "https://example.test/a.png",
                    "created_at": 1767225600.5
                },
                {
                    "id": 123,
                    "message_id": 456,
                    "asset_pointer": "pointer2",
                    "conversation_id": "conv2",
                    "url": "https://example.test/b.png",
                    "created_at": "1767225600"
                }
            ]
        })))
        .mount(&server)
        .await;

    let items = client(&server).get_image_generations(100).await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].id, "img1");
    assert_eq!(items[0].created_at, 1_767_225_600.5);
    assert_eq!(items[1].id, "123");
    assert_eq!(items[1].message_id, "456");
    assert_eq!(items[1].created_at, 1_767_225_600.0);
}

#[tokio::test]
async fn treats_conversation_deleted_404_as_already_deleted() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/conversation/conv1"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"detail": "conversation_deleted"})),
        )
        .mount(&server)
        .await;

    let result = client(&server).delete_conversation("conv1").await.unwrap();
    assert_eq!(result, DeleteConversationResult::AlreadyDeleted);
}

#[tokio::test]
async fn deletes_conversation_successfully() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/conversation/conv1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&server)
        .await;

    let result = client(&server).delete_conversation("conv1").await.unwrap();
    assert_eq!(result, DeleteConversationResult::Deleted);
}

#[tokio::test]
async fn downloads_image_to_destination_atomically() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/image.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"image-bytes".to_vec()))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let destination: PathBuf = temp.path().join("nested").join("image.png");
    client(&server)
        .download_image(&format!("{}/image.png", server.uri()), &destination)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&destination).unwrap(), b"image-bytes");
    let leftovers: Vec<_> = std::fs::read_dir(destination.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".c2n-"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files should be cleaned up");
}

#[tokio::test]
async fn paginates_and_deletes_library_images() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/library/nodes"))
        .and(query_param("categories", "image"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                {
                    "id": "n1",
                    "file_id": "f1",
                    "parent_directory_id": "p1",
                    "name": "a.png"
                }
            ],
            "cursor": "next"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/library/nodes"))
        .and(query_param("cursor", "next"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                {
                    "id": "n2",
                    "file_id": "f2",
                    "parent_directory_id": "p2",
                    "name": "b.png"
                }
            ]
        })))
        .mount(&server)
        .await;
    for node in ["n1", "n2"] {
        Mock::given(method("POST"))
            .and(path(format!("/files/library/files/{node}/delete_stream")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("data: {\"ok\":true}\n\ndata: [DONE]\n\n"),
            )
            .mount(&server)
            .await;
    }

    let reporter = Reporter::new(Verbosity::Quiet, None);
    let deleted = client(&server)
        .delete_library_images_by_query(None, 2, &reporter)
        .await
        .unwrap();
    assert_eq!(deleted, 2);
}

#[tokio::test]
async fn stops_after_ten_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/my/recent/image_gen"))
        .respond_with(
            ResponseTemplate::new(500)
                .insert_header("Retry-After", "0")
                .set_body_string("still failing"),
        )
        .mount(&server)
        .await;

    let error = client(&server)
        .get_image_generations(100)
        .await
        .expect_err("persistent server errors should fail");
    assert!(matches!(error, c2n::shared::http::HttpError::Status { .. }));

    let requests = server.received_requests().await.unwrap();
    let generation_requests = requests
        .iter()
        .filter(|request| request.url.path() == "/my/recent/image_gen")
        .count();
    assert_eq!(generation_requests, 10);
}

#[tokio::test]
async fn retries_server_errors_and_honors_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/my/recent/image_gen"))
        .respond_with(
            ResponseTemplate::new(500)
                .insert_header("Retry-After", "0")
                .set_body_string("temporary failure"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/my/recent/image_gen"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(&server)
        .await;

    let items = client(&server).get_image_generations(100).await.unwrap();
    assert!(items.is_empty());

    let requests = server.received_requests().await.unwrap();
    let generation_requests = requests
        .iter()
        .filter(|request| request.url.path() == "/my/recent/image_gen")
        .count();
    assert_eq!(generation_requests, 2);
}
