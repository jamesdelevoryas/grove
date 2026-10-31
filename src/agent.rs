//! One agent's loop. Every agent, the watchers included, runs this same loop with its own
//! system prompt and tools, so the human can focus any of them and talk to it.
//!
//! History is append-only and only grows when the model answers: input waits in
//! `pending_*` until a response arrives, then the user message, any operator note, and the
//! assistant message are appended together. An interrupted request therefore leaves no
//! half-turn behind, and pausing for an upgrade only has to save the pending input.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::api::{self, StopReason, Update};
use crate::config;
use crate::hub::Hub;
use crate::infer;
use crate::protocol::*;
use crate::store::MessageLog;
use crate::tools;

pub struct Agent {
    meta: AgentMeta,
    hub: Hub,
    rx: mpsc::UnboundedReceiver<Input>,
    messages: Vec<Value>,
    log: MessageLog,
    tools: Arc<Vec<Value>>,
    /// Set by input that should start a turn.
    wake: bool,
    /// Why the current request or tool run stopped early.
    abort: Option<Abort>,
    /// The last turn failed; keep showing the error until new input arrives.
    errored: bool,
    /// Paused for an upgrade: input is queued and saved, but no turn starts until Resume.
    paused: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Abort {
    Interrupt,
    Pause,
    Stop,
    Clear,
    Closed,
}

impl Agent {
    pub fn new(
        hub: Hub,
        meta: AgentMeta,
        messages: Vec<Value>,
        tools: Vec<Value>,
        log: MessageLog,
        rx: mpsc::UnboundedReceiver<Input>,
    ) -> Self {
        let mut agent = Self {
            meta,
            hub,
            rx,
            messages,
            log,
            tools: Arc::new(tools),
            wake: false,
            abort: None,
            errored: false,
            paused: false,
        };
        agent.reconcile();
        agent.repair();
        agent
    }

    pub fn meta(&self) -> &AgentMeta {
        &self.meta
    }

    /// A crash after a turn was written to the log but before meta.json was saved leaves
    /// meta's pending input already in history; drop it rather than send it twice.
    fn reconcile(&mut self) {
        if self.messages.len() != self.meta.committed {
            if self.messages.len() > self.meta.committed {
                self.meta.pending_results.clear();
                self.meta.pending.clear();
                self.meta.directives.clear();
            }
            self.meta.committed = self.messages.len();
        }
    }

