//! Tools. Every role gets a fixed, sorted tool list (stable for the prompt cache):
//! coding tools, messaging between agents, and a few role-specific powers.

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use crate::hub::Hub;
use crate::protocol::{AgentId, Effort, Input, Level, Role};

const MAX_OUTPUT_HEAD_BYTES: usize = 6_000;
const MAX_OUTPUT_TAIL_BYTES: usize = 24_000;
const BASH_PIPE_HEAD_BYTES: usize = 2_000;
const BASH_PIPE_TAIL_BYTES: usize = 12_000;
const TERM_GRACE: Duration = Duration::from_secs(2);
const PIPE_GRACE: Duration = Duration::from_secs(1);
const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;
const MAX_BASH_TIMEOUT_SECS: u64 = 600;
const READ_DEFAULT_LINES: usize = 2_000;

pub struct Ctx {
    pub hub: Hub,
    pub agent: AgentId,
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
}

pub struct Output {
    pub content: String,
    pub is_error: bool,
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": schema,
        // Large inputs (file contents) stream as they're generated. The server no longer
        // validates them, so `run` checks required fields itself.
        "eager_input_streaming": true,
    })
}

fn path_prop() -> Value {
    json!({ "type": "string", "description": "Absolute, or relative to the working directory." })
}

/// The tool list for a role, sorted by name.
pub fn definitions(role: Role, extra: &[Value]) -> Vec<Value> {
    let mut tools = Vec::new();
    let coding = matches!(role, Role::Main | Role::Sub | Role::Improver);
    if coding {
        tools.push(tool(
            "bash",
            "Run a command with `bash -c` in the working directory; returns exit status, stdout and stderr. No stdin. On timeout or interrupt the command is killed and you get its partial output. Background processes it starts are killed when it finishes unless they detach (setsid). Long output keeps its start and end.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer", "description": "Default 120, max 600." }
                },
                "required": ["command"]
            }),
        ));
        tools.push(tool(
            "write_file",
            "Create or overwrite a file, creating parent directories. Prefer edit_file for changes to existing files.",
            json!({
                "type": "object",
                "properties": { "path": path_prop(), "content": { "type": "string" } },
                "required": ["path", "content"]
            }),
        ));
        tools.push(tool(
            "edit_file",
            "Replace one exact occurrence of `old_string` with `new_string`. `old_string` must match exactly (whitespace included) and be unique; add surrounding lines to make it so. Set `replace_all` to replace every occurrence.",
            json!({
                "type": "object",
                "properties": {
                    "path": path_prop(),
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ));
        tools.push(tool(
            "spawn_agent",
            "Start a subagent that works in parallel on a self-contained task. It gets its own context, the same coding tools, and the same working directory. Returns at once with its id; when the subagent finishes a turn, its final message is delivered to you. Give it everything it needs in `task`: it can't see your conversation.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Short kebab-case name, e.g. `fix-tests`." },
                    "task": { "type": "string" },
                    "effort": { "type": "string", "enum": ["low", "medium", "high", "xhigh", "max"] }
                },
                "required": ["name", "task"]
            }),
        ));
    }
    tools.push(tool(
        "read_file",
        "Read a text file with line numbers. Use offset/limit (1-based lines) for large files.",
        json!({
            "type": "object",
            "properties": {
                "path": path_prop(),
                "offset": { "type": "integer" },
                "limit": { "type": "integer" }
            },
            "required": ["path"]
        }),
    ));
    tools.push(tool(
        "list_dir",
        "List a directory's entries, sorted; directories end with `/`.",
        json!({ "type": "object", "properties": { "path": path_prop() }, "required": ["path"] }),
    ));
    tools.push(tool(
        "list_agents",
        "List every agent in this grove session: id, role, status, and what it was asked to do.",
        json!({ "type": "object", "properties": {} }),
    ));
    tools.push(tool(
        "send_message",
        "Send a message to another agent by id (see list_agents). It arrives with that agent's next model request. Messages to an idle agent wake it, except messages from watchers (supervisor, memory), which wait for its next turn.",
        json!({
            "type": "object",
            "properties": { "to": { "type": "string" }, "text": { "type": "string" } },
            "required": ["to", "text"]
        }),
    ));
    if matches!(role, Role::Supervisor | Role::Memory | Role::Improver) {
        tools.push(tool(
            "notify_user",
            "Show a short notice to the human in grove's UI. `alert` is for things that need their attention now.",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "level": { "type": "string", "enum": ["info", "warn", "alert"] }
                },
                "required": ["text"]
            }),
        ));
    }
    if role == Role::Supervisor {
        tools.push(tool(
            "interrupt_agent",
            "Interrupt an agent now: its model request and running tool calls are cancelled, and it receives your reason as an operator message. It stays alive and the human can redirect it.",
            json!({
                "type": "object",
                "properties": { "agent": { "type": "string" }, "reason": { "type": "string" } },
                "required": ["agent", "reason"]
            }),
        ));
        tools.push(tool(
            "stop_agent",
            "Stop an agent for good (it accepts no further work until the human restarts it). Only for clearly unsafe behaviour that an interrupt didn't fix.",
            json!({
                "type": "object",
                "properties": { "agent": { "type": "string" }, "reason": { "type": "string" } },
                "required": ["agent", "reason"]
            }),
        ));
    }
    if role == Role::Improver {
        tools.push(tool(
            "request_upgrade",
            "Ask to rebuild grove from its source tree and restart into the new binary. Every agent is paused, grove re-executes itself, and they resume where they were. Only call this once the change is committed and `cargo build --release` and `cargo test` pass.",
            json!({
                "type": "object",
                "properties": { "summary": { "type": "string", "description": "What changed, one or two lines." } },
                "required": ["summary"]
            }),
        ));
    }
    for t in extra {
        tools.push(t.clone());
    }
    tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    tools
}

