//! Drawing. Only the visible slice of the transcript is touched each frame; lines come
//! from the per-entry cache.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::app::{App, Mode};
use super::theme::Theme;
use super::wrap::line_width;
use crate::protocol::{Level, Role, Status};

pub const SPINNER: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];
const SIDEBAR_W: u16 = 28;
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
    let input_rows = rows.len().clamp(1, MAX_INPUT_ROWS);
    let menu = app.menu();
    let busy = app.focused().is_some_and(|v| v.status.busy());
    let suggestions = match app.focused() {
        Some(v) if !busy && v.suggestions.len() > 1 && app.editor.is_empty() => {
            v.suggestions.clone()
        }
        _ => Vec::new(),
    };

    let [header, body, activity, menu_area, input, sugg, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(if busy { 1 } else { 0 }),
        Constraint::Length(menu.len().min(8) as u16),
        Constraint::Length(input_rows as u16 + 2),
        Constraint::Length(if suggestions.is_empty() { 0 } else { 1 }),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f.buffer_mut(), header, app, &theme);

    let show_sidebar = app.sidebar && area.width >= 90;
    let (side, main) = if show_sidebar {
        let [s, m] =
            Layout::horizontal([Constraint::Length(SIDEBAR_W), Constraint::Min(20)]).areas(body);
        (Some(s), m)
    } else {
        (None, body)
    };
    match side {
        Some(s) => draw_sidebar(f.buffer_mut(), s, app, &theme),
        None => {
            app.hit.sidebar = Rect::default();
            app.hit.sidebar_rows.clear();
        }
    }
    let transcript = Rect {
        x: main.x + 2,
        y: main.y,
        width: main.width.saturating_sub(3),
        height: main.height,
    };
    draw_transcript(f.buffer_mut(), transcript, app, &theme);

    if busy {
        draw_activity(f.buffer_mut(), activity, app, &theme);
    }
    if !menu.is_empty() {
        draw_menu(f.buffer_mut(), menu_area, &menu, app.menu_sel, &theme);
    }

    // Input box.
    let border = if app.mode == Mode::Input {
        theme.dim()
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
    if app.editor.is_empty() {
        let placeholder = match app.focused() {
            Some(v) if !busy && !v.suggestions.is_empty() => format!("{}   ⇥", v.suggestions[0]),
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
    if app.mode == Mode::Input {
        f.set_cursor_position(Position {
            x: inner.x + cursor.1 as u16,
            y: inner.y + (cursor.0 - first_row) as u16,
        });
    }

    if !suggestions.is_empty() {
        let mut spans = vec![Span::styled("  ⇥ ", theme.accent())];
        for (i, s) in suggestions.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("   ·   ", theme.dim()));
            }
            spans.push(Span::styled(s.clone(), theme.muted()));
        }
        f.buffer_mut()
            .set_line(sugg.x, sugg.y, &Line::from(spans), sugg.width);
    }

    draw_status(f.buffer_mut(), status, app, &theme);

    if app.mode == Mode::Help {
        draw_help(f, area, &theme);
    }
}

