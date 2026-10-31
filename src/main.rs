//! grove: a multi-agent coding agent with a fast terminal UI.
//!
//! Sessions run in a background server (`grove server`, started on demand) so they
//! outlive the terminal; `grove` itself is a UI that attaches to them.

mod agent;
mod api;
mod client;
mod config;
mod hub;
mod infer;
mod mcp;
mod prompts;
mod protocol;
mod server;
mod store;
mod tools;
mod tui;
mod watchers;
mod wire;

use std::os::unix::process::CommandExt;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use config::Config;
use protocol::Effort;
use store::Store;
use wire::{Reply, Request};

const USAGE: &str = "usage:
  grove                     the session list (start new sessions from its prompt box)
  grove <prompt>            start a session with this prompt and open it
  grove --bg <prompt>       start a session in the background; prints its id
  grove -c, --continue      open the latest session in this directory
  grove -r, --resume <id>   open a session (also: grove attach <id>)
  grove list [--json]       list sessions (also: grove agents)
  grove stop <id>           stop a session's agents (it stays resumable)
  grove rm <id>             stop and delete a session
  grove shutdown            stop the background server (sessions stay resumable)

options: --effort low|medium|high|xhigh|max   --model <id>

Sessions run in a background server, so they keep going when you close the terminal.
environment: ANTHROPIC_API_KEY (or keychain item grove-anthropic-api-key),
GROVE_SUPERVISOR / GROVE_MEMORY / GROVE_IMPROVER = 0 to turn a watcher off,
GROVE_THEME = light | dark, GROVE_SRC, GROVE_HOME, MEMORY_URL";

enum Command {
    Ui(tui::Start),
    Background(String),
    List { json: bool },
    Stop(String),
    Delete(String),
    Shutdown,
    Server { resume: Vec<String> },
}

fn parse_args(config: &mut Config) -> Result<Command> {
    let mut words = Vec::new();
    let mut resume = None;
    let mut cont = false;
    let mut bg = false;
    let mut json = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let in_server = words.first().map(String::as_str) == Some("server");
        match arg.as_str() {
            _ if in_server => words.push(arg),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-c" | "--continue" => cont = true,
            "-r" | "--resume" => resume = Some(args.next().context("--resume needs a session id")?),
            "--bg" => bg = true,
            "--json" => json = true,
            "--effort" => {
                let e = args.next().context("--effort needs a level")?;
                config.effort = Effort::parse(&e).with_context(|| format!("unknown effort {e}"))?;
            }
            "--model" => config.model = args.next().context("--model needs a model id")?,
            other if other.starts_with('-') => bail!("unknown option {other}\n\n{USAGE}"),
            other => words.push(other.to_string()),
        }
    }
    let arg1 = || words.get(1).cloned();
    // A command word is a command only with its exact arguments; otherwise the words are
    // a prompt (`grove list the TODOs`).
    let n = words.len();
    let first = match words.first().map(String::as_str) {
        Some("list" | "agents" | "shutdown" | "help") if n == 1 => {
            words.first().map(String::as_str)
        }
        Some("attach" | "stop" | "rm") if n == 2 => words.first().map(String::as_str),
        Some("server") => Some("server"),
        _ => None,
    };
    match first {
        Some("server") => {
            let mut ids = Vec::new();
            let mut take = false;
            for w in &words[1..] {
                if w == "--resume" {
                    take = true;
                } else if take {
                    ids.push(w.clone());
                }
            }
            return Ok(Command::Server { resume: ids });
        }
        Some("list") | Some("agents") => return Ok(Command::List { json }),
        Some("attach") => {
            let id = arg1().context("attach needs a session id")?;
            return Ok(Command::Ui(tui::Start::Attach(id)));
        }
        Some("stop") => return Ok(Command::Stop(arg1().context("stop needs a session id")?)),
        Some("rm") => return Ok(Command::Delete(arg1().context("rm needs a session id")?)),
        Some("shutdown") => return Ok(Command::Shutdown),
        Some("help") => {
            println!("{USAGE}");
            std::process::exit(0);
        }
        _ => {}
    }
    if let Some(id) = resume {
        return Ok(Command::Ui(tui::Start::Attach(id)));
    }
    if cont {
        let cwd = std::env::current_dir()?;
        let id = Store::latest_for(&config.home, &cwd)
            .context("no earlier session in this directory")?;
        return Ok(Command::Ui(tui::Start::Attach(id)));
    }
    if words.is_empty() {
        if bg {
            bail!("--bg needs a prompt");
        }
        return Ok(Command::Ui(tui::Start::List));
    }
    let prompt = words.join(" ");
    if bg {
        return Ok(Command::Background(prompt));
    }
    Ok(Command::Ui(tui::Start::New(prompt)))
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // An upgraded grove starts inside the old one's fullscreen; if it fails before
            // its UI is up, give the terminal back before printing.
            if std::env::var_os("GROVE_UPGRADED").is_some() {
                tui::restore_terminal();
            }
            eprintln!("grove: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn effort_override(config: &Config) -> Option<Effort> {
    if config.effort == config::DEFAULT_EFFORT {
        None
    } else {
        Some(config.effort)
    }
}