/// Runs a tool. Failures become error output for the model, never a failed turn.
pub async fn run(ctx: &Ctx, name: &str, input: &Value) -> Output {
    let (content, is_error) = match call(ctx, name, input).await {
        Ok(content) => (content, false),
        Err(e) => (format!("{e:#}"), true),
    };
    Output {
        content: truncate(&content, MAX_OUTPUT_HEAD_BYTES, MAX_OUTPUT_TAIL_BYTES),
        is_error,
    }
}

fn str_field<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input[field]
        .as_str()
        .with_context(|| format!("missing string field `{field}`"))
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

async fn call(ctx: &Ctx, name: &str, input: &Value) -> Result<String> {
    match name {
        "read_file" => {
            let path = resolve(&ctx.cwd, str_field(input, "path")?);
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = input["limit"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(READ_DEFAULT_LINES);
            let mut out = String::new();
            let mut shown = 0;
            let mut total: usize = 0;
            for (i, line) in text.lines().enumerate() {
                total += 1;
                let n = i + 1;
                if n < offset || shown >= limit {
                    continue;
                }
                out.push_str(&format!("{n:>6}\t{line}\n"));
                shown += 1;
            }
            if shown < total.saturating_sub(offset - 1) {
                out.push_str(&format!(
                    "[showing lines {offset}-{} of {total}]\n",
                    offset + shown - 1
                ));
            }
            Ok(out)
        }
        "list_dir" => {
            let path = resolve(&ctx.cwd, str_field(input, "path")?);
            let mut names = Vec::new();
            for entry in
                std::fs::read_dir(&path).with_context(|| format!("listing {}", path.display()))?
            {
                let entry = entry?;
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type()?.is_dir() {
                    name.push('/');
                }
                names.push(name);
            }
            names.sort();
            Ok(names.join("\n"))
        }
        "write_file" => {
            let path = resolve(&ctx.cwd, str_field(input, "path")?);
            let content = str_field(input, "content")?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(format!(
                "Wrote {} lines to {}.",
                content.lines().count(),
                path.display()
            ))
        }
        "edit_file" => {
            let path = resolve(&ctx.cwd, str_field(input, "path")?);
            let old = str_field(input, "old_string")?;
            let new = str_field(input, "new_string")?;
            let all = input["replace_all"].as_bool().unwrap_or(false);
            if old.is_empty() {
                bail!("`old_string` is empty; use write_file to create a file");
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let count = text.matches(old).count();
            if count == 0 {
                bail!("`old_string` not found in {}", path.display());
            }
            if count > 1 && !all {
                bail!(
                    "`old_string` appears {count} times in {}; add context or set replace_all",
                    path.display()
                );
            }
            let updated = if all {
                text.replace(old, new)
            } else {
                text.replacen(old, new, 1)
            };
            std::fs::write(&path, updated)?;
            Ok(format!(
                "Edited {} ({count} replacement{}).",
                path.display(),
                if count == 1 { "" } else { "s" }
            ))
        }
        "bash" => {
            let command = str_field(input, "command")?;
            let secs = input["timeout_secs"]
                .as_u64()
                .unwrap_or(DEFAULT_BASH_TIMEOUT_SECS);
            let timeout = Duration::from_secs(secs.min(MAX_BASH_TIMEOUT_SECS));
            run_bash(command, &ctx.cwd, &ctx.agent, timeout, ctx.cancel.clone()).await
        }
        "list_agents" => Ok(ctx.hub.describe_agents()),
        "send_message" => {
            let to = str_field(input, "to")?;
            let text = str_field(input, "text")?;
            if to == ctx.agent {
                bail!("that's you");
            }
            ctx.hub.message(&ctx.agent, to, text)?;
            Ok(format!("Sent to {to}."))
        }
        "spawn_agent" => {
            let name = str_field(input, "name")?;
            let task = str_field(input, "task")?;
            let effort = input["effort"].as_str().and_then(Effort::parse);
            let id = ctx
                .hub
                .spawn_sub(&ctx.agent, name, task, effort, &ctx.cwd)?;
            Ok(format!(
                "Started subagent `{id}`. Its result will arrive as a message."
            ))
        }
        "notify_user" => {
            let text = str_field(input, "text")?;
            let level = match input["level"].as_str() {
                Some("alert") => Level::Alert,
                Some("warn") => Level::Warn,
                _ => Level::Info,
            };
            ctx.hub.notify(&ctx.agent, level, text);
            Ok("Shown.".into())
        }
        "interrupt_agent" | "stop_agent" => {
            let target = str_field(input, "agent")?;
            let reason = str_field(input, "reason")?;
            if target == ctx.agent {
                bail!("you can't interrupt yourself");
            }
            let stop = name == "stop_agent";
            ctx.hub
                .send(target, if stop { Input::Stop } else { Input::Interrupt })?;
            let verb = if stop { "stopped" } else { "interrupted" };
            ctx.hub.send(
                target,
                Input::Directive {
                    from: ctx.agent.clone(),
                    text: format!("The supervisor {verb} you: {reason}"),
                },
            )?;
            ctx.hub.notify(
                target,
                Level::Alert,
                &format!("supervisor {verb} {target}: {reason}"),
            );
            Ok(format!("{target} {verb}."))
        }
        "request_upgrade" => {
            let summary = str_field(input, "summary")?;
            ctx.hub.request_upgrade(&ctx.agent, summary);
            Ok("Upgrade requested. The human approves it with /upgrade (or it runs automatically if they enabled that); you'll be paused and resumed.".into())
        }
        other => {
            if let Some(mcp) = ctx.hub.mcp_for(other) {
                let (text, is_error) = mcp.call(other, input).await?;
                if is_error {
                    bail!(text);
                }
                return Ok(text);
            }
            bail!("unknown tool: {other}")
        }
    }
}

enum End {
    Exited(ExitStatus),
    TimedOut,
    Interrupted,
}

async fn run_bash(
    command: &str,
    cwd: &Path,
    agent: &str,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<String> {
    let start = Instant::now();
    let mut child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("GROVE_AGENT", agent)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group: the terminal's ctrl-c doesn't reach it, and killpg reaches
        // everything it started.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .context("starting bash")?;
    let pgid = child.id().context("bash exited before we got its pid")? as i32;

    let stop_pipes = CancellationToken::new();
    let stdout = tokio::spawn(capture(
        child.stdout.take().context("no stdout")?,
        stop_pipes.clone(),
    ));
    let stderr = tokio::spawn(capture(
        child.stderr.take().context("no stderr")?,
        stop_pipes.clone(),
    ));

    let end = tokio::select! {
        status = child.wait() => End::Exited(status?),
        _ = tokio::time::sleep(timeout) => End::TimedOut,
        _ = cancel.cancelled() => End::Interrupted,
    };
    let (status, why) = match end {
        End::Exited(status) => (status, None),
        End::TimedOut => (
            terminate(&mut child, pgid).await?,
            Some(format!("timed out after {}s", timeout.as_secs())),
        ),
        End::Interrupted => (
            terminate(&mut child, pgid).await?,
            Some(format!(
                "interrupted after {:.1}s",
                start.elapsed().as_secs_f64()
            )),
        ),
    };
    killpg(pgid, libc::SIGKILL);

    let joined = async { (stdout.await, stderr.await) };
    tokio::pin!(joined);
    let (stdout, stderr) = match tokio::time::timeout(PIPE_GRACE, &mut joined).await {
        Ok(done) => done,
        Err(_) => {
            stop_pipes.cancel();
            joined.await
        }
    };
    let (stdout, stderr) = (stdout?, stderr?);
    let mut text = String::new();
    if let Some(why) = &why {
        text.push_str(&format!("[{why}; partial output below]\n"));
    }
    text.push_str(&describe(status));
    let out = stdout.render();
    let err = stderr.render();
    if !out.is_empty() {
        text.push_str(&format!("\n\nstdout:\n{out}"));
    }
    if !err.is_empty() {
        text.push_str(&format!("\n\nstderr:\n{err}"));
    }
    if why.is_some() || !status.success() {
        bail!(text);
    }
    Ok(text)
}

async fn terminate(child: &mut Child, pgid: i32) -> std::io::Result<ExitStatus> {
    killpg(pgid, libc::SIGTERM);
    if let Ok(status) = tokio::time::timeout(TERM_GRACE, child.wait()).await {
        return status;
    }
    killpg(pgid, libc::SIGKILL);
    child.wait().await
}

fn killpg(pgid: i32, signal: i32) {
    // Safety: only sends a signal; ESRCH if the group is already gone.
    unsafe {
        libc::killpg(pgid, signal);
    }
}

fn describe(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code: {code}");
    }
    match status.signal() {
        Some(signal) => format!("killed by signal {signal}"),
        None => "exit status unknown".to_string(),
    }
}

struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
    abandoned: bool,
}

impl Capture {
    fn render(&self) -> String {
        let dropped = self.total - self.head.len() - self.tail.len();
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        if dropped > 0 {
            text.push_str(&format!("\n[… {dropped} bytes cut …]\n"));
        }
        text.push_str(&String::from_utf8_lossy(&self.tail));
        if self.abandoned {
            text.push_str("\n[stopped reading: a process that left the command's process group still holds this pipe]\n");
        }
        text
    }
}

async fn capture(mut pipe: impl AsyncRead + Unpin, stop: CancellationToken) -> Capture {
    let mut capture = Capture {
        head: Vec::new(),
        tail: Vec::new(),
        total: 0,
        abandoned: false,
    };
    let mut chunk = [0u8; 8192];
    loop {
        let read = tokio::select! {
            read = pipe.read(&mut chunk) => read,
            _ = stop.cancelled() => {
                capture.abandoned = true;
                break;
            }
        };
        let n = match read {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        capture.total += n;
        let mut bytes = &chunk[..n];
        let room = BASH_PIPE_HEAD_BYTES - capture.head.len();
        if room > 0 {
            let take = room.min(bytes.len());
            capture.head.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
        capture.tail.extend_from_slice(bytes);
        if capture.tail.len() > 2 * BASH_PIPE_TAIL_BYTES {
            let excess = capture.tail.len() - BASH_PIPE_TAIL_BYTES;
            capture.tail.drain(..excess);
        }
    }
    if capture.tail.len() > BASH_PIPE_TAIL_BYTES {
        let excess = capture.tail.len() - BASH_PIPE_TAIL_BYTES;
        capture.tail.drain(..excess);
    }
    capture
}

/// Keeps the first `head` and last `tail` bytes, noting what was cut.
pub fn truncate(s: &str, head: usize, tail: usize) -> String {
    if s.len() <= head + tail {
        return s.to_string();
    }
    let mut head_end = head;
    while !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - tail;
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n\n[… {} of {} bytes cut …]\n\n{}",
        &s[..head_end],
        tail_start - head_end,
        s.len(),
        &s[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bash_returns_output() {
        let out = run_bash(
            "echo hi; echo err >&2",
            Path::new("."),
            "t",
            Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.contains("exit code: 0"), "{out}");
        assert!(out.contains("stdout:\nhi\n"), "{out}");
        assert!(out.contains("stderr:\nerr\n"), "{out}");
    }

    #[tokio::test]
    async fn bash_interrupt_is_prompt() {
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.cancel();
        });
        let start = Instant::now();
        let err = run_bash(
            "echo before; sleep 30",
            Path::new("."),
            "t",
            Duration::from_secs(60),
            cancel,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(err.contains("interrupted"), "{err}");
        assert!(err.contains("before"), "{err}");
    }

    #[test]
    fn tool_lists_are_sorted_and_role_specific() {
        let names = |role| {
            definitions(role, &[])
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let main = names(Role::Main);
        let mut sorted = main.clone();
        sorted.sort();
        assert_eq!(main, sorted);
        assert!(main.contains(&"spawn_agent".to_string()));
        assert!(!main.contains(&"interrupt_agent".to_string()));
        let sup = names(Role::Supervisor);
        assert!(sup.contains(&"interrupt_agent".to_string()));
        assert!(!sup.contains(&"bash".to_string()));
    }
}
