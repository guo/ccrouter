use anyhow::Result;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use tracing::warn;

use crate::config::ModelMap;

// ── Session cache ─────────────────────────────────────────────────────────────
// Keyed by profile_id. Tracks the last response_id and a hash of the full
// message history so we can detect conversation continuations.

struct Session {
    response_id: String,
    // FNV-1a hash of the full serialized messages array from the previous turn.
    messages_hash: u64,
}

static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

// ── Request transform ─────────────────────────────────────────────────────────

/// Convert an Anthropic Messages request into an OpenAI Responses API request.
/// Uses `previous_response_id` when this is a continuation of a known session
/// (messages[:-1] hash matches the stored session hash).
pub fn to_responses_request(body: Value, profile_id: &str, model_map: &ModelMap) -> Result<Value> {
    let obj = body.as_object().ok_or_else(|| anyhow::anyhow!("Request body is not an object"))?;

    let model = obj.get("model").and_then(|v| v.as_str()).unwrap_or("claude-3-5-sonnet-20241022");
    let mapped_model = model_map.resolve(model);

    let system_prompt = extract_system(obj.get("system"));
    let messages = obj.get("messages")
        .and_then(|m| m.as_array())
        .ok_or_else(|| anyhow::anyhow!("Missing 'messages' field"))?;

    let stream = obj.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let max_tokens = obj.get("max_tokens");
    let temperature = obj.get("temperature");
    let top_p = obj.get("top_p");
    let tools = obj.get("tools").and_then(|t| t.as_array());

    // Session continuity detection
    let prev_messages = &messages[..messages.len().saturating_sub(1)];
    let prev_hash = fnv1a(&serde_json::to_string(prev_messages).unwrap_or_default());
    let last_msg = messages.last().ok_or_else(|| anyhow::anyhow!("Empty messages array"))?;

    let prev_response_id: Option<String> = {
        let cache = sessions().lock().unwrap();
        cache.get(profile_id).and_then(|s| {
            if s.messages_hash == prev_hash {
                Some(s.response_id.clone())
            } else {
                None
            }
        })
    };

    let mut req = json!({
        "model": mapped_model,
        "stream": stream,
    });
    let r = req.as_object_mut().unwrap();

    if let Some(sys) = system_prompt {
        r.insert("instructions".to_string(), json!(sys));
    }
    if let Some(max) = max_tokens {
        r.insert("max_output_tokens".to_string(), max.clone());
    }
    if let Some(temp) = temperature {
        r.insert("temperature".to_string(), temp.clone());
    }
    if let Some(tp) = top_p {
        r.insert("top_p".to_string(), tp.clone());
    }

    // Convert tools (Anthropic → Responses API function format)
    if let Some(tools_arr) = tools {
        let rt: Vec<Value> = tools_arr.iter().map(|t| {
            let name = t.get("name").cloned().unwrap_or(json!(""));
            let desc = t.get("description").cloned().unwrap_or(json!(""));
            let params = t.get("input_schema").cloned().unwrap_or(json!({"type":"object","properties":{}}));
            json!({"type":"function","name":name,"description":desc,"parameters":params})
        }).collect();
        r.insert("tools".to_string(), json!(rt));
    }

    if let Some(rid) = prev_response_id {
        // Continuation: send only the new input
        r.insert("previous_response_id".to_string(), json!(rid));
        r.insert("input".to_string(), last_message_to_input(last_msg));
    } else {
        // New conversation: send full history
        r.insert("input".to_string(), messages_to_input(messages));
    }

    Ok(req)
}

fn extract_system(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(arr)) => {
            let t: String = arr.iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>().join("\n");
            if t.is_empty() { None } else { Some(t) }
        }
        _ => None,
    }
}

/// Convert all Anthropic messages to Responses API input array (full replay).
fn messages_to_input(messages: &[Value]) -> Value {
    let mut items: Vec<Value> = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content");
        append_message_items(&mut items, role, content);
    }
    json!(items)
}

