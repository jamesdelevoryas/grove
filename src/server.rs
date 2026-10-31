//! The grove server: one background process per user that owns every running session,
//! so sessions outlive the terminal. UIs and the CLI talk to it over a unix socket
//! (`wire.rs`). It is started on demand by the first `grove` and detaches from the
//! terminal.
//!
//! Each loaded session keeps a replay log of its UI events (streaming deltas are folded
//! away once their block finishes), so a UI that attaches later sees the whole transcript
//! and then the live stream.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc};

use crate::api;
use crate::config::{self, Config};
use crate::hub::Hub;
use crate::protocol::*;
use crate::store::{SessionInfo, Store};
use crate::watchers;
use crate::wire::{Reply, Request, SOCKET, SessionSummary, build_id};

const TITLE_CHARS: usize = 80;

pub struct Server {
    config: Config,
    client: api::Client,
    state: Mutex<State>,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    list_changed: Notify,
    upgrading: AtomicBool,
    build: String,
}

#[derive(Default)]
struct State {
    live: HashMap<String, Live>,
    conns: HashMap<u64, Conn>,
    next_conn: u64,
    proposal: Option<String>,
}

struct Conn {
    tx: mpsc::UnboundedSender<Arc<String>>,
    attached: Option<String>,
}

struct Live {
    hub: Hub,
    info: SessionInfo,
    /// Every event a newly attached UI needs, minus streaming deltas of finished blocks
    /// and the status/meta churn (sent as a final snapshot instead).
    replay: Vec<Event>,
    /// Deltas of blocks still streaming, per agent.
    streaming: HashMap<AgentId, Vec<Event>>,
    metas: HashMap<AgentId, AgentMeta>,
    order: Vec<AgentId>,
    statuses: HashMap<AgentId, Status>,
    suggestions: HashMap<AgentId, Vec<String>>,
    updated: chrono::DateTime<chrono::Local>,
}

impl Live {
    /// Records an event; returns true if the session list changed.
    fn record(&mut self, event: &Event) -> bool {
        self.updated = chrono::Local::now();
        let agent = &event.agent;
        match &event.kind {
            EventKind::TextDelta(_) | EventKind::ProgressDelta(_) => {
                self.streaming
                    .entry(agent.clone())
                    .or_default()
                    .push(event.clone());
                false
            }
            EventKind::BlockDone(_) | EventKind::ResponseDiscarded | EventKind::TurnEnd => {
                self.streaming.remove(agent);
                self.replay.push(event.clone());
                false
            }
            EventKind::Status(status) => {
                if *status == Status::Thinking {
                    self.suggestions.remove(agent);
                }
                self.statuses.insert(agent.clone(), status.clone());
                true
            }
            EventKind::Meta(meta) => {
                self.metas.insert(agent.clone(), meta.clone());
                true
            }
            EventKind::Spawned(meta) => {
                if !self.order.contains(agent) {
                    self.order.push(agent.clone());
                }
                self.metas.insert(agent.clone(), meta.clone());
                self.statuses.insert(agent.clone(), Status::Idle);
                self.replay.push(event.clone());
                true
            }
            EventKind::Suggestions(items) => {
                self.suggestions.insert(agent.clone(), items.clone());
                false
            }
            EventKind::Cleared => {
                self.replay
                    .retain(|e| &e.agent != agent || matches!(e.kind, EventKind::Spawned(_)));
                self.streaming.remove(agent);
                self.replay.push(event.clone());
                false
            }
            EventKind::UserText(text) => {
                self.replay.push(event.clone());
                let is_main = self.metas.get(agent).is_some_and(|m| m.role == Role::Main);
                if is_main && self.info.title.is_none() {
                    let first = text.lines().next().unwrap_or("").trim();
                    let mut title: String = first.chars().take(TITLE_CHARS).collect();
                    if first.chars().count() > TITLE_CHARS {
                        title.push('…');
                    }
                    self.info.title = Some(title);
                    let mut store = self.hub.store().clone();
                    store.session = self.info.clone();
                    let _ = store.save_info();
                    return true;
                }
                false
            }
            _ => {
                self.replay.push(event.clone());
                false
            }
        }
    }

    /// What a newly attached UI is sent, in order.
    fn snapshot(&self) -> Vec<Event> {
        let mut out = self.replay.clone();
        for id in &self.order {
            if let Some(meta) = self.metas.get(id) {
                out.push(Event {
                    agent: id.clone(),
                    kind: EventKind::Meta(meta.clone()),
                });
            }
            if let Some(status) = self.statuses.get(id) {
                out.push(Event {
                    agent: id.clone(),
                    kind: EventKind::Status(status.clone()),
                });
            }
            if let Some(items) = self.suggestions.get(id) {
                out.push(Event {
                    agent: id.clone(),
                    kind: EventKind::Suggestions(items.clone()),
                });
            }
            if let Some(deltas) = self.streaming.get(id) {
                out.extend(deltas.iter().cloned());
            }
        }
        out
    }

