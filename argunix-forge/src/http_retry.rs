//! Retry transient transport-level errors on outbound forge HTTP calls.
//!
//! Forges (especially smaller / self-hosted ones like gitlab.opencode.de
//! and codeberg) occasionally drop a TCP connect, fail a TLS handshake,
//! or time out mid-request. reqwest surfaces these as
//! `Error::is_connect()` / `is_timeout()` / `is_request()`. A single
//! retry with a short backoff usually clears it. We deliberately do
//! NOT retry on HTTP responses (a 401, 404, 5xx is a real answer that
//! the call site must interpret — e.g. 401 pauses the forge).
//!
//! Retry is applied uniformly to GETs and writes. The writes
//! (`post_check`, `ensure_webhook`) are tolerant of duplicate delivery
//! at the forge level: GitLab statuses 400 on no-op transitions (and
//! we already swallow that), GitHub/Forgejo accept idempotent re-posts,
//! and hook PUT/PATCH is idempotent. A transport error means the
//! request either never reached the server or the response never came
//! back, so the worst case from retrying is a duplicate that the forge
//! collapses to a no-op.

use crate::ForgeError;
use std::time::Duration;

/// Total attempts (1 initial + 2 retries).
const MAX_ATTEMPTS: u32 = 3;
/// Backoff before retry 1; retry 2 waits twice as long.
const BASE_BACKOFF_MS: u64 = 200;

/// Send the request built by `build`, retrying transient transport-level
/// failures. The closure is called fresh on every attempt so that
/// `RequestBuilder` (consumed by `send`) can be rebuilt each time.
pub(crate) async fn send_with_retry<F>(build: F) -> Result<reqwest::Response, ForgeError>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match build().send().await {
            Ok(resp) => return Ok(resp),
            Err(e) if is_transient(&e) && attempt < MAX_ATTEMPTS => {
                let backoff = Duration::from_millis(BASE_BACKOFF_MS << (attempt - 1));
                tracing::warn!(
                    error = %e,
                    attempt,
                    next_backoff_ms = backoff.as_millis() as u64,
                    "transient forge HTTP error; retrying",
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn is_transient(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout() || e.is_request()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A reachable server with a 200 should not be retried.
    #[tokio::test]
    async fn success_response_makes_one_attempt() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(move |_: &wiremock::Request| {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200)
            })
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/ok", server.uri());
        let resp = send_with_retry(|| client.get(&url)).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A 5xx is a real answer and must NOT be retried (the caller decides).
    #[tokio::test]
    async fn server_error_response_makes_one_attempt() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        Mock::given(method("GET"))
            .and(path("/err"))
            .respond_with(move |_: &wiremock::Request| {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            })
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/err", server.uri());
        let resp = send_with_retry(|| client.get(&url)).await.unwrap();
        assert_eq!(resp.status().as_u16(), 500);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A connect refusal (server never started on that port) should be
    /// retried up to MAX_ATTEMPTS, then surface as ForgeError::Http.
    #[tokio::test]
    async fn connect_failure_retries_then_errors() {
        // Bind & immediately drop a listener so we have a port no one
        // is listening on. Linux will reject SYN with RST →
        // reqwest::Error::is_connect() == true.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}/x");

        let started = std::time::Instant::now();
        let err = send_with_retry(|| client.get(&url)).await.unwrap_err();
        let elapsed = started.elapsed();

        assert!(matches!(err, ForgeError::Http(_)), "got {err:?}");
        // 200ms + 400ms backoff = at least 600ms total wait before final error.
        assert!(
            elapsed >= Duration::from_millis(550),
            "did not back off across retries: {elapsed:?}",
        );
    }
}
