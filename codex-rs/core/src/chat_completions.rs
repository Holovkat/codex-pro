use std::sync::Arc;
use std::time::Duration;

use crate::AuthManager;
use crate::ModelProviderInfo;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::config_types::ProviderKind;
use crate::error::CodexErr;
use crate::error::ConnectionFailedError;
use crate::error::ResponseStreamFailed;
use crate::error::Result;
use crate::error::RetryLimitReachedError;
use crate::error::UnexpectedResponseError;
use crate::model_family::ModelFamily;
use crate::openai_tools::create_tools_json_for_chat_completions_api;
use crate::protocol::TokenUsage;
use crate::util::backoff;
use bytes::Bytes;
use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use reqwest::StatusCode;
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

fn provider_supports_streaming(provider: &ModelProviderInfo) -> bool {
    provider
        .base_url
        .as_deref()
        .map(|url| {
            let lower = url.to_ascii_lowercase();
            !(lower.contains("api.z.ai/api/coding/paas/")
                || lower.contains("open.bigmodel.cn/api/coding/paas/"))
        })
        .unwrap_or(true)
}

#[derive(Default)]
struct ThinkParser {
    buffer: String,
}

#[derive(Default)]
struct ThinkExtraction {
    visible: String,
    reasoning: Vec<String>,
}

impl ThinkParser {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, fragment: &str) -> ThinkExtraction {
        self.buffer.push_str(fragment);
        self.extract(false)
    }

    fn flush(&mut self) -> ThinkExtraction {
        self.extract(true)
    }

    fn strip_all(text: &str) -> ThinkExtraction {
        let mut parser = ThinkParser::new();
        let mut total = parser.push(text);
        let remainder = parser.flush();
        total.visible.push_str(&remainder.visible);
        total.reasoning.extend(remainder.reasoning);
        total
    }

    fn extract(&mut self, finalize: bool) -> ThinkExtraction {
        let data = std::mem::take(&mut self.buffer);
        let mut visible = String::new();
        let mut reasoning = Vec::new();
        let mut idx = 0;

        while let Some(start_rel) = data[idx..].find("<think>") {
            let start = idx + start_rel;
            visible.push_str(&data[idx..start]);
            let think_start = start + "<think>".len();
            if let Some(end_rel) = data[think_start..].find("</think>") {
                let end = think_start + end_rel;
                reasoning.push(data[think_start..end].to_string());
                idx = end + "</think>".len();
            } else {
                if finalize {
                    visible.push_str(&data[start..]);
                    idx = data.len();
                } else {
                    self.buffer = data[start..].to_string();
                    return ThinkExtraction { visible, reasoning };
                }
                break;
            }
        }

        let remainder = &data[idx..];
        if finalize {
            visible.push_str(remainder);
            self.buffer.clear();
        } else {
            let keep = longest_suffix_prefix(remainder, "<think>");
            let safe_len = remainder.len() - keep;
            visible.push_str(&remainder[..safe_len]);
            self.buffer = remainder[safe_len..].to_string();
        }

        ThinkExtraction { visible, reasoning }
    }
}

fn longest_suffix_prefix(haystack: &str, needle: &str) -> usize {
    let max = needle.len().min(haystack.len());
    for len in (1..=max).rev() {
        if haystack.ends_with(&needle[..len]) {
            return len;
        }
    }
    0
}

fn append_reasoning(reasoning_text: &mut String, segment: &str) {
    if segment.is_empty() {
        return;
    }
    if !reasoning_text.is_empty() {
        reasoning_text.push('\n');
    }
    reasoning_text.push_str(segment);
}

fn apply_provider_reasoning_overrides(payload: &mut JsonValue, provider: &ModelProviderInfo) {
    if let Some(obj) = payload.as_object_mut() {
        match provider.provider_kind {
            ProviderKind::Ollama => {
                obj.insert(
                    "think".to_string(),
                    json!(provider.reasoning_controls.think_enabled),
                );
            }
            ProviderKind::AnthropicClaude => {
                let mut thinking = JsonMap::new();
                if let Some(tokens) = provider.reasoning_controls.anthropic_budget_tokens {
                    thinking.insert("budget_tokens".to_string(), json!(tokens));
                }
                if let Some(weight) = provider.reasoning_controls.anthropic_budget_weight {
                    thinking.insert("budget_weight".to_string(), json!(weight));
                }
                if !thinking.is_empty() {
                    obj.insert("thinking".to_string(), JsonValue::Object(thinking));
                }
            }
            ProviderKind::OpenAiResponses => {}
        }
    }
}

