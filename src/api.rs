//! The Claude Messages API over raw HTTP (there is no official Rust SDK), streaming.
//!
//! Every request: adaptive thinking with `display: "updates"` (progress notes between tool
//! calls come back as thinking text), an explicit effort (Opus 5.5 defaults to medium),
//! top-level automatic prompt caching, and server-side refusal fallbacks.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::protocol::Effort;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const BETAS: &str = "thinking-display-updates-2026-08-18,server-side-fallback-2026-07-01";
const MAX_ATTEMPTS: u32 = 5;
const STREAM_STALL: Duration = Duration::from_secs(180);

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    api_key: Option<String>,
}

pub struct Request<'a> {
    pub model: &'a str,
    pub effort: Effort,
    pub max_tokens: u32,
    pub system: &'a str,
    pub tools: &'a [Value],
    /// Borrowed, so a long history is serialized straight from the agent's copy.
    pub messages: &'a [&'a Value],
}

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    tools: &'a [Value],
    messages: &'a [&'a Value],
    cache_control: Value,
    thinking: Value,
    output_config: Value,
    fallbacks: &'static str,
    stream: bool,
}

impl Request<'_> {
    /// The request body, serialized once so the streaming future owns it.
    pub fn body(&self) -> Vec<u8> {
        let body = Body {
            model: self.model,
            max_tokens: self.max_tokens,
            system: self.system,
            tools: self.tools,
            messages: self.messages,
            cache_control: json!({ "type": "ephemeral" }),
            thinking: json!({ "type": "adaptive", "display": "updates" }),
            output_config: json!({ "effort": self.effort.as_str() }),
            fallbacks: "default",
            stream: true,
        };
        serde_json::to_vec(&body).expect("request serializes")
    }
}

#[derive(Debug)]
pub struct Response {
    pub content: Vec<Value>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    PauseTurn,
    Refusal,
    StopSequence,
    #[serde(other)]
    Unknown,
}

