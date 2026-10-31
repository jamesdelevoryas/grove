//! System prompts. Each is built once when an agent is created and saved in its meta, so
//! it never changes under the prompt cache.

use std::path::Path;

use crate::config::Config;
use crate::protocol::Role;

const MAIN: &str = include_str!("prompts/main.md");
const SUB: &str = include_str!("prompts/sub.md");
const SUPERVISOR: &str = include_str!("prompts/supervisor.md");
const MEMORY: &str = include_str!("prompts/memory.md");
const IMPROVER: &str = include_str!("prompts/improver.md");
const SESSION: &str = include_str!("prompts/session.md");

pub fn system(role: Role, id: &str, cwd: &Path, config: &Config, extra: &str) -> String {
    let body = match role {
        Role::Main => MAIN,
        Role::Sub => SUB,
        Role::Supervisor => SUPERVISOR,
        Role::Memory => MEMORY,
        Role::Improver => IMPROVER,
    };
    let mut out = String::new();
    out.push_str(body.trim());
    out.push_str("\n\n");
    out.push_str(SESSION.trim());
    out.push_str("\n\n# Environment\n\n");
    out.push_str(&format!("- Your agent id: `{id}`\n"));
    out.push_str(&format!("- Working directory: {}\n", cwd.display()));
    out.push_str(&format!(
        "- Platform: {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    out.push_str(&format!(
        "- Date when this session started: {}\n",
        chrono::Local::now().format("%A %Y-%m-%d")
    ));
    if let Some(branch) = git_branch(cwd) {
        out.push_str(&format!("- Git branch: {branch}\n"));
    }
    out.push_str(&format!(
        "- grove's own source tree: {}\n",
        config.src.display()
    ));
    if !extra.trim().is_empty() {
        out.push('\n');
        out.push_str(extra.trim());
        out.push('\n');
    }
    out
}

fn git_branch(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