/// Implementation for the classic Chat Completions API.
pub(crate) async fn stream_chat_completions(
    prompt: &Prompt,
    model_family: &ModelFamily,
    client: &reqwest::Client,
    provider: &ModelProviderInfo,
    provider_id: &str,
    auth_manager: &Option<Arc<AuthManager>>,
    otel_event_manager: &OtelEventManager,
) -> Result<ResponseStream> {
    if prompt.output_schema.is_some() {
        return Err(CodexErr::UnsupportedOperation(
            "output_schema is not supported for Chat Completions API".to_string(),
        ));
    }

    let supports_streaming = provider_supports_streaming(provider);

    // Build messages array
    let mut messages = Vec::<serde_json::Value>::new();

    let full_instructions = prompt.get_full_instructions(model_family);
    messages.push(json!({"role": "system", "content": full_instructions}));

    let input = prompt.get_formatted_input();
    // Pre-scan: map Reasoning blocks to the adjacent assistant anchor after the last user.
    // - If the last emitted message is a user message, drop all reasoning.
    // - Otherwise, for each Reasoning item after the last user message, attach it
    //   to the immediate previous assistant message (stop turns) or the immediate
    //   next assistant anchor (tool-call turns: function/local shell call, or assistant message).
    let mut reasoning_by_anchor_index: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();

    // Determine the last role that would be emitted to Chat Completions.
    let mut last_emitted_role: Option<&str> = None;
    for item in &input {
        match item {
            ResponseItem::Message { role, .. } => last_emitted_role = Some(role.as_str()),
            ResponseItem::FunctionCall { .. } | ResponseItem::LocalShellCall { .. } => {
                last_emitted_role = Some("assistant")
            }
            ResponseItem::FunctionCallOutput { .. } => last_emitted_role = Some("tool"),
            ResponseItem::Reasoning { .. } | ResponseItem::Other => {}
            ResponseItem::CustomToolCall { .. } => {}
            ResponseItem::CustomToolCallOutput { .. } => {}
            ResponseItem::WebSearchCall { .. } => {}
        }
    }

    // Find the last user message index in the input.
    let mut last_user_index: Option<usize> = None;
    for (idx, item) in input.iter().enumerate() {
        if let ResponseItem::Message { role, .. } = item
            && role == "user"
        {
            last_user_index = Some(idx);
        }
    }

    // Attach reasoning only if the conversation does not end with a user message.
    if !matches!(last_emitted_role, Some("user")) {
        for (idx, item) in input.iter().enumerate() {
            // Only consider reasoning that appears after the last user message.
            if let Some(u_idx) = last_user_index
                && idx <= u_idx
            {
                continue;
            }

            if let ResponseItem::Reasoning {
                content: Some(items),
                ..
            } = item
            {
                let mut text = String::new();
                for c in items {
                    match c {
                        ReasoningItemContent::ReasoningText { text: t }
                        | ReasoningItemContent::Text { text: t } => text.push_str(t),
                    }
                }
                if text.trim().is_empty() {
                    continue;
                }

                // Prefer immediate previous assistant message (stop turns)
                let mut attached = false;
                if idx > 0
                    && let ResponseItem::Message { role, .. } = &input[idx - 1]
                    && role == "assistant"
                {
                    reasoning_by_anchor_index
                        .entry(idx - 1)
                        .and_modify(|v| v.push_str(&text))
                        .or_insert(text.clone());
                    attached = true;
                }

                // Otherwise, attach to immediate next assistant anchor (tool-calls or assistant message)
                if !attached && idx + 1 < input.len() {
                    match &input[idx + 1] {
                        ResponseItem::FunctionCall { .. } | ResponseItem::LocalShellCall { .. } => {
                            reasoning_by_anchor_index
                                .entry(idx + 1)
                                .and_modify(|v| v.push_str(&text))
                                .or_insert(text.clone());
                        }
                        ResponseItem::Message { role, .. } if role == "assistant" => {
                            reasoning_by_anchor_index
                                .entry(idx + 1)
                                .and_modify(|v| v.push_str(&text))
                                .or_insert(text.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Track last assistant text we emitted to avoid duplicate assistant messages
    // in the outbound Chat Completions payload (can happen if a final
    // aggregated assistant message was recorded alongside an earlier partial).
    let mut last_assistant_text: Option<String> = None;

    for (idx, item) in input.iter().enumerate() {
        match item {
            ResponseItem::Message { role, content, .. } => {
                let mut text = String::new();
                for c in content {
                    match c {
                        ContentItem::InputText { text: t }
                        | ContentItem::OutputText { text: t } => {
                            text.push_str(t);
                        }
                        _ => {}
                    }
                }
                // Skip exact-duplicate assistant messages.
                if role == "assistant" {
                    if let Some(prev) = &last_assistant_text
                        && prev == &text
                    {
                        continue;
                    }
                    last_assistant_text = Some(text.clone());
                }

                let mut msg = json!({"role": role, "content": text});
                if role == "assistant"
                    && let Some(reasoning) = reasoning_by_anchor_index.get(&idx)
                    && let Some(obj) = msg.as_object_mut()
                {
                    obj.insert("reasoning".to_string(), json!(reasoning));
                }
                messages.push(msg);
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                let mut msg = json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    }]
                });
                if let Some(reasoning) = reasoning_by_anchor_index.get(&idx)
                    && let Some(obj) = msg.as_object_mut()
                {
                    obj.insert("reasoning".to_string(), json!(reasoning));
                }
                messages.push(msg);
            }
            ResponseItem::LocalShellCall {
                id,
                call_id: _,
                status,
                action,
            } => {
                // Confirm with API team.
                let mut msg = json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id.clone().unwrap_or_else(|| "".to_string()),
                        "type": "local_shell_call",
                        "status": status,
                        "action": action,
                    }]
                });
                if let Some(reasoning) = reasoning_by_anchor_index.get(&idx)
                    && let Some(obj) = msg.as_object_mut()
                {
                    obj.insert("reasoning".to_string(), json!(reasoning));
                }
                messages.push(msg);
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output.content,
                }));
            }
            ResponseItem::CustomToolCall {
                id,
                call_id: _,
                name,
                input,
                status: _,
            } => {
                messages.push(json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "custom",
                        "custom": {
                            "name": name,
                            "input": input,
                        }
                    }]
                }));
            }
            ResponseItem::CustomToolCallOutput { call_id, output } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output,
                }));
            }
            ResponseItem::Reasoning { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::Other => {
                // Omit these items from the conversation history.
                continue;
            }
        }
    }

    let strip_think = provider.provider_kind == ProviderKind::Ollama
        && provider.reasoning_controls.postprocess_reasoning;

    if !supports_streaming {
        return chat_completions_non_streaming(
            prompt,
            model_family,
            client,
            provider,
            provider_id,
            auth_manager,
            otel_event_manager,
            strip_think,
            messages,
        )
        .await;
    }

    let tools_json = create_tools_json_for_chat_completions_api(&prompt.tools)?;
    let mut payload = json!({
        "model": model_family.slug,
        "messages": messages,
        "stream": true,
        "tools": tools_json,
    });
    apply_provider_reasoning_overrides(&mut payload, provider);

    let mut attempt = 0;
    let max_retries = provider.request_max_retries();
    loop {
        attempt += 1;

        let auth = auth_manager.as_ref().and_then(|manager| {
            manager.auth_for_provider(provider_id, provider.requires_openai_auth)
        });

        debug!(
            "POST to {}: {}",
            provider.get_full_url(&auth),
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );

        let req_builder = provider.create_request_builder(client, &auth).await?;

        let res = otel_event_manager
            .log_request(attempt, || {
                req_builder
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .json(&payload)
                    .send()
            })
            .await;

        match res {
            Ok(resp) if resp.status().is_success() => {
                let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
                let stream = resp.bytes_stream().map_err(|e| {
                    CodexErr::ResponseStreamFailed(ResponseStreamFailed {
                        source: e,
                        request_id: None,
                    })
                });
                tokio::spawn(process_chat_sse(
                    stream,
                    tx_event,
                    provider.stream_idle_timeout(),
                    otel_event_manager.clone(),
                    strip_think,
                ));
                return Ok(ResponseStream { rx_event });
            }
            Ok(res) => {
                let status = res.status();
                if !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) {
                    let body = (res.text().await).unwrap_or_default();
                    return Err(CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body,
                        request_id: None,
                    }));
                }

                if attempt > max_retries {
                    return Err(CodexErr::RetryLimit(RetryLimitReachedError {
                        status,
                        request_id: None,
                    }));
                }

                let retry_after_secs = res
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());

                let delay = retry_after_secs
                    .map(|s| Duration::from_millis(s * 1_000))
                    .unwrap_or_else(|| backoff(attempt));
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                if attempt > max_retries {
                    return Err(CodexErr::ConnectionFailed(ConnectionFailedError {
                        source: e,
                    }));
                }
                let delay = backoff(attempt);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn chat_completions_non_streaming(
    prompt: &Prompt,
    model_family: &ModelFamily,
    client: &reqwest::Client,
    provider: &ModelProviderInfo,
    provider_id: &str,
    auth_manager: &Option<Arc<AuthManager>>,
    otel_event_manager: &OtelEventManager,
    strip_think: bool,
    messages: Vec<serde_json::Value>,
) -> Result<ResponseStream> {
    let tools_json = create_tools_json_for_chat_completions_api(&prompt.tools)?;
    let mut payload = json!({
        "model": model_family.slug,
        "messages": messages,
        "stream": false,
        "tools": tools_json,
    });
    apply_provider_reasoning_overrides(&mut payload, provider);

    let mut attempt = 0;
    let max_retries = provider.request_max_retries();
    loop {
        attempt += 1;
        let auth = auth_manager.as_ref().and_then(|manager| {
            manager.auth_for_provider(provider_id, provider.requires_openai_auth)
        });

        debug!(
            "POST (non-streaming) to {}: {}",
            provider.get_full_url(&auth),
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );

        let req_builder = provider.create_request_builder(client, &auth).await?;

        let res = otel_event_manager
            .log_request(attempt, || {
                req_builder
                    .header(reqwest::header::ACCEPT, "application/json")
                    .json(&payload)
                    .send()
            })
            .await;

        match res {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.map_err(|source| {
                    CodexErr::ConnectionFailed(ConnectionFailedError { source })
                })?;

                let response_id = body
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();

                let choice = body
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .cloned()
                    .ok_or_else(|| {
                        CodexErr::UnexpectedStatus(UnexpectedResponseError {
                            status: StatusCode::OK,
                            body: "chat completions response missing choices".to_string(),
                            request_id: body
                                .get("request_id")
                                .and_then(|v| v.as_str())
                                .map(std::string::ToString::to_string),
                        })
                    })?;

                let response_items = parse_non_streaming_response_items(&choice, strip_think);
                let token_usage = body.get("usage").and_then(parse_token_usage);

                let (tx, rx) = mpsc::channel::<Result<ResponseEvent>>(4);
                for item in response_items {
                    let _ = tx.send(Ok(ResponseEvent::OutputItemDone(item))).await;
                }
                let _ = tx
                    .send(Ok(ResponseEvent::Completed {
                        response_id,
                        token_usage,
                    }))
                    .await;
                drop(tx);
                return Ok(ResponseStream { rx_event: rx });
            }
            Ok(res) => {
                let status = res.status();
                if !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) {
                    let body = (res.text().await).unwrap_or_default();
                    return Err(CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body,
                        request_id: None,
                    }));
                }

                if attempt > max_retries {
                    return Err(CodexErr::RetryLimit(RetryLimitReachedError {
                        status,
                        request_id: None,
                    }));
                }

                let retry_after_secs = res
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());

                let delay = retry_after_secs
                    .map(|s| Duration::from_millis(s * 1_000))
                    .unwrap_or_else(|| backoff(attempt));
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                if attempt > max_retries {
                    return Err(CodexErr::ConnectionFailed(ConnectionFailedError {
                        source: e,
                    }));
                }
                let delay = backoff(attempt);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

