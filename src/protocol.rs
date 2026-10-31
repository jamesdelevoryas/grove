//! The vocabulary shared by the hub, agents, watchers, and the UI.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type AgentId = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The agent the human starts with.
    Main,
    /// Spawned by another agent; reports back to its parent when a turn ends.
    Sub,
    /// Watches every agent and can interrupt or stop them.
    Supervisor,
    /// Reads the conversation, recalls and saves memory.
    Memory,
    /// Improves grove itself, then asks for an upgrade.
    Improver,
}

impl Role {
    pub fn is_watcher(self) -> bool {
        matches!(self, Role::Supervisor | Role::Memory | Role::Improver)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const ALL: [Effort; 5] = [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Effort> {
        let s = s.trim().to_lowercase();
        Effort::ALL.into_iter().find(|e| e.as_str() == s)
    }

    pub fn next(self) -> Effort {
        let i = Effort::ALL.iter().position(|e| *e == self).unwrap_or(0);
        Effort::ALL[(i + 1) % Effort::ALL.len()]
    }
}

/// Everything about an agent that survives a restart, besides its messages.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentMeta {
    pub id: AgentId,
    pub name: String,
    pub role: Role,
    pub parent: Option<AgentId>,
    pub model: String,
    pub effort: Effort,
    pub cwd: PathBuf,
    /// Fixed at creation so the prompt cache survives restarts.
    pub system: String,
    pub created: chrono::DateTime<chrono::Local>,
    /// Set when the agent was paused mid-turn for an upgrade: it picks up again on start.
    #[serde(default)]
    pub resume_on_start: bool,
    #[serde(default)]
    pub usage: UsageTotals,
    #[serde(default)]
    pub stopped: bool,
    /// Input accepted but not yet answered (saved on pause so an upgrade loses nothing):
    /// tool results first, then text blocks, then operator notes.
    #[serde(default)]
    pub pending_results: Vec<Value>,
    #[serde(default)]
    pub pending: Vec<Value>,
    #[serde(default)]
    pub directives: Vec<String>,
    /// How many messages of history this meta accounts for (see Agent::reconcile).
    #[serde(default)]
    pub committed: usize,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The last request's whole prompt size, i.e. how full the context is.
    pub last_context: u64,
}

impl UsageTotals {
    /// Dollars at Opus 5.5 rates ($4 in, $20 out, $0.20 cache read, $5 cache write per MTok).
    pub fn cost(&self) -> f64 {
        (self.input as f64 * 4.0
            + self.output as f64 * 20.0
            + self.cache_read as f64 * 0.2
            + self.cache_write as f64 * 5.0)
            / 1_000_000.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Status {
    Idle,
    /// Waiting on the model.
    Thinking,
    /// Running this many tool calls.
    Tools(usize),
    Paused,
    Stopped,
    Error(String),
}

impl Status {
    pub fn busy(&self) -> bool {
        matches!(self, Status::Thinking | Status::Tools(_))
    }
}

/// What can be sent to an agent.
#[derive(Debug, Serialize, Deserialize)]
pub enum Input {
    /// A prompt from the human.
    User(String),
    /// A message from another agent. `wake` starts a turn if the agent is idle; otherwise
    /// the message waits for its next turn.
    Message {
        from: AgentId,
        text: String,
        wake: bool,
    },
    /// An operator note (from the supervisor or grove itself), sent as a `system` message.
    Directive {
        from: AgentId,
        text: String,
    },
    /// A batch of activity for a watcher to look at.
    Feed(String),
    /// Stop the current inference or tool calls; the agent goes idle.
    Interrupt,
    /// Like Interrupt, but the agent resumes its turn after grove restarts.
    Pause,
    Stop,
    SetEffort(Effort),
    SetModel(String),
    /// Start a fresh conversation.
    Clear,
    /// Try the turn again after an error (pending input is still there).
    Retry,
    /// Undo a Pause (an upgrade that didn't happen).
    Resume,
    /// End the agent's task (its session is being unloaded). Nothing is marked stopped.
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    Info,
    Warn,
    Alert,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub agent: AgentId,
    pub kind: EventKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EventKind {
    Spawned(AgentMeta),
    /// The saved conversation of an agent that was just restored.
    History(Arc<Vec<Value>>),
    Status(Status),
    Meta(AgentMeta),
    UserText(String),
    Injected {
        from: AgentId,
        text: String,
        directive: bool,
    },
    Feed(String),
    TextDelta(String),
    /// The model's between-tool-call progress notes (thinking display "updates").
    ProgressDelta(String),
    /// A finished assistant content block, as it will be sent back to the API.
    BlockDone(Value),
    ToolStart {
        id: String,
    },
    ToolDone {
        id: String,
        content: String,
        is_error: bool,
    },
    /// An interrupted response: whatever streamed so far was thrown away.
    ResponseDiscarded,
    TurnEnd,
    Cleared,
    /// Suggested next prompts, made by forking the conversation after a turn. The UI
    /// drops them once the agent starts working again.
    Suggestions(Vec<String>),
    /// Input that was queued but not yet answered when the agent was restored.
    Pending(Vec<Value>),
    Notice {
        level: Level,
        text: String,
    },
}

impl EventKind {
    /// Events the watcher agents care about. Streaming deltas are left out: watchers see
    /// the finished blocks instead.
    pub fn is_semantic(&self) -> bool {
        !matches!(
            self,
            EventKind::TextDelta(_)
                | EventKind::ProgressDelta(_)
                | EventKind::Status(_)
                | EventKind::Meta(_)
                | EventKind::History(_)
                | EventKind::Suggestions(_)
                | EventKind::Pending(_)
        )
    }
}

/// Requests from a session's hub to whoever runs it (the grove server).
#[derive(Debug)]
pub enum UiCommand {
    /// The improver has an upgrade ready (the human applies it with /upgrade).
    Proposal { summary: String },
}
