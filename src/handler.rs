//! Transparent proxying to Mistral's OpenAI-compatible API.
//!
//! Requests are forwarded verbatim (method, path, query, JSON body) with the
//! Authorization header replaced by the key vibe stores; responses —
//! including SSE streams — are relayed byte-for-byte. Model ids that match a
//! Vibe alias in `$VIBE_HOME/config.toml` are mapped onto the provider model
//! name, and `/v1/models` merges Vibe's aliases into the upstream list.
//!
//! Additional features:
//! - Forwards x-affinity header (or uses a generated session ID) for Mistral's prompt cache
//! - Rewrites Mistral's context overflow errors to OpenCode-compatible format

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde_json::Value;
use tracing::{info, warn};
use uuid::Uuid;

use crate::key;
use crate::models::{ModelRegistry, DEFAULT_MODEL_NAME};
use crate::overflow::rewrite_overflow_response;

/// Request headers never forwarded upstream.
const HOP_HEADERS: [&str; 6] = [
    "host",
    "authorization",
    "content-length",
    "connection",
    "accept-encoding",
    "transfer-encoding",
];

/// Response headers never relayed downstream.
const RESP_DROP_HEADERS: [&str; 2] = ["content-length", "transfer-encoding"];

/// Header name for session affinity
const AFFINITY_HEADER: &str = "x-affinity";

/// Header name for OpenCode session ID
const SESSION_ID_HEADER: &str = "x-opencode-session-id";

pub struct AppState {
    pub client: reqwest::Client,
    /// Upstream base, e.g. `https://api.mistral.ai/v1` (no trailing slash).
    pub api_base: String,
    /// Optional key clients must present to the proxy itself.
    pub proxy_api_key: Option<String>,
    pub models: ModelRegistry,
}

fn openai_error(status: StatusCode, message: String, err_type: &str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": { "message": message, "type": err_type }
        })),
    )
        .into_response()
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let Some(expected) = state.proxy_api_key.as_deref() else {
        return Ok(());
    };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let alt = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    if bearer == Some(expected) || alt == Some(expected) {
        Ok(())
    } else {
        Err(openai_error(
            StatusCode::UNAUTHORIZED,
            "Invalid API key".to_string(),
            "invalid_request_error",
        ))
    }
}

pub async fn health() -> impl IntoResponse {
    (StatusCode::OK, "I'm alive!")
}

/// Local path -> upstream path. A `/v1` prefix is stripped so `/v1/models`,
/// `/models`, `/v1/chat/completions` and `/chat/completions` all map onto the
/// same upstream route under `api_base` (which already ends in `/v1`).
fn upstream_path(path: &str) -> &str {
    let p = path.trim_start_matches('/');
    p.strip_prefix("v1/").unwrap_or(p)
}

/// Rewrite the `model` field of a chat/completions request body: Vibe aliases
/// and `vibe-default` map onto the provider model name; everything else
/// passes through untouched (unknown Mistral ids stay valid).
fn map_request_model(models: &ModelRegistry, body: &mut Value) {
    let Some(model) = body.get("model").and_then(|m| m.as_str()) else {
        return;
    };
    let resolved = if model == "vibe-default" {
        DEFAULT_MODEL_NAME.to_string()
    } else {
        models.resolve(model)
    };
    if resolved != model {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".to_string(), Value::String(resolved));
        }
    }
}

/// Merge Vibe's model ids into the upstream /models listing, always
/// returning the OpenAI `{"object": "list", "data": [...]}` shape.
async fn list_models(state: &AppState, headers: &HeaderMap) -> Response {
    if let Err(resp) = check_auth(state, headers) {
        return resp;
    }
    let Some(key) = key::resolve_api_key() else {
        return openai_error(
            StatusCode::BAD_GATEWAY,
            "no Mistral API key available; run `vibe --setup` first".to_string(),
            "server_error",
        );
    };

    let upstream: Option<Value> = match state
        .client
        .get(format!("{}/models", state.api_base))
        .bearer_auth(&key)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            match resp.bytes().await {
                Ok(bytes) => serde_json::from_slice(&bytes).ok(),
                Err(e) => {
                    warn!("error reading upstream /models body: {e}");
                    None
                }
            }
        }
        Ok(resp) => {
            warn!("upstream /models returned {}", resp.status());
            None
        }
        Err(e) => {
            warn!("upstream /models request failed: {e}");
            None
        }
    };

    // Mistral has at times returned a bare array; accept both shapes.
    let mut data: Vec<Value> = match upstream {
        Some(Value::Array(items)) => items,
        Some(Value::Object(ref obj)) => match obj.get("data") {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };

    let known: Vec<String> = data
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
        .map(str::to_string)
        .collect();
    for id in &state.models.ids {
        if !known.iter().any(|k| k == id) {
            data.push(serde_json::json!({
                "id": id,
                "object": "model",
                "owned_by": "mistral-vibe",
            }));
        }
    }

    Json(serde_json::json!({ "object": "list", "data": data })).into_response()
}