/// Convert just the last message to a Responses API input (continuation mode).
fn last_message_to_input(msg: &Value) -> Value {
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
    let content = msg.get("content");

    // Simple text message → plain string input
    if role == "user" {
        if let Some(Value::String(s)) = content {
            return json!(s);
        }
        // Tool results → function_call_output items
        if let Some(Value::Array(parts)) = content {
            let tool_results: Vec<&Value> = parts.iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                .collect();
            if !tool_results.is_empty() {
                let items: Vec<Value> = tool_results.iter().map(|tr| {
                    let call_id = tr.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                    let out = match tr.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Array(arr)) => arr.iter()
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>().join("\n"),
                        _ => String::new(),
                    };
                    json!({"type":"function_call_output","call_id":call_id,"output":out})
                }).collect();
                return json!(items);
            }
            // Mixed/text-only array
            let text: String = parts.iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>().join("\n");
            if !text.is_empty() { return json!(text); }
        }
    }

    // Fallback: wrap as input_text
    let text = content.and_then(|c| c.as_str()).unwrap_or("").to_string();
    json!(text)
}

/// Append Responses API input items for a single Anthropic message.
fn append_message_items(items: &mut Vec<Value>, role: &str, content: Option<&Value>) {
    match (role, content) {
        ("user", Some(Value::String(s))) => {
            items.push(json!({"type":"message","role":"user","content":[{"type":"input_text","text":s}]}));
        }
        ("user", Some(Value::Array(parts))) => {
            let tool_results: Vec<&Value> = parts.iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                .collect();
            if !tool_results.is_empty() {
                for tr in tool_results {
                    let call_id = tr.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                    let out = match tr.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Array(arr)) => arr.iter()
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>().join("\n"),
                        _ => String::new(),
                    };
                    items.push(json!({"type":"function_call_output","call_id":call_id,"output":out}));
                }
            } else {
                let text: String = parts.iter()
                    .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>().join("\n");
                items.push(json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}));
            }
        }
        ("assistant", Some(Value::Array(parts))) => {
            // Check for tool_use blocks
            let tool_uses: Vec<&Value> = parts.iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .collect();
            if !tool_uses.is_empty() {
                for tu in tool_uses {
                    let call_id = tu.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = tu.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args = tu.get("input").cloned().unwrap_or(json!({}));
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": serde_json::to_string(&args).unwrap_or_default(),
                    }));
                }
            } else {
                let text: String = parts.iter()
                    .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>().join("\n");
                items.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}));
            }
        }
        ("assistant", Some(Value::String(s))) => {
            items.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":s}]}));
        }
        _ => {}
    }
}

// ── Non-streaming response transform ─────────────────────────────────────────