/// What the stream reports while a response is generated.
pub enum Update<'a> {
    Text(&'a str),
    Progress(&'a str),
    BlockDone(&'a Value),
}

impl Client {
    pub fn new(api_key: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            // Keep idle connections warm: the next request skips the TLS handshake.
            .pool_idle_timeout(Duration::from_secs(300))
            .tcp_nodelay(true)
            .build()
            .expect("http client");
        Self { http, api_key }
    }

    pub fn has_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// Sends a streaming request and builds the full response from it. `on_update` sees
    /// text and progress notes as they arrive. Dropping the future cancels the request.
    pub async fn send(
        &self,
        body: Vec<u8>,
        extra_beta: Option<&str>,
        mut on_update: impl FnMut(Update<'_>),
    ) -> Result<Response> {
        let Some(key) = &self.api_key else {
            bail!(
                "no Anthropic API key: set ANTHROPIC_API_KEY, or store one with\n  security add-generic-password -s grove-anthropic-api-key -a grove -w"
            );
        };
        let betas = match extra_beta {
            Some(extra) => format!("{BETAS},{extra}"),
            None => BETAS.to_string(),
        };
        let mut resp = self.post_with_retry(key, &betas, body).await?;

        let mut stream = MessageStream::default();
        let mut buf: Vec<u8> = Vec::new();
        loop {
            // The API sends pings while it works; this long a silence means a dead connection.
            let chunk = match tokio::time::timeout(STREAM_STALL, resp.chunk()).await {
                Ok(chunk) => chunk?,
                Err(_) => bail!(
                    "the response stream stalled ({}s without data)",
                    STREAM_STALL.as_secs()
                ),
            };
            let Some(chunk) = chunk else { break };
            buf.extend_from_slice(&chunk);
            while let Some(end) = find_event_end(&buf) {
                let event: Vec<u8> = buf.drain(..end).collect();
                for line in std::str::from_utf8(&event)?.lines() {
                    if let Some(data) = line.strip_prefix("data:") {
                        let event: Value = serde_json::from_str(data.trim_start())
                            .context("bad JSON in stream")?;
                        stream.apply(&event, &mut on_update)?;
                    }
                }
            }
        }
        stream.finish()
    }

    /// Retries overload, rate limits, and server errors, which all fail before the stream
    /// starts, honouring retry-after.
    async fn post_with_retry(
        &self,
        key: &str,
        betas: &str,
        body: Vec<u8>,
    ) -> Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self
                .http
                .post(API_URL)
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", betas)
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .await;
            let resp = match result {
                Ok(resp) => resp,
                Err(e) if attempt < MAX_ATTEMPTS && (e.is_connect() || e.is_timeout()) => {
                    tokio::time::sleep(backoff(attempt, None)).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            let retryable =
                status.as_u16() == 429 || status.as_u16() == 529 || status.is_server_error();
            if retryable && attempt < MAX_ATTEMPTS {
                let after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
                tokio::time::sleep(backoff(attempt, after)).await;
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            bail!("API error {status}: {}", error_message(&text));
        }
    }
}

fn backoff(attempt: u32, retry_after: Option<u64>) -> Duration {
    if let Some(secs) = retry_after {
        return Duration::from_secs(secs.min(60));
    }
    Duration::from_millis(500 * 2u64.pow(attempt.min(6)))
}

/// The `error.message` of an API error body, or the body itself.
fn error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body)
        && let Some(msg) = v["error"]["message"].as_str()
    {
        return msg.to_string();
    }
    body.to_string()
}

/// Index just past the blank line that ends the next SSE event, if one is complete.
fn find_event_end(buf: &[u8]) -> Option<usize> {
    for i in 1..buf.len() {
        if buf[i - 1] == b'\n' && buf[i] == b'\n' {
            return Some(i + 1);
        }
        if i >= 3 && &buf[i - 3..=i] == b"\r\n\r\n" {
            return Some(i + 1);
        }
    }
    None
}

/// Builds the message out of stream events. Blocks stay raw JSON so thinking blocks (and
/// signatures) and fallback blocks are echoed back exactly.
#[derive(Default)]
struct MessageStream {
    content: Vec<Value>,
    partial_json: Vec<String>,
    usage: Option<Usage>,
    stop_reason: Option<StopReason>,
}

impl MessageStream {
    fn apply(&mut self, event: &Value, on_update: &mut impl FnMut(Update<'_>)) -> Result<()> {
        match event["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.usage =
                    Some(Usage::deserialize(&event["message"]["usage"]).unwrap_or_default());
            }
            "content_block_start" => {
                let index = block_index(event)?;
                // Blocks normally arrive in order; tolerate gaps rather than fail.
                while self.content.len() <= index {
                    self.content.push(Value::Null);
                    self.partial_json.push(String::new());
                }
                self.content[index] = event["content_block"].clone();
            }
            "content_block_delta" => {
                let index = block_index(event)?;
                let Some(block) = self.content.get_mut(index) else {
                    bail!("stream: delta for unknown block {index}");
                };
                let delta = &event["delta"];
                match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let text = delta["text"].as_str().unwrap_or("");
                        append(block, "text", text);
                        on_update(Update::Text(text));
                    }
                    "thinking_delta" => {
                        let text = delta["thinking"].as_str().unwrap_or("");
                        append(block, "thinking", text);
                        on_update(Update::Progress(text));
                    }
                    "signature_delta" => {
                        append(
                            block,
                            "signature",
                            delta["signature"].as_str().unwrap_or(""),
                        );
                    }
                    "input_json_delta" => {
                        self.partial_json[index]
                            .push_str(delta["partial_json"].as_str().unwrap_or(""));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = block_index(event)?;
                let Some(block) = self.content.get_mut(index) else {
                    bail!("stream: stop for unknown block {index}");
                };
                let json = &self.partial_json[index];
                if !json.is_empty() {
                    // Eager input streaming means the server doesn't validate tool input, so
                    // it can be broken (e.g. cut off at max_tokens). Keep the raw text; the
                    // agent reports it to the model as an error.
                    match serde_json::from_str::<Value>(json) {
                        Ok(input) => block["input"] = input,
                        Err(_) => {
                            block["input"] = json!({});
                            block["_invalid_input"] = Value::String(json.clone());
                        }
                    }
                }
                on_update(Update::BlockDone(block));
            }
            "message_delta" => {
                if let Some(reason) = event["delta"].get("stop_reason")
                    && !reason.is_null()
                {
                    self.stop_reason =
                        Some(StopReason::deserialize(reason).unwrap_or(StopReason::Unknown));
                }
                if let Some(usage) = &mut self.usage {
                    let u = &event["usage"];
                    if let Some(n) = u["output_tokens"].as_u64() {
                        usage.output_tokens = n;
                    }
                    if let Some(n) = u["input_tokens"].as_u64() {
                        usage.input_tokens = n;
                    }
                    if let Some(n) = u["cache_read_input_tokens"].as_u64() {
                        usage.cache_read_input_tokens = Some(n);
                    }
                    if let Some(n) = u["cache_creation_input_tokens"].as_u64() {
                        usage.cache_creation_input_tokens = Some(n);
                    }
                }
            }
            "error" => bail!(
                "API error during stream: {}",
                event["error"]["message"]
                    .as_str()
                    .unwrap_or(&event["error"].to_string())
            ),
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<Response> {
        let mut content = Vec::new();
        for block in self.content {
            if !block.is_null() {
                content.push(block);
            }
        }
        Ok(Response {
            content,
            stop_reason: self
                .stop_reason
                .context("stream ended before the message finished")?,
            usage: self.usage.context("stream ended before message_start")?,
        })
    }
}

fn block_index(event: &Value) -> Result<usize> {
    let index = event["index"]
        .as_u64()
        .context("stream event without an index")?;
    Ok(index as usize)
}

fn append(block: &mut Value, field: &str, text: &str) {
    if let Some(Value::String(existing)) = block.get_mut(field) {
        existing.push_str(text);
    } else {
        block[field] = Value::String(text.to_string());
    }
}

/// Strips fields we added to blocks for our own bookkeeping before they go back to the API.
pub fn clean_block(block: &Value) -> Value {
    let mut block = block.clone();
    if let Some(obj) = block.as_object_mut() {
        obj.remove("_invalid_input");
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_message_from_events() {
        let events = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"bash","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}),
        ];
        let mut stream = MessageStream::default();
        let mut text = String::new();
        for e in &events {
            stream
                .apply(e, &mut |u| {
                    if let Update::Text(t) = u {
                        text.push_str(t)
                    }
                })
                .unwrap();
        }
        let resp = stream.finish().unwrap();
        assert_eq!(text, "Hello");
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.usage.output_tokens, 42);
        assert_eq!(resp.content[1]["input"]["command"], "ls");
    }

    #[test]
    fn finds_event_boundaries() {
        assert_eq!(find_event_end(b"data: x\n\nrest"), Some(9));
        assert_eq!(find_event_end(b"data: x\r\n\r\nrest"), Some(11));
        assert_eq!(find_event_end(b"data: x\n"), None);
    }
}
