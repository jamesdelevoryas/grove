//! Settings. Defaults are consts here; a few can be overridden by environment variables
//! (listed next to each) so trying things doesn't need a rebuild.

use std::path::PathBuf;

use crate::protocol::Effort;

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_EFFORT: Effort = Effort::High;

/// Watcher agents run at low effort: they read a lot and act rarely.
pub const SUPERVISOR_EFFORT: Effort = Effort::Low;
pub const MEMORY_EFFORT: Effort = Effort::Low;
pub const IMPROVER_EFFORT: Effort = Effort::High;

/// Streaming lets us ask for the full output budget without HTTP timeouts.
pub const MAX_TOKENS: u32 = 64_000;

/// How long the supervisor feed waits to batch events. Prompts and tool calls are flushed
/// quickly; everything else rides along with the next flush or the slow timer.
pub const SUPERVISOR_FAST_FLUSH_MS: u64 = 250;
pub const SUPERVISOR_SLOW_FLUSH_MS: u64 = 4_000;
pub const MEMORY_FLUSH_MS: u64 = 1_500;
/// The improver looks at friction in batches; it's the most expensive watcher to wake.
pub const IMPROVER_BATCH_EVENTS: usize = 12;
pub const IMPROVER_IDLE_FLUSH_SECS: u64 = 90;

/// A watcher whose own context grows past this starts over, so feeding it stays cheap.
pub const WATCHER_RESET_INPUT_TOKENS: u64 = 120_000;

/// UI frame cap while streaming. Keystrokes always draw immediately.
pub const FRAME_MIN_MS: u64 = 8;
pub const SPINNER_MS: u64 = 90;

#[derive(Clone, Debug)]
pub struct Config {
    pub model: String,
    pub effort: Effort,
    /// GROVE_SUPERVISOR=0 to disable.
    pub supervisor: bool,
    /// GROVE_MEMORY=0 to disable.
    pub memory: bool,
    /// GROVE_IMPROVER=0 to disable. The improver also only watches (vs. only answering
    /// /improve requests) when GROVE_IMPROVER_WATCH is not 0.
    pub improver: bool,
    pub improver_watch: bool,
    /// GROVE_HOME, default ~/.grove: sessions, history.
    pub home: PathBuf,
    /// GROVE_SRC: grove's own source tree, which the improver edits and /upgrade builds.
    pub src: PathBuf,
    /// The memory MCP server: MEMORY_URL, else `memory_url` in ~/.grove/config. Without
    /// one the memory agent doesn't run. (/mcp is appended if missing.)
    pub memory_url: Option<String>,
    /// GROVE_THEME=light|dark; default: guessed from the Ghostty config, else dark.
    pub light_theme: bool,
}

impl Config {
    pub fn load() -> Self {
        let home_dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
        let home = match std::env::var("GROVE_HOME") {
            Ok(dir) => PathBuf::from(dir),
            Err(_) => home_dir.join(".grove"),
        };
        let src = match std::env::var("GROVE_SRC") {
            Ok(dir) => PathBuf::from(dir),
            Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        };
        let memory_url = match std::env::var("MEMORY_URL") {
            Ok(url) if !url.trim().is_empty() => Some(url),
            _ => file_setting(&home, "memory_url"),
        };
        let memory_url = memory_url.map(|url| {
            let url = url.trim().trim_end_matches('/').to_string();
            if url.ends_with("/mcp") {
                url
            } else {
                format!("{url}/mcp")
            }
        });
        let effort = match std::env::var("GROVE_EFFORT") {
            Ok(e) => Effort::parse(&e).unwrap_or(DEFAULT_EFFORT),
            Err(_) => DEFAULT_EFFORT,
        };
        let light_theme = match std::env::var("GROVE_THEME").as_deref() {
            Ok("light") => true,
            Ok("dark") => false,
            _ => ghostty_theme_is_light(&home_dir),
        };
        Self {
            model: std::env::var("GROVE_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into()),
            effort,
            supervisor: flag("GROVE_SUPERVISOR"),
            memory: flag("GROVE_MEMORY"),
            improver: flag("GROVE_IMPROVER"),
            improver_watch: flag("GROVE_IMPROVER_WATCH"),
            home,
            src,
            memory_url,
            light_theme,
        }
    }
}

/// A `key = value` line from ~/.grove/config (lines starting with # are comments).
fn file_setting(home: &std::path::Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(home.join("config")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == key {
            let v = v.trim().trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Unset or anything but "0"/"false"/"off" means on.
fn flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(v.as_str(), "0" | "false" | "off"),
        Err(_) => true,
    }
}

/// Reads the active `theme = ...` line of ~/.config/ghostty/config.
fn ghostty_theme_is_light(home: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(home.join(".config/ghostty/config")) else {
        return false;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("theme") else {
            continue;
        };
        let value = rest.trim_start().trim_start_matches('=').trim();
        // "light:x,dark:y" follows the system appearance; we can't know which, so say dark.
        if value.contains(',') {
            return false;
        }
        return value.contains("light");
    }
    false
}

/// The Anthropic API key: ANTHROPIC_API_KEY, else the macOS keychain item
/// `grove-anthropic-api-key` (any account).
pub fn api_key() -> Option<String> {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key.trim().to_string());
    }
    keychain("grove-anthropic-api-key", None)
}

/// A generic password from the macOS keychain.
pub fn keychain(service: &str, account: Option<&str>) -> Option<String> {
    let mut cmd = std::process::Command::new("security");
    cmd.args(["find-generic-password", "-s", service]);
    if let Some(account) = account {
        cmd.args(["-a", account]);
    }
    cmd.arg("-w");
    let out = cmd.stderr(std::process::Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}