/// Convert a Responses API response to Anthropic Messages format.
/// Also updates the session cache so the next turn can use previous_response_id.
pub fn responses_to_anthropic_response(resp: Value, original_model: &str, profile_id: &str, messages_hash: u64) -> Result<Value> {
    let resp_id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("resp_unknown");
    let output = resp.get("output").and_then(|o| o.as_array());

    let mut content: Vec<Value> = Vec::new();
    let mut stop_reason = "end_turn";

    if let Some(items) = output {
        for item in items {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("message") => {
                    if let Some(blocks) = item.get("content").and_then(|c| c.as_array()) {
                        for block in blocks {
                            match block.get("type").and_then(|t| t.as_str()) {
                                Some("output_text") => {
                                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                        if !text.is_empty() {
                                            content.push(json!({"type":"text","text":text}));
                                        }
                                    }
                                }
                                Some("refusal") => {
                                    if let Some(r) = block.get("refusal").and_then(|r| r.as_str()) {
                                        content.push(json!({"type":"text","text":r}));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Some("function_call") => {
                    stop_reason = "tool_use";
                    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args_str = item.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
                    let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
                    content.push(json!({"type":"tool_use","id":call_id,"name":name,"input":input}));
                }
                _ => {}
            }
        }
    }

    let usage = resp.get("usage");
    let input_tokens = usage.and_then(|u| u.get("input_tokens")).and_then(|t| t.as_u64()).unwrap_or(0);
    let output_tokens = usage.and_then(|u| u.get("output_tokens")).and_then(|t| t.as_u64()).unwrap_or(0);

    // Store session for next turn
    {
        let mut cache = sessions().lock().unwrap();
        cache.insert(profile_id.to_string(), Session {
            response_id: resp_id.to_string(),
            messages_hash,
        });
    }

    Ok(json!({
        "id": resp_id,
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": original_model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    }))
}

// ── Streaming SSE transform ───────────────────────────────────────────────────

pub fn responses_to_anthropic_sse(
    upstream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    original_model: &str,
    message_id: &str,
    profile_id: &str,
    messages_hash: u64,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::convert::Infallible>> + Send>> {
    let model = original_model.to_string();
    let msg_id = message_id.to_string();
    let profile = profile_id.to_string();

    let stream = async_stream::stream! {
        let mut upstream = Box::pin(upstream);
        let mut buffer = String::new();
        let mut emitted_start = false;
        // index → (content_index, type: "text"|"tool_use", accumulated_text, call_id, name)
        let mut output_items: HashMap<usize, (usize, String, String, String, String)> = HashMap::new();
        let mut content_counter: usize = 0;
        let mut output_tokens: u64 = 0;
        let mut resp_id = String::new();

        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => { warn!("Upstream error: {}", e); break; }
            };
            let text = match std::str::from_utf8(&chunk) {
                Ok(t) => t,
                Err(_) => { warn!("Non-UTF8 chunk"); continue; }
            };
            buffer.push_str(text);

            loop {
                let Some(pos) = buffer.find("\n\n") else { break };
                let line = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();
                if line.trim().is_empty() { continue; }

                // Strip "event: TYPE\ndata: JSON" or "data: JSON"
                let data = parse_sse_data(&line);
                let Some(data) = data else { continue };

                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(e) => { warn!("SSE parse error: {} — {}", e, data); continue; }
                };

                let event_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");

                match event_type {
                    "response.created" | "response.in_progress" => {
                        // Capture response id for session
                        if let Some(r) = parsed.get("response").and_then(|r| r.get("id")).and_then(|id| id.as_str()) {
                            resp_id = r.to_string();
                        }

                        if !emitted_start {
                            emitted_start = true;
                            yield Ok(Bytes::from(sse_event("message_start", &json!({
                                "type": "message_start",
                                "message": {
                                    "id": msg_id,
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [],
                                    "model": model,
                                    "stop_reason": null,
                                    "stop_sequence": null,
                                    "usage": {"input_tokens": 0, "output_tokens": 0}
                                }
                            }))));
                        }
                    }

                    "response.output_item.added" => {
                        let output_index = parsed.get("output_index")
                            .and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        let item = parsed.get("item");
                        let item_type = item.and_then(|i| i.get("type")).and_then(|t| t.as_str()).unwrap_or("message");

                        let content_index = content_counter;
                        content_counter += 1;

                        if item_type == "function_call" {
                            let call_id = item.and_then(|i| i.get("call_id")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let name = item.and_then(|i| i.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                            output_items.insert(output_index, (content_index, "tool_use".into(), String::new(), call_id.clone(), name.clone()));
                            yield Ok(Bytes::from(sse_event("content_block_start", &json!({
                                "type": "content_block_start",
                                "index": content_index,
                                "content_block": {"type":"tool_use","id":call_id,"name":name,"input":{}}
                            }))));
                        } else {
                            // message item — text
                            output_items.insert(output_index, (content_index, "text".into(), String::new(), String::new(), String::new()));
                            yield Ok(Bytes::from(sse_event("content_block_start", &json!({
                                "type": "content_block_start",
                                "index": content_index,
                                "content_block": {"type":"text","text":""}
                            }))));
                        }
                    }

                    "response.output_text.delta" => {
                        let output_index = parsed.get("output_index")
                            .and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        let delta = parsed.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                        if let Some(entry) = output_items.get_mut(&output_index) {
                            entry.2.push_str(delta);
                            yield Ok(Bytes::from(sse_event("content_block_delta", &json!({
                                "type": "content_block_delta",
                                "index": entry.0,
                                "delta": {"type":"text_delta","text":delta}
                            }))));
                        }
                    }

                    "response.function_call_arguments.delta" => {
                        let output_index = parsed.get("output_index")
                            .and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        let delta = parsed.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                        if let Some(entry) = output_items.get_mut(&output_index) {
                            entry.2.push_str(delta);
                            yield Ok(Bytes::from(sse_event("content_block_delta", &json!({
                                "type": "content_block_delta",
                                "index": entry.0,
                                "delta": {"type":"input_json_delta","partial_json":delta}
                            }))));
                        }
                    }

                    "response.output_item.done" => {
                        let output_index = parsed.get("output_index")
                            .and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        if let Some(entry) = output_items.get(&output_index) {
                            yield Ok(Bytes::from(sse_event("content_block_stop", &json!({
                                "type": "content_block_stop",
                                "index": entry.0
                            }))));
                        }
                    }

                    "response.completed" => {
                        // Capture response id and usage
                        let response = parsed.get("response");
                        if let Some(id) = response.and_then(|r| r.get("id")).and_then(|id| id.as_str()) {
                            resp_id = id.to_string();
                        }
                        if let Some(usage) = response.and_then(|r| r.get("usage")) {
                            if let Some(id) = response.and_then(|r| r.get("id")).and_then(|id| id.as_str()) {
                                resp_id = id.to_string();
                            }
                            output_tokens = usage.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(output_tokens);
                        }

                        let has_tool_use = output_items.values().any(|e| e.1 == "tool_use");
                        let stop_reason = if has_tool_use { "tool_use" } else { "end_turn" };

                        yield Ok(Bytes::from(sse_event("message_delta", &json!({
                            "type": "message_delta",
                            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                            "usage": {"output_tokens": output_tokens}
                        }))));
                        yield Ok(Bytes::from(sse_event("message_stop", &json!({"type":"message_stop"}))));

                        // Save session
                        if !resp_id.is_empty() {
                            let mut cache = sessions().lock().unwrap();
                            cache.insert(profile.clone(), Session {
                                response_id: resp_id.clone(),
                                messages_hash,
                            });
                        }
                        return;
                    }

                    "error" => {
                        warn!("Responses API error event: {}", data);
                        break;
                    }

                    _ => {}
                }
            }
        }

        // Stream ended without response.completed
        yield Ok(Bytes::from(sse_event("message_delta", &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": output_tokens}
        }))));
        yield Ok(Bytes::from(sse_event("message_stop", &json!({"type":"message_stop"}))));
    };

    Box::pin(stream)
}

fn parse_sse_data(line: &str) -> Option<&str> {
    // Handle "event: TYPE\ndata: JSON" or bare "data: JSON"
    for part in line.split('\n') {
        if let Some(d) = part.strip_prefix("data: ") {
            let d = d.trim();
            if !d.is_empty() {
                return Some(d);
            }
        }
    }
    None
}

fn sse_event(event_type: &str, data: &Value) -> String {
    format!("event: {}\ndata: {}\n\n", event_type, serde_json::to_string(data).unwrap_or_default())
}

/// Compute the hash of the full messages array (for session tracking).
pub fn messages_hash(body: &Value) -> u64 {
    let msgs = body.get("messages").unwrap_or(&Value::Null);
    fnv1a(&serde_json::to_string(msgs).unwrap_or_default())
}
