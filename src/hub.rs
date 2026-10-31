//! The hub: every agent's mailbox, the event fan-out to the UI and the watchers, and the
//! operations that span agents (spawning, messaging, pausing everyone for an upgrade).

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::Agent;
use crate::api;
use crate::config::Config;
use crate::mcp::Mcp;
use crate::prompts;
use crate::protocol::*;
use crate::store::Store;

#[derive(Clone)]
pub struct Hub(Arc<Inner>);

struct Inner {
    agents: Mutex<Vec<Handle>>,
    ui: mpsc::UnboundedSender<Event>,
    ui_cmd: mpsc::UnboundedSender<UiCommand>,
    observers: Mutex<Vec<mpsc::UnboundedSender<Arc<Event>>>>,
    client: api::Client,
    store: Store,
    config: Config,
    memory: RwLock<Option<Arc<Mcp>>>,
    upgrade: Mutex<UpgradeState>,
}

struct Handle {
    meta: AgentMeta,
    tx: mpsc::UnboundedSender<Input>,
    status: Status,
}

#[derive(Default)]
struct UpgradeState {
    proposal: Option<String>,
    running: bool,
}

impl Hub {
    pub fn new(
        config: Config,
        store: Store,
        client: api::Client,
        ui: mpsc::UnboundedSender<Event>,
        ui_cmd: mpsc::UnboundedSender<UiCommand>,
    ) -> Self {
        Hub(Arc::new(Inner {
            agents: Mutex::new(Vec::new()),
            ui,
            ui_cmd,
            observers: Mutex::new(Vec::new()),
            client,
            store,
            config,
            memory: RwLock::new(None),
            upgrade: Mutex::new(UpgradeState::default()),
        }))
    }

    pub fn config(&self) -> &Config {
        &self.0.config
    }

    pub fn store(&self) -> &Store {
        &self.0.store
    }

    pub fn client(&self) -> &api::Client {
        &self.0.client
    }

    // ---- events ----

    pub fn emit(&self, agent: &str, kind: EventKind) {
        let event = Event {
            agent: agent.to_string(),
            kind,
        };
        if event.kind.is_semantic() {
            let shared = Arc::new(event.clone());
            let mut observers = self.0.observers.lock().unwrap();
            observers.retain(|tx| tx.send(shared.clone()).is_ok());
        }
        let _ = self.0.ui.send(event);
    }