fn parse_non_streaming_response_items(
    choice: &serde_json::Value,
    strip_think: bool,
) -> Vec<ResponseItem> {
    let mut items = Vec::new();

    let message_map = choice
        .get("message")
        .and_then(|m| m.as_object())
        .cloned()
        .unwrap_or_default();

    if let Some(reasoning_value) = message_map.get("reasoning_content")
        && let Some(reasoning_text) = collect_text_field(reasoning_value)
    {
        items.push(ResponseItem::Reasoning {
            id: String::new(),
            summary: Vec::new(),
            content: Some(vec![ReasoningItemContent::ReasoningText {
                text: reasoning_text,
            }]),
            encrypted_content: None,
        });
    }

    if let Some(tool_calls) = message_map.get("tool_calls").and_then(|tc| tc.as_array()) {
        for call in tool_calls {
            let function = call.get("function").and_then(|f| f.as_object());
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let arguments_value = function.and_then(|f| f.get("arguments"));
            let arguments = arguments_value
                .map(|value| match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "{}".to_string());
            let call_id = call
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            items.push(ResponseItem::FunctionCall {
                id: None,
                name,
                arguments,
                call_id,
            });
        }
    }

    if let Some(content_value) = message_map.get("content")
        && let Some(mut text) = collect_text_field(content_value)
    {
        let mut think_reasoning: Vec<String> = Vec::new();
        if strip_think {
            let mut extraction = ThinkParser::strip_all(&text);
            text = extraction.visible;
            think_reasoning.append(&mut extraction.reasoning);
        }

        for segment in think_reasoning {
            if !segment.trim().is_empty() {
                items.push(ResponseItem::Reasoning {
                    id: String::new(),
                    summary: Vec::new(),
                    content: Some(vec![ReasoningItemContent::ReasoningText {
                        text: segment.clone(),
                    }]),
                    encrypted_content: None,
                });
            }
        }

        if !text.is_empty() {
            items.push(ResponseItem::Message {
                id: None,
                role: message_map
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("assistant")
                    .to_string(),
                content: vec![ContentItem::OutputText { text }],
            });
        }
    }

    items
}