/// The transparent forwarder for every other route.
async fn forward(state: &AppState, method: Method, path: String, query: Option<String>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if let Err(resp) = check_auth(state, &headers) {
        return resp;
    }
    let Some(key) = key::resolve_api_key() else {
        return openai_error(
            StatusCode::BAD_GATEWAY,
            "no Mistral API key available; run `vibe --setup` first".to_string(),
            "server_error",
        );
    };

    let route = upstream_path(&path);
    let mut url = format!("{}/{}", state.api_base, route);
    if let Some(q) = query.as_deref().filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }

    // Model mapping applies only to the completion endpoints.
    let body: axum::body::Bytes = if route.ends_with("/completions") {
        match serde_json::from_slice::<Value>(&body) {
            Ok(mut v) if v.is_object() => {
                map_request_model(&state.models, &mut v);
                serde_json::to_vec(&v)
                    .map(axum::body::Bytes::from)
                    .unwrap_or(body)
            }
            _ => body,
        }
    } else {
        body
    };

    let (model, stream_requested) = serde_json::from_slice::<Value>(&body)
        .ok()
        .map(|v| {
            (
                v.get("model").and_then(|m| m.as_str()).map(str::to_string),
                v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
            )
        })
        .unwrap_or((None, false));
    info!(
        "proxy request: {} /{}{} (model: {}, stream: {})",
        method,
        route,
        if stream_requested { " [stream]" } else { "" },
        model.as_deref().unwrap_or("-"),
        stream_requested
    );

    let mut req = state.client.request(method, &url).bearer_auth(&key);
    
    // Add x-affinity header for Mistral's prompt cache
    // Try to get from x-affinity header first, then x-opencode-session-id, then generate a new one
    let affinity_value = headers
        .get(AFFINITY_HEADER)
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get(SESSION_ID_HEADER)
                .and_then(|v| v.to_str().ok())
        })
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    
    req = req.header(AFFINITY_HEADER, affinity_value);
    
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        // Skip headers we've already set
        if lower == AFFINITY_HEADER || lower == SESSION_ID_HEADER {
            continue;
        }
        if let Ok(hv) = HeaderValue::from_bytes(value.as_bytes()) {
            req = req.header(name, hv);
        }
    }

    let upstream_resp = match req.body(reqwest::Body::from(body)).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("upstream request to {url} failed: {e}");
            return openai_error(
                StatusCode::BAD_GATEWAY,
                format!("Mistral request failed: {e}"),
                "server_error",
            );
        }
    };

    let status = StatusCode::from_u16(upstream_resp.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    
    // Extract headers first, before any potential body consumption
    let upstream_headers = upstream_resp.headers().clone();
    
    // For non-streaming BAD_REQUEST responses, check for overflow errors
    if status == StatusCode::BAD_REQUEST && !stream_requested {
        // Need to check for overflow errors - consume the body
        let resp_for_body = upstream_resp;
        if let Ok(body_bytes) = resp_for_body.bytes().await {
            if let Some(rewritten) = rewrite_overflow_response(status, &body_bytes) {
                return rewritten;
            }
            // If not rewritten, use the original body
            let mut response = Response::builder().status(status);
            {
                let headers_mut = response.headers_mut().unwrap();
                for (name, value) in upstream_headers.iter() {
                    let lower = name.as_str().to_ascii_lowercase();
                    if RESP_DROP_HEADERS.contains(&lower.as_str()) {
                        continue;
                    }
                    if let (Ok(n), Ok(v)) = (
                        HeaderName::from_bytes(name.as_str().as_bytes()),
                        HeaderValue::from_bytes(value.as_bytes()),
                    ) {
                        headers_mut.insert(n, v);
                    }
                }
            }
            return response.body(Body::from(body_bytes)).unwrap();
        }
        // If we couldn't read the body or it's streaming, fall through
        // But we already consumed the response, so we need to return an error
        return openai_error(
            StatusCode::BAD_GATEWAY,
            "Failed to read upstream response body".to_string(),
            "server_error",
        );
    }

    // For streaming or non-error responses, forward as-is
    let mut response = Response::builder().status(status);
    {
        let headers_mut = response.headers_mut().unwrap();
        for (name, value) in upstream_headers.iter() {
            let lower = name.as_str().to_ascii_lowercase();
            if RESP_DROP_HEADERS.contains(&lower.as_str()) {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_str().as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                headers_mut.insert(n, v);
            }
        }
    }

    // Relay the upstream body as it arrives, so SSE streams stay live.
    let stream = upstream_resp
        .bytes_stream()
        .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));
    response.body(Body::from_stream(stream)).unwrap()
}

/// Axum extractor glue: any method, any path, forwarded verbatim.
pub async fn proxy_any(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(str::to_string);
    let headers = parts.headers.clone();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                format!("failed to read request body: {e}"),
                "invalid_request_error",
            )
        }
    };
    forward(&state, parts.method, path, query, headers, bytes).await
}

/// /v1/models with Vibe aliases merged in.
pub async fn models_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    list_models(&state, &headers).await
}

pub fn build_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client")
}
