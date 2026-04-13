use anyhow::Result;
use serde_json::{json, Value};

use crate::config::ModelMap;

/// Convert an Anthropic Messages API request body into an OpenAI Chat Completions body.
pub fn to_openai_request(mut body: Value, model_map: &ModelMap) -> Result<Value> {
    let obj = body.as_object_mut().ok_or_else(|| anyhow::anyhow!("Request body is not an object"))?;

    // Map model name
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("claude-3-5-sonnet-20241022")
        .to_string();
    let mapped_model = model_map.resolve(&model);
    obj.insert("model".to_string(), json!(mapped_model));

    // Extract and remove Anthropic-specific fields
    let system_prompt: Option<String> = match obj.remove("system") {
        Some(Value::String(s)) => Some(s),
        Some(Value::Array(arr)) => {
            // system can be an array of content blocks
            let text = arr
                .iter()
                .filter_map(|block| {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        block.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() { None } else { Some(text) }
        }
        _ => None,
    };

    // Convert messages
    let messages = obj
        .remove("messages")
        .ok_or_else(|| anyhow::anyhow!("Missing 'messages' field"))?;
    let messages = messages.as_array().ok_or_else(|| anyhow::anyhow!("'messages' is not an array"))?;

    let mut openai_messages: Vec<Value> = Vec::new();

    // Prepend system message if present
    if let Some(sys) = system_prompt {
        openai_messages.push(json!({ "role": "system", "content": sys }));
    }

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content");

        let openai_content = match content {
            Some(Value::String(s)) => json!(s),
            Some(Value::Array(parts)) => convert_content_parts(parts)?,
            Some(other) => other.clone(),
            None => json!(""),
        };

        // Handle tool_use / tool_result roles
        let openai_role = match role {
            "assistant" => "assistant",
            "user" => "user",
            _ => role,
        };

        // Check if this is a tool result message
        if role == "user" {
            if let Some(Value::Array(parts)) = content {
                let tool_results: Vec<&Value> = parts
                    .iter()
                    .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    .collect();

                if !tool_results.is_empty() {
                    for tr in tool_results {
                        let tool_call_id = tr.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                        let result_content = match tr.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(Value::Array(arr)) => arr
                                .iter()
                                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            _ => String::new(),
                        };
                        openai_messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_call_id,
                            "content": result_content,
                        }));
                    }

                    // Also add non-tool-result parts as a regular user message
                    let text_parts: Vec<&Value> = parts
                        .iter()
                        .filter(|p| p.get("type").and_then(|t| t.as_str()) != Some("tool_result"))
                        .collect();
                    if !text_parts.is_empty() {
                        let combined: String = text_parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !combined.is_empty() {
                            openai_messages.push(json!({ "role": "user", "content": combined }));
                        }
                    }
                    continue;
                }
            }
        }

        // Check if assistant message contains tool_use blocks
        if role == "assistant" {
            if let Some(Value::Array(parts)) = content {
                let tool_uses: Vec<&Value> = parts
                    .iter()
                    .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                    .collect();

                if !tool_uses.is_empty() {
                    let text_content: String = parts
                        .iter()
                        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n");

                    let tool_calls: Vec<Value> = tool_uses
                        .iter()
                        .map(|tu| {
                            let id = tu.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            let name = tu.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let input = tu.get("input").cloned().unwrap_or(json!({}));
                            json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(&input).unwrap_or_default(),
                                }
                            })
                        })
                        .collect();

                    let mut assistant_msg = json!({ "role": "assistant", "tool_calls": tool_calls });
                    if !text_content.is_empty() {
                        assistant_msg["content"] = json!(text_content);
                    }
                    openai_messages.push(assistant_msg);
                    continue;
                }
            }
        }

        openai_messages.push(json!({ "role": openai_role, "content": openai_content }));
    }

    obj.insert("messages".to_string(), json!(openai_messages));

    // Rename max_tokens if present; o-series models need max_completion_tokens
    if let Some(max_tokens) = obj.remove("max_tokens") {
        let is_o_series = mapped_model.starts_with("o1") || mapped_model.starts_with("o3") || mapped_model.contains("-o1") || mapped_model.contains("-o3");
        if is_o_series {
            obj.insert("max_completion_tokens".to_string(), max_tokens);
        } else {
            obj.insert("max_tokens".to_string(), max_tokens);
        }
    }

    // Convert tools (Anthropic input_schema → OpenAI parameters)
    if let Some(tools) = obj.remove("tools") {
        if let Some(tools_arr) = tools.as_array() {
            let openai_tools: Vec<Value> = tools_arr
                .iter()
                .map(|tool| {
                    let name = tool.get("name").cloned().unwrap_or(json!(""));
                    let description = tool.get("description").cloned().unwrap_or(json!(""));
                    let parameters = tool.get("input_schema").cloned().unwrap_or(json!({"type": "object", "properties": {}}));
                    json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": description,
                            "parameters": parameters,
                        }
                    })
                })
                .collect();
            obj.insert("tools".to_string(), json!(openai_tools));
        }
    }

    // Convert tool_choice
    if let Some(tc) = obj.remove("tool_choice") {
        let openai_tc = match tc.get("type").and_then(|t| t.as_str()) {
            Some("auto") => json!("auto"),
            Some("any") => json!("required"),
            Some("tool") => {
                let name = tc.get("name").cloned().unwrap_or(json!(""));
                json!({ "type": "function", "function": { "name": name } })
            }
            _ => json!("auto"),
        };
        obj.insert("tool_choice".to_string(), openai_tc);
    }

    // Remove Anthropic-only fields
    for field in &["thinking", "betas", "anthropic-beta", "top_k", "metadata"] {
        obj.remove(*field);
    }

    Ok(body)
}

