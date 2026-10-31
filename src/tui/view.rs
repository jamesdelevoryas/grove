//! Drawing. Only the visible slice of the transcript is touched each frame; lines come
//! from the per-entry cache.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::app::{App, Mode, Screen};
use super::theme::Theme;
use super::wrap::line_width;
use crate::protocol::{Level, Status};

pub const SPINNER: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];
const MAX_INPUT_ROWS: usize = 10;

pub fn short_model(model: &str) -> String {
    let m = model.strip_prefix("claude-").unwrap_or(model);
    let parts: Vec<&str> = m.split('-').collect();
    if parts.len() >= 3 {
        let n = parts.len();
        let all_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        if all_digits(parts[n - 1]) && all_digits(parts[n - 2]) {
            return format!(
                "{} {}.{}",
                parts[..n - 2].join("-"),
                parts[n - 2],
                parts[n - 1]
            );
        }
    }
    m.to_string()
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let theme = app.theme;
    app.screen_width = area.width;
    if let Some(pager) = app.pager.as_mut() {
        pager.draw(f.buffer_mut(), area, &theme);
        return;
    }
    if area.width < 20 || area.height < 8 {
        Paragraph::new("grove: window too small").render(area, f.buffer_mut());
        return;
    }

    let input_inner_w = area.width.saturating_sub(4) as usize;
    let (rows, cursor) = app.editor.layout(input_inner_w, 2);
    let menu = app.menu();
    let in_session = app.screen == Screen::Session;
    let busy = in_session && app.focused().is_some_and(|v| v.status.busy());
    // Suggested next prompts show as ghost text inside the empty input, one per row.
    let suggestions = match app.focused() {
        Some(v) if in_session && !busy && app.editor.is_empty() => v.suggestions.clone(),
        _ => Vec::new(),
    };
    let input_rows = rows.len().max(suggestions.len()).clamp(1, MAX_INPUT_ROWS);

    let [header, body, activity, menu_area, input, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(if busy { 1 } else { 0 }),
        Constraint::Length(menu.len().min(8) as u16),
        Constraint::Length(input_rows as u16 + 2),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f.buffer_mut(), header, app, &theme);

    if !in_session {
        draw_list(f.buffer_mut(), body, app, &theme);
    } else {
        draw_session_body(f.buffer_mut(), body, app, &theme);
    }
    if busy {
        draw_activity(f.buffer_mut(), activity, app, &theme);
    }
    if !menu.is_empty() {
        draw_menu(f.buffer_mut(), menu_area, &menu, app.menu_sel, &theme);
    }
    draw_input(
        f,
        input,
        status,
        app,
        &rows,
        cursor,
        input_rows,
        &suggestions,
    );
    if app.mode == Mode::Help {
        draw_help(f, area, &theme);
    }
}

fn draw_session_body(buf: &mut Buffer, main: Rect, app: &mut App, theme: &Theme) {
    let transcript = Rect {
        x: main.x + 2,
        y: main.y,
        width: main.width.saturating_sub(3),
        height: main.height,
    };
    draw_transcript(buf, transcript, app, theme);
}

