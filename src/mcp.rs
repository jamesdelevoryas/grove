//! An MCP client over streamable HTTP (James's memory server). Adapted from Winter.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use rmcp::{
    RoleClient, ServiceExt,
    model::CallToolRequestParams,
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};

pub struct Mcp {
    name: String,
    client: RunningService<RoleClient, ()>,
    tools: Vec<Value>,
    known: HashMap<String, ()>,
}

impl Mcp {
    /// Connects and lists tools, keeping only those named in `allow` (all if empty).
    pub async fn connect(name: &str, url: &str, token: &str, allow: &[&str]) -> Result<Self> {
        let config =
            StreamableHttpClientTransportConfig::with_uri(url).auth_header(token.to_string());
        let transport = StreamableHttpClientTransport::from_config(config);
        let client = ().serve(transport).await.with_context(|| format!("connecting to {url}"))?;
        let mut tools = Vec::new();
        let mut known = HashMap::new();
        let mut listed = client.list_all_tools().await?;
        // Deterministic order keeps the tool list (and so the prompt cache) stable.
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        for tool in listed {
            let tool_name = tool.name.to_string();
            if !allow.is_empty() && !allow.contains(&tool_name.as_str()) {
                continue;
            }
            tools.push(json!({
                "name": tool_name,
                "description": tool.description.map(|d| d.to_string()).unwrap_or_default(),
                "input_schema": Value::Object((*tool.input_schema).clone()),
            }));
            known.insert(tool_name, ());
        }
        Ok(Self {
            name: name.to_string(),
            client,
            tools,
            known,
        })
    }

    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    pub fn has(&self, tool: &str) -> bool {
        self.known.contains_key(tool)
    }

    pub fn instructions(&self) -> Option<String> {
        let info = self.client.peer_info()?;
        info.instructions.as_ref().map(|s| s.trim().to_string())
    }

    /// Calls a tool; returns (text, is_error).
    pub async fn call(&self, tool: &str, input: &Value) -> Result<(String, bool)> {
        if !self.has(tool) {
            bail!("no MCP tool {tool}");
        }
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Value::Object(arguments) = input {
            params = params.with_arguments(arguments.clone());
        }
        let result = self
            .client
            .call_tool(params)
            .await
            .with_context(|| format!("calling {tool} on {}", self.name))?;
        let mut text = String::new();
        for block in &result.content {
            if let Some(t) = block.as_text() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t.text);
            }
        }
        if text.is_empty()
            && let Some(structured) = &result.structured_content
        {
            text = structured.to_string();
        }
        Ok((text, result.is_error.unwrap_or(false)))
    }
}
