//! The terminal UI: fullscreen, redrawn only when something changed, with streaming capped
//! at ~120 fps and keystrokes drawn immediately.
//!
//! Ctrl+O freezes it: grove leaves the alternate screen, releases the mouse, and prints
//! the focused transcript as plain ANSI text into the terminal's normal screen, so the
//! terminal's own scrollback, selection, search and copy all work. Agents keep running;
//! Ctrl+O (or Esc/q/Enter) comes back to the live view.

mod app;
mod editor;
mod markdown;
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
    Event as TermEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
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
    let mut frozen = false;
    let mut dirty = true;
    let mut urgent = true;
    let frame = Duration::from_millis(config::FRAME_MIN_MS);
    let mut last_draw = Instant::now() - frame;
    let mut spinner = tokio::time::interval(Duration::from_millis(config::SPINNER_MS));
    spinner.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let exit = loop {
        if dirty && !frozen && (urgent || last_draw.elapsed() >= frame) {
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
                if frozen {
                    if is_unfreeze(&ev) {
                        frozen = false;
                        thaw(&mut term)?;
                        dirty = true;
                        urgent = true;
                    }
                    continue;
                }
                match ev {
                    TermEvent::Key(key) => match app.on_key(key) {
                        Action::Freeze => {
                            freeze(&mut app)?;
                            frozen = true;
                        }
                        Action::Redraw => full_redraw(&mut term)?,
                        Action::None => {}
                    },
                    TermEvent::Mouse(m) => app.on_mouse(m),
                    TermEvent::Paste(text) => app.paste(&text),
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
                dirty = true;
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

fn is_unfreeze(ev: &TermEvent) -> bool {
    let TermEvent::Key(key) = ev else {
        return false;
    };
    if key.kind == KeyEventKind::Release {
        return false;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q'))
        || (ctrl && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('c')))
}

/// Prints the focused transcript into the normal screen for native scrollback and copy.
fn freeze(app: &mut App) -> Result<()> {
    let mut out = stdout();
    execute!(out, DisableMouseCapture, LeaveAlternateScreen, cursor::Hide)?;
    let (cols, _) = crossterm::terminal::size()?;
    let width = (cols as usize).saturating_sub(2).max(20);
    let theme = app.theme;
    let mut text = String::new();
    // Start clean (screen and scrollback), so the scrollback is exactly the transcript.
    text.push_str("\x1b[H\x1b[2J\x1b[3J");
    if let Some(view) = app.views.get_mut(app.focus) {
        let heights = view.transcript.layout(width, &theme);
        for (i, _) in heights.iter().enumerate() {
            for line in view.transcript.lines(i) {
                text.push(' ');
                text.push_str(&ansi_line(line, width));
                text.push_str("\r\n");
            }
            text.push_str("\r\n");
        }
        let footer = format!(
            " \x1b[0m{}── frozen: {} · scroll, select and copy freely · ctrl+o or esc to resume · agents keep running ──\x1b[0m\r\n",
            sgr(Style::new().fg(theme.accent)),
            view.meta.id
        );
        text.push_str(&footer);
    }
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(())
}

fn thaw(term: &mut Term) -> Result<()> {
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    full_redraw(term)
}

/// Clears the screen and forgets the last frame, so the next draw repaints everything.
/// (`Terminal::clear` would first ask the terminal for its cursor position: a blocking
/// round-trip we don't need in fullscreen.)
fn full_redraw(term: &mut Term) -> Result<()> {
    let size = term.size()?;
    term.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))?;
    Ok(())
}

fn ansi_line(line: &Line, width: usize) -> String {
    let mut s = String::new();
    let mut used = 0;
    for span in &line.spans {
        let style = line.style.patch(span.style);
        s.push_str("\x1b[0m");
        s.push_str(&sgr(style));
        s.push_str(&span.content);
        used += unicode_width::UnicodeWidthStr::width(span.content.as_ref());
    }
    // Carry full-row backgrounds to the edge, like the live view.
    if line.style.bg.is_some() && used < width {
        s.push_str("\x1b[0m");
        s.push_str(&sgr(line.style));
        s.push_str(&" ".repeat(width - used));
    }
    s.push_str("\x1b[0m");
    s
}

fn sgr(style: Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    if let Some(fg) = style.fg {
        codes.push(color_code(fg, false));
    }
    if let Some(bg) = style.bg {
        codes.push(color_code(bg, true));
    }
    let m = style.add_modifier;
    if m.contains(Modifier::BOLD) {
        codes.push("1".into());
    }
    if m.contains(Modifier::DIM) {
        codes.push("2".into());
    }
    if m.contains(Modifier::ITALIC) {
        codes.push("3".into());
    }
    if m.contains(Modifier::UNDERLINED) {
        codes.push("4".into());
    }
    if m.contains(Modifier::REVERSED) {
        codes.push("7".into());
    }
    if m.contains(Modifier::CROSSED_OUT) {
        codes.push("9".into());
    }
    if codes.is_empty() {
        return String::new();
    }
    format!("\x1b[{}m", codes.join(";"))
}

fn color_code(c: Color, bg: bool) -> String {
    let base = |n: u8| -> String {
        if bg {
            (n + 10).to_string()
        } else {
            n.to_string()
        }
    };
    match c {
        Color::Reset => base(39),
        Color::Black => base(30),
        Color::Red => base(31),
        Color::Green => base(32),
        Color::Yellow => base(33),
        Color::Blue => base(34),
        Color::Magenta => base(35),
        Color::Cyan => base(36),
        Color::Gray => base(37),
        Color::DarkGray => base(90),
        Color::LightRed => base(91),
        Color::LightGreen => base(92),
        Color::LightYellow => base(93),
        Color::LightBlue => base(94),
        Color::LightMagenta => base(95),
        Color::LightCyan => base(96),
        Color::White => base(97),
        Color::Indexed(n) => format!("{};5;{n}", if bg { 48 } else { 38 }),
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", if bg { 48 } else { 38 }),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    #[test]
    fn ansi_lines_carry_styles_and_backgrounds() {
        let line = Line::from(vec![Span::styled("hi", Style::new().fg(Color::Red))])
            .style(Style::new().bg(Color::Rgb(1, 2, 3)));
        let s = ansi_line(&line, 5);
        assert!(s.contains("\x1b[31;48;2;1;2;3mhi"), "{s:?}");
        assert!(s.contains("   "), "{s:?}");
        assert!(s.ends_with("\x1b[0m"));
    }
}