    fn summary(&self) -> SessionSummary {
        let mut busy = false;
        for s in self.statuses.values() {
            if s.busy() {
                busy = true;
            }
        }
        let mut cost = 0.0;
        for m in self.metas.values() {
            cost += m.usage.cost();
        }
        SessionSummary {
            id: self.info.id.clone(),
            cwd: self.info.cwd.clone(),
            created: self.info.created,
            title: self.info.title.clone(),
            live: true,
            busy,
            agents: self.metas.len(),
            cost,
            updated: self.updated,
        }
    }
}

fn line(reply: &Reply) -> Arc<String> {
    Arc::new(serde_json::to_string(reply).expect("reply serializes"))
}

/// Holds an exclusive lock on ~/.grove/server.lock for the life of the process (and is
/// released at exec), so two servers can never share a socket.
fn take_lock(home: &std::path::Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(home)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join("server.lock"))?;
    // Safety: flock on a file descriptor we own.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!("another grove server is already running");
    }
    Ok(file)
}

pub async fn run(config: Config, resume: Vec<String>) -> Result<()> {
    let _lock = take_lock(&config.home)?;
    let socket = config.home.join(SOCKET);
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    let _ = std::fs::write(
        config.home.join("server.pid"),
        std::process::id().to_string(),
    );
    eprintln!(
        "grove server {} listening on {}",
        std::process::id(),
        socket.display()
    );

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let server = Arc::new(Server {
        client: api::Client::new(config::api_key()),
        config,
        state: Mutex::new(State::default()),
        cmd_tx,
        list_changed: Notify::new(),
        upgrading: AtomicBool::new(false),
        build: build_id(),
    });
    for id in resume {
        if let Err(e) = server.load(&id) {
            eprintln!("couldn't resume session {id}: {e:#}");
        }
    }
    tokio::spawn(server.clone().commands(cmd_rx));
    tokio::spawn(server.clone().list_broadcaster());

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(server.clone().connection(stream));
    }
}

impl Server {
    fn send_to(&self, conn: u64, reply: &Reply) {
        let st = self.state.lock().unwrap();
        if let Some(c) = st.conns.get(&conn) {
            let _ = c.tx.send(line(reply));
        }
    }

    fn broadcast(&self, reply: &Reply) {
        let l = line(reply);
        let st = self.state.lock().unwrap();
        for c in st.conns.values() {
            let _ = c.tx.send(l.clone());
        }
    }

    /// Loads a session from disk and starts its agents (no-op if it's running).
    fn load(self: &Arc<Self>, id: &str) -> Result<()> {
        if self.state.lock().unwrap().live.contains_key(id) {
            return Ok(());
        }
        let store = Store::open(&self.config.home, id)?;
        self.start(store, None, None, None)?;
        Ok(())
    }

    fn create(
        self: &Arc<Self>,
        cwd: PathBuf,
        prompt: Option<String>,
        model: Option<String>,
        effort: Option<Effort>,
    ) -> Result<String> {
        let store = Store::create(&self.config.home, &cwd)?;
        self.start(store, prompt, model, effort)
    }

    fn start(
        self: &Arc<Self>,
        store: Store,
        prompt: Option<String>,
        model: Option<String>,
        effort: Option<Effort>,
    ) -> Result<String> {
        let mut config = self.config.clone();
        if let Some(m) = model {
            config.model = m;
        }
        if let Some(e) = effort {
            config.effort = e;
        }
        let id = store.session.id.clone();
        let saved = store.load_agents()?;
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let hub = Hub::new(
            config.clone(),
            store.clone(),
            self.client.clone(),
            ui_tx,
            self.cmd_tx.clone(),
        );
        {
            let mut st = self.state.lock().unwrap();
            st.live.insert(
                id.clone(),
                Live {
                    hub: hub.clone(),
                    info: store.session.clone(),
                    replay: Vec::new(),
                    streaming: HashMap::new(),
                    metas: HashMap::new(),
                    order: Vec::new(),
                    statuses: HashMap::new(),
                    suggestions: HashMap::new(),
                    updated: chrono::Local::now(),
                },
            );
        }
        tokio::spawn(self.clone().pump(id.clone(), ui_rx));

        let mut restored_watchers: Vec<(AgentMeta, Vec<Value>)> = Vec::new();
        for (meta, history) in saved {
            if meta.role.is_watcher() {
                restored_watchers.push((meta, history));
                continue;
            }
            let tools = hub.tools_for(meta.role);
            hub.start(meta, history, tools);
        }
        let main_id = match hub.find_role(Role::Main) {
            Some(main) => main,
            None => {
                let meta = hub.new_meta(
                    Role::Main,
                    "main",
                    None,
                    config.effort,
                    &store.session.cwd,
                    "",
                );
                let tools = hub.tools_for(Role::Main);
                hub.start(meta, Vec::new(), tools)
            }
        };
        watchers::start(&hub, restored_watchers);
        if !self.client.has_key() {
            hub.notify(
                &main_id,
                Level::Alert,
                "No Anthropic API key. Set ANTHROPIC_API_KEY, or store one in the keychain:\n  security add-generic-password -s grove-anthropic-api-key -a grove -w",
            );
        }
        if let Some(prompt) = prompt {
            hub.send(&main_id, Input::User(prompt))?;
        }
        self.list_changed.notify_one();
        Ok(id)
    }