#[allow(clippy::too_many_arguments)]
fn draw_input(
    f: &mut Frame,
    input: Rect,
    status: Rect,
    app: &mut App,
    rows: &[String],
    cursor: (usize, usize),
    input_rows: usize,
    suggestions: &[String],
) {
    let theme = app.theme;
    // Input box.
    // On the list, the prompt box only has focus (and the cursor) when no session is
    // highlighted.
    let prompt_focused =
        app.mode == Mode::Input && !(app.screen == Screen::List && app.list_sel.is_some());
    let border = if prompt_focused {
        theme.muted()
    } else {
        Style::new().fg(theme.dim).add_modifier(Modifier::DIM)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border);
    let inner = block.inner(input).inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });
    block.render(input, f.buffer_mut());
    // A short terminal can give the box fewer rows than the text wants.
    let input_rows = input_rows.min(inner.height as usize);
    if input_rows == 0 {
        draw_status(f.buffer_mut(), status, app, &theme);
        return;
    }
    let first_row = cursor.0.saturating_sub(input_rows - 1);
    if app.editor.is_empty() && !suggestions.is_empty() {
        for (k, s) in suggestions.iter().take(input_rows).enumerate() {
            let prefix = if k == 0 { "❯ " } else { "  " };
            let line = Line::from(vec![
                Span::styled(prefix, theme.accent()),
                Span::styled(s.clone(), theme.dim()),
            ]);
            let y = inner.y + k as u16;
            f.buffer_mut().set_line(inner.x, y, &line, inner.width);
            // Tab fills the first, a second Tab the next.
            let hint = "⇥".repeat(k + 1);
            let w = hint.chars().count() as u16;
            if inner.width > w + 4 {
                f.buffer_mut()
                    .set_string(inner.x + inner.width - w, y, hint, theme.accent());
            }
        }
    } else if app.editor.is_empty() {
        let placeholder = match app.focused() {
            _ if app.screen == Screen::List && app.list_sel.is_some() => format!(
                "Type to start a new session in {}",
                tilde(&app.cwd.display().to_string())
            ),
            _ if app.screen == Screen::List => format!(
                "Start a new session in {} · ↑ back to the list",
                tilde(&app.cwd.display().to_string())
            ),
            Some(v) if v.meta.role.is_watcher() => format!("Talk to the {} agent…", v.meta.id),
            _ => "Ask, or type / for commands · ? for shortcuts".to_string(),
        };
        let line = Line::from(vec![
            Span::styled("❯ ", theme.accent()),
            Span::styled(placeholder, theme.dim()),
        ]);
        f.buffer_mut()
            .set_line(inner.x, inner.y, &line, inner.width);
    } else {
        for (k, row) in rows.iter().enumerate().skip(first_row).take(input_rows) {
            let prefix = if k == 0 {
                Span::styled("❯ ", theme.accent())
            } else {
                Span::raw("  ")
            };
            let line = Line::from(vec![prefix, Span::raw(row.clone())]);
            f.buffer_mut().set_line(
                inner.x,
                inner.y + (k - first_row) as u16,
                &line,
                inner.width,
            );
        }
    }
    if prompt_focused {
        f.set_cursor_position(Position {
            x: inner.x + cursor.1 as u16,
            y: inner.y + (cursor.0 - first_row) as u16,
        });
    }

    draw_status(f.buffer_mut(), status, app, &theme);
}

fn tilde(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && path.starts_with(&home) {
        return format!("~{}", &path[home.len()..]);
    }
    path.to_string()
}