fn convert_content_parts(parts: &[Value]) -> Result<Value> {
    // If all parts are text, collapse to a single string
    let all_text = parts
        .iter()
        .all(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"));

    if all_text {
        let combined = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(json!(combined));
    }

    // Mixed content: build OpenAI content array
    let openai_parts: Vec<Value> = parts
        .iter()
        .filter_map(|p| {
            match p.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    let text = p.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    Some(json!({ "type": "text", "text": text }))
                }
                Some("image") => {
                    // Anthropic: { type: "image", source: { type: "base64", media_type, data } }
                    // OpenAI:   { type: "image_url", image_url: { url: "data:media_type;base64,..." } }
                    if let Some(source) = p.get("source") {
                        if source.get("type").and_then(|t| t.as_str()) == Some("base64") {
                            let media_type = source.get("media_type").and_then(|t| t.as_str()).unwrap_or("image/jpeg");
                            let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
                            return Some(json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{};base64,{}", media_type, data) }
                            }));
                        } else if source.get("type").and_then(|t| t.as_str()) == Some("url") {
                            let url = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
                            return Some(json!({ "type": "image_url", "image_url": { "url": url } }));
                        }
                    }
                    None
                }
                _ => None,
            }
        })
        .collect();

    Ok(json!(openai_parts))
}

/// Convert a non-streaming OpenAI Chat Completions response to Anthropic Messages format.
pub fn openai_to_anthropic_response(body: Value, original_model: &str) -> Result<Value> {
    let choices = body.get("choices").and_then(|c| c.as_array()).ok_or_else(|| anyhow::anyhow!("No 'choices' in response"))?;

    let choice = choices.first().ok_or_else(|| anyhow::anyhow!("Empty choices array"))?;
    let message = choice.get("message").ok_or_else(|| anyhow::anyhow!("No 'message' in choice"))?;

    let mut content: Vec<Value> = Vec::new();

    // Tool calls
    if let Some(tool_calls) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
        for tc in tool_calls {
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let func = tc.get("function");
            let name = func.and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
            let args_str = func.and_then(|f| f.get("arguments")).and_then(|a| a.as_str()).unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }

    // Text content
    if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            content.push(json!({ "type": "text", "text": text }));
        }
    }

    let stop_reason = match choice.get("finish_reason").and_then(|r| r.as_str()) {
        Some("stop") => "end_turn",
        Some("tool_calls") => "tool_use",
        Some("length") => "max_tokens",
        _ => "end_turn",
    };

    let usage = body.get("usage");
    let input_tokens = usage.and_then(|u| u.get("prompt_tokens")).and_then(|t| t.as_u64()).unwrap_or(0);
    let output_tokens = usage.and_then(|u| u.get("completion_tokens")).and_then(|t| t.as_u64()).unwrap_or(0);

    let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("msg_unknown");

    Ok(json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": original_model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }
    }))
}
