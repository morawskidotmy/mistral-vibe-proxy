//! Mistral context overflow error rewriting.
//!
//! Mistral reports an oversized prompt as:
//!   400 {"type":"invalid_request_prompt_too_long","code":"3059",
//!        "message":"Prompt 264620 > 262144 maximum context length"}
//!
//! OpenCode does not recognize that wording as a context overflow, so it shows
//! the error instead of compacting the conversation and retrying. Rewriting
//! the body into the common form lets OpenCode's built-in overflow recovery
//! take over.

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};

/// Mistral's overflow error pattern
const MISTRAL_OVERFLOW_PATTERN: &str = "prompt";
const MISTRAL_OVERFLOW_TYPE: &str = "invalid_request_prompt_too_long";

/// Rewrites Mistral's context overflow error to OpenCode-compatible format
pub fn rewrite_overflow_response(status: StatusCode, body: &[u8]) -> Option<Response> {
    // Only rewrite 400 Bad Request responses
    if status != StatusCode::BAD_REQUEST {
        return None;
    }

    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return None,
    };

    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return None,
    };

    let message = obj
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let error_type = obj
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Check if this is a Mistral overflow error
    let is_overflow = error_type == MISTRAL_OVERFLOW_TYPE
        || message.contains("maximum context length")
        || (message.contains(MISTRAL_OVERFLOW_PATTERN)
            && message.contains(">")
            && message.contains("context length"));

    if !is_overflow {
        return None;
    }

    // Extract token counts if present in message
    let rewritten_message = if let Some(captures) = extract_token_counts(message) {
        format!(
            "Prompt of {} tokens exceeds the context window (maximum context length is {} tokens)",
            captures.0, captures.1
        )
    } else if !message.is_empty() {
        format!("{} exceeds the context window", message)
    } else {
        "Prompt exceeds the context window".to_string()
    };

    let mut rewritten = json!({
        "message": rewritten_message,
        "code": "context_length_exceeded",
        "mistral_code": obj.get("code"),
        "mistral_message": message,
    });

    // Copy over any other fields from the original error
    for (key, value) in obj {
        if key != "message" && key != "type" && key != "code" {
            rewritten[key] = value.clone();
        }
    }

    // Build response with rewritten body
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&rewritten).unwrap()))
        .ok()
}

/// Extract token counts from Mistral's overflow message
/// Expected format: "Prompt 264620 > 262144 maximum context length"
fn extract_token_counts(message: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = message.split('>').collect();
    if parts.len() != 2 {
        return None;
    }

    let left = parts[0].trim();
    let right = parts[1].trim();

    // Extract numbers from left and right parts
    let left_num = left.split_whitespace().last().and_then(|s| {
        s.parse::<u64>().ok().map(|n| n.to_string())
    });
    let right_num = right.split_whitespace().next().and_then(|s| {
        s.parse::<u64>().ok().map(|n| n.to_string())
    });

    match (left_num, right_num) {
        (Some(l), Some(r)) => Some((l, r)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_token_counts() {
        let msg = "Prompt 264620 > 262144 maximum context length";
        let counts = extract_token_counts(msg);
        assert_eq!(counts, Some(("264620".to_string(), "262144".to_string())));
    }

    #[test]
    fn test_rewrite_overflow() {
        let body = json!({
            "type": "invalid_request_prompt_too_long",
            "code": "3059",
            "message": "Prompt 264620 > 262144 maximum context length"
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let response = rewrite_overflow_response(StatusCode::BAD_REQUEST, &bytes);
        assert!(response.is_some());
        
        let response = response.unwrap();
        let status = response.status();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_no_rewrite_non_400() {
        let body = json!({
            "type": "invalid_request_prompt_too_long",
            "code": "3059",
            "message": "Prompt 264620 > 262144 maximum context length"
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let response = rewrite_overflow_response(StatusCode::INTERNAL_SERVER_ERROR, &bytes);
        assert!(response.is_none());
    }
}