    /// A feed of semantic events (no streaming deltas), for watchers.
    pub fn observe(&self) -> mpsc::UnboundedReceiver<Arc<Event>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.0.observers.lock().unwrap().push(tx);
        rx
    }

    pub fn notify(&self, agent: &str, level: Level, text: &str) {
        self.emit(
            agent,
            EventKind::Notice {
                level,
                text: text.to_string(),
            },
        );
    }

    // ---- agents ----

    pub fn set_status(&self, id: &str, status: Status) {
        {
            let mut agents = self.0.agents.lock().unwrap();
            for h in agents.iter_mut() {
                if h.meta.id == id {
                    h.status = status.clone();
                }
            }
        }
        self.emit(id, EventKind::Status(status));
    }

    pub fn update_meta(&self, meta: &AgentMeta) {
        {
            let mut agents = self.0.agents.lock().unwrap();
            for h in agents.iter_mut() {
                if h.meta.id == meta.id {
                    h.meta = meta.clone();
                }
            }
        }
        if let Err(e) = self.0.store.save_meta(meta) {
            self.notify(
                &meta.id,
                Level::Alert,
                &format!("can't save agent state: {e:#}"),
            );
        }
        self.emit(&meta.id, EventKind::Meta(meta.clone()));
    }

    pub fn role_of(&self, id: &str) -> Option<Role> {
        let agents = self.0.agents.lock().unwrap();
        for h in agents.iter() {
            if h.meta.id == id {
                return Some(h.meta.role);
            }
        }
        None
    }

    pub fn find_role(&self, role: Role) -> Option<AgentId> {
        let agents = self.0.agents.lock().unwrap();
        for h in agents.iter() {
            if h.meta.role == role {
                return Some(h.meta.id.clone());
            }
        }
        None
    }

    pub fn any_busy(&self) -> bool {
        let agents = self.0.agents.lock().unwrap();
        for h in agents.iter() {
            if h.status.busy() {
                return true;
            }
        }
        false
    }

    fn all_paused(&self, ids: &[AgentId]) -> bool {
        let agents = self.0.agents.lock().unwrap();
        for h in agents.iter() {
            if ids.contains(&h.meta.id) && h.status != Status::Paused {
                return false;
            }
        }
        true
    }

    pub fn send(&self, to: &str, input: Input) -> Result<()> {
        let agents = self.0.agents.lock().unwrap();
        for h in agents.iter() {
            if h.meta.id == to {
                if h.tx.send(input).is_err() {
                    bail!("agent {to} has shut down");
                }
                return Ok(());
            }
        }
        bail!("no agent named {to}; see list_agents")
    }

    /// A message between agents. Watchers' messages never wake an idle agent: they're
    /// context for its next turn, not a reason to start one.
    pub fn message(&self, from: &str, to: &str, text: &str) -> Result<()> {
        let wake = !self.role_of(from).is_some_and(Role::is_watcher);
        self.send(
            to,
            Input::Message {
                from: from.to_string(),
                text: text.to_string(),
                wake,
            },
        )
    }

    pub fn describe_agents(&self) -> String {
        let agents = self.0.agents.lock().unwrap();
        let mut out = String::new();
        for h in agents.iter() {
            let status = match &h.status {
                Status::Idle => "idle".to_string(),
                Status::Thinking => "thinking".to_string(),
                Status::Tools(n) => format!("running {n} tool call(s)"),
                Status::Paused => "paused".to_string(),
                Status::Stopped => "stopped".to_string(),
                Status::Error(e) => format!("error: {e}"),
            };
            let parent = match &h.meta.parent {
                Some(p) => format!(", parent {p}"),
                None => String::new(),
            };
            out.push_str(&format!(
                "- {} ({:?}{parent}): {status}\n",
                h.meta.id, h.meta.role
            ));
        }
        out
    }

    fn unique_id(&self, base: &str) -> AgentId {
        let mut slug = String::new();
        for c in base.chars() {
            if c.is_ascii_alphanumeric() {
                slug.push(c.to_ascii_lowercase());
            } else if !slug.ends_with('-') {
                slug.push('-');
            }
        }
        let slug = slug.trim_matches('-').chars().take(24).collect::<String>();
        let slug = if slug.is_empty() {
            "agent".to_string()
        } else {
            slug
        };
        let agents = self.0.agents.lock().unwrap();
        let taken = |id: &str| agents.iter().any(|h| h.meta.id == id);
        if !taken(&slug) {
            return slug;
        }
        let mut n = 2;
        loop {
            let id = format!("{slug}-{n}");
            if !taken(&id) {
                return id;
            }
            n += 1;
        }
    }

    pub fn new_meta(
        &self,
        role: Role,
        name: &str,
        parent: Option<AgentId>,
        effort: Effort,
        cwd: &Path,
        extra_system: &str,
    ) -> AgentMeta {
        let id = self.unique_id(name);
        let system = prompts::system(role, &id, cwd, &self.0.config, extra_system);
        AgentMeta {
            id,
            name: name.to_string(),
            role,
            parent,
            model: self.0.config.model.clone(),
            effort,
            cwd: cwd.to_path_buf(),
            system,
            created: chrono::Local::now(),
            resume_on_start: false,
            usage: UsageTotals::default(),
            stopped: false,
            pending_results: Vec::new(),
            pending: Vec::new(),
            directives: Vec::new(),
            committed: 0,
        }
    }

    /// Starts an agent's task. `history` is its saved conversation, if restored.
    pub fn start(&self, meta: AgentMeta, history: Vec<Value>, tools: Vec<Value>) -> AgentId {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = meta.id.clone();
        let log = match self.0.store.message_log(&id) {
            Ok(log) => log,
            Err(e) => {
                self.emit(&id, EventKind::Spawned(meta.clone()));
                self.notify(
                    &id,
                    Level::Alert,
                    &format!("can't open the message log: {e:#}"),
                );
                return id;
            }
        };
        // The agent repairs its saved state first (e.g. answering tool calls a crash cut
        // off), so the UI is shown the repaired version.
        let shared_history = Arc::new(history.clone());
        let agent = Agent::new(self.clone(), meta, history, tools, log, rx);
        let meta = agent.meta().clone();
        if let Err(e) = self.0.store.save_meta(&meta) {
            self.notify(&id, Level::Alert, &format!("can't save agent state: {e:#}"));
        }
        self.0.agents.lock().unwrap().push(Handle {
            meta: meta.clone(),
            tx,
            status: Status::Idle,
        });
        self.emit(&id, EventKind::Spawned(meta.clone()));
        if !shared_history.is_empty() {
            self.emit(&id, EventKind::History(shared_history));
        }
        let mut pending = meta.pending_results.clone();
        pending.extend(meta.pending.iter().cloned());
        if !pending.is_empty() {
            self.emit(&id, EventKind::Pending(pending));
        }
        tokio::spawn(agent.run());
        id
    }

    pub fn tools_for(&self, role: Role) -> Vec<Value> {
        let mut extra = Vec::new();
        if role == Role::Memory
            && let Some(mcp) = self.0.memory.read().unwrap().as_ref()
        {
            extra = mcp.tools().to_vec();
        }
        crate::tools::definitions(role, &extra)
    }

    pub fn spawn_sub(
        &self,
        parent: &str,
        name: &str,
        task: &str,
        effort: Option<Effort>,
        cwd: &Path,
    ) -> Result<AgentId> {
        let effort = effort.unwrap_or(self.0.config.effort);
        let meta = self.new_meta(Role::Sub, name, Some(parent.to_string()), effort, cwd, "");
        let tools = self.tools_for(Role::Sub);
        let id = self.start(meta, Vec::new(), tools);
        self.send(
            &id,
            Input::Message {
                from: parent.to_string(),
                text: task.to_string(),
                wake: true,
            },
        )?;
        Ok(id)
    }

    // ---- memory server ----

    pub fn set_memory(&self, mcp: Arc<Mcp>) {
        *self.0.memory.write().unwrap() = Some(mcp);
    }

    pub fn mcp_for(&self, tool: &str) -> Option<Arc<Mcp>> {
        let guard = self.0.memory.read().unwrap();
        let mcp = guard.as_ref()?;
        if mcp.has(tool) {
            Some(mcp.clone())
        } else {
            None
        }
    }

    // ---- self-upgrade ----

    pub fn request_upgrade(&self, from: &str, summary: &str) {
        self.0.upgrade.lock().unwrap().proposal = Some(summary.to_string());
        self.notify(
            from,
            Level::Alert,
            &format!("upgrade ready: {summary} — run /upgrade to rebuild and restart"),
        );
    }

    pub fn upgrade_proposal(&self) -> Option<String> {
        self.0.upgrade.lock().unwrap().proposal.clone()
    }

    /// Builds grove from its source tree while everything keeps running; only once that
    /// succeeds are the agents paused (cancelling inference and tool calls, saving queued
    /// input) and the UI told to exec the new binary, which resumes them.
    pub fn upgrade(&self, reporter: &str) {
        {
            let mut state = self.0.upgrade.lock().unwrap();
            if state.running {
                self.notify(reporter, Level::Warn, "an upgrade is already running");
                return;
            }
            state.running = true;
        }
        let hub = self.clone();
        let reporter = reporter.to_string();
        tokio::spawn(async move {
            let result = hub.build_and_pause(&reporter).await;
            hub.0.upgrade.lock().unwrap().running = false;
            if let Err(e) = result {
                hub.notify(&reporter, Level::Alert, &format!("upgrade failed: {e:#}"));
            }
        });
    }

    async fn build_and_pause(&self, reporter: &str) -> Result<()> {
        let src = self.0.config.src.clone();
        self.notify(
            reporter,
            Level::Info,
            &format!("building grove in {} …", src.display()),
        );
        let out = tokio::process::Command::new("cargo")
            .args(["build", "--release", "--quiet"])
            .current_dir(&src)
            .output()
            .await?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = err
                .lines()
                .rev()
                .take(30)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            bail!("cargo build failed:\n{}", tail.join("\n"));
        }
        let binary = src.join("target/release/grove");
        if !binary.exists() {
            bail!("build succeeded but {} is missing", binary.display());
        }
        self.notify(
            reporter,
            Level::Info,
            "build ok — pausing agents to restart",
        );
        let ids: Vec<AgentId> = {
            let agents = self.0.agents.lock().unwrap();
            let mut ids = Vec::new();
            for h in agents.iter() {
                ids.push(h.meta.id.clone());
            }
            ids
        };
        for id in &ids {
            let _ = self.send(id, Input::Pause);
        }
        // Each agent saves its state, then reports Paused. Wait for every one of them.
        let mut all_paused = false;
        for _ in 0..200 {
            if self.all_paused(&ids) {
                all_paused = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if !all_paused {
            for id in &ids {
                let _ = self.send(id, Input::Resume);
            }
            bail!("agents didn't pause within 5s; resumed them, not restarting");
        }
        self.0.upgrade.lock().unwrap().proposal = None;
        let _ = self.0.ui_cmd.send(UiCommand::Exec {
            binary,
            session: self.0.store.session.id.clone(),
        });
        Ok(())
    }
}