fn draw_header(buf: &mut Buffer, area: Rect, app: &App, theme: &Theme) {
    let store = app.hub.store();
    let home = std::env::var("HOME").unwrap_or_default();
    let mut cwd = store.session.cwd.display().to_string();
    if !home.is_empty() && cwd.starts_with(&home) {
        cwd = format!("~{}", &cwd[home.len()..]);
    }
    let left = Line::from(vec![
        Span::styled(" grove", theme.accent().add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {cwd}"), theme.muted()),
        Span::styled(format!("  · {}", store.session.id), theme.dim()),
    ]);
    buf.set_line(area.x, area.y, &left, area.width);

    // Watchers at a glance, and a pending self-upgrade.
    let mut spans = Vec::new();
    if let Some(summary) = app.hub.upgrade_proposal() {
        let short: String = summary.chars().take(40).collect();
        spans.push(Span::styled(
            format!("⬆ upgrade ready: {short} (/upgrade)  "),
            Style::new().fg(theme.green),
        ));
    }
    for v in &app.views {
        if !v.meta.role.is_watcher() {
            continue;
        }
        let (dot, style) = status_glyph(&v.status, app.spinner, theme);
        spans.push(Span::styled(dot, style));
        spans.push(Span::styled(format!(" {}  ", v.meta.id), theme.dim()));
    }
    let right = Line::from(spans);
    let w = line_width(&right) as u16;
    if w + 40 < area.width {
        buf.set_line(area.x + area.width - w, area.y, &right, w);
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

fn status_word(status: &Status) -> String {
    match status {
        Status::Thinking => "thinking".into(),
        Status::Tools(n) => format!("tools {n}"),
        Status::Idle => "idle".into(),
        Status::Paused => "paused".into(),
        Status::Stopped => "stopped".into(),
        Status::Error(_) => "error".into(),
    }
}

fn draw_sidebar(buf: &mut Buffer, area: Rect, app: &mut App, theme: &Theme) {
    buf.set_style(area, Style::new().bg(theme.panel_bg));
    app.hit.sidebar = area;
    app.hit.sidebar_rows.clear();
    let w = area.width.saturating_sub(1) as usize;
    let mut y = area.y + 1;
    buf.set_line(
        area.x + 1,
        y,
        &Line::from(Span::styled(
            "AGENTS",
            theme.dim().add_modifier(Modifier::BOLD),
        )),
        area.width,
    );
    y += 2;
    let mut watchers_started = false;
    for i in 0..app.views.len() {
        if y >= area.y + area.height {
            break;
        }
        let v = &app.views[i];
        if v.meta.role.is_watcher() && !watchers_started {
            watchers_started = true;
            y += 1;
            if y >= area.y + area.height {
                break;
            }
            buf.set_line(
                area.x + 1,
                y,
                &Line::from(Span::styled(
                    "WATCHERS",
                    theme.dim().add_modifier(Modifier::BOLD),
                )),
                area.width,
            );
            y += 2;
            if y >= area.y + area.height {
                break;
            }
        }
        let focused = i == app.focus;
        let (glyph, gstyle) = status_glyph(&v.status, app.spinner, theme);
        let indent = if v.meta.role == Role::Sub { "  " } else { "" };
        let marker = if focused {
            Span::styled("▌", theme.accent())
        } else {
            Span::raw(" ")
        };
        let name_style = if focused { theme.bold() } else { Style::new() };
        let mut spans = vec![
            marker,
            Span::raw(indent),
            Span::styled(glyph, gstyle),
            Span::raw(" "),
            Span::styled(v.meta.id.clone(), name_style),
        ];
        if v.unread {
            spans.push(Span::styled(" •", theme.accent()));
        }
        let word = status_word(&v.status);
        let left_w = line_width(&Line::from(spans.clone()));
        let pad = w.saturating_sub(left_w + word.width() + 1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(word, theme.dim()));
        let row_style = if focused {
            Style::new().bg(theme.sel_bg)
        } else {
            Style::new()
        };
        let row = Rect {
            x: area.x,
            y,
            width: area.width.saturating_sub(1),
            height: 1,
        };
        buf.set_style(row, row_style);
        buf.set_line(area.x, y, &Line::from(spans), area.width.saturating_sub(1));
        app.hit.sidebar_rows.push((y, i));
        y += 1;
    }
    // Hints at the bottom of the panel.
    let hint_y = area.y + area.height.saturating_sub(2);
    if hint_y > y {
        buf.set_line(
            area.x + 1,
            hint_y,
            &Line::from(Span::styled("ctrl+n/p switch", theme.dim())),
            area.width,
        );
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
        Mode::Navigate => "↑↓ select · enter expand · y copy · esc back",
        Mode::Help => "any key to close",
        Mode::Input => {
            if app.focused().is_some_and(|v| v.scroll > 0) {
                "esc jump to bottom · ctrl+o freeze"
            } else {
                "shift+tab effort · shift+↑ browse · ctrl+o freeze · ? help"
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
        ("ctrl+t", "toggle agents panel"),
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
