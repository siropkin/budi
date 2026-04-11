//! Proxy route handlers for forwarding AI agent traffic to upstream providers.
//!
//! Supports two protocol families (ADR-0082):
//! - OpenAI Chat Completions: `POST /v1/chat/completions`, `GET /v1/models`
//! - Anthropic Messages: `POST /v1/messages`
//!
//! Streaming responses use [`SseTapStream`] to extract token metadata from SSE
//! chunks without buffering or modifying the pass-through data (ADR-0082 §5).

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use futures_util::Stream;
use serde_json::Value;

use budi_core::proxy::{ProxyAttribution, ProxyEvent, ProxyProvider};

use crate::ProxyState;

const HEADER_BUDI_REPO: &str = "x-budi-repo";
const HEADER_BUDI_BRANCH: &str = "x-budi-branch";
const HEADER_BUDI_CWD: &str = "x-budi-cwd";

const MAX_BODY_SIZE: usize = 16 * 1024 * 1024; // 16 MiB per ADR-0082

/// Proxy handler for Anthropic Messages API.
/// `POST /v1/messages`
pub async fn anthropic_messages(
    State(state): State<ProxyState>,
    req: Request<Body>,
) -> impl IntoResponse {
    proxy_request(state, req, ProxyProvider::Anthropic, "/v1/messages").await
}

/// Proxy handler for OpenAI Chat Completions API.
/// `POST /v1/chat/completions`
pub async fn openai_chat_completions(
    State(state): State<ProxyState>,
    req: Request<Body>,
) -> impl IntoResponse {
    proxy_request(state, req, ProxyProvider::OpenAi, "/v1/chat/completions").await
}

/// Proxy handler for OpenAI Models API.
/// `GET /v1/models`
pub async fn openai_models(
    State(state): State<ProxyState>,
    req: Request<Body>,
) -> impl IntoResponse {
    proxy_request(state, req, ProxyProvider::OpenAi, "/v1/models").await
}

fn upstream_base_url(state: &ProxyState, provider: ProxyProvider) -> &str {
    match provider {
        ProxyProvider::Anthropic => &state.anthropic_upstream,
        ProxyProvider::OpenAi => &state.openai_upstream,
    }
}

fn proxy_error_json(message: &str) -> Value {
    serde_json::json!({
        "error": {
            "type": "proxy_error",
            "message": message,
        }
    })
}

fn extract_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn proxy_request(
    state: ProxyState,
    req: Request<Body>,
    provider: ProxyProvider,
    path: &str,
) -> Response<Body> {
    let start = Instant::now();
    let method = req.method().clone();
    let incoming_headers = req.headers().clone();

    let attribution = ProxyAttribution::resolve(
        extract_header(&incoming_headers, HEADER_BUDI_REPO).as_deref(),
        extract_header(&incoming_headers, HEADER_BUDI_BRANCH).as_deref(),
        extract_header(&incoming_headers, HEADER_BUDI_CWD).as_deref(),
    );

    let body_bytes: axum::body::Bytes = match read_body(req).await {
        Ok(bytes) => bytes,
        Err(resp) => return resp,
    };

    let (model, is_streaming) = if method == Method::POST && !body_bytes.is_empty() {
        extract_request_metadata(&body_bytes)
    } else {
        (String::new(), false)
    };

    let upstream_url = format!("{}{}", upstream_base_url(&state, provider), path);
    let mut upstream_req = state.http_client.request(method, &upstream_url);
    // ADR-0082 §5: no read timeout on streaming responses. The stream ends
    // when upstream closes or sends a terminal event. Non-streaming requests
    // keep a generous whole-request timeout.
    if !is_streaming {
        upstream_req = upstream_req.timeout(std::time::Duration::from_secs(300));
    }

    upstream_req = forward_headers(upstream_req, &incoming_headers, provider);

    if !body_bytes.is_empty() {
        upstream_req = upstream_req.body(body_bytes.clone());
    }

    let upstream_resp = match upstream_req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            tracing::warn!(
                provider = provider.as_str(),
                model = %model,
                duration_ms,
                "Upstream request failed: {e}"
            );
            record_event(
                &state,
                provider,
                &model,
                None,
                None,
                duration_ms,
                502,
                is_streaming,
                &attribution,
            );
            return build_error_response(
                StatusCode::BAD_GATEWAY,
                &proxy_error_json(&format!("upstream unreachable: {e}")),
            );
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let is_sse = resp_headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));

    if is_sse {
        let tap = SseTapStream {
            inner: Box::pin(upstream_resp.bytes_stream()),
            provider,
            model,
            status_code: status.as_u16(),
            start,
            state,
            attribution,
            line_buf: Vec::new(),
            input_tokens: None,
            output_tokens: None,
            recorded: false,
        };
        let body = Body::from_stream(tap);

        let mut response = Response::builder().status(status);
        copy_response_headers(&resp_headers, response.headers_mut().unwrap());
        return response.body(body).unwrap();
    }

    let resp_bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            tracing::warn!(
                provider = provider.as_str(),
                model = %model,
                duration_ms,
                "Failed to read upstream response: {e}"
            );
            record_event(
                &state,
                provider,
                &model,
                None,
                None,
                duration_ms,
                502,
                is_streaming,
                &attribution,
            );
            return build_error_response(
                StatusCode::BAD_GATEWAY,
                &proxy_error_json(&format!("failed to read upstream response: {e}")),
            );
        }
    };

    let duration_ms = start.elapsed().as_millis() as i64;

    let (input_tokens, output_tokens) = if status.is_success() {
        extract_response_tokens(&resp_bytes, provider)
    } else {
        (None, None)
    };

    record_event(
        &state,
        provider,
        &model,
        input_tokens,
        output_tokens,
        duration_ms,
        status.as_u16(),
        is_streaming,
        &attribution,
    );

    let mut response = Response::builder().status(status);
    copy_response_headers(&resp_headers, response.headers_mut().unwrap());
    response
        .body(Body::from(resp_bytes))
        .unwrap_or_else(|_| build_error_response(StatusCode::INTERNAL_SERVER_ERROR, &proxy_error_json("failed to build response")))
}

