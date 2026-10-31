//! The terminal UI: fullscreen, redrawn only when something changed, with streaming capped
//! at ~120 fps and keystrokes drawn immediately.
//!
//! Ctrl+O freezes it into a still snapshot of the focused transcript, scrolled with vi
//! keys and with the mouse released for native selection (see `pager.rs`). Agents keep
//! running; q, Esc or Ctrl+O comes back to the live view.

mod app;
mod editor;
mod markdown;
mod pager;
#[cfg(test)]
mod snapshot;
mod theme;
mod transcript;
mod view;
mod wrap;

use std::io::{Stdout, Write, stdout};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use base64::Engine;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub use theme::Theme;

use crate::config;
use crate::hub::Hub;
use crate::protocol::{Event, Input, UiCommand};
use app::{Action, App};

pub enum Exit {
    Quit,
    Exec { binary: PathBuf, session: String },
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Restores the terminal when dropped, so every error path out of `run` leaves a usable
/// shell. Disarmed only for the exec into an upgraded grove, which keeps the screen.
struct Guard {
    enhanced: bool,
    armed: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.armed {
            restore(self.enhanced);
        }
    }
}

fn setup() -> Result<(Term, Guard)> {
    crossterm::terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    // Unambiguous escape codes: Esc arrives at once instead of after a timeout.
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // The UI runs on the main thread: a panic there ends grove, so give the terminal
        // back first. A panic in an agent or tool task only ends that task; printing it
        // would scribble over the live screen, so it goes to a log instead.
        if std::thread::current().name() == Some("main") {
            restore(enhanced);
            previous(info);
            return;
        }
        log_panic(&info.to_string());
    }));
    let term = Terminal::new(CrosstermBackend::new(stdout()))?;
    Ok((
        term,
        Guard {
            enhanced,
            armed: true,
        },
    ))
}

fn log_panic(text: &str) {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = std::path::Path::new(&home).join(".grove/panic.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{} {text}", chrono::Local::now().to_rfc3339());
    }
}

/// Puts the terminal back to normal; safe to call when it isn't in grove's modes.
pub fn restore_terminal() {
    restore(true);
}

fn restore(enhanced: bool) {
    let mut out = stdout();
    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

pub async fn run(
    hub: Hub,
    mut events: mpsc::UnboundedReceiver<Event>,
    mut commands: mpsc::UnboundedReceiver<UiCommand>,
    theme: Theme,
) -> Result<Exit> {
    let (mut term, mut guard) = setup()?;
    let mut app = App::new(hub.clone(), theme);
    let mut input = EventStream::new();
    let mut dirty = true;
    let mut urgent = true;
    let frame = Duration::from_millis(config::FRAME_MIN_MS);
    let mut last_draw = Instant::now() - frame;
    let mut spinner = tokio::time::interval(Duration::from_millis(config::SPINNER_MS));
    spinner.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let exit = loop {
        let frozen = app.pager.is_some();
        if dirty && (urgent || last_draw.elapsed() >= frame) {
            draw(&mut term, &mut app)?;
            dirty = false;
            urgent = false;
            last_draw = Instant::now();
        }
        if let Some(text) = app.copy.take() {
            copy(&text);
        }
        if app.quit {
            break Exit::Quit;
        }
        let next_frame = last_draw + frame;
        let waiting = dirty && !frozen;
        // The spinner timer only runs while something is busy: an idle grove is silent.
        let animate = !frozen && app.animating();
        let toast_expiry = app.toast_deadline().map(Instant::from_std);

        tokio::select! {
            biased;
            maybe = input.next() => {
                let Some(Ok(ev)) = maybe else { break Exit::Quit };
                match ev {
                    TermEvent::Key(key) => match app.on_key(key) {
                        // Frozen, the terminal owns the mouse: native drag-select and copy.
                        Action::Freeze => execute!(stdout(), DisableMouseCapture)?,
                        Action::Unfreeze => execute!(stdout(), EnableMouseCapture)?,
                        Action::Redraw => full_redraw(&mut term)?,
                        Action::None => {}
                    },
                    TermEvent::Mouse(m) if !frozen => app.on_mouse(m),
                    TermEvent::Paste(text) if !frozen => app.paste(&text),
                    TermEvent::Resize(_, _) => term.autoresize()?,
                    _ => {}
                }
                dirty = true;
                urgent = true;
            }
            Some(cmd) = commands.recv() => match cmd {
                UiCommand::Exec { binary, session } => break Exit::Exec { binary, session },
            },
            Some(ev) = events.recv() => {
                app.on_event(ev);
                // Drain the backlog so a burst of deltas costs one frame.
                while let Ok(ev) = events.try_recv() {
                    app.on_event(ev);
                }
                // The frozen view is a snapshot: agent activity doesn't touch it.
                if !frozen {
                    dirty = true;
                }
            }
            _ = spinner.tick(), if animate => {
                app.spinner = app.spinner.wrapping_add(1);
                app.expire_toast();
                dirty = true;
            }
            _ = tokio::time::sleep_until(toast_expiry.unwrap_or(next_frame)), if toast_expiry.is_some() && !frozen => {
                if app.expire_toast() {
                    dirty = true;
                }
            }
            _ = tokio::time::sleep_until(next_frame), if waiting => {}
        }
    };

    if matches!(exit, Exit::Quit) {
        // Interrupt running work so no tool process outlives grove.
        for v in &app.views {
            if v.status.busy() {
                let _ = hub.send(&v.meta.id, Input::Interrupt);
            }
        }
        for _ in 0..80 {
            if !hub.any_busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // The guard restores the terminal as it drops.
    } else {
        // Hand the terminal over still in the alternate screen (no flash), but in cooked
        // mode: the new process records the mode it finds as the one to restore on exit.
        guard.armed = false;
        if guard.enhanced {
            let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = crossterm::terminal::disable_raw_mode();
    }
    Ok(exit)
}

fn draw(term: &mut Term, app: &mut App) -> Result<()> {
    // Synchronized output: the terminal shows the whole frame at once, never half of it.
    execute!(term.backend_mut(), BeginSynchronizedUpdate)?;
    term.draw(|f| view::draw(f, app))?;
    execute!(term.backend_mut(), EndSynchronizedUpdate)?;
    Ok(())
}

/// Clears the screen and forgets the last frame, so the next draw repaints everything.
/// (`Terminal::clear` would first ask the terminal for its cursor position: a blocking
/// round-trip we don't need in fullscreen.)
fn full_redraw(term: &mut Term) -> Result<()> {
    let size = term.size()?;
    term.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))?;
    Ok(())
}

/// OSC 52 (works over SSH too), plus pbcopy when local on macOS.
fn copy(text: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
    if cfg!(target_os = "macos")
        && std::env::var_os("SSH_TTY").is_none()
        && let Ok(mut child) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
    {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}
