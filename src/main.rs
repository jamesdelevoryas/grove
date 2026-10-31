//! grove: a multi-agent coding agent with a fast terminal UI.

mod agent;
mod api;
mod config;
mod hub;
mod infer;
mod mcp;
mod prompts;
mod protocol;
mod store;
mod tools;
mod tui;
mod watchers;

use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use config::Config;
use hub::Hub;
use protocol::{Effort, Input, Level, Role};
use store::Store;

const USAGE: &str = "usage: grove [options] [prompt]

  -c, --continue        resume the latest session in this directory
  -r, --resume <id>     resume a session by id
      --effort <level>  low | medium | high | xhigh | max (default high)
      --model <id>      model id (default claude-opus-5-5)
  -h, --help

environment: ANTHROPIC_API_KEY (or keychain item grove-anthropic-api-key),
GROVE_SUPERVISOR / GROVE_MEMORY / GROVE_IMPROVER = 0 to turn a watcher off,
GROVE_THEME = light | dark, GROVE_SRC, GROVE_HOME, MEMORY_URL";

struct Args {
    resume: Option<String>,
    continue_latest: bool,
    prompt: Option<String>,
}

fn parse_args(config: &mut Config) -> Result<Args> {
    let mut args = Args {
        resume: None,
        continue_latest: false,
        prompt: None,
    };
    let mut words = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-c" | "--continue" => args.continue_latest = true,
            "-r" | "--resume" => {
                args.resume = Some(it.next().context("--resume needs a session id")?)
            }
            "--effort" => {
                let e = it.next().context("--effort needs a level")?;
                config.effort = Effort::parse(&e).with_context(|| format!("unknown effort {e}"))?;
            }
            "--model" => config.model = it.next().context("--model needs a model id")?,
            other if other.starts_with('-') => bail!("unknown option {other}\n\n{USAGE}"),
            other => words.push(other.to_string()),
        }
    }
    if !words.is_empty() {
        args.prompt = Some(words.join(" "));
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("grove: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let mut config = Config::load();
    let args = parse_args(&mut config)?;
    let cwd = std::env::current_dir()?;

    let resume_id = match (&args.resume, args.continue_latest) {
        (Some(id), _) => Some(id.clone()),
        (None, true) => Some(
            Store::latest_for(&config.home, &cwd)
                .context("no earlier session in this directory")?,
        ),
        _ => None,
    };
    let store = match &resume_id {
        Some(id) => Store::open(&config.home, id)?,
        None => Store::create(&config.home, &cwd)?,
    };
    let theme = if config.light_theme {
        tui::Theme::light()
    } else {
        tui::Theme::dark()
    };

    let client = api::Client::new(config::api_key());
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let hub = Hub::new(config.clone(), store.clone(), client.clone(), ui_tx, cmd_tx);

    // Restore saved agents (watchers are started by their module), or start fresh.
    let mut restored_watchers = Vec::new();
    if resume_id.is_some() {
        for (meta, history) in store.load_agents()? {
            if meta.role.is_watcher() {
                restored_watchers.push((meta, history));
                continue;
            }
            let tools = hub.tools_for(meta.role);
            hub.start(meta, history, tools);
        }
    }
    let main_id = match hub.find_role(Role::Main) {
        Some(id) => id,
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

    if !client.has_key() {
        hub.notify(
            &main_id,
            Level::Alert,
            "No Anthropic API key. Set ANTHROPIC_API_KEY, or store one in the keychain:\n  security add-generic-password -s grove-anthropic-api-key -a grove -w",
        );
    }
    if let Some(prompt) = args.prompt {
        hub.send(&main_id, Input::User(prompt))?;
    }

    match tui::run(hub, ui_rx, cmd_rx, theme).await? {
        tui::Exit::Quit => {
            println!(
                "grove session {} — resume with: grove --resume {}",
                store.session.id, store.session.id
            );
            Ok(())
        }
        tui::Exit::Exec { binary, session } => {
            // Replace this process; the terminal stays in fullscreen mode across the swap.
            let err = std::process::Command::new(&binary)
                .args(["--resume", &session])
                .exec();
            Err(err).with_context(|| format!("exec {}", binary.display()))
        }
    }
}