fn age(t: chrono::DateTime<chrono::Local>) -> String {
    let secs = (chrono::Local::now() - t).num_seconds().max(0);
    if secs < 60 {
        "now".into()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// The session list: one row per session, live ones first.
fn draw_list(buf: &mut Buffer, area: Rect, app: &mut App, theme: &Theme) {
    app.hit.list_rows.clear();
    let w = area.width as usize;
    if app.sessions.is_empty() {
        let text = "No sessions yet. Type a prompt below to start one.";
        let x = area.x + (area.width.saturating_sub(text.width() as u16)) / 2;
        buf.set_string(x, area.y + area.height / 2, text, theme.dim());
        return;
    }
    let title_row = Line::from(vec![
        Span::styled("  SESSIONS", theme.dim().add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", app.sessions.len()), theme.dim()),
    ]);
    buf.set_line(area.x, area.y + 1, &title_row, area.width);
    let top = area.y + 3;
    let height = area.height.saturating_sub(3) as usize;
    if height == 0 {
        return;
    }
    let first = app.list_sel.unwrap_or(0).saturating_sub(height - 1);
    let cwd_w = 26usize.min(w / 4);
    // cwd, " NN agents", " ttttt", "  $NN.NN"
    let right_w = cwd_w + 10 + 6 + 8;
    for (row, i) in (first..app.sessions.len()).take(height).enumerate() {
        let s = &app.sessions[i];
        let y = top + row as u16;
        let selected = app.list_sel == Some(i);
        let full = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        if selected {
            buf.set_style(full, Style::new().bg(theme.sel_bg));
        }
        let (glyph, gstyle) = if s.busy {
            (SPINNER[app.spinner % SPINNER.len()], theme.accent())
        } else if s.live {
            ("◐", Style::new().fg(theme.green))
        } else {
            ("○", theme.dim())
        };
        let marker = if selected {
            Span::styled("▌", theme.accent())
        } else {
            Span::raw(" ")
        };
        // Left of the title: marker, space, glyph, two spaces; after it, two spaces.
        let title_w = w.saturating_sub(right_w + 5 + 2 + 1).max(10);
        let (title, tstyle) = match &s.title {
            Some(t) => (
                clip_to(t, title_w),
                if selected { theme.bold() } else { Style::new() },
            ),
            None => (
                clip_to(&format!("(no prompt yet) {}", s.id), title_w),
                theme.dim(),
            ),
        };
        let mut spans = vec![
            marker,
            Span::raw(" "),
            Span::styled(glyph, gstyle),
            Span::raw("  "),
        ];
        let pad = title_w.saturating_sub(title.width());
        spans.push(Span::styled(title, tstyle));
        spans.push(Span::raw(" ".repeat(pad + 2)));
        let cwd = clip_to(&tilde(&s.cwd.display().to_string()), cwd_w);
        let cwd_pad = cwd_w.saturating_sub(cwd.width());
        spans.push(Span::styled(cwd, theme.muted()));
        spans.push(Span::raw(" ".repeat(cwd_pad)));
        let agents = if s.live {
            format!("{:>2} agents", s.agents)
        } else {
            "         ".to_string()
        };
        spans.push(Span::styled(format!(" {agents}"), theme.dim()));
        spans.push(Span::styled(format!(" {:>5}", age(s.updated)), theme.dim()));
        let cost = if s.cost > 0.0 {
            format!(" ${:.2}", s.cost)
        } else {
            String::new()
        };
        spans.push(Span::styled(format!("{cost:>8}"), theme.dim()));
        buf.set_line(area.x, y, &Line::from(spans), area.width);
        app.hit.list_rows.push((y, i));
    }
}

fn clip_to(s: &str, width: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    if first.width() <= width {
        return first.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in first.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

fn draw_header(buf: &mut Buffer, area: Rect, app: &App, theme: &Theme) {
    // Right side first: a pending self-upgrade, then every agent (the focused one bright,
    // a dot on unread). The left side gets whatever room is left.
    let mut spans = Vec::new();
    if let Some(summary) = &app.proposal {
        let short: String = summary.chars().take(40).collect();
        spans.push(Span::styled(
            format!("⬆ upgrade ready: {short} (/upgrade)  "),
            Style::new().fg(theme.green),
        ));
    }
    for (i, v) in app.views.iter().enumerate() {
        let (dot, style) = status_glyph(&v.status, app.spinner, theme);
        spans.push(Span::styled(dot, style));
        let name_style = if i == app.focus {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.dim()
        };
        spans.push(Span::styled(format!(" {}", v.meta.id), name_style));
        if v.unread {
            spans.push(Span::styled("•", theme.accent()));
        }
        spans.push(Span::raw("  "));
    }
    let right = Line::from(spans);
    let mut right_w = line_width(&right);
    if right_w + 30 > area.width as usize {
        right_w = 0;
    }
    let room = (area.width as usize).saturating_sub(right_w + 2);

    let mut left = vec![Span::styled(
        " grove",
        theme.accent().add_modifier(Modifier::BOLD),
    )];
    match &app.session {
        Some(info) => {
            let cwd = format!("  {}", tilde(&info.cwd.display().to_string()));
            let mut used = 6 + cwd.width();
            left.push(Span::styled(cwd, theme.muted()));
            if let Some(t) = &info.title {
                let space = room.saturating_sub(used + 4).min(50);
                if space >= 12 {
                    let title = clip_to(t, space);
                    used += 4 + title.width();
                    left.push(Span::styled(format!("  · {title}"), Style::new()));
                }
            }
            if used + 4 + info.id.len() <= room {
                left.push(Span::styled(format!("  · {}", info.id), theme.dim()));
            }
        }
        None => left.push(Span::styled("  sessions", theme.muted())),
    }
    buf.set_line(area.x, area.y, &Line::from(left), room as u16);
    if right_w > 0 {
        buf.set_line(
            area.x + area.width - right_w as u16,
            area.y,
            &right,
            right_w as u16,
        );
    }
}

fn status_glyph(status: &Status, spinner: usize, theme: &Theme) -> (String, Style) {
    match status {
        Status::Thinking => (SPINNER[spinner % SPINNER.len()].to_string(), theme.accent()),
        Status::Tools(_) => (
            SPINNER[spinner % SPINNER.len()].to_string(),
            Style::new().fg(theme.cyan),
        ),
        Status::Idle => ("○".into(), theme.dim()),
        Status::Paused => ("‖".into(), Style::new().fg(theme.yellow)),
        Status::Stopped => ("■".into(), Style::new().fg(theme.red)),
        Status::Error(_) => ("✕".into(), Style::new().fg(theme.red)),
    }
}

fn draw_transcript(buf: &mut Buffer, area: Rect, app: &mut App, theme: &Theme) {
    app.hit.transcript = area;
    app.hit.rows = vec![None; area.height as usize];
    app.transcript_height = area.height as usize;
    let mode = app.mode;
    let Some(view) = app.views.get_mut(app.focus) else {
        let line = Line::from(Span::styled("starting…", theme.dim()));
        buf.set_line(area.x, area.y, &line, area.width);
        return;
    };
    let w = area.width as usize;
    let h = area.height as usize;
    let heights = view.transcript.layout(w, theme);
    let mut total = 0;
    for hgt in &heights {
        total += hgt;
    }
    // Stay put while scrolled up, even as output arrives below.
    if view.scroll > 0 && total > view.last_total {
        view.scroll += total - view.last_total;
    }
    view.last_total = total;
    view.scroll = view.scroll.min(total.saturating_sub(h));

    let selected = if mode == Mode::Navigate {
        view.selected
    } else {
        None
    };
    if let Some(sel) = selected {
        let mut start = 0;
        for hgt in heights.iter().take(sel) {
            start += hgt;
        }
        let end = start + heights.get(sel).copied().unwrap_or(1);
        let bottom = total - view.scroll;
        let top = bottom.saturating_sub(h);
        if start < top {
            view.scroll = total.saturating_sub(start + h);
        } else if end > bottom {
            view.scroll = total.saturating_sub(end);
        }
    }

    let bottom = total - view.scroll;
    let top = bottom.saturating_sub(h);
    let mut y = 0;
    for (i, hgt) in heights.iter().enumerate() {
        if y + hgt <= top {
            y += hgt;
            continue;
        }
        if y >= bottom {
            break;
        }
        let lines = view.transcript.lines(i);
        let is_sel = selected == Some(i);
        for k in 0..*hgt {
            let ly = y + k;
            if ly < top || ly >= bottom {
                continue;
            }
            let row = (ly - top) as u16;
            let screen_y = area.y + row;
            app.hit.rows[row as usize] = Some(i);
            if let Some(line) = lines.get(k) {
                if line.style.bg.is_some() {
                    buf.set_style(
                        Rect {
                            x: area.x,
                            y: screen_y,
                            width: area.width,
                            height: 1,
                        },
                        line.style,
                    );
                }
                buf.set_line(area.x, screen_y, line, area.width);
            }
            if is_sel && k < lines.len() {
                buf.set_style(
                    Rect {
                        x: area.x,
                        y: screen_y,
                        width: area.width,
                        height: 1,
                    },
                    Style::new().bg(theme.sel_bg),
                );
                if area.x >= 2 {
                    buf.set_string(area.x - 2, screen_y, "▌", theme.accent());
                }
            }
        }
        y += hgt;
    }

    if view.scroll > 0 {
        let label = format!(" ↓ {} more ", view.scroll);
        let lw = label.width() as u16;
        if lw < area.width {
            buf.set_string(
                area.x + area.width - lw,
                area.y + area.height - 1,
                label,
                Style::new().fg(theme.accent).bg(theme.sel_bg),
            );
        }
    }
}

fn draw_activity(buf: &mut Buffer, area: Rect, app: &App, theme: &Theme) {
    let Some(view) = app.focused() else { return };
    let secs = view.busy_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let what = match &view.status {
        Status::Tools(n) => format!("Running {n} tool{}…", if *n == 1 { "" } else { "s" }),
        _ => "Thinking…".to_string(),
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", SPINNER[app.spinner % SPINNER.len()]),
            theme.accent(),
        ),
        Span::styled(what, theme.accent()),
        Span::styled(format!("  {secs}s · esc to interrupt"), theme.dim()),
    ]);
    buf.set_line(area.x, area.y, &line, area.width);
}

fn draw_menu(buf: &mut Buffer, area: Rect, menu: &[(&str, &str)], sel: usize, theme: &Theme) {
    for (i, (name, desc)) in menu.iter().take(area.height as usize).enumerate() {
        let selected = i == sel.min(menu.len() - 1);
        let row = Rect {
            x: area.x,
            y: area.y + i as u16,
            width: area.width,
            height: 1,
        };
        if selected {
            buf.set_style(row, Style::new().bg(theme.sel_bg));
        }
        let name_style = if selected {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let line = Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{name:<14}"), name_style),
            Span::styled(desc.to_string(), theme.dim()),
        ]);
        buf.set_line(area.x, row.y, &line, area.width);
    }
}

fn draw_status(buf: &mut Buffer, area: Rect, app: &App, theme: &Theme) {
    let left = match (&app.toast, app.focused()) {
        (Some(t), _) => {
            let style = match t.level {
                Level::Alert => Style::new().fg(theme.red).add_modifier(Modifier::BOLD),
                Level::Warn => Style::new().fg(theme.yellow),
                Level::Info => theme.muted(),
            };
            Line::from(Span::styled(format!(" {}", t.text), style))
        }
        (None, Some(v)) => {
            let u = &v.meta.usage;
            let mut total = 0.0;
            for other in &app.views {
                total += other.meta.usage.cost();
            }
            Line::from(vec![
                Span::styled(format!(" {}", v.meta.id), theme.accent()),
                Span::styled(
                    format!(
                        " · {} · {}",
                        short_model(&v.meta.model),
                        v.meta.effort.as_str()
                    ),
                    theme.muted(),
                ),
                Span::styled(
                    format!(
                        " · {:.1}k ctx · ${total:.2}",
                        u.last_context as f64 / 1000.0
                    ),
                    theme.dim(),
                ),
            ])
        }
        _ => Line::default(),
    };
    buf.set_line(area.x, area.y, &left, area.width);
    let hint = match app.mode {
        _ if app.screen == Screen::List && app.list_sel.is_some() => {
            "↑↓ select · enter open · type to start a session · ctrl+x stop · del delete"
        }
        _ if app.screen == Screen::List => {
            "enter start session · ↑ or esc back to the list · ctrl+c quit (sessions keep running)"
        }
        Mode::Navigate => "↑↓ select · enter expand · y copy · esc back",
        Mode::Help => "any key to close",
        Mode::Input => {
            if app.focused().is_some_and(|v| v.scroll > 0) {
                "esc jump to bottom · ctrl+o freeze"
            } else {
                "esc sessions · shift+tab effort · shift+↑ browse · ctrl+o freeze · ? help"
            }
        }
    };
    let hw = hint.width() as u16 + 1;
    if hw + line_width(&left) as u16 + 2 < area.width {
        buf.set_string(area.x + area.width - hw, area.y, hint, theme.dim());
    }
}

fn draw_help(f: &mut Frame, area: Rect, theme: &Theme) {
    let keys: &[(&str, &str)] = &[
        ("enter", "send"),
        ("shift+enter · ctrl+j", "newline"),
        ("esc", "interrupt the focused agent"),
        ("ctrl+c", "clear input · interrupt · twice to quit"),
        ("ctrl+o", "freeze: j/k ^d/^u gg/G / v y, mouse-select"),
        ("shift+tab", "cycle effort"),
        ("tab", "fill a suggested prompt (again: next one)"),
        ("ctrl+n · ctrl+p", "next · previous agent"),
        ("alt+1…9", "jump to agent"),
        ("shift+↑ · ctrl+g", "browse the transcript"),
        ("  ↑↓ enter y", "  select · expand · copy"),
        ("pgup · pgdn · wheel", "scroll"),
        ("↑ · ↓", "prompt history"),
        ("ctrl+a/e/k/u/w/y", "readline editing"),
        ("/", "commands"),
    ];
    let width = 66u16.min(area.width.saturating_sub(4));
    let height = (keys.len() as u16 + 3).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    Clear.render(rect, f.buffer_mut());
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.accent())
        .title(Span::styled(
            " keys ",
            theme.accent().add_modifier(Modifier::BOLD),
        ))
        .style(Style::new().bg(theme.panel_bg));
    let inner = block.inner(rect);
    block.render(rect, f.buffer_mut());
    for (i, (k, d)) in keys.iter().enumerate() {
        if i as u16 + 1 >= inner.height {
            break;
        }
        let line = Line::from(vec![
            Span::styled(format!(" {k:<22}"), theme.accent()),
            Span::styled(d.to_string(), Style::new()),
        ]);
        f.buffer_mut()
            .set_line(inner.x, inner.y + 1 + i as u16, &line, inner.width);
    }
}