fn collect_text_field(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            if s.trim().is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        }
        serde_json::Value::Array(parts) => {
            let mut buf = String::new();
            for part in parts {
                if let Some(s) = part.get("text").and_then(|v| v.as_str()) {
                    buf.push_str(s);
                } else if let Some(s) = part.get("content").and_then(|v| v.as_str()) {
                    buf.push_str(s);
                } else if let Some(s) = part.get("string_value").and_then(|v| v.as_str()) {
                    buf.push_str(s);
                } else if let Some(s) = part.as_str() {
                    buf.push_str(s);
                }
            }
            if buf.trim().is_empty() {
                None
            } else {
                Some(buf)
            }
        }
        _ => None,
    }
}

fn parse_token_usage(usage: &serde_json::Value) -> Option<TokenUsage> {
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| input_tokens + output_tokens);

    Some(TokenUsage {
        input_tokens,
        cached_input_tokens: cached,
        output_tokens,
        reasoning_output_tokens: reasoning_tokens,
        total_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn think_parser_strips_single_segment() {
        let mut parser = ThinkParser::new();
        let first = parser.push("Hello <think>plan</think> world");
        assert_eq!(first.visible, "Hello  world");
        assert_eq!(first.reasoning, vec!["plan".to_string()]);

        let flush = parser.flush();
        assert!(flush.visible.is_empty());
        assert!(flush.reasoning.is_empty());
    }

    #[test]
    fn think_parser_handles_chunk_boundaries() {
        let mut parser = ThinkParser::new();

        let chunk1 = parser.push("Hel");
        assert_eq!(chunk1.visible, "Hel");
        assert!(chunk1.reasoning.is_empty());

        let chunk2 = parser.push("lo <thi");
        assert_eq!(chunk2.visible, "lo ");
        assert!(chunk2.reasoning.is_empty());

        let chunk3 = parser.push("nk>foo</think> world");
        assert_eq!(chunk3.visible, " world");
        assert_eq!(chunk3.reasoning, vec!["foo".to_string()]);

        let flush = parser.flush();
        assert!(flush.visible.is_empty());
        assert!(flush.reasoning.is_empty());
    }

    #[test]
    fn think_parser_multiple_segments() {
        let extraction = ThinkParser::strip_all("A<think>x</think>B<think>y</think>");
        assert_eq!(extraction.visible, "AB");
        assert_eq!(extraction.reasoning, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn think_parser_leaves_incomplete_tag_visible() {
        let extraction = ThinkParser::strip_all("<think>partial");
        assert_eq!(extraction.visible, "<think>partial");
        assert!(extraction.reasoning.is_empty());
    }
}

/// Lightweight SSE processor for the Chat Completions streaming format. The
/// output is mapped onto Codex's internal [`ResponseEvent`] so that the rest
/// of the pipeline can stay agnostic of the underlying wire format.
async fn process_chat_sse<S>(
    stream: S,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    idle_timeout: Duration,
    otel_event_manager: OtelEventManager,
    strip_think_tags: bool,
) where
    S: Stream<Item = Result<Bytes>> + Unpin,
{
    let mut stream = stream.eventsource();

    // State to accumulate a function call across streaming chunks.
    // OpenAI may split the `arguments` string over multiple `delta` events
    // until the chunk whose `finish_reason` is `tool_calls` is emitted. We
    // keep collecting the pieces here and forward a single
    // `ResponseItem::FunctionCall` once the call is complete.
    #[derive(Default)]
    struct FunctionCallState {
        name: Option<String>,
        arguments: String,
        call_id: Option<String>,
        active: bool,
    }

    let mut fn_call_state = FunctionCallState::default();
    let mut assistant_text = String::new();
    let mut reasoning_text = String::new();
    let mut think_parser = strip_think_tags.then(ThinkParser::new);

    loop {
        let start = std::time::Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        let duration = start.elapsed();
        otel_event_manager.log_sse_event(&response, duration);

        let sse = match response {
            Ok(Some(Ok(ev))) => ev,
            Ok(Some(Err(e))) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(e.to_string(), None)))
                    .await;
                return;
            }
            Ok(None) => {
                // Stream closed gracefully – emit Completed with dummy id.
                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: None,
                    }))
                    .await;
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(
                        "idle timeout waiting for SSE".into(),
                        None,
                    )))
                    .await;
                return;
            }
        };

        // OpenAI Chat streaming sends a literal string "[DONE]" when finished.
        if sse.data.trim() == "[DONE]" {
            if let Some(parser) = think_parser.as_mut() {
                let extracted = parser.flush();
                if !extracted.visible.is_empty() {
                    assistant_text.push_str(&extracted.visible);
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputTextDelta(
                            extracted.visible.clone(),
                        )))
                        .await;
                }
                for segment in extracted.reasoning {
                    append_reasoning(&mut reasoning_text, segment.trim());
                    let _ = tx_event
                        .send(Ok(ResponseEvent::ReasoningContentDelta(segment)))
                        .await;
                }
            }
            // Emit any finalized items before closing so downstream consumers receive
            // terminal events for both assistant content and raw reasoning.
            if !assistant_text.is_empty() {
                let item = ResponseItem::Message {
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: std::mem::take(&mut assistant_text),
                    }],
                    id: None,
                };
                let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
            }

            if !reasoning_text.is_empty() {
                let item = ResponseItem::Reasoning {
                    id: String::new(),
                    summary: Vec::new(),
                    content: Some(vec![ReasoningItemContent::ReasoningText {
                        text: std::mem::take(&mut reasoning_text),
                    }]),
                    encrypted_content: None,
                };
                let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
            }

            let _ = tx_event
                .send(Ok(ResponseEvent::Completed {
                    response_id: String::new(),
                    token_usage: None,
                }))
                .await;
            return;
        }

        // Parse JSON chunk
        let chunk: serde_json::Value = match serde_json::from_str(&sse.data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        trace!("chat_completions received SSE chunk: {chunk:?}");

        let choice_opt = chunk.get("choices").and_then(|c| c.get(0));

        if let Some(choice) = choice_opt {
            // Handle assistant content tokens as streaming deltas.
            if let Some(content) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str())
                && !content.is_empty()
            {
                if let Some(parser) = think_parser.as_mut() {
                    let extracted = parser.push(content);
                    if !extracted.visible.is_empty() {
                        assistant_text.push_str(&extracted.visible);
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputTextDelta(
                                extracted.visible.clone(),
                            )))
                            .await;
                    }
                    for segment in extracted.reasoning {
                        append_reasoning(&mut reasoning_text, segment.trim());
                        let _ = tx_event
                            .send(Ok(ResponseEvent::ReasoningContentDelta(segment)))
                            .await;
                    }
                } else {
                    assistant_text.push_str(content);
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputTextDelta(content.to_string())))
                        .await;
                }
            }

            // Forward any reasoning/thinking deltas if present.
            // Some providers stream `reasoning` as a plain string while others
            // nest the text under an object (e.g. `{ "reasoning": { "text": "…" } }`).
            if let Some(reasoning_val) = choice.get("delta").and_then(|d| d.get("reasoning")) {
                let mut maybe_text = reasoning_val
                    .as_str()
                    .map(str::to_string)
                    .filter(|s| !s.is_empty());

                if maybe_text.is_none() && reasoning_val.is_object() {
                    if let Some(s) = reasoning_val
                        .get("text")
                        .and_then(|t| t.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        maybe_text = Some(s.to_string());
                    } else if let Some(s) = reasoning_val
                        .get("content")
                        .and_then(|t| t.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        maybe_text = Some(s.to_string());
                    }
                }

                if let Some(reasoning) = maybe_text {
                    // Accumulate so we can emit a terminal Reasoning item at the end.
                    append_reasoning(&mut reasoning_text, reasoning.trim());
                    let _ = tx_event
                        .send(Ok(ResponseEvent::ReasoningContentDelta(reasoning)))
                        .await;
                }
            }

            // Some providers only include reasoning on the final message object.
            if let Some(message_reasoning) = choice.get("message").and_then(|m| m.get("reasoning"))
            {
                // Accept either a plain string or an object with { text | content }
                if let Some(s) = message_reasoning.as_str() {
                    if !s.is_empty() {
                        append_reasoning(&mut reasoning_text, s.trim());
                        let _ = tx_event
                            .send(Ok(ResponseEvent::ReasoningContentDelta(s.to_string())))
                            .await;
                    }
                } else if let Some(obj) = message_reasoning.as_object()
                    && let Some(s) = obj
                        .get("text")
                        .and_then(|v| v.as_str())
                        .or_else(|| obj.get("content").and_then(|v| v.as_str()))
                    && !s.is_empty()
                {
                    append_reasoning(&mut reasoning_text, s.trim());
                    let _ = tx_event
                        .send(Ok(ResponseEvent::ReasoningContentDelta(s.to_string())))
                        .await;
                }
            }

            // Handle streaming function / tool calls.
            if let Some(tool_calls) = choice
                .get("delta")
                .and_then(|d| d.get("tool_calls"))
                .and_then(|tc| tc.as_array())
                && let Some(tool_call) = tool_calls.first()
            {
                // Mark that we have an active function call in progress.
                fn_call_state.active = true;

                // Extract call_id if present.
                if let Some(id) = tool_call.get("id").and_then(|v| v.as_str()) {
                    fn_call_state.call_id.get_or_insert_with(|| id.to_string());
                }

                // Extract function details if present.
                if let Some(function) = tool_call.get("function") {
                    if let Some(name) = function.get("name").and_then(|n| n.as_str()) {
                        fn_call_state.name.get_or_insert_with(|| name.to_string());
                    }

                    if let Some(args_fragment) = function.get("arguments").and_then(|a| a.as_str())
                    {
                        fn_call_state.arguments.push_str(args_fragment);
                    }
                }
            }

            // Emit end-of-turn when finish_reason signals completion.
            if let Some(finish_reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                if let Some(parser) = think_parser.as_mut() {
                    let extracted = parser.flush();
                    if !extracted.visible.is_empty() {
                        assistant_text.push_str(&extracted.visible);
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputTextDelta(
                                extracted.visible.clone(),
                            )))
                            .await;
                    }
                    for segment in extracted.reasoning {
                        append_reasoning(&mut reasoning_text, segment.trim());
                        let _ = tx_event
                            .send(Ok(ResponseEvent::ReasoningContentDelta(segment)))
                            .await;
                    }
                }
                match finish_reason {
                    "tool_calls" if fn_call_state.active => {
                        // First, flush the terminal raw reasoning so UIs can finalize
                        // the reasoning stream before any exec/tool events begin.
                        if !reasoning_text.is_empty() {
                            let item = ResponseItem::Reasoning {
                                id: String::new(),
                                summary: Vec::new(),
                                content: Some(vec![ReasoningItemContent::ReasoningText {
                                    text: std::mem::take(&mut reasoning_text),
                                }]),
                                encrypted_content: None,
                            };
                            let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
                        }

                        // Then emit the FunctionCall response item.
                        let item = ResponseItem::FunctionCall {
                            id: None,
                            name: fn_call_state.name.clone().unwrap_or_else(|| "".to_string()),
                            arguments: fn_call_state.arguments.clone(),
                            call_id: fn_call_state.call_id.clone().unwrap_or_else(String::new),
                        };

                        let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
                    }
                    "stop" => {
                        // Regular turn without tool-call. Emit the final assistant message
                        // as a single OutputItemDone so non-delta consumers see the result.
                        if !assistant_text.is_empty() {
                            let item = ResponseItem::Message {
                                role: "assistant".to_string(),
                                content: vec![ContentItem::OutputText {
                                    text: std::mem::take(&mut assistant_text),
                                }],
                                id: None,
                            };
                            let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
                        }
                        // Also emit a terminal Reasoning item so UIs can finalize raw reasoning.
                        if !reasoning_text.is_empty() {
                            let item = ResponseItem::Reasoning {
                                id: String::new(),
                                summary: Vec::new(),
                                content: Some(vec![ReasoningItemContent::ReasoningText {
                                    text: std::mem::take(&mut reasoning_text),
                                }]),
                                encrypted_content: None,
                            };
                            let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
                        }
                    }
                    _ => {}
                }

                // Emit Completed regardless of reason so the agent can advance.
                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: None,
                    }))
                    .await;

                // Prepare for potential next turn (should not happen in same stream).
                // fn_call_state = FunctionCallState::default();

                return; // End processing for this SSE stream.
            }
        }
    }
}

