//! On-disk state. Each agent's conversation is an append-only JSONL file, which is what
//! lets grove stop mid-task, replace its own binary, and pick up where it left off.
//!
//! ~/.grove/
//!   history                         prompt history, one JSON string per line
//!   sessions/<session>/session.json
//!   sessions/<session>/agents/<agent>/meta.json
//!   sessions/<session>/agents/<agent>/messages.jsonl

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::{AgentId, AgentMeta};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: PathBuf,
    pub created: chrono::DateTime<chrono::Local>,
    /// The first prompt, as a name for the session list.
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Clone)]
pub struct Store {
    pub home: PathBuf,
    pub session: SessionInfo,
}

impl Store {
    pub fn create(home: &Path, cwd: &Path) -> Result<Self> {
        let now = chrono::Local::now();
        let id = format!(
            "{}-{:04x}",
            now.format("%Y%m%d-%H%M%S"),
            std::process::id() & 0xffff
        );
        let session = SessionInfo {
            id,
            cwd: cwd.to_path_buf(),
            created: now,
            title: None,
        };
        let store = Self {
            home: home.to_path_buf(),
            session,
        };
        fs::create_dir_all(store.session_dir().join("agents"))?;
        write_json(&store.session_dir().join("session.json"), &store.session)?;
        Ok(store)
    }

    pub fn open(home: &Path, id: &str) -> Result<Self> {
        let path = home.join("sessions").join(id).join("session.json");
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let session: SessionInfo = serde_json::from_str(&text)?;
        Ok(Self {
            home: home.to_path_buf(),
            session,
        })
    }

    /// Every session on disk, newest first.
    pub fn list_all(home: &Path) -> Vec<SessionInfo> {
        let mut out = Vec::new();
        let Ok(dir) = fs::read_dir(home.join("sessions")) else {
            return out;
        };
        for entry in dir {
            let Ok(entry) = entry else { continue };
            let Ok(text) = fs::read_to_string(entry.path().join("session.json")) else {
                continue;
            };
            if let Ok(info) = serde_json::from_str::<SessionInfo>(&text) {
                out.push(info);
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.created));
        out
    }

    /// The newest session started in `cwd`.
    pub fn latest_for(home: &Path, cwd: &Path) -> Option<String> {
        for info in Self::list_all(home) {
            if info.cwd == cwd {
                return Some(info.id);
            }
        }
        None
    }

    pub fn save_info(&self) -> Result<()> {
        write_json(&self.session_dir().join("session.json"), &self.session)
    }

    /// Deletes a session's directory.
    pub fn delete(home: &Path, id: &str) -> Result<()> {
        if id.is_empty() || id.contains('/') || id.contains("..") {
            anyhow::bail!("bad session id {id:?}");
        }
        let dir = home.join("sessions").join(id);
        fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", dir.display()))
    }

    pub fn session_dir(&self) -> PathBuf {
        self.home.join("sessions").join(&self.session.id)
    }

    fn agent_dir(&self, id: &str) -> PathBuf {
        self.session_dir().join("agents").join(id)
    }

    pub fn save_meta(&self, meta: &AgentMeta) -> Result<()> {
        let dir = self.agent_dir(&meta.id);
        fs::create_dir_all(&dir)?;
        write_json(&dir.join("meta.json"), meta)
    }

    /// Every agent saved in this session, oldest first, with its conversation.
    pub fn load_agents(&self) -> Result<Vec<(AgentMeta, Vec<Value>)>> {
        let mut out = Vec::new();
        let dir = self.session_dir().join("agents");
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let meta_path = entry.path().join("meta.json");
            let Ok(text) = fs::read_to_string(&meta_path) else {
                continue;
            };
            let meta: AgentMeta = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", meta_path.display()))?;
            let messages = self.load_messages(&meta.id)?;
            out.push((meta, messages));
        }
        out.sort_by_key(|(m, _)| m.created);
        Ok(out)
    }

    fn load_messages(&self, id: &AgentId) -> Result<Vec<Value>> {
        let path = self.agent_dir(id).join("messages.jsonl");
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };
        let (messages, intact) = parse_log(&text);
        if !intact {
            // Rewrite without the damaged tail, so later appends start on a clean line.
            let mut clean = Vec::new();
            for m in &messages {
                clean.extend(serde_json::to_vec(m)?);
                clean.push(b'\n');
            }
            let tmp = path.with_extension("jsonl.tmp");
            fs::write(&tmp, clean)?;
            fs::rename(&tmp, &path)?;
        }
        Ok(messages)
    }

    pub fn message_log(&self, id: &AgentId) -> Result<MessageLog> {
        let dir = self.agent_dir(id);
        fs::create_dir_all(&dir)?;
        let path = dir.join("messages.jsonl");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(MessageLog { path, file })
    }
}

pub struct MessageLog {
    path: PathBuf,
    file: File,
}

impl MessageLog {
    /// Appends messages with one write, so an exchange lands whole or (after a crash
    /// mid-write) as a torn tail that `parse_log` drops.
    pub fn append_all(&mut self, messages: &[Value]) -> Result<()> {
        let mut bytes = Vec::new();
        for m in messages {
            bytes.extend(serde_json::to_vec(m)?);
            bytes.push(b'\n');
        }
        self.file.write_all(&bytes)?;
        self.file.flush()?;
        Ok(())
    }

    /// Moves the log aside for a fresh conversation (kept, not deleted).
    pub fn rotate(&mut self) -> Result<()> {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let aside = self.path.with_file_name(format!("messages.{stamp}.jsonl"));
        fs::rename(&self.path, aside)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        Ok(())
    }
}

/// Write-then-rename, so a crash never leaves half a file.
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// The longest valid history in a log: complete, parseable lines, ending with an assistant
/// message (every exchange is written as user [system] assistant, so anything after the
/// last assistant message is an exchange cut off mid-write). Returns whether the log was
/// intact.
fn parse_log(text: &str) -> (Vec<Value>, bool) {
    let mut messages = Vec::new();
    // Every record ends with a newline; anything after the last one was cut off.
    let complete = match text.rfind('\n') {
        Some(i) => &text[..i + 1],
        None => "",
    };
    let mut intact = complete.len() == text.len();
    for line in complete.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => messages.push(v),
            Err(_) => {
                intact = false;
                break;
            }
        }
    }
    while let Some(last) = messages.last() {
        if last["role"] == "assistant" {
            break;
        }
        messages.pop();
        intact = false;
    }
    (messages, intact)
}

pub fn load_history(path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        if let Ok(s) = serde_json::from_str::<String>(line) {
            out.push(s);
        }
    }
    out
}

pub fn append_history(path: &Path, entry: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_recovery_keeps_whole_exchanges() {
        let u = r#"{"role":"user","content":[]}"#;
        let a = r#"{"role":"assistant","content":[]}"#;
        let s = r#"{"role":"system","content":"x"}"#;
        let good = format!("{u}\n{a}\n");
        assert_eq!(
            parse_log(&good),
            (
                vec![
                    serde_json::from_str(u).unwrap(),
                    serde_json::from_str(a).unwrap()
                ],
                true
            )
        );
        // Torn final record.
        let (m, intact) = parse_log(&format!("{good}{u}\n{s}\n{{\"role\":\"assi"));
        assert_eq!(m.len(), 2);
        assert!(!intact);
        // A whole exchange minus its assistant message.
        let (m, intact) = parse_log(&format!("{good}{u}\n"));
        assert_eq!(m.len(), 2);
        assert!(!intact);
        assert_eq!(parse_log(""), (vec![], true));
    }
}