    /// A crash between the assistant's tool calls and their results leaves calls without
    /// results; answer them so the conversation stays valid.
    fn repair(&mut self) {
        let Some(last) = self.messages.last() else {
            return;
        };
        if last["role"] != "assistant" || !self.meta.pending_results.is_empty() {
            return;
        }
        let mut results = Vec::new();
        if let Some(blocks) = last["content"].as_array() {
            for block in blocks {
                if block["type"] == "tool_use" {
                    results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": block["id"],
                        "content": "grove restarted before this tool call finished; it may or may not have run.",
                        "is_error": true,
                    }));
                }
            }
        }
        if !results.is_empty() {
            self.meta.pending_results = results;
            self.meta.resume_on_start = true;
        }
    }

    pub async fn run(mut self) {
        let id = self.meta.id.clone();
        if self.meta.stopped {
            self.hub.set_status(&id, Status::Stopped);
        } else if self.meta.resume_on_start {
            self.meta.resume_on_start = false;
            self.meta.directives.push(
                "grove was rebuilt and restarted (a self-upgrade) while you were mid-task. Tool calls that were running were cut short; their results say so. Continue where you left off.".into(),
            );
            self.save_meta();
            self.turn().await;
        }
        loop {
            // An Exit that arrived mid-turn ends the task once the turn has wound down.
            if self.abort == Some(Abort::Closed) {
                break;
            }
            if self.paused {
                self.hub.set_status(&id, Status::Paused);
            } else if self.meta.stopped {
                self.hub.set_status(&id, Status::Stopped);
            } else if !self.errored {
                self.hub.set_status(&id, Status::Idle);
            }
            let Some(input) = self.rx.recv().await else {
                break;
            };
            self.errored = false;
            self.accept(input);
            while let Ok(more) = self.rx.try_recv() {
                self.accept(more);
            }
            if self.abort == Some(Abort::Closed) {
                break;
            }
            self.abort = None;
            if self.paused {
                // Whatever arrived must survive the exec; it runs after Resume or restart.
                self.save_meta();
                continue;
            }
            if self.meta.stopped {
                self.wake = false;
                continue;
            }
            if self.wake {
                self.wake = false;
                self.turn().await;
            }
        }
    }

    /// Takes in one input. Called both while idle and mid-turn; mid-turn, text input is
    /// queued for the next model request ("steering").
    fn accept(&mut self, input: Input) {
        let id = self.meta.id.clone();
        match input {
            Input::User(text) => {
                // A word from the human restarts a stopped agent.
                if self.meta.stopped {
                    self.meta.stopped = false;
                    self.save_meta();
                }
                self.hub.emit(&id, EventKind::UserText(text.clone()));
                self.meta
                    .pending
                    .push(json!({ "type": "text", "text": text }));
                self.wake = true;
            }
            Input::Message { from, text, wake } => {
                self.hub.emit(
                    &id,
                    EventKind::Injected {
                        from: from.clone(),
                        text: text.clone(),
                        directive: false,
                    },
                );
                let wrapped = format!("<message from=\"{from}\">\n{text}\n</message>");
                self.meta
                    .pending
                    .push(json!({ "type": "text", "text": wrapped }));
                if wake {
                    self.wake = true;
                }
            }
            Input::Directive { from, text } => {
                self.hub.emit(
                    &id,
                    EventKind::Injected {
                        from: from.clone(),
                        text: text.clone(),
                        directive: true,
                    },
                );
                self.meta.directives.push(format!("[{from}] {text}"));
            }
            Input::Feed(text) => {
                self.hub.emit(&id, EventKind::Feed(text.clone()));
                self.meta
                    .pending
                    .push(json!({ "type": "text", "text": text }));
                self.wake = true;
            }
            Input::Interrupt => self.abort = Some(Abort::Interrupt),
            Input::Pause => {
                self.abort = Some(Abort::Pause);
                self.paused = true;
                // Queued input must survive the restart even if we're idle.
                self.save_meta();
            }
            Input::Resume => {
                // The upgrade was abandoned: carry on in this process.
                self.paused = false;
                if self.meta.resume_on_start {
                    self.meta.resume_on_start = false;
                    self.wake = true;
                }
                self.save_meta();
            }
            Input::Stop => {
                self.abort = Some(Abort::Stop);
                self.meta.stopped = true;
                self.save_meta();
            }
            Input::Clear => {
                self.abort = Some(Abort::Clear);
                self.messages.clear();
                self.meta.committed = 0;
                self.meta.pending.clear();
                self.meta.pending_results.clear();
                self.meta.directives.clear();
                self.meta.usage.last_context = 0;
                if let Err(e) = self.log.rotate() {
                    self.hub
                        .notify(&id, Level::Warn, &format!("couldn't rotate the log: {e:#}"));
                }
                self.save_meta();
                self.hub.emit(&id, EventKind::Cleared);
            }
            Input::SetEffort(effort) => {
                // A top-level effort change costs one cache miss on the next request.
                self.meta.effort = effort;
                self.save_meta();
            }
            Input::SetModel(model) => {
                self.meta.model = model;
                self.save_meta();
            }
            Input::Retry => self.wake = true,
            Input::Exit => self.abort = Some(Abort::Closed),
        }
    }

    fn save_meta(&self) {
        self.hub.update_meta(&self.meta);
    }

    /// Appends one exchange to history with a single write, so a crash never leaves half
    /// of it in the log, then records how much history meta.json accounts for.
    fn commit(&mut self, messages: Vec<Value>) {
        if let Err(e) = self.log.append_all(&messages) {
            self.hub.notify(
                &self.meta.id,
                Level::Alert,
                &format!("couldn't save the conversation: {e:#}"),
            );
        }
        for m in messages {
            self.messages.push(m);
        }
        self.meta.committed = self.messages.len();
        self.save_meta();
    }

    fn has_pending(&self) -> bool {
        !self.meta.pending_results.is_empty()
            || !self.meta.pending.is_empty()
            || !self.meta.directives.is_empty()
    }

    /// The not-yet-answered part of the conversation: a user message (tool results first)
    /// and, if there are operator notes, a system message after it.
    fn tail(&self) -> Vec<Value> {
        let mut content = self.meta.pending_results.clone();
        content.extend(self.meta.pending.iter().cloned());
        if content.is_empty() {
            content.push(json!({ "type": "text", "text": "(continue)" }));
        }
        let mut tail = vec![json!({ "role": "user", "content": content })];
        if !self.meta.directives.is_empty() {
            tail.push(json!({ "role": "system", "content": self.meta.directives.join("\n\n") }));
        }
        tail
    }

    async fn turn(&mut self) {
        let id = self.meta.id.clone();
        let mut ended_normally = false;
        loop {
            if !self.has_pending() {
                ended_normally = true;
                break;
            }
            self.hub.set_status(&id, Status::Thinking);
            let tail = self.tail();
            let counts = (
                self.meta.pending_results.len(),
                self.meta.pending.len(),
                self.meta.directives.len(),
            );
            let response = match self.infer(&tail).await {
                Ok(Some(r)) => r,
                Ok(None) => {
                    self.hub.emit(&id, EventKind::ResponseDiscarded);
                    self.after_abort();
                    return;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    self.hub.notify(&id, Level::Alert, &msg);
                    let short: String = msg.lines().next().unwrap_or("").chars().take(60).collect();
                    self.hub.set_status(&id, Status::Error(short));
                    self.hub.emit(&id, EventKind::TurnEnd);
                    // Pending input stays queued; /retry or the next message sends it again.
                    // A watcher's activity batches are disposable, though: drop them rather
                    // than resend an ever-growing backlog while the API is failing.
                    if self.meta.role.is_watcher() {
                        self.meta.pending.retain(|b| {
                            !b["text"]
                                .as_str()
                                .is_some_and(|t| t.starts_with("[activity]"))
                        });
                    }
                    self.errored = true;
                    self.save_meta();
                    return;
                }
            };
            self.record_usage(&response.usage);

            if response.stop_reason == StopReason::Refusal {
                // Keep nothing from a declined exchange; the human can rephrase. Tool
                // results still have to answer the calls already in history, so they stay
                // queued, with their content withheld.
                for result in self.meta.pending_results.iter_mut().take(counts.0) {
                    result["content"] =
                        json!("[withheld: the model declined to continue after this result]");
                    result["is_error"] = json!(true);
                }
                self.meta.pending.drain(..counts.1);
                self.meta.directives.drain(..counts.2);
                self.hub.notify(
                    &id,
                    Level::Warn,
                    "the model declined this request (refusal); nothing was added to the conversation",
                );
                self.save_meta();
                break;
            }

            // The request was answered: what was pending is now history.
            self.meta.pending_results.drain(..counts.0);
            self.meta.pending.drain(..counts.1);
            self.meta.directives.drain(..counts.2);
            let mut content = Vec::new();
            let mut has_text_or_call = false;
            for block in &response.content {
                if block["type"] == "text" || block["type"] == "tool_use" {
                    has_text_or_call = true;
                }
                content.push(api::clean_block(block));
            }
            // An assistant message with nothing to say would make every later request
            // invalid; give it a placeholder.
            if !has_text_or_call {
                content.push(json!({ "type": "text", "text": "(no response)" }));
            }
            let mut exchange = tail;
            exchange.push(json!({ "role": "assistant", "content": content }));
            self.commit(exchange);

            let mut calls = Vec::new();
            for block in &response.content {
                if block["type"] == "tool_use" {
                    calls.push(block.clone());
                }
                if block["type"] == "fallback" {
                    let from = block["from"]["model"].as_str().unwrap_or("?");
                    let to = block["to"]["model"].as_str().unwrap_or("?");
                    self.hub.notify(
                        &id,
                        Level::Info,
                        &format!("{from} declined; {to} continued"),
                    );
                }
            }
            if response.stop_reason == StopReason::MaxTokens && calls.is_empty() {
                self.hub
                    .notify(&id, Level::Warn, "the response hit max_tokens");
            }
            if !calls.is_empty() {
                self.run_tools(calls).await;
                self.save_meta();
                if self.abort.is_some() {
                    self.after_abort();
                    return;
                }
            }
            // Otherwise loop: input that arrived mid-response gets its own request.
        }
        self.hub.emit(&id, EventKind::TurnEnd);
        if ended_normally {
            self.after_turn();
        }
    }

    fn after_abort(&mut self) {
        let id = self.meta.id.clone();
        match self.abort {
            Some(Abort::Pause) => {
                self.meta.resume_on_start = true;
                self.save_meta();
                self.hub.set_status(&id, Status::Paused);
            }
            Some(Abort::Stop) => self.hub.set_status(&id, Status::Stopped),
            _ => {}
        }
        self.hub.emit(&id, EventKind::TurnEnd);
    }

    fn after_turn(&mut self) {
        let id = self.meta.id.clone();
        // Subagents report back with their final words.
        if self.meta.role == Role::Sub
            && let Some(parent) = self.meta.parent.clone()
        {
            let text = last_assistant_text(&self.messages);
            let report = format!("Subagent `{id}` finished its turn:\n\n{text}");
            if self.hub.message(&id, &parent, &report).is_err() {
                self.hub
                    .notify(&id, Level::Warn, &format!("couldn't report to {parent}"));
            }
        }
        // Watchers start over before their context gets expensive to feed.
        if self.meta.role.is_watcher()
            && self.meta.usage.last_context > config::WATCHER_RESET_INPUT_TOKENS
        {
            self.accept(Input::Clear);
            self.abort = None;
            self.hub
                .notify(&id, Level::Info, "context reset (watcher context limit)");
        }
        if matches!(self.meta.role, Role::Main | Role::Sub) {
            infer::spawn_suggestions(
                self.hub.clone(),
                id,
                self.meta.model.clone(),
                self.meta.effort,
                self.meta.system.clone(),
                self.tools.clone(),
                self.messages.clone(),
            );
        }
    }

    fn record_usage(&mut self, usage: &api::Usage) {
        let u = &mut self.meta.usage;
        u.input += usage.input_tokens;
        u.output += usage.output_tokens;
        let read = usage.cache_read_input_tokens.unwrap_or(0);
        let write = usage.cache_creation_input_tokens.unwrap_or(0);
        u.cache_read += read;
        u.cache_write += write;
        u.last_context = usage.input_tokens + read + write;
    }

    /// One model request. Returns None if it was interrupted; input that isn't an
    /// interruption is accepted (queued) while the response streams.
    async fn infer(&mut self, tail: &[Value]) -> anyhow::Result<Option<api::Response>> {
        let body = {
            let mut messages: Vec<&Value> = Vec::with_capacity(self.messages.len() + tail.len());
            for m in &self.messages {
                messages.push(m);
            }
            for m in tail {
                messages.push(m);
            }
            api::Request {
                model: &self.meta.model,
                effort: self.meta.effort,
                max_tokens: config::MAX_TOKENS,
                system: &self.meta.system,
                tools: &self.tools,
                messages: &messages,
            }
            .body()
        };
        let hub = self.hub.clone();
        let id = self.meta.id.clone();
        let client = self.hub.client().clone();
        let on_update = move |update: Update<'_>| match update {
            Update::Text(t) => hub.emit(&id, EventKind::TextDelta(t.to_string())),
            Update::Progress(t) => hub.emit(&id, EventKind::ProgressDelta(t.to_string())),
            Update::BlockDone(b) => hub.emit(&id, EventKind::BlockDone(b.clone())),
        };
        let request = client.send(body, None, on_update);
        tokio::pin!(request);
        loop {
            tokio::select! {
                result = &mut request => return result.map(Some),
                input = self.rx.recv() => {
                    match input {
                        Some(input) => self.accept(input),
                        None => self.abort = Some(Abort::Closed),
                    }
                    if self.abort.is_some() {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// Runs the calls concurrently. An interrupt cancels them; every call still gets a
    /// result, so the conversation stays valid.
    async fn run_tools(&mut self, calls: Vec<Value>) {
        let id = self.meta.id.clone();
        self.hub.set_status(&id, Status::Tools(calls.len()));
        let cancel = CancellationToken::new();
        let mut results: Vec<Option<tools::Output>> = Vec::new();
        let mut set = JoinSet::new();
        for (i, call) in calls.iter().enumerate() {
            results.push(None);
            let name = call["name"].as_str().unwrap_or("").to_string();
            let tool_id = call["id"].as_str().unwrap_or("").to_string();
            self.hub.emit(&id, EventKind::ToolStart { id: tool_id });
            if let Some(raw) = call.get("_invalid_input") {
                let raw = raw.as_str().unwrap_or("");
                results[i] = Some(tools::Output {
                    content: format!(
                        "The tool input wasn't valid JSON (possibly cut off at max_tokens), so the call didn't run. Received: {}",
                        tools::truncate(raw, 300, 200)
                    ),
                    is_error: true,
                });
                continue;
            }
            let ctx = tools::Ctx {
                hub: self.hub.clone(),
                agent: id.clone(),
                cwd: self.meta.cwd.clone(),
                cancel: cancel.clone(),
            };
            let input = call["input"].clone();
            set.spawn(async move {
                let out = tokio::select! {
                    out = tools::run(&ctx, &name, &input) => out,
                    // Bash handles cancellation itself and reports partial output; this
                    // catches every other tool.
                    _ = async { ctx.cancel.cancelled().await; tokio::time::sleep(std::time::Duration::from_secs(4)).await } => tools::Output {
                        content: "interrupted before it finished".into(),
                        is_error: true,
                    },
                };
                (i, out)
            });
        }
        while !set.is_empty() {
            tokio::select! {
                joined = set.join_next() => {
                    if let Some(Ok((i, out))) = joined {
                        results[i] = Some(out);
                    }
                }
                input = self.rx.recv(), if self.abort.is_none() => {
                    match input {
                        Some(input) => self.accept(input),
                        None => self.abort = Some(Abort::Closed),
                    }
                    if self.abort.is_some() {
                        cancel.cancel();
                    }
                }
            }
        }
        // A /clear mid-run threw the conversation away; its results have nowhere to go.
        if self.abort == Some(Abort::Clear) {
            return;
        }
        for (i, call) in calls.iter().enumerate() {
            let tool_id = call["id"].as_str().unwrap_or("").to_string();
            let out = results[i].take().unwrap_or(tools::Output {
                content: "interrupted".into(),
                is_error: true,
            });
            let why = match self.abort {
                Some(Abort::Pause) => " (grove paused for an upgrade)",
                Some(Abort::Interrupt) | Some(Abort::Stop) => " (interrupted)",
                _ => "",
            };
            let content =
                if out.is_error && !why.is_empty() && out.content.starts_with("interrupted") {
                    format!("{}{why}", out.content)
                } else {
                    out.content
                };
            self.hub.emit(
                &id,
                EventKind::ToolDone {
                    id: tool_id.clone(),
                    content: content.clone(),
                    is_error: out.is_error,
                },
            );
            let mut result =
                json!({ "type": "tool_result", "tool_use_id": tool_id, "content": content });
            if out.is_error {
                result["is_error"] = json!(true);
            }
            self.meta.pending_results.push(result);
        }
    }
}

pub fn last_assistant_text(messages: &[Value]) -> String {
    for m in messages.iter().rev() {
        if m["role"] != "assistant" {
            continue;
        }
        let mut text = String::new();
        if let Some(blocks) = m["content"].as_array() {
            for b in blocks {
                if b["type"] == "text"
                    && let Some(t) = b["text"].as_str()
                {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(t);
                }
            }
        }
        if !text.is_empty() {
            return text;
        }
    }
    "(no text)".to_string()
}