    /// Records a session's events and forwards them to attached UIs.
    async fn pump(self: Arc<Self>, id: String, mut rx: mpsc::UnboundedReceiver<Event>) {
        while let Some(event) = rx.recv().await {
            let l = line(&Reply::Event {
                session: id.clone(),
                event: event.clone(),
            });
            let changed = {
                let mut st = self.state.lock().unwrap();
                let Some(live) = st.live.get_mut(&id) else {
                    break;
                };
                let changed = live.record(&event);
                for c in st.conns.values() {
                    if c.attached.as_deref() == Some(id.as_str()) {
                        let _ = c.tx.send(l.clone());
                    }
                }
                changed
            };
            if changed {
                self.list_changed.notify_one();
            }
        }
    }

    /// Unloads a session: interrupts its work and ends its tasks. It stays on disk.
    async fn stop(&self, id: &str) {
        let live = self.state.lock().unwrap().live.remove(id);
        if let Some(live) = live {
            live.hub.interrupt_all().await;
            live.hub.close();
        }
        let mut st = self.state.lock().unwrap();
        for c in st.conns.values_mut() {
            if c.attached.as_deref() == Some(id) {
                c.attached = None;
            }
        }
        drop(st);
        self.list_changed.notify_one();
    }

    fn summaries(&self) -> Vec<SessionSummary> {
        let infos = Store::list_all(&self.config.home);
        let st = self.state.lock().unwrap();
        let mut live = Vec::new();
        let mut rest = Vec::new();
        for info in infos {
            match st.live.get(&info.id) {
                Some(l) => live.push(l.summary()),
                None => rest.push(SessionSummary {
                    id: info.id,
                    cwd: info.cwd,
                    created: info.created,
                    title: info.title,
                    live: false,
                    busy: false,
                    agents: 0,
                    cost: 0.0,
                    updated: info.created,
                }),
            }
        }
        live.sort_by_key(|s| std::cmp::Reverse(s.updated));
        live.extend(rest);
        live
    }

    async fn list_broadcaster(self: Arc<Self>) {
        loop {
            self.list_changed.notified().await;
            // Coalesce bursts (status flips during a turn).
            tokio::time::sleep(Duration::from_millis(150)).await;
            let list = self.summaries();
            self.broadcast(&Reply::Sessions { list });
        }
    }

    async fn connection(self: Arc<Self>, stream: UnixStream) {
        let (rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Arc<String>>();
        let conn = {
            let mut st = self.state.lock().unwrap();
            st.next_conn += 1;
            let n = st.next_conn;
            st.conns.insert(n, Conn { tx, attached: None });
            n
        };
        tokio::spawn(async move {
            while let Some(l) = rx.recv().await {
                if wr.write_all(l.as_bytes()).await.is_err() || wr.write_all(b"\n").await.is_err() {
                    break;
                }
            }
        });
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            match serde_json::from_str::<Request>(&l) {
                Ok(req) => self.handle(conn, req).await,
                Err(e) => self.send_to(
                    conn,
                    &Reply::Error {
                        message: format!("bad request: {e}"),
                    },
                ),
            }
        }
        self.state.lock().unwrap().conns.remove(&conn);
    }