/// Optional client-side aggregation helper
///
/// Stream adapter that merges the incremental `OutputItemDone` chunks coming from
/// [`process_chat_sse`] into a *running* assistant message, **suppressing the
/// per-token deltas**.  The stream stays silent while the model is thinking
/// and only emits two events per turn:
///
///   1. `ResponseEvent::OutputItemDone` with the *complete* assistant message
///      (fully concatenated).
///   2. The original `ResponseEvent::Completed` right after it.
///
/// This mirrors the behaviour the TypeScript CLI exposes to its higher layers.
///
/// The adapter is intentionally *lossless*: callers who do **not** opt in via
/// [`AggregateStreamExt::aggregate()`] keep receiving the original unmodified
/// events.
#[derive(Copy, Clone, Eq, PartialEq)]
enum AggregateMode {
    AggregatedOnly,
    Streaming,
}
pub(crate) struct AggregatedChatStream<S> {
    inner: S,
    cumulative: String,
    cumulative_reasoning: String,
    pending: std::collections::VecDeque<ResponseEvent>,
    mode: AggregateMode,
}

impl<S> Stream for AggregatedChatStream<S>
where
    S: Stream<Item = Result<ResponseEvent>> + Unpin,
{
    type Item = Result<ResponseEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // First, flush any buffered events from the previous call.
        if let Some(ev) = this.pending.pop_front() {
            return Poll::Ready(Some(Ok(ev)));
        }

        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(ResponseEvent::OutputItemDone(item)))) => {
                    // If this is an incremental assistant message chunk, accumulate but
                    // do NOT emit yet. Forward any other item (e.g. FunctionCall) right
                    // away so downstream consumers see it.

                    let is_assistant_message = matches!(
                        &item,
                        codex_protocol::models::ResponseItem::Message { role, .. } if role == "assistant"
                    );

                    if is_assistant_message {
                        match this.mode {
                            AggregateMode::AggregatedOnly => {
                                // Only use the final assistant message if we have not
                                // seen any deltas; otherwise, deltas already built the
                                // cumulative text and this would duplicate it.
                                if this.cumulative.is_empty()
                                    && let codex_protocol::models::ResponseItem::Message {
                                        content,
                                        ..
                                    } = &item
                                    && let Some(text) = content.iter().find_map(|c| match c {
                                        codex_protocol::models::ContentItem::OutputText {
                                            text,
                                        } => Some(text),
                                        _ => None,
                                    })
                                {
                                    this.cumulative.push_str(text);
                                }
                                // Swallow assistant message here; emit on Completed.
                                continue;
                            }
                            AggregateMode::Streaming => {
                                // In streaming mode, if we have not seen any deltas, forward
                                // the final assistant message directly. If deltas were seen,
                                // suppress the final message to avoid duplication.
                                if this.cumulative.is_empty() {
                                    return Poll::Ready(Some(Ok(ResponseEvent::OutputItemDone(
                                        item,
                                    ))));
                                } else {
                                    continue;
                                }
                            }
                        }
                    }

                    // Not an assistant message – forward immediately.
                    return Poll::Ready(Some(Ok(ResponseEvent::OutputItemDone(item))));
                }
                Poll::Ready(Some(Ok(ResponseEvent::RateLimits(snapshot)))) => {
                    return Poll::Ready(Some(Ok(ResponseEvent::RateLimits(snapshot))));
                }
                Poll::Ready(Some(Ok(ResponseEvent::Completed {
                    response_id,
                    token_usage,
                }))) => {
                    // Build any aggregated items in the correct order: Reasoning first, then Message.
                    let mut emitted_any = false;

                    if !this.cumulative_reasoning.is_empty()
                        && matches!(this.mode, AggregateMode::AggregatedOnly)
                    {
                        let aggregated_reasoning =
                            codex_protocol::models::ResponseItem::Reasoning {
                                id: String::new(),
                                summary: Vec::new(),
                                content: Some(vec![
                                    codex_protocol::models::ReasoningItemContent::ReasoningText {
                                        text: std::mem::take(&mut this.cumulative_reasoning),
                                    },
                                ]),
                                encrypted_content: None,
                            };
                        this.pending
                            .push_back(ResponseEvent::OutputItemDone(aggregated_reasoning));
                        emitted_any = true;
                    }

                    // Always emit the final aggregated assistant message when any
                    // content deltas have been observed. In AggregatedOnly mode this
                    // is the sole assistant output; in Streaming mode this finalizes
                    // the streamed deltas into a terminal OutputItemDone so callers
                    // can persist/render the message once per turn.
                    if !this.cumulative.is_empty() {
                        let aggregated_message = codex_protocol::models::ResponseItem::Message {
                            id: None,
                            role: "assistant".to_string(),
                            content: vec![codex_protocol::models::ContentItem::OutputText {
                                text: std::mem::take(&mut this.cumulative),
                            }],
                        };
                        this.pending
                            .push_back(ResponseEvent::OutputItemDone(aggregated_message));
                        emitted_any = true;
                    }

                    // Always emit Completed last when anything was aggregated.
                    if emitted_any {
                        this.pending.push_back(ResponseEvent::Completed {
                            response_id: response_id.clone(),
                            token_usage: token_usage.clone(),
                        });
                        // Return the first pending event now.
                        if let Some(ev) = this.pending.pop_front() {
                            return Poll::Ready(Some(Ok(ev)));
                        }
                    }

                    // Nothing aggregated – forward Completed directly.
                    return Poll::Ready(Some(Ok(ResponseEvent::Completed {
                        response_id,
                        token_usage,
                    })));
                }
                Poll::Ready(Some(Ok(ResponseEvent::Created))) => {
                    // These events are exclusive to the Responses API and
                    // will never appear in a Chat Completions stream.
                    continue;
                }
                Poll::Ready(Some(Ok(ResponseEvent::OutputTextDelta(delta)))) => {
                    // Always accumulate deltas so we can emit a final OutputItemDone at Completed.
                    this.cumulative.push_str(&delta);
                    if matches!(this.mode, AggregateMode::Streaming) {
                        // In streaming mode, also forward the delta immediately.
                        return Poll::Ready(Some(Ok(ResponseEvent::OutputTextDelta(delta))));
                    } else {
                        continue;
                    }
                }
                Poll::Ready(Some(Ok(ResponseEvent::ReasoningContentDelta(delta)))) => {
                    // Always accumulate reasoning deltas so we can emit a final Reasoning item at Completed.
                    this.cumulative_reasoning.push_str(&delta);
                    if matches!(this.mode, AggregateMode::Streaming) {
                        // In streaming mode, also forward the delta immediately.
                        return Poll::Ready(Some(Ok(ResponseEvent::ReasoningContentDelta(delta))));
                    } else {
                        continue;
                    }
                }
                Poll::Ready(Some(Ok(ResponseEvent::ReasoningSummaryDelta(_)))) => {
                    continue;
                }
                Poll::Ready(Some(Ok(ResponseEvent::ReasoningSummaryPartAdded))) => {
                    continue;
                }
                Poll::Ready(Some(Ok(ResponseEvent::WebSearchCallBegin { call_id }))) => {
                    return Poll::Ready(Some(Ok(ResponseEvent::WebSearchCallBegin { call_id })));
                }
            }
        }
    }
}

