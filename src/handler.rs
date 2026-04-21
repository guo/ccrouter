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
    config::{AnthropicAuthMode, ApiFormat, Config, Profile},
    responses::{messages_hash, responses_to_anthropic_response, responses_to_anthropic_sse, to_responses_request},
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
    handle_anthropic_request(state, headers, body, false).await
}

pub async fn handle_count_tokens(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, StatusCode> {
    handle_anthropic_request(state, headers, body, true).await
}

async fn handle_anthropic_request(
    state: SharedState,
    headers: HeaderMap,
    body: Bytes,
    is_count_tokens: bool,
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
            ApiFormat::Responses => "responses-api",
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
        ApiFormat::Anthropic => {
            forward_anthropic(profile, api_key, headers, body_value, is_count_tokens).await
        }
        ApiFormat::OpenAI => {
            if is_count_tokens {
                warn!("count_tokens is not supported for OpenAI transform profiles");
                return Err(StatusCode::NOT_IMPLEMENTED);
            }
            forward_openai(profile, api_key, headers, body_value).await
        }
        ApiFormat::Responses => {
            if is_count_tokens {
                warn!("count_tokens is not supported for Responses API profiles");
                return Err(StatusCode::NOT_IMPLEMENTED);
            }
            forward_responses(profile, api_key, headers, body_value).await
        }
    }
}

/// Pass-through to an Anthropic-compatible endpoint: just swap auth + base URL.
async fn forward_anthropic(
    profile: Profile,
    api_key: Option<String>,
    original_headers: HeaderMap,
    body: Value,
    is_count_tokens: bool,
) -> Result<Response<Body>, StatusCode> {
    let is_streaming = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let path = if is_count_tokens {
        &profile.count_tokens_path
    } else {
        &profile.messages_path
    };
    let url = upstream_url(&profile.base_url, path);

    let client = reqwest::Client::new();
    let mut req = client.post(&url).json(&body);

    if let Some(key) = api_key {
        req = apply_anthropic_auth(req, &profile.auth_mode, &key);
        debug!("outgoing auth mode: {:?}", profile.auth_mode);
    }

    req = forward_headers(req, &original_headers, profile.inject_claude_code_beta, is_count_tokens);
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

/// Transform Anthropic request → OpenAI Responses API, forward, transform response back.
async fn forward_responses(
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

    let hash = messages_hash(&body);

    let responses_body = match to_responses_request(body, &profile.id, &profile.model_map) {
        Ok(b) => b,
        Err(e) => {
            warn!("Responses transform error: {}", e);
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    debug!("Responses API body: {}", serde_json::to_string_pretty(&responses_body).unwrap_or_default());

    let url = format!("{}/responses", profile.base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let mut req = client.post(&url).json(&responses_body);

    if let Some(key) = api_key {
        req = req.header("authorization", format!("Bearer {}", key));
    }
    if let Some(val) = original_headers.get("accept") {
        req = req.header("accept", val);
    }
    debug!("responses upstream url: {}", url);

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
        let anthropic_stream = responses_to_anthropic_sse(stream, &original_model, &msg_id, &profile.id, hash);
        Ok(Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .body(Body::from_stream(anthropic_stream))
            .unwrap())
    } else {
        let resp_bytes = response.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
        let resp_value: Value = serde_json::from_slice(&resp_bytes).map_err(|e| {
            warn!("Cannot parse Responses API response: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

        let anthropic_resp = match responses_to_anthropic_response(resp_value, &original_model, &profile.id, hash) {
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

fn forward_headers(
    mut req: reqwest::RequestBuilder,
    headers: &HeaderMap,
    inject_claude_code_beta: bool,
    is_count_tokens: bool,
) -> reqwest::RequestBuilder {
    let passthrough = [
        "accept",
        "content-type",
        "anthropic-version",
        "anthropic-beta",
        "user-agent",
        "x-app",
        "anthropic-dangerous-direct-browser-access",
        "x-claude-code-session-id",
        "x-client-request-id",
        "x-stainless-lang",
        "x-stainless-package-version",
        "x-stainless-os",
        "x-stainless-arch",
        "x-stainless-runtime",
        "x-stainless-runtime-version",
        "x-stainless-retry-count",
        "x-stainless-timeout",
        "accept-encoding",
    ];
    for key in &passthrough {
        if let Some(val) = headers.get(*key) {
            req = req.header(*key, val);
        }
    }

    if headers.get("content-type").is_none() {
        req = req.header("content-type", "application/json");
    }
    if headers.get("anthropic-version").is_none() {
        req = req.header("anthropic-version", "2023-06-01");
    }
    if headers.get("accept").is_none() {
        req = req.header("accept", "application/json");
    }

    let mut beta_tokens = split_beta_tokens(headers.get("anthropic-beta"));
    if inject_claude_code_beta {
        ensure_beta_token(&mut beta_tokens, "claude-code-20250219");
    }
    if is_count_tokens {
        ensure_beta_token(&mut beta_tokens, "token-counting-2024-11-01");
    }
    if !beta_tokens.is_empty() {
        req = req.header("anthropic-beta", beta_tokens.join(","));
    }

    req
}

fn split_beta_tokens(value: Option<&HeaderValue>) -> Vec<String> {
    value
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn ensure_beta_token(tokens: &mut Vec<String>, token: &str) {
    if !tokens.iter().any(|t| t == token) {
        tokens.push(token.to_string());
    }
}

fn apply_anthropic_auth(
    req: reqwest::RequestBuilder,
    mode: &AnthropicAuthMode,
    key: &str,
) -> reqwest::RequestBuilder {
    match mode {
        AnthropicAuthMode::XApiKey => req.header("x-api-key", key),
        AnthropicAuthMode::Bearer => req.header("authorization", format!("Bearer {}", key)),
        AnthropicAuthMode::Both => req
            .header("x-api-key", key)
            .header("authorization", format!("Bearer {}", key)),
        AnthropicAuthMode::None => req,
    }
}

fn upstream_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    };
    if base.ends_with(&path) {
        base.to_string()
    } else {
        format!("{}{}", base, path)
    }
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