    fn attach(self: &Arc<Self>, conn: u64, session: &str) -> Result<()> {
        self.load(session)?;
        let mut st = self.state.lock().unwrap();
        let Some(live) = st.live.get(session) else {
            bail!("no session {session}")
        };
        let mut lines = vec![line(&Reply::Attached {
            info: live.info.clone(),
        })];
        for event in live.snapshot() {
            lines.push(line(&Reply::Event {
                session: session.to_string(),
                event,
            }));
        }
        // Under the lock, so no live event can slip in between the snapshot and the stream.
        let Some(c) = st.conns.get_mut(&conn) else {
            return Ok(());
        };
        c.attached = Some(session.to_string());
        for l in lines {
            let _ = c.tx.send(l);
        }
        Ok(())
    }

    fn hub(&self, session: &str) -> Option<Hub> {
        self.state
            .lock()
            .unwrap()
            .live
            .get(session)
            .map(|l| l.hub.clone())
    }

    async fn handle(self: &Arc<Self>, conn: u64, req: Request) {
        let result: Result<()> = match req {
            Request::Hello => {
                self.send_to(
                    conn,
                    &Reply::Hello {
                        build: self.build.clone(),
                        pid: std::process::id(),
                    },
                );
                self.send_to(
                    conn,
                    &Reply::Sessions {
                        list: self.summaries(),
                    },
                );
                let proposal = self.state.lock().unwrap().proposal.clone();
                self.send_to(conn, &Reply::Proposal { summary: proposal });
                Ok(())
            }
            Request::Create {
                cwd,
                prompt,
                model,
                effort,
                attach,
            } => match self.create(cwd, prompt, model, effort) {
                Ok(id) => {
                    self.send_to(
                        conn,
                        &Reply::Created {
                            session: id.clone(),
                        },
                    );
                    if attach {
                        self.attach(conn, &id)
                    } else {
                        Ok(())
                    }
                }
                Err(e) => Err(e),
            },
            Request::Attach { session } => self.attach(conn, &session),
            Request::Detach => {
                if let Some(c) = self.state.lock().unwrap().conns.get_mut(&conn) {
                    c.attached = None;
                }
                Ok(())
            }
            Request::Input {
                session,
                agent,
                input,
            } => {
                let loaded = self.load(&session);
                match (loaded, self.hub(&session)) {
                    (Ok(()), Some(hub)) => hub.send(&agent, input),
                    (Err(e), _) => Err(e),
                    (Ok(()), None) => Err(anyhow::anyhow!("no session {session}")),
                }
            }
            Request::Spawn {
                session,
                parent,
                name,
                task,
            } => match self.hub(&session) {
                Some(hub) => {
                    let cwd = hub.store().session.cwd.clone();
                    hub.spawn_sub(&parent, &name, &task, None, &cwd).map(|_| ())
                }
                None => Err(anyhow::anyhow!("session {session} isn't running")),
            },
            Request::Upgrade { session } => {
                tokio::spawn(self.clone().upgrade(session, None));
                Ok(())
            }
            Request::Stop { session } => {
                self.stop(&session).await;
                self.send_to(
                    conn,
                    &Reply::Info {
                        message: format!("stopped {session}"),
                    },
                );
                Ok(())
            }
            Request::Delete { session } => {
                self.stop(&session).await;
                let result = Store::delete(&self.config.home, &session);
                if result.is_ok() {
                    self.send_to(
                        conn,
                        &Reply::Info {
                            message: format!("deleted {session}"),
                        },
                    );
                }
                self.list_changed.notify_one();
                result
            }
            Request::Shutdown => {
                self.send_to(
                    conn,
                    &Reply::Info {
                        message: "shutting down".into(),
                    },
                );
                self.shutdown().await;
                Ok(())
            }
        };
        if let Err(e) = result {
            self.send_to(
                conn,
                &Reply::Error {
                    message: format!("{e:#}"),
                },
            );
        }
    }

