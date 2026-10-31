//! Small model calls that make the UI smarter (suggested next prompts, and later things
//! like session titles or summaries of long tool output).
//!
//! The main trick is the *fork*: re-send an agent's exact request prefix (model, system,
//! tools, history) and append one question. The whole prefix is a prompt-cache hit, so the
//! model sees the full conversation for the price of cache reads. A per-message effort
//! change (a `system` message with only `output_config`) keeps it fast without touching
//! the cached prefix; if the API rejects that, we retry without it.

use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::api;
use crate::hub::Hub;
use crate::protocol::{AgentId, Effort, EventKind};

const PER_MESSAGE_EFFORT_BETA: &str = "mid-conversation-output-config-2026-07-01";
const FORK_MAX_TOKENS: u32 = 2_000;

const SUGGEST_PROMPT: &str = "[grove UI request — not from the human] Suggest the two most useful prompts the human might send next in this conversation, written in their voice, each under 80 characters. Make them distinct: e.g. one that continues the work and one that checks or improves it. Don't call tools. Reply with only a JSON array of two strings.";

/// The pieces of an agent's request a fork must copy byte-for-byte.
pub struct Fork {
    pub model: String,
    pub effort: Effort,
    pub system: String,
    pub tools: Arc<Vec<Value>>,
    pub messages: Vec<Value>,
}

impl Fork {
    /// Asks `question` at the end of the conversation; returns the text reply.
    pub async fn ask(
        &self,
        client: &api::Client,
        question: &str,
        effort: Effort,
    ) -> Result<String> {
        match self.ask_once(client, question, Some(effort)).await {
            Ok(text) => Ok(text),
            // Per-message effort is a beta; fall back to the agent's own effort.
            Err(_) => self.ask_once(client, question, None).await,
        }
    }

    async fn ask_once(
        &self,
        client: &api::Client,
        question: &str,
        effort: Option<Effort>,
    ) -> Result<String> {
        let question = json!({ "role": "user", "content": [{ "type": "text", "text": question }] });
        let effort_msg = effort.map(|e| json!({ "role": "system", "content": [], "output_config": { "effort": e.as_str() } }));
        let mut messages: Vec<&Value> = Vec::with_capacity(self.messages.len() + 2);
        for m in &self.messages {
            messages.push(m);
        }
        messages.push(&question);
        if let Some(m) = &effort_msg {
            messages.push(m);
        }
        let body = api::Request {
            model: &self.model,
            effort: self.effort,
            max_tokens: FORK_MAX_TOKENS,
            system: &self.system,
            tools: &self.tools,
            messages: &messages,
        }
        .body();
        let beta = if effort.is_some() {
            Some(PER_MESSAGE_EFFORT_BETA)
        } else {
            None
        };
        let response = client.send(body, beta, |_| {}).await?;
        let mut text = String::new();
        for block in &response.content {
            if block["type"] == "text"
                && let Some(t) = block["text"].as_str()
            {
                text.push_str(t);
            }
        }
        if text.is_empty() {
            bail!(
                "the fork answered with no text (stop reason {:?})",
                response.stop_reason
            );
        }
        Ok(text)
    }
}

/// Parses the first JSON array of strings in `text`.
pub fn parse_string_array(text: &str) -> Vec<String> {
    let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text[start..=end]) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        if let Some(s) = item.as_str() {
            let s = s.trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
        }
    }
    out
}

/// After a turn: two suggested next prompts, delivered as an event.
pub fn spawn_suggestions(
    hub: Hub,
    agent: AgentId,
    model: String,
    effort: Effort,
    system: String,
    tools: Arc<Vec<Value>>,
    messages: Vec<Value>,
) {
    if !hub.client().has_key() || messages.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let fork = Fork {
            model,
            effort,
            system,
            tools,
            messages,
        };
        let Ok(text) = fork.ask(hub.client(), SUGGEST_PROMPT, Effort::Low).await else {
            return;
        };
        let mut items = parse_string_array(&text);
        items.truncate(2);
        if !items.is_empty() {
            hub.emit(&agent, EventKind::Suggestions(items));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_arrays_in_prose() {
        assert_eq!(parse_string_array("Sure: [\"a\", \"b\"]"), vec!["a", "b"]);
        assert_eq!(
            parse_string_array("```json\n[\"run the tests\",\"commit\"]\n```"),
            vec!["run the tests", "commit"]
        );
        assert!(parse_string_array("nothing").is_empty());
    }
}