async fn run() -> Result<()> {
    let mut config = Config::load();
    let command = parse_args(&mut config)?;
    let home = config.home.clone();
    let cwd = std::env::current_dir()?;
    match command {
        Command::Server { resume } => server::run(config, resume).await,
        Command::List { json } => {
            let reply = client::request(&home, true, Request::Hello, |r| {
                matches!(r, Reply::Sessions { .. })
            })
            .await?;
            let Reply::Sessions { list } = reply else {
                unreachable!()
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            if list.is_empty() {
                println!("no sessions");
            }
            for s in &list {
                let state = if s.busy {
                    "busy"
                } else if s.live {
                    "idle"
                } else {
                    "stopped"
                };
                let title = s.title.clone().unwrap_or_else(|| "(no prompt yet)".into());
                println!("{}  {state:<7}  {}  {}", s.id, s.cwd.display(), title);
            }
            Ok(())
        }
        Command::Background(prompt) => {
            let req = Request::Create {
                cwd,
                prompt: Some(prompt),
                model: Some(config.model.clone()),
                effort: effort_override(&config),
                attach: false,
            };
            let reply =
                client::request(&home, true, req, |r| matches!(r, Reply::Created { .. })).await?;
            let Reply::Created { session } = reply else {
                unreachable!()
            };
            println!("{session}");
            Ok(())
        }
        Command::Stop(session) => {
            client::request(&home, true, Request::Stop { session }, |r| {
                matches!(r, Reply::Info { .. })
            })
            .await?;
            Ok(())
        }
        Command::Delete(session) => {
            client::request(&home, true, Request::Delete { session }, |r| {
                matches!(r, Reply::Info { .. })
            })
            .await?;
            Ok(())
        }
        Command::Shutdown => {
            match client::connect(&home, false, Duration::from_millis(200)).await {
                Ok((remote, mut rx)) => {
                    remote.send(Request::Shutdown);
                    // The server pauses its sessions, then exits; wait for it to go.
                    while let Some(m) = rx.recv().await {
                        if matches!(m, client::Incoming::Disconnected) {
                            break;
                        }
                    }
                    println!("grove server stopped");
                }
                Err(_) => println!("no grove server running"),
            }
            Ok(())
        }
        Command::Ui(start) => {
            let theme = if config.light_theme {
                tui::Theme::light()
            } else {
                tui::Theme::dark()
            };
            // After an upgrade the server is restarting: wait for it rather than race it.
            let upgraded = std::env::var_os("GROVE_UPGRADED").is_some();
            let wait = if upgraded {
                Duration::from_secs(30)
            } else {
                Duration::from_secs(5)
            };
            let (remote, rx) = client::connect(&home, !upgraded, wait).await?;
            let model = if config.model == config::DEFAULT_MODEL {
                None
            } else {
                Some(config.model.clone())
            };
            let effort = effort_override(&config);
            match tui::run(remote, rx, theme, home, cwd, start, model, effort).await? {
                tui::Exit::Quit => Ok(()),
                tui::Exit::Exec { binary, session } => {
                    let mut cmd = std::process::Command::new(&binary);
                    if let Some(id) = &session {
                        cmd.args(["attach", id]);
                    }
                    let err = cmd.env("GROVE_UPGRADED", "1").exec();
                    tui::restore_terminal();
                    Err(err).with_context(|| format!("exec {}", binary.display()))
                }
            }
        }
    }
}