    /// Pauses every session (mid-turn agents pick up again next time it's loaded) and
    /// exits.
    async fn shutdown(&self) {
        let hubs: Vec<Hub> = self
            .state
            .lock()
            .unwrap()
            .live
            .values()
            .map(|l| l.hub.clone())
            .collect();
        for hub in &hubs {
            hub.pause_all().await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = std::fs::remove_file(self.config.home.join(SOCKET));
        std::process::exit(0);
    }

    async fn commands(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<UiCommand>) {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                UiCommand::Proposal { summary } => {
                    self.state.lock().unwrap().proposal = Some(summary.clone());
                    self.broadcast(&Reply::Proposal {
                        summary: Some(summary),
                    });
                }
            }
        }
    }

    /// Progress notes for an upgrade: into the requesting agent's transcript if known,
    /// and to every UI.
    fn note(&self, session: &Option<String>, reporter: &Option<String>, level: Level, text: &str) {
        if let (Some(s), Some(r)) = (session, reporter)
            && let Some(hub) = self.hub(s)
        {
            hub.notify(r, level, text);
        }
        let reply = if level == Level::Info {
            Reply::Info {
                message: text.to_string(),
            }
        } else {
            Reply::Error {
                message: text.to_string(),
            }
        };
        self.broadcast(&reply);
    }

    /// Builds grove while everything keeps running; then pauses every session's agents,
    /// tells the UIs, and execs the new binary, which resumes the same sessions.
    async fn upgrade(self: Arc<Self>, session: Option<String>, reporter: Option<String>) {
        if self.upgrading.swap(true, Ordering::SeqCst) {
            self.note(
                &session,
                &reporter,
                Level::Warn,
                "an upgrade is already running",
            );
            return;
        }
        let result = self.build_and_restart(&session, &reporter).await;
        self.upgrading.store(false, Ordering::SeqCst);
        if let Err(e) = result {
            self.note(
                &session,
                &reporter,
                Level::Alert,
                &format!("upgrade failed: {e:#}"),
            );
        }
    }

    async fn build_and_restart(
        &self,
        session: &Option<String>,
        reporter: &Option<String>,
    ) -> Result<()> {
        let src = self.config.src.clone();
        self.note(
            session,
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
            let lines: Vec<&str> = err.lines().collect();
            let tail = lines[lines.len().saturating_sub(30)..].join("\n");
            bail!("cargo build failed:\n{tail}");
        }
        let binary = src.join("target/release/grove");
        if !binary.exists() {
            bail!("build succeeded but {} is missing", binary.display());
        }
        self.note(
            session,
            reporter,
            Level::Info,
            "build ok — pausing every session to restart",
        );
        let live: Vec<(String, Hub)> = {
            let st = self.state.lock().unwrap();
            let mut out = Vec::new();
            for (id, l) in st.live.iter() {
                out.push((id.clone(), l.hub.clone()));
            }
            out
        };
        let mut paused: Vec<Hub> = Vec::new();
        for (_, hub) in &live {
            if !hub.pause_all().await {
                for h in &paused {
                    h.resume_all();
                }
                bail!("agents didn't pause within 5s; resumed them, not restarting");
            }
            paused.push(hub.clone());
        }
        self.broadcast(&Reply::Restarting {
            binary: binary.clone(),
        });
        // Let the writers flush the notice before the sockets close at exec.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut args = vec!["server".to_string(), "--resume".to_string()];
        for (id, _) in &live {
            args.push(id.clone());
        }
        let err = std::process::Command::new(&binary).args(&args).exec();
        for h in &paused {
            h.resume_all();
        }
        bail!("exec {}: {err}", binary.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_folds_finished_streams() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let dir = std::env::temp_dir().join(format!("grove-srv-{}", std::process::id()));
        let store = Store::create(&dir, std::path::Path::new("/tmp")).unwrap();
        let hub = Hub::new(
            Config::load(),
            store.clone(),
            api::Client::new(None),
            ui_tx,
            cmd_tx,
        );
        let mut live = Live {
            hub,
            info: store.session.clone(),
            replay: Vec::new(),
            streaming: HashMap::new(),
            metas: HashMap::new(),
            order: Vec::new(),
            statuses: HashMap::new(),
            suggestions: HashMap::new(),
            updated: chrono::Local::now(),
        };
        let ev = |kind| Event {
            agent: "main".into(),
            kind,
        };
        let deltas = |snap: &[Event]| {
            snap.iter()
                .filter(|e| matches!(e.kind, EventKind::TextDelta(_)))
                .count()
        };
        let meta = live.hub.new_meta(
            Role::Main,
            "main",
            None,
            Effort::High,
            std::path::Path::new("/tmp"),
            "",
        );
        live.record(&ev(EventKind::Spawned(meta)));
        live.record(&ev(EventKind::TextDelta("He".into())));
        live.record(&ev(EventKind::TextDelta("llo".into())));
        assert_eq!(deltas(&live.snapshot()), 2, "in-flight deltas are replayed");
        live.record(&ev(EventKind::BlockDone(
            serde_json::json!({"type":"text","text":"Hello"}),
        )));
        live.record(&ev(EventKind::Status(Status::Idle)));
        let snap = live.snapshot();
        assert_eq!(
            deltas(&snap),
            0,
            "finished deltas fold into the block: {snap:?}"
        );
        assert!(
            snap.iter()
                .any(|e| matches!(e.kind, EventKind::BlockDone(_)))
        );
        // One status, the latest, rather than the history of flips.
        assert_eq!(
            snap.iter()
                .filter(|e| matches!(e.kind, EventKind::Status(_)))
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
