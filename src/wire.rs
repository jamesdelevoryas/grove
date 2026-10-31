//! The protocol between grove's UI (or CLI) and the grove server: one JSON object per line
//! over a unix socket at ~/.grove/server.sock.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::{AgentId, Effort, Event, Input};
use crate::store::SessionInfo;

pub const SOCKET: &str = "server.sock";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Also subscribes the connection to session-list updates.
    Hello,
    Create {
        cwd: PathBuf,
        prompt: Option<String>,
        model: Option<String>,
        effort: Option<Effort>,
        /// Attach this connection to the new session.
        attach: bool,
    },
    /// Loads the session if it isn't running, then streams its events (history first).
    Attach {
        session: String,
    },
    Detach,
    Input {
        session: String,
        agent: AgentId,
        input: Input,
    },
    Spawn {
        session: String,
        parent: AgentId,
        name: String,
        task: String,
    },
    /// Rebuild grove and restart the server into it. `session` gets the progress notes.
    Upgrade {
        session: Option<String>,
    },
    /// Stop a session's agents and unload it (it stays on disk, resumable).
    Stop {
        session: String,
    },
    /// Stop and delete a session.
    Delete {
        session: String,
    },
    /// Stop every session (resumable) and exit the server.
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub cwd: PathBuf,
    pub created: chrono::DateTime<chrono::Local>,
    pub title: Option<String>,
    /// Loaded in the server, agents running or ready.
    pub live: bool,
    /// Some agent is thinking or running tools.
    pub busy: bool,
    pub agents: usize,
    pub cost: f64,
    pub updated: chrono::DateTime<chrono::Local>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Reply {
    /// `build` identifies the server's binary (path and mtime), so a UI can tell when
    /// it's talking to a different build.
    Hello {
        build: String,
        pid: u32,
    },
    Sessions {
        list: Vec<SessionSummary>,
    },
    Created {
        session: String,
    },
    Attached {
        info: SessionInfo,
    },
    Event {
        session: String,
        event: Event,
    },
    Proposal {
        summary: Option<String>,
    },
    Error {
        message: String,
    },
    Info {
        message: String,
    },
    /// The server is about to exec this binary. UIs should exec it too and reattach.
    Restarting {
        binary: PathBuf,
    },
}

/// Identifies the running binary: its path and modification time.
pub fn build_id() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "unknown".into();
    };
    let mtime = std::fs::metadata(&exe)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}@{mtime}", exe.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{EventKind, Status};

    #[test]
    fn messages_round_trip() {
        let r = Request::Input {
            session: "s".into(),
            agent: "main".into(),
            input: Input::User("hi".into()),
        };
        let line = serde_json::to_string(&r).unwrap();
        assert!(line.contains("\"op\":\"input\""), "{line}");
        let back: Request = serde_json::from_str(&line).unwrap();
        assert!(matches!(back, Request::Input { input: Input::User(t), .. } if t == "hi"));

        let e = Reply::Event {
            session: "s".into(),
            event: Event {
                agent: "main".into(),
                kind: EventKind::Status(Status::Tools(2)),
            },
        };
        let line = serde_json::to_string(&e).unwrap();
        let back: Reply = serde_json::from_str(&line).unwrap();
        assert!(
            matches!(back, Reply::Event { event, .. } if matches!(event.kind, EventKind::Status(Status::Tools(2))))
        );
    }
}
