//! On-disk state. Each agent's conversation is an append-only JSONL file, which is what
//! lets grove stop mid-task, replace its own binary, and pick up where it left off.
//!
//! ~/.grove/
//!   history                         prompt history, one JSON string per line
//!   sessions/<session>/session.json
//!   sessions/<session>/agents/<agent>/meta.json
//!   sessions/<session>/agents/<agent>/messages.jsonl

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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

    /// The newest session started in `cwd`.
    pub fn latest_for(home: &Path, cwd: &Path) -> Option<String> {
        let mut best: Option<SessionInfo> = None;
        for entry in fs::read_dir(home.join("sessions")).ok()? {
            let Ok(entry) = entry else { continue };
            let Ok(text) = fs::read_to_string(entry.path().join("session.json")) else {
                continue;
            };
            let Ok(info) = serde_json::from_str::<SessionInfo>(&text) else {
                continue;
            };
            if info.cwd != cwd {
                continue;
            }
            if best.as_ref().is_none_or(|b| info.created > b.created) {
                best = Some(info);
            }
        }
        best.map(|b| b.id)
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
        let Ok(file) = File::open(&path) else {
            return Ok(Vec::new());
        };
        let mut messages = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            // A torn last line (crash mid-write) is dropped rather than failing the load.
            match serde_json::from_str(&line) {
                Ok(v) => messages.push(v),
                Err(_) => break,
            }
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

    pub fn history_path(&self) -> PathBuf {
        self.home.join("history")
    }
}

pub struct MessageLog {
    path: PathBuf,
    file: File,
}

impl MessageLog {
    pub fn append(&mut self, message: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
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
