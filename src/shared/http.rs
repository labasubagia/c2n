//! HTTP error handling and retry policy.

use std::future::Future;
use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use chrono::{DateTime, Utc};
use reqwest::header::HeaderMap;
use reqwest::{Client, Method, StatusCode};
use url::Url;

use crate::shared::constants::{
    HTTP_TIMEOUT_SECONDS, MAX_HTTP_ATTEMPTS, MAX_RETRY_DELAY_SECONDS, MIN_RETRY_DELAY_SECONDS,
};

const BODY_MAX_CHARS: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("HTTP {status} for {method} {url}: {body}")]
    Status {
        status: StatusCode,
        method: Method,
        url: String,
        body: String,
        retry_after: Option<Duration>,
    },
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl HttpError {
    pub fn status(
        status: StatusCode,
        method: Method,
        url: &str,
        body: impl Into<String>,
        retry_after: Option<Duration>,
    ) -> Self {
        Self::Status {
            status,
            method,
            url: safe_url(url),
            body: truncate_body(body.into()),
            retry_after,
        }
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Status { status, .. } => {
                *status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
            }
            Self::Request(error) => {
                error.is_timeout()
                    || error.is_connect()
                    || error.is_request()
                    || error.is_body()
                    || error.is_decode()
            }
            Self::Io(_) => false,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// Build a client with the shared timeout.
pub fn build_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECONDS))
        .build()
}

/// Run an HTTP operation with exponential backoff and `Retry-After` support.
pub async fn retry_http<T, F, Fut>(operation: F) -> Result<T, HttpError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, HttpError>>,
{
    let policy = ExponentialBuilder::default()
        .with_min_delay(Duration::from_secs(MIN_RETRY_DELAY_SECONDS))
        .with_max_delay(Duration::from_secs(MAX_RETRY_DELAY_SECONDS))
        // `backon` counts retries after the first attempt.
        .with_max_times(MAX_HTTP_ATTEMPTS.saturating_sub(1));

    operation
        .retry(policy)
        .when(HttpError::is_retryable)
        .adjust(|error, current| {
            // `None` means the backoff has exhausted its retry budget. Do not
            // let a `Retry-After` header extend the retry loop indefinitely.
            let current = current?;
            let delay = error.retry_after().unwrap_or(current);
            Some(delay.min(Duration::from_secs(MAX_RETRY_DELAY_SECONDS)))
        })
        .notify(|error, delay| {
            tracing::warn!(
                delay_seconds = delay.as_secs_f64(),
                error = %error,
                "retrying HTTP request"
            );
        })
        .await
}

/// Turn a non-success response into a detailed `HttpError`.
pub async fn ensure_success(
    response: reqwest::Response,
    method: Method,
) -> Result<reqwest::Response, HttpError> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let url = response.url().as_str().to_owned();
    let retry_after = parse_retry_after(response.headers());
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<could not read response body>".to_owned());
    Err(HttpError::status(status, method, &url, body, retry_after))
}

/// Strip query strings and fragments so logs never expose secrets.
pub fn safe_url(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.set_password(None).ok();
            parsed.set_username("").ok();
            parsed.to_string()
        }
        Err(_) => url.split(['?', '#']).next().unwrap_or(url).to_owned(),
    }
}

fn truncate_body(body: String) -> String {
    let mut chars = body.chars();
    let truncated: String = chars.by_ref().take(BODY_MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.trim().parse::<f64>() {
        if seconds.is_finite() && seconds >= 0.0 {
            // Cap before converting so an absurd header cannot overflow
            // `Duration` and panic.
            let seconds = seconds.min(MAX_RETRY_DELAY_SECONDS as f64);
            return Some(Duration::from_secs_f64(seconds));
        }
        return None;
    }
    if let Ok(date) = DateTime::parse_from_rfc2822(value) {
        let seconds = (date.with_timezone(&Utc) - Utc::now()).num_seconds().max(0);
        return Some(Duration::from_secs(seconds as u64));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_query_strings() {
        assert_eq!(
            safe_url("https://example.test/path?token=secret#fragment"),
            "https://example.test/path"
        );
    }

    #[test]
    fn caps_absurd_retry_after_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("1e300"),
        );
        assert_eq!(
            parse_retry_after(&headers),
            Some(Duration::from_secs(MAX_RETRY_DELAY_SECONDS))
        );
    }

    #[test]
    fn classifies_retryable_errors() {
        let error = HttpError::status(
            StatusCode::TOO_MANY_REQUESTS,
            Method::GET,
            "https://example.test",
            "slow down",
            Some(Duration::from_secs(5)),
        );
        assert!(error.is_retryable());
        assert_eq!(error.retry_after(), Some(Duration::from_secs(5)));

        let error = HttpError::status(
            StatusCode::BAD_REQUEST,
            Method::POST,
            "https://example.test",
            "bad",
            None,
        );
        assert!(!error.is_retryable());
    }

    #[test]
    fn truncates_long_bodies() {
        let body = "x".repeat(600);
        let truncated = truncate_body(body);
        assert_eq!(truncated.chars().count(), BODY_MAX_CHARS + 3);
        assert!(truncated.ends_with("..."));
    }
}