/// Extension trait that activates aggregation on any stream of [`ResponseEvent`].
pub(crate) trait AggregateStreamExt: Stream<Item = Result<ResponseEvent>> + Sized {
    /// Returns a new stream that emits **only** the final assistant message
    /// per turn instead of every incremental delta.  The produced
    /// `ResponseEvent` sequence for a typical text turn looks like:
    ///
    /// ```ignore
    ///     OutputItemDone(<full message>)
    ///     Completed
    /// ```
    ///
    /// No other `OutputItemDone` events will be seen by the caller.
    ///
    /// Usage:
    ///
    /// ```ignore
    /// let agg_stream = client.stream(&prompt).await?.aggregate();
    /// while let Some(event) = agg_stream.next().await {
    ///     // event now contains cumulative text
    /// }
    /// ```
    fn aggregate(self) -> AggregatedChatStream<Self> {
        AggregatedChatStream::new(self, AggregateMode::AggregatedOnly)
    }
}

impl<T> AggregateStreamExt for T where T: Stream<Item = Result<ResponseEvent>> + Sized {}

impl<S> AggregatedChatStream<S> {
    fn new(inner: S, mode: AggregateMode) -> Self {
        AggregatedChatStream {
            inner,
            cumulative: String::new(),
            cumulative_reasoning: String::new(),
            pending: std::collections::VecDeque::new(),
            mode,
        }
    }

    pub(crate) fn streaming_mode(inner: S) -> Self {
        Self::new(inner, AggregateMode::Streaming)
    }
}
