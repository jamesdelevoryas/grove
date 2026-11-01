//! The watcher agents and their feeds. A feed turns the hub's event stream into batched
//! `[activity]` digests sent to its watcher as ordinary input, so watching never blocks
//! the agents being watched: if a watcher is mid-turn, the batch is queued and rides along
//! with its next request.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::config;
use crate::hub::Hub;
use crate::mcp::Mcp;
use crate::protocol::*;

/// Memory tools the memory agent gets (the server may have more).
const MEMORY_TOOLS: &[&str] = &[
    "memory_index",
    "memory_read",
    "memory_search",
    "memory_list",
    "memory_recall",
    "memory_session",
    "memory_write",
];

/// Starts the watchers. `restored` holds watchers saved in a resumed session; the rest
/// are created fresh.
pub fn start(hub: &Hub, mut restored: Vec<(AgentMeta, Vec<Value>)>) {
    let cwd = hub.config().src.clone();
    let session_cwd = hub.store().session.cwd.clone();

    if hub.config().supervisor {
        let id = match take_role(&mut restored, Role::Supervisor) {
            Some((meta, history)) => hub.start(meta, history, hub.tools_for(Role::Supervisor)),
            None => {
                let meta = hub.new_meta(
                    Role::Supervisor,
                    "supervisor",
                    None,
                    config::SUPERVISOR_EFFORT,
                    &session_cwd,
                    "",
                );
                hub.start(meta, Vec::new(), hub.tools_for(Role::Supervisor))
            }
        };
        tokio::spawn(supervisor_feed(hub.clone(), id, hub.observe()));
    }

    if hub.config().improver {
        let id = match take_role(&mut restored, Role::Improver) {
            Some((meta, history)) => hub.start(meta, history, hub.tools_for(Role::Improver)),
            None => {
                let meta = hub.new_meta(
                    Role::Improver,
                    "improver",
                    None,
                    config::IMPROVER_EFFORT,
                    &cwd,
                    "",
                );
                hub.start(meta, Vec::new(), hub.tools_for(Role::Improver))
            }
        };
        if hub.config().improver_watch {
            tokio::spawn(improver_feed(hub.clone(), id, hub.observe()));
        }
    }

    // The memory agent needs a memory server to talk to; none is configured by default.
    if hub.config().memory
        && let Some(url) = hub.config().memory_url.clone()
    {
        let saved = take_role(&mut restored, Role::Memory);
        let hub = hub.clone();
        // Connecting takes a moment; the memory agent starts once its tools exist.
        tokio::spawn(async move {
            let mcp = match connect_memory(&url).await {
                Ok(mcp) => Arc::new(mcp),
                Err(e) => {
                    let main = hub.find_role(Role::Main).unwrap_or_else(|| "main".into());
                    hub.notify(&main, Level::Warn, &format!("memory agent off: {e:#}"));
                    return;
                }
            };
            let instructions = mcp.instructions().unwrap_or_default();
            hub.set_memory(mcp);
            let id = match saved {
                Some((meta, history)) => hub.start(meta, history, hub.tools_for(Role::Memory)),
                None => {
                    let extra = format!("# The memory server's instructions\n\n{instructions}");
                    let meta = hub.new_meta(
                        Role::Memory,
                        "memory",
                        None,
                        config::MEMORY_EFFORT,
                        &session_cwd,
                        &extra,
                    );
                    hub.start(meta, Vec::new(), hub.tools_for(Role::Memory))
                }
            };
            memory_feed(hub.clone(), id, hub.observe()).await;
        });
    }
}

fn take_role(
    restored: &mut Vec<(AgentMeta, Vec<Value>)>,
    role: Role,
) -> Option<(AgentMeta, Vec<Value>)> {
    let i = restored.iter().position(|(m, _)| m.role == role)?;
    Some(restored.remove(i))
}

async fn connect_memory(url: &str) -> anyhow::Result<Mcp> {
    // The bearer token: keychain memory-mcp-token (account "grove", then "claude-code"),
    // else MEMORY_TOKEN.
    let token = match config::keychain("memory-mcp-token", Some("grove")) {
        Some(t) => t,
        None => match config::keychain("memory-mcp-token", Some("claude-code")) {
            Some(t) => t,
            None => match std::env::var("MEMORY_TOKEN") {
                Ok(t) => t,
                Err(_) => {
                    anyhow::bail!("no memory token (keychain memory-mcp-token or MEMORY_TOKEN)")
                }
            },
        },
    };
    let connect = Mcp::connect("memory", url, &token, MEMORY_TOOLS);
    match tokio::time::timeout(Duration::from_secs(15), connect).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!("timed out connecting to {url}"),
    }
}

// ---- digests ----

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.replace('\n', " ⏎ ");
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out.replace('\n', " ⏎ ")
}

