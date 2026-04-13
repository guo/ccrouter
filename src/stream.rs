use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::pin::Pin;
use tracing::warn;

/// Convert an OpenAI SSE stream into an Anthropic SSE stream.
///
/// OpenAI emits:  data: {"choices":[{"delta":{"content":"hello"},...}]}\n\n
/// Anthropic emits a sequence of typed events that Claude Code expects.
pub fn openai_to_anthropic_sse(
    upstream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    original_model: &str,
    message_id: &str,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::convert::Infallible>> + Send>> {
    let model = original_model.to_string();
    let msg_id = message_id.to_string();

    let stream = async_stream::stream! {
        let mut upstream = Box::pin(upstream);
        let mut buffer = String::new();
        let mut emitted_start = false;
        let mut emitted_content_start = false;
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;

        // Accumulate tool call fragments: index → (id, name, arguments)
        let mut tool_calls: std::collections::HashMap<usize, (String, String, String)> = std::collections::HashMap::new();
        let mut has_tool_calls = false;

        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => { warn!("Upstream error: {}", e); break; }
            };

            let text = match std::str::from_utf8(&chunk) {
                Ok(t) => t,
                Err(_) => { warn!("Non-UTF8 chunk from upstream"); continue; }
            };
            buffer.push_str(text);

            // Process complete SSE lines
            loop {
                if let Some(pos) = buffer.find("\n\n") {
                    let line = buffer[..pos].to_string();
                    buffer = buffer[pos + 2..].to_string();

                    if line.trim().is_empty() {
                        continue;
                    }

                    // Strip "data: " prefix
                    let data = if line.starts_with("data: ") {
                        line[6..].trim().to_string()
                    } else {
                        continue;
                    };

                    if data == "[DONE]" {
                        // Finish up tool calls if any
                        if has_tool_calls {
                            let mut sorted: Vec<_> = tool_calls.into_iter().collect();
                            sorted.sort_by_key(|(i, _)| *i);

                            for (block_idx, (_orig_idx, (tc_id, tc_name, tc_args))) in sorted.iter().enumerate() {
                                let block_start = sse_event("content_block_start", &json!({
                                    "type": "content_block_start",
                                    "index": block_idx,
                                    "content_block": {
                                        "type": "tool_use",
                                        "id": tc_id,
                                        "name": tc_name,
                                        "input": {}
                                    }
                                }));
                                yield Ok(Bytes::from(block_start));

                                // Emit the full arguments as a single delta
                                let delta = sse_event("content_block_delta", &json!({
                                    "type": "content_block_delta",
                                    "index": block_idx,
                                    "delta": {
                                        "type": "input_json_delta",
                                        "partial_json": tc_args
                                    }
                                }));
                                yield Ok(Bytes::from(delta));

                                let block_stop = sse_event("content_block_stop", &json!({
                                    "type": "content_block_stop",
                                    "index": block_idx
                                }));
                                yield Ok(Bytes::from(block_stop));
                            }
                        }

                        // message_delta with usage
                        let msg_delta = sse_event("message_delta", &json!({
                            "type": "message_delta",
                            "delta": {
                                "stop_reason": if has_tool_calls { "tool_use" } else { "end_turn" },
                                "stop_sequence": null
                            },
                            "usage": { "output_tokens": output_tokens }
                        }));
                        yield Ok(Bytes::from(msg_delta));

                        let msg_stop = sse_event("message_stop", &json!({ "type": "message_stop" }));
                        yield Ok(Bytes::from(msg_stop));
                        return;
                    }

                    let parsed: Value = match serde_json::from_str(&data) {
                        Ok(v) => v,
                        Err(e) => { warn!("Cannot parse SSE data: {} — {}", e, data); continue; }
                    };

                    // Capture usage if present (streaming usage)
                    if let Some(usage) = parsed.get("usage") {
                        if let Some(pt) = usage.get("prompt_tokens").and_then(|t| t.as_u64()) {
                            input_tokens = pt;
                        }
                        if let Some(ct) = usage.get("completion_tokens").and_then(|t| t.as_u64()) {
                            output_tokens = ct;
                        }
                    }

                    let choices = match parsed.get("choices").and_then(|c| c.as_array()) {
                        Some(c) if !c.is_empty() => c.clone(),
                        _ => continue,
                    };

                    let choice = &choices[0];
                    let delta = match choice.get("delta") {
                        Some(d) => d,
                        None => continue,
                    };

                    // Emit message_start once
                    if !emitted_start {
                        emitted_start = true;
                        let msg_start = sse_event("message_start", &json!({
                            "type": "message_start",
                            "message": {
                                "id": msg_id,
                                "type": "message",
                                "role": "assistant",
                                "content": [],
                                "model": model,
                                "stop_reason": null,
                                "stop_sequence": null,
                                "usage": { "input_tokens": input_tokens, "output_tokens": 0 }
                            }
                        }));
                        yield Ok(Bytes::from(msg_start));
                    }

                    // Handle tool_calls in delta
                    if let Some(tc_arr) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
                        has_tool_calls = true;
                        for tc in tc_arr {
                            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                            let entry = tool_calls.entry(index).or_insert_with(|| (String::new(), String::new(), String::new()));

                            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                if !id.is_empty() { entry.0 = id.to_string(); }
                            }
                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                    if !name.is_empty() { entry.1 = name.to_string(); }
                                }
                                if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                                    entry.2.push_str(args);
                                }
                            }
                        }
                        output_tokens += 1;
                        continue;
                    }

                    // Handle text content
                    if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                        if !emitted_content_start {
                            emitted_content_start = true;
                            let block_start = sse_event("content_block_start", &json!({
                                "type": "content_block_start",
                                "index": 0,
                                "content_block": { "type": "text", "text": "" }
                            }));
                            yield Ok(Bytes::from(block_start));
                        }

                        let delta_event = sse_event("content_block_delta", &json!({
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": { "type": "text_delta", "text": text }
                        }));
                        yield Ok(Bytes::from(delta_event));
                        output_tokens += 1;
                    }

                    // finish_reason signals end before [DONE] on some providers
                    if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                        if !reason.is_empty() && reason != "null" && emitted_content_start && !has_tool_calls {
                            let block_stop = sse_event("content_block_stop", &json!({
                                "type": "content_block_stop",
                                "index": 0
                            }));
                            yield Ok(Bytes::from(block_stop));
                        }
                    }
                } else {
                    break;
                }
            }
        }

        // If stream ended without [DONE]
        if emitted_content_start {
            let block_stop = sse_event("content_block_stop", &json!({ "type": "content_block_stop", "index": 0 }));
            yield Ok(Bytes::from(block_stop));
        }

        let msg_delta = sse_event("message_delta", &json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": output_tokens }
        }));
        yield Ok(Bytes::from(msg_delta));

        let msg_stop = sse_event("message_stop", &json!({ "type": "message_stop" }));
        yield Ok(Bytes::from(msg_stop));
    };

    Box::pin(stream)
}

fn sse_event(event_type: &str, data: &Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        event_type,
        serde_json::to_string(data).unwrap_or_default()
    )
}
