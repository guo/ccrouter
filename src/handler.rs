use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

fn mask_secret(value: &str) -> String {
    if value.len() <= 8 {
        return "***".to_string();
    }
    format!("{}***{}", &value[..4], &value[value.len() - 4..])
}

fn log_request_headers(prefix: &str, headers: &HeaderMap) {
    let mut lines = Vec::new();
    for (name, value) in headers.iter() {
        let raw = value.to_str().unwrap_or("<binary>");
        let masked = match name.as_str() {
            "authorization" => raw
                .strip_prefix("Bearer ")
                .map(|t| format!("Bearer {}", mask_secret(t)))
                .unwrap_or_else(|| mask_secret(raw)),
            "x-api-key" | "x-goog-api-key" => mask_secret(raw),
            _ => raw.to_string(),
        };
        lines.push(format!("{}: {}", name.as_str(), masked));
    }
    debug!("{} headers:\n{}", prefix, lines.join("\n"));
}


use crate::{
    config::{ApiFormat, Config, Profile},
    stream::openai_to_anthropic_sse,
    transform::{openai_to_anthropic_response, to_openai_request},
};

pub type SharedState = Arc<RwLock<Config>>;

/// Forward a Anthropic-format POST /v1/messages request to the active profile.
pub async fn handle_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, StatusCode> {
    let config = state.read().await.clone();

    let profile = match config.active_profile() {
        Some(p) => p.clone(),
        None => {
            warn!("No active profile configured (active = '{}')", config.active.profile);
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
    };

    info!(
        "→ [{}] {} ({})",
        profile.id,
        profile.base_url,
        match profile.format {
            ApiFormat::Anthropic => "pass-through",
            ApiFormat::OpenAI => "transform",
        }
    );
    log_request_headers("incoming", &headers);

    let body_value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!("Invalid JSON body: {}", e);
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    // Always use the profile's own key — the caller's ANTHROPIC_AUTH_TOKEN is a
    // placeholder that Claude Code requires, but ccrouter ignores it and swaps
    // in the real provider credential from its config.
    let api_key = profile.api_key();

    match profile.format {
        ApiFormat::Anthropic => forward_anthropic(profile, api_key, headers, body_value).await,
        ApiFormat::OpenAI => forward_openai(profile, api_key, headers, body_value).await,
    }
}

/// Pass-through to an Anthropic-compatible endpoint: just swap auth + base URL.
async fn forward_anthropic(
    profile: Profile,
    api_key: Option<String>,
    original_headers: HeaderMap,
    body: Value,
) -> Result<Response<Body>, StatusCode> {
    let is_streaming = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let url = format!("{}/v1/messages", profile.base_url.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let mut req = client.post(&url).json(&body);

    if let Some(key) = api_key {
        req = req.header("x-api-key", &key).header("authorization", format!("Bearer {}", key));
        debug!("outgoing auth: authorization + x-api-key set");
    }

    // Forward safe headers from original request
    req = forward_headers(req, &original_headers);
    debug!("anthropic upstream url: {}", url);

    let response = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Upstream request failed: {}", e);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = response.headers().clone();

    if is_streaming {
        let stream = response.bytes_stream();
        let body = Body::from_stream(stream.map(|r| r.map_err(std::io::Error::other)));
        let mut resp = Response::new(body);
        *resp.status_mut() = status;
        copy_response_headers(&resp_headers, resp.headers_mut());
        Ok(resp)
    } else {
        let bytes = response.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
        let mut resp = Response::new(Body::from(bytes));
        *resp.status_mut() = status;
        copy_response_headers(&resp_headers, resp.headers_mut());
        Ok(resp)
    }
}

/// Transform Anthropic request → OpenAI, forward, then transform response back.
async fn forward_openai(
    profile: Profile,
    api_key: Option<String>,
    original_headers: HeaderMap,
    body: Value,
) -> Result<Response<Body>, StatusCode> {
    let is_streaming = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let original_model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("claude-3-5-sonnet-20241022")
        .to_string();

    let openai_body = match to_openai_request(body, &profile.model_map) {
        Ok(b) => b,
        Err(e) => {
            warn!("Transform error: {}", e);
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    debug!("OpenAI body: {}", serde_json::to_string_pretty(&openai_body).unwrap_or_default());

    let url = format!("{}/chat/completions", profile.base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let mut req = client.post(&url).json(&openai_body);

    if let Some(key) = api_key {
        req = req.header("authorization", format!("Bearer {}", key));
        debug!("outgoing auth: authorization set");
    }
    // Forward accept but NOT anthropic-specific headers (would confuse OpenAI endpoints).
    if let Some(val) = original_headers.get("accept") {
        req = req.header("accept", val);
    }
    debug!("openai upstream url: {}", url);

    let response = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Upstream request failed: {}", e);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let status = response.status();
    if !status.is_success() {
        let code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let body_bytes = response.bytes().await.unwrap_or_default();
        warn!("Upstream error {}: {}", code, String::from_utf8_lossy(&body_bytes));
        return Ok(Response::builder()
            .status(code)
            .header("content-type", "application/json")
            .body(Body::from(body_bytes))
            .unwrap());
    }

    let msg_id = format!("msg_{}", uuid_short());

    if is_streaming {
        let stream = response.bytes_stream();
        let anthropic_stream = openai_to_anthropic_sse(stream, &original_model, &msg_id);

        let body = Body::from_stream(anthropic_stream);
        Ok(Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .body(body)
            .unwrap())
    } else {
        let resp_bytes = response.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
        let resp_value: Value = serde_json::from_slice(&resp_bytes).map_err(|e| {
            warn!("Cannot parse OpenAI response: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

        let anthropic_resp = match openai_to_anthropic_response(resp_value, &original_model) {
            Ok(r) => r,
            Err(e) => {
                warn!("Response transform error: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        };

        let out = serde_json::to_vec(&anthropic_resp).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Body::from(out))
            .unwrap())
    }
}

fn forward_headers(mut req: reqwest::RequestBuilder, headers: &HeaderMap) -> reqwest::RequestBuilder {
    let passthrough = [
        "anthropic-version",
        "anthropic-beta",
        "content-type",
        "accept",
    ];
    for key in &passthrough {
        if let Some(val) = headers.get(*key) {
            req = req.header(*key, val);
        }
    }

    // Ensure the Claude Code beta marker is present for Anthropic-compatible gateways.
    let beta = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let merged_beta = if beta.is_empty() {
        "claude-code-20250219".to_string()
    } else if beta.contains("claude-code-20250219") {
        beta.to_string()
    } else {
        format!("{},claude-code-20250219", beta)
    };
    req.header("anthropic-beta", merged_beta)
}

fn copy_response_headers(src: &reqwest::header::HeaderMap, dst: &mut HeaderMap) {
    let copy_keys = ["content-type", "cache-control", "connection", "transfer-encoding"];
    for key in &copy_keys {
        if let Some(val) = src.get(*key) {
            if let Ok(name) = HeaderName::from_bytes(key.as_bytes()) {
                if let Ok(value) = HeaderValue::from_bytes(val.as_bytes()) {
                    dst.insert(name, value);
                }
            }
        }
    }
}

fn uuid_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{:x}", t.subsec_nanos())
}