/// Wraps an upstream SSE byte stream to extract token metadata without
/// modifying the pass-through data. Records a [`ProxyEvent`] when the stream
/// completes, errors, or is dropped (client disconnect).
///
/// ADR-0082 §5: "capture happens via a tee/tap on the byte stream, not by
/// deserializing and re-serializing every chunk."
struct SseTapStream {
    inner: Pin<Box<dyn Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Send>>,
    provider: ProxyProvider,
    model: String,
    status_code: u16,
    start: Instant,
    state: ProxyState,
    attribution: ProxyAttribution,
    line_buf: Vec<u8>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    recorded: bool,
}

impl Stream for SseTapStream {
    type Item = Result<axum::body::Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                this.scan_sse_bytes(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish_recording();
                Poll::Ready(Some(Err(std::io::Error::other(e))))
            }
            Poll::Ready(None) => {
                this.finish_recording();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for SseTapStream {
    fn drop(&mut self) {
        self.finish_recording();
    }
}

impl SseTapStream {
    /// Scan incoming bytes for complete SSE `data:` lines and extract tokens.
    fn scan_sse_bytes(&mut self, bytes: &[u8]) {
        self.line_buf.extend_from_slice(bytes);
        while let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.line_buf.drain(..=pos).collect();
            let trimmed = line.strip_suffix(b"\n").unwrap_or(&line);
            let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
            if let Some(data) = trimmed.strip_prefix(b"data: ") {
                if data == b"[DONE]" {
                    continue;
                }
                if let Ok(json) = serde_json::from_slice::<Value>(data) {
                    self.extract_tokens(&json);
                }
            }
        }
    }

    /// Best-effort token extraction from a single SSE data event.
    fn extract_tokens(&mut self, json: &Value) {
        match self.provider {
            ProxyProvider::Anthropic => {
                // message_start → .message.usage.input_tokens
                if let Some(n) = json
                    .pointer("/message/usage/input_tokens")
                    .and_then(|v| v.as_i64())
                {
                    self.input_tokens = Some(n);
                }
                // message_delta → .usage.output_tokens
                if let Some(n) = json
                    .pointer("/usage/output_tokens")
                    .and_then(|v| v.as_i64())
                {
                    self.output_tokens = Some(n);
                }
                // Fallback: .usage.input_tokens (some event shapes)
                if self.input_tokens.is_none()
                    && let Some(n) =
                        json.pointer("/usage/input_tokens").and_then(|v| v.as_i64())
                {
                    self.input_tokens = Some(n);
                }
            }
            ProxyProvider::OpenAi => {
                // Final chunk (or stream_options include_usage) → .usage.*
                if let Some(usage) = json.get("usage").filter(|u| !u.is_null()) {
                    if let Some(n) = usage.get("prompt_tokens").and_then(|v| v.as_i64()) {
                        self.input_tokens = Some(n);
                    }
                    if let Some(n) = usage.get("completion_tokens").and_then(|v| v.as_i64()) {
                        self.output_tokens = Some(n);
                    }
                }
            }
        }
    }

    /// Record the proxy event exactly once (normal end, error, or Drop).
    fn finish_recording(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        let duration_ms = self.start.elapsed().as_millis() as i64;
        record_event(
            &self.state,
            self.provider,
            &self.model,
            self.input_tokens,
            self.output_tokens,
            duration_ms,
            self.status_code,
            true,
            &self.attribution,
        );
    }
}

async fn read_body(req: Request<Body>) -> Result<axum::body::Bytes, Response<Body>> {
    let body = req.into_body();
    match axum::body::to_bytes(body, MAX_BODY_SIZE).await {
        Ok(bytes) => Ok(bytes),
        Err(_) => Err(build_error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            &proxy_error_json("request body exceeds 16 MiB limit"),
        )),
    }
}