/// One line describing an event, or None if the watcher shouldn't see it.
fn digest(event: &Event, detail: usize) -> Option<String> {
    let a = &event.agent;
    let line = match &event.kind {
        EventKind::UserText(text) => format!("[{a}] human: {}", clip(text, detail * 2)),
        EventKind::Injected {
            from,
            text,
            directive,
        } => {
            let kind = if *directive {
                "operator note"
            } else {
                "message"
            };
            format!("[{a}] {kind} from {from}: {}", clip(text, detail))
        }
        EventKind::BlockDone(block) => match block["type"].as_str() {
            Some("text") => format!(
                "[{a}] says: {}",
                clip(block["text"].as_str().unwrap_or(""), detail)
            ),
            Some("tool_use") => format!(
                "[{a}] calls {}: {}",
                block["name"].as_str().unwrap_or("?"),
                clip(&block["input"].to_string(), detail * 2)
            ),
            _ => return None,
        },
        EventKind::ToolDone {
            content, is_error, ..
        } => {
            let tag = if *is_error {
                "tool error"
            } else {
                "tool result"
            };
            format!("[{a}] {tag}: {}", clip(content, detail / 2))
        }
        EventKind::Notice { level, text } => {
            format!("[{a}] notice ({level:?}): {}", clip(text, detail))
        }
        EventKind::Spawned(meta) => format!(
            "[{a}] started ({:?}{})",
            meta.role,
            meta.parent
                .as_ref()
                .map(|p| format!(", by {p}"))
                .unwrap_or_default()
        ),
        EventKind::Cleared => format!("[{a}] conversation cleared"),
        _ => return None,
    };
    Some(line)
}

fn batch(lines: &mut Vec<String>) -> String {
    let mut text = String::from("[activity]\n");
    for line in lines.drain(..) {
        text.push_str(&line);
        text.push('\n');
    }
    text
}

/// Watches every other agent. Human prompts and tool calls are flushed within a quarter
/// second; everything else within a few seconds.
async fn supervisor_feed(hub: Hub, me: AgentId, mut rx: mpsc::UnboundedReceiver<Arc<Event>>) {
    let mut lines: Vec<String> = Vec::new();
    let mut deadline: Option<Instant> = None;
    loop {
        let due = deadline;
        let sleep = async move {
            match due {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { return };
                if event.agent == me {
                    continue;
                }
                // Other watchers' actions are watched, but not their notices: two watchers
                // reacting to each other's notices would wake each other forever.
                if matches!(event.kind, EventKind::Notice { .. }) && hub.role_of(&event.agent).is_some_and(Role::is_watcher) {
                    continue;
                }
                let Some(line) = digest(&event, 600) else { continue };
                lines.push(line);
                let urgent = matches!(&event.kind, EventKind::UserText(_))
                    || matches!(&event.kind, EventKind::BlockDone(b) if b["type"] == "tool_use");
                let wait = if urgent { config::SUPERVISOR_FAST_FLUSH_MS } else { config::SUPERVISOR_SLOW_FLUSH_MS };
                let due = Instant::now() + Duration::from_millis(wait);
                deadline = Some(match deadline {
                    Some(d) if d < due => d,
                    _ => due,
                });
            }
            _ = sleep => {
                deadline = None;
                if !lines.is_empty() {
                    let _ = hub.send(&me, Input::Feed(batch(&mut lines)));
                }
            }
        }
    }
}

/// Follows the human-facing conversation (main and subagents) and wakes the memory agent
/// shortly after each human prompt.
async fn memory_feed(hub: Hub, me: AgentId, mut rx: mpsc::UnboundedReceiver<Arc<Event>>) {
    let mut lines: Vec<String> = Vec::new();
    let mut deadline: Option<Instant> = None;
    loop {
        let due = deadline;
        let sleep = async move {
            match due {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { return };
                if !matches!(hub.role_of(&event.agent), Some(Role::Main) | Some(Role::Sub)) {
                    continue;
                }
                let relevant = match &event.kind {
                    EventKind::UserText(_) => true,
                    EventKind::BlockDone(b) => b["type"] == "text",
                    _ => false,
                };
                if !relevant {
                    continue;
                }
                let Some(line) = digest(&event, 800) else { continue };
                lines.push(line);
                if lines.len() > 40 {
                    lines.remove(0);
                }
                if matches!(&event.kind, EventKind::UserText(_)) && deadline.is_none() {
                    deadline = Some(Instant::now() + Duration::from_millis(config::MEMORY_FLUSH_MS));
                }
            }
            _ = sleep => {
                deadline = None;
                if !lines.is_empty() {
                    let _ = hub.send(&me, Input::Feed(batch(&mut lines)));
                }
            }
        }
    }
}

/// Collects friction: human prompts to main, errors, warnings. Wakes the improver in
/// batches, since it's the costliest watcher.
async fn improver_feed(hub: Hub, me: AgentId, mut rx: mpsc::UnboundedReceiver<Arc<Event>>) {
    let mut lines: Vec<String> = Vec::new();
    let mut friction = 0usize;
    let idle = Duration::from_secs(config::IMPROVER_IDLE_FLUSH_SECS);
    let mut last_event = Instant::now();
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { return };
                let role = hub.role_of(&event.agent);
                // Friction in the human's work, not in other watchers.
                if event.agent == me || role.is_some_and(Role::is_watcher) {
                    continue;
                }
                let (keep, rough) = match &event.kind {
                    EventKind::UserText(_) => (role == Some(Role::Main), false),
                    EventKind::ToolDone { is_error, .. } => (*is_error, true),
                    EventKind::Notice { level, .. } => (*level != Level::Info, true),
                    _ => (false, false),
                };
                if !keep {
                    continue;
                }
                let Some(line) = digest(&event, 500) else { continue };
                lines.push(line);
                if rough {
                    friction += 1;
                }
                last_event = Instant::now();
                if lines.len() >= config::IMPROVER_BATCH_EVENTS {
                    let _ = hub.send(&me, Input::Feed(batch(&mut lines)));
                    friction = 0;
                }
            }
            _ = tokio::time::sleep_until(last_event + idle), if friction > 0 => {
                let _ = hub.send(&me, Input::Feed(batch(&mut lines)));
                friction = 0;
            }
        }
    }
}