fn extract_request_metadata(body: &[u8]) -> (String, bool) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (String::new(), false),
    };
    let model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_streaming = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (model, is_streaming)
}

fn extract_response_tokens(body: &[u8], provider: ProxyProvider) -> (Option<i64>, Option<i64>) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let usage = parsed.get("usage");
    match provider {
        ProxyProvider::Anthropic => {
            let input = usage.and_then(|u| u.get("input_tokens")).and_then(|v| v.as_i64());
            let output = usage.and_then(|u| u.get("output_tokens")).and_then(|v| v.as_i64());
            (input, output)
        }
        ProxyProvider::OpenAi => {
            let input = usage
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(|v| v.as_i64());
            let output = usage
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|v| v.as_i64());
            (input, output)
        }
    }
}

fn forward_headers(
    mut req: reqwest::RequestBuilder,
    headers: &HeaderMap,
    provider: ProxyProvider,
) -> reqwest::RequestBuilder {
    if let Some(ct) = headers.get(header::CONTENT_TYPE) {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    match provider {
        ProxyProvider::Anthropic => {
            if let Some(key) = headers.get("x-api-key") {
                req = req.header("x-api-key", key);
            }
            if let Some(ver) = headers.get("anthropic-version") {
                req = req.header("anthropic-version", ver);
            }
            if let Some(beta) = headers.get("anthropic-beta") {
                req = req.header("anthropic-beta", beta);
            }
        }
        ProxyProvider::OpenAi => {
            if let Some(auth) = headers.get(header::AUTHORIZATION) {
                req = req.header(header::AUTHORIZATION, auth);
            }
        }
    }
    // Forward accept header if present
    if let Some(accept) = headers.get(header::ACCEPT) {
        req = req.header(header::ACCEPT, accept);
    }
    req
}

fn copy_response_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for name in &[
        header::CONTENT_TYPE,
        header::CACHE_CONTROL,
        header::HeaderName::from_static("x-request-id"),
        header::HeaderName::from_static("request-id"),
    ] {
        if let Some(val) = from.get(name) {
            to.insert(name.clone(), val.clone());
        }
    }
    // SSE-specific headers
    if from
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"))
    {
        to.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
    }
}

fn build_error_response(status: StatusCode, body: &Value) -> Response<Body> {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn record_event(
    state: &ProxyState,
    provider: ProxyProvider,
    model: &str,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    duration_ms: i64,
    status_code: u16,
    is_streaming: bool,
    attribution: &ProxyAttribution,
) {
    let cost_cents = if status_code < 400 {
        budi_core::proxy::compute_proxy_cost_cents(provider, model, input_tokens, output_tokens)
    } else {
        0.0
    };

    let event = ProxyEvent {
        timestamp: chrono::Utc::now().to_rfc3339(),
        provider: provider.to_string(),
        model: model.to_string(),
        input_tokens,
        output_tokens,
        duration_ms,
        status_code,
        is_streaming,
        repo_id: attribution.repo_id.clone(),
        git_branch: attribution.git_branch.clone(),
        ticket_id: attribution.ticket_id.clone(),
        cost_cents,
    };

    tracing::info!(
        provider = %event.provider,
        model = %event.model,
        status = event.status_code,
        duration_ms = event.duration_ms,
        input_tokens = ?event.input_tokens,
        output_tokens = ?event.output_tokens,
        streaming = event.is_streaming,
        repo_id = %event.repo_id,
        git_branch = %event.git_branch,
        ticket_id = %event.ticket_id,
        cost_cents = event.cost_cents,
        "Proxy request completed"
    );

    let db_path = state.analytics_db_path.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = record_event_blocking(&db_path, &event) {
            tracing::warn!("Failed to record proxy event: {e}");
        }
    });
}

fn record_event_blocking(
    db_path: &std::path::Path,
    event: &ProxyEvent,
) -> anyhow::Result<()> {
    let conn = budi_core::analytics::open_db(db_path)?;
    budi_core::proxy::ensure_proxy_schema(&conn)?;
    budi_core::proxy::insert_proxy_event(&conn, event)?;
    // Only insert into the messages table for successful requests with a model.
    if event.status_code < 400
        && !event.model.is_empty()
        && let Err(e) = budi_core::proxy::insert_proxy_message(&conn, event)
    {
        tracing::debug!("Failed to insert proxy message: {e}");
    }
    Ok(())
}
