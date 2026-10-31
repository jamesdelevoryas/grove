//! UI state and input handling.

use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use super::editor::Editor;
use super::pager::{Pager, PagerAction};
use super::theme::Theme;
use super::transcript::Transcript;
use std::path::PathBuf;

use crate::client::Remote;
use crate::protocol::*;
use crate::store::{self, SessionInfo};
use crate::wire::{Reply, Request, SessionSummary};

const TOAST_FOR: Duration = Duration::from_secs(6);

pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "keys and commands"),
    ("/clear", "fresh conversation for the focused agent"),
    ("/effort", "low · medium · high · xhigh · max"),
    ("/model", "set the focused agent's model"),
    ("/spawn", "<name> <task> — start a subagent"),
    ("/interrupt", "interrupt the focused agent"),
    ("/stop", "stop the focused agent"),
    ("/retry", "retry the last turn after an error"),
    ("/upgrade", "rebuild grove and restart into it"),
    ("/improve", "<idea> — ask the improver"),
    ("/memory", "<question> — ask the memory agent"),
    ("/supervisor", "<text> — talk to the supervisor"),
    ("/focus", "<agent> — switch to an agent"),
    ("/cost", "token usage and cost"),
    ("/sessions", "back to the session list"),
    ("/rm", "delete the highlighted session (list)"),
    ("/quit", "exit grove (sessions keep running)"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Every session, with a prompt box that starts new ones.
    List,
    /// Inside one session.
    Session,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Input,
    /// Moving a cursor over transcript entries.
    Navigate,
    Help,
}

pub struct Toast {
    pub level: Level,
    pub text: String,
    pub at: Instant,
}

pub struct AgentView {
    pub meta: AgentMeta,
    pub status: Status,
    pub transcript: Transcript,
    /// Lines scrolled up from the bottom; 0 follows new output.
    pub scroll: usize,
    pub last_total: usize,
    pub unread: bool,
    pub suggestions: Vec<String>,
    pub busy_since: Option<Instant>,
    pub selected: Option<usize>,
}

pub enum Action {
    None,
    /// The frozen view opened: release the mouse so the terminal can select text.
    Freeze,
    /// It closed: take the mouse back.
    Unfreeze,
    Redraw,
}

/// Where things were drawn last frame, for mouse hit-testing.
#[derive(Default)]
pub struct Hit {
    pub transcript: Rect,
    /// Entry index for each transcript row on screen.
    pub rows: Vec<Option<usize>>,
    /// Screen row → index into the session list.
    pub list_rows: Vec<(u16, usize)>,
}

pub struct App {
    pub remote: Remote,
    pub home: PathBuf,
    /// Where new sessions start.
    pub cwd: PathBuf,
    pub screen: Screen,
    /// The attached session.
    pub session: Option<SessionInfo>,
    pub sessions: Vec<SessionSummary>,
    /// The highlighted session, or None when the prompt box has focus (and the cursor).
    pub list_sel: Option<usize>,
    /// The user has moved around the list; until then it follows the newest session.
    list_touched: bool,
    pub proposal: Option<String>,
    /// Set when the server restarts into a new build: the UI execs it too.
    pub exec: Option<PathBuf>,
    delete_armed: Option<(String, Instant)>,
    /// Between Attached and Synced: replaying history, so no toasts or unread marks.
    syncing: bool,
    last_interrupt: Option<Instant>,
    /// --model / --effort from the command line, for sessions started here.
    pub new_model: Option<String>,
    pub new_effort: Option<Effort>,
    pub theme: Theme,
    pub views: Vec<AgentView>,
    pub focus: usize,
    pub editor: Editor,
    history: Vec<String>,
    hist_pos: Option<usize>,
    draft: String,
    pub mode: Mode,
    pub menu_sel: usize,
    quit_armed: Option<Instant>,
    pub quit: bool,
    pub spinner: usize,
    pub toast: Option<Toast>,
    suggestion_next: usize,
    pub transcript_height: usize,
    pub hit: Hit,
    /// Text to put on the clipboard after the next draw.
    pub copy: Option<String>,
    /// The frozen view (ctrl+o), when open.
    pub pager: Option<Pager>,
    pub screen_width: u16,
}

impl App {
    pub fn new(remote: Remote, theme: Theme, home: PathBuf, cwd: PathBuf) -> Self {
        let history = store::load_history(&home.join("history"));
        Self {
            remote,
            home,
            cwd,
            screen: Screen::List,
            session: None,
            sessions: Vec::new(),
            list_sel: None,
            list_touched: false,
            proposal: None,
            exec: None,
            delete_armed: None,
            syncing: false,
            last_interrupt: None,
            new_model: None,
            new_effort: None,
            theme,
            views: Vec::new(),
            focus: 0,
            editor: Editor::default(),
            history,
            hist_pos: None,
            draft: String::new(),
            mode: Mode::Input,
            menu_sel: 0,
            quit_armed: None,
            quit: false,
            spinner: 0,
            toast: None,
            suggestion_next: 0,
            transcript_height: 20,
            hit: Hit::default(),
            copy: None,
            pager: None,
            screen_width: 100,
        }
    }

    /// Sends input to an agent of the attached session.
    fn input(&self, agent: &str, input: Input) -> anyhow::Result<()> {
        let Some(info) = &self.session else {
            anyhow::bail!("not in a session");
        };
        self.remote.send(Request::Input {
            session: info.id.clone(),
            agent: agent.to_string(),
            input,
        });
        Ok(())
    }

    fn find_role(&self, role: Role) -> Option<AgentId> {
        for v in &self.views {
            if v.meta.role == role {
                return Some(v.meta.id.clone());
            }
        }
        None
    }

    pub fn on_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Hello { build, .. } => {
                if build != crate::wire::build_id() {
                    self.toast(
                        Level::Warn,
                        "the grove server is running a different build; /upgrade restarts it",
                    );
                }
            }
            Reply::Sessions { list } => {
                self.sessions = list;
                let n = self.sessions.len();
                // Open on the newest session rather than in the prompt box.
                if !self.list_touched && self.list_sel.is_none() && n > 0 {
                    self.list_sel = Some(0);
                }
                if let Some(i) = self.list_sel {
                    self.list_sel = if n == 0 { None } else { Some(i.min(n - 1)) };
                }
            }
            Reply::Created { session } => {
                if self.screen == Screen::List {
                    self.toast(Level::Info, format!("started session {session}"));
                }
            }
            Reply::Attached { info } => {
                self.syncing = true;
                self.views.clear();
                self.focus = 0;
                self.session = Some(info);
                self.screen = Screen::Session;
                self.mode = Mode::Input;
                self.pager = None;
                self.suggestion_next = 0;
            }
            Reply::Event { session, event } => {
                if self.session.as_ref().is_some_and(|s| s.id == session) {
                    self.on_event(event);
                }
            }
            Reply::Proposal { summary } => self.proposal = summary,
            Reply::Error { message } => self.toast(Level::Alert, message),
            Reply::Info { message } => self.toast(Level::Info, message),
            Reply::Restarting { binary } => self.exec = Some(binary),
            Reply::Synced { session } => {
                if self.session.as_ref().is_some_and(|s| s.id == session) {
                    self.syncing = false;
                }
            }
            Reply::Stopped { session } => {
                if self.session.as_ref().is_some_and(|s| s.id == session) {
                    self.leave_session();
                    self.toast(Level::Info, format!("session {session} was stopped"));
                }
            }
        }
    }

    pub fn enter_session(&mut self, id: &str) {
        self.remote.send(Request::Attach {
            session: id.to_string(),
        });
    }

    pub fn leave_session(&mut self) {
        self.remote.send(Request::Detach);
        // Keep the list's cursor on the session we just left.
        if let Some(info) = &self.session
            && let Some(i) = self.sessions.iter().position(|s| s.id == info.id)
        {
            self.list_sel = Some(i);
            self.list_touched = true;
        }
        self.session = None;
        self.views.clear();
        self.screen = Screen::List;
        self.mode = Mode::Input;
        self.pager = None;
    }

    fn selected_session(&self) -> Option<String> {
        let i = self.list_sel?;
        self.sessions.get(i).map(|s| s.id.clone())
    }

    fn list_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let empty = self.editor.is_empty();
        let n = self.sessions.len();
        match key.code {
            KeyCode::Char('c') if ctrl => self.ctrl_c(),
            KeyCode::Char('d') if ctrl && empty => self.arm_quit(),
            KeyCode::Char('l') if ctrl => return Action::Redraw,
            _ => {}
        }
        if matches!(key.code, KeyCode::Char('c' | 'd' | 'l')) && ctrl {
            return Action::None;
        }
        self.list_touched = true;
        match self.list_sel {
            // A session is highlighted: arrows move, enter opens. The prompt box has no
            // cursor until you move past the bottom or start typing.
            Some(i) => match key.code {
                KeyCode::Up => self.list_sel = Some(i.saturating_sub(1)),
                KeyCode::Char('p') if ctrl => self.list_sel = Some(i.saturating_sub(1)),
                KeyCode::Down => self.list_sel = if i + 1 < n { Some(i + 1) } else { None },
                KeyCode::Char('n') if ctrl => {
                    self.list_sel = if i + 1 < n { Some(i + 1) } else { None }
                }
                KeyCode::Home => self.list_sel = Some(0),
                KeyCode::End => self.list_sel = Some(n.saturating_sub(1)),
                KeyCode::Enter | KeyCode::Right => {
                    if let Some(id) = self.selected_session() {
                        self.enter_session(&id);
                    }
                }
                KeyCode::Char('x') if ctrl => {
                    if let Some(id) = self.selected_session() {
                        self.remote.send(Request::Stop { session: id });
                    }
                }
                KeyCode::Delete => self.delete_selected(),
                KeyCode::Esc => {}
                // Typing starts a new session: focus moves to the prompt, keeping the key.
                KeyCode::Char(c) if !ctrl && !alt => {
                    self.list_sel = None;
                    self.editor.insert_char(c);
                    self.menu_sel = 0;
                }
                _ => {}
            },
            // The prompt box has focus.
            None => match key.code {
                KeyCode::Up if empty && n > 0 => self.list_sel = Some(n - 1),
                KeyCode::Char('p') if ctrl && n > 0 => self.list_sel = Some(n - 1),
                KeyCode::Esc if self.menu().is_empty() && n > 0 => self.list_sel = Some(n - 1),
                _ => self.input_key(key, ctrl, alt, shift),
            },
        }
        Action::None
    }

    /// Deletes the highlighted session; asks for a second press first.
    fn delete_selected(&mut self) {
        let Some(id) = self.selected_session() else {
            return;
        };
        if let Some((armed, at)) = &self.delete_armed
            && *armed == id
            && at.elapsed() < Duration::from_secs(3)
        {
            self.delete_armed = None;
            self.remote.send(Request::Delete { session: id });
            return;
        }
        self.delete_armed = Some((id.clone(), Instant::now()));
        self.toast(Level::Warn, format!("delete {id}? press again to confirm"));
    }

    /// Freezes the focused transcript into a pager at full width.
    fn open_pager(&mut self) -> bool {
        let width = (self.screen_width as usize).saturating_sub(2).max(20);
        let theme = self.theme;
        let Some(view) = self.views.get_mut(self.focus) else {
            return false;
        };
        let heights = view.transcript.layout(width, &theme);
        let mut lines = Vec::new();
        for i in 0..heights.len() {
            for line in view.transcript.lines(i) {
                lines.push(line.clone());
            }
            lines.push(ratatui::text::Line::default());
        }
        if lines.is_empty() {
            self.toast(Level::Info, "nothing to freeze yet");
            return false;
        }
        lines.pop();
        self.pager = Some(Pager::new(&view.meta.id, lines));
        true
    }

    pub fn focused(&self) -> Option<&AgentView> {
        self.views.get(self.focus)
    }

    fn focused_mut(&mut self) -> Option<&mut AgentView> {
        self.views.get_mut(self.focus)
    }

    fn focused_id(&self) -> String {
        self.focused()
            .map(|v| v.meta.id.clone())
            .unwrap_or_default()
    }

    pub fn animating(&self) -> bool {
        if self.screen == Screen::List {
            return self.sessions.iter().any(|s| s.busy);
        }
        for v in &self.views {
            if v.status.busy() {
                return true;
            }
        }
        false
    }

    pub fn toast(&mut self, level: Level, text: impl Into<String>) {
        self.toast = Some(Toast {
            level,
            text: text.into(),
            at: Instant::now(),
        });
    }

    pub fn toast_deadline(&self) -> Option<Instant> {
        self.toast.as_ref().map(|t| t.at + TOAST_FOR)
    }

    pub fn expire_toast(&mut self) -> bool {
        if let Some(t) = &self.toast
            && t.at.elapsed() >= TOAST_FOR
        {
            self.toast = None;
            return true;
        }
        false
    }

    // ---- hub events ----

    pub fn on_event(&mut self, event: Event) {
        if let EventKind::Spawned(meta) = &event.kind {
            if self.views.iter().any(|v| v.meta.id == meta.id) {
                return;
            }
            let view = AgentView {
                meta: meta.clone(),
                status: Status::Idle,
                transcript: Transcript::default(),
                scroll: 0,
                last_total: 0,
                unread: false,
                suggestions: Vec::new(),
                busy_since: None,
                selected: None,
            };
            // Working agents first (in start order), watchers after.
            let mut at = self.views.len();
            if !meta.role.is_watcher() {
                for (i, v) in self.views.iter().enumerate() {
                    if v.meta.role.is_watcher() {
                        at = i;
                        break;
                    }
                }
            }
            let focused_id = self.focused_id();
            self.views.insert(at, view);
            if let Some(i) = self.views.iter().position(|v| v.meta.id == focused_id) {
                self.focus = i;
            }
            return;
        }
        let Some(i) = self.views.iter().position(|v| v.meta.id == event.agent) else {
            return;
        };
        let focused = i == self.focus;
        let syncing = self.syncing;
        let mut toast = None;
        {
            let view = &mut self.views[i];
            match &event.kind {
                EventKind::Status(status) => {
                    if status.busy() && view.busy_since.is_none() {
                        view.busy_since = Some(Instant::now());
                    }
                    if !status.busy() {
                        view.busy_since = None;
                    }
                    if *status == Status::Thinking {
                        view.suggestions.clear();
                    }
                    view.status = status.clone();
                }
                EventKind::Meta(meta) => view.meta = meta.clone(),
                EventKind::Suggestions(items) => {
                    if !view.status.busy() {
                        view.suggestions = items.clone();
                    }
                }
                kind => {
                    if let EventKind::Notice { level, text } = kind
                        && *level != Level::Info
                    {
                        toast = Some((
                            *level,
                            format!("{}: {}", view.meta.id, text.lines().next().unwrap_or("")),
                        ));
                    }
                    if matches!(kind, EventKind::UserText(_)) {
                        view.suggestions.clear();
                    }
                    if view.transcript.apply(kind)
                        && !focused
                        && !syncing
                        && !matches!(kind, EventKind::History(_))
                    {
                        view.unread = true;
                    }
                }
            }
        }
        if let Some((level, text)) = toast
            && !syncing
        {
            self.toast(level, text);
        }
    }

    // ---- keys ----

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        // The frozen view takes every key while it's open.
        if let Some(pager) = &mut self.pager {
            return match pager.key(key) {
                PagerAction::Close => {
                    self.pager = None;
                    Action::Unfreeze
                }
                PagerAction::Copy(text) => {
                    self.copy = Some(text);
                    Action::None
                }
                PagerAction::None => Action::None,
            };
        }
        if self.screen == Screen::List {
            return self.list_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // Global keys.
        match key.code {
            KeyCode::Char('o') if ctrl => {
                return if self.open_pager() {
                    Action::Freeze
                } else {
                    Action::None
                };
            }
            KeyCode::Char('l') if ctrl => return Action::Redraw,
            KeyCode::Char('c') if ctrl => {
                self.ctrl_c();
                return Action::None;
            }
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => {
                self.arm_quit();
                return Action::None;
            }
            KeyCode::Char('n') if ctrl => {
                self.cycle_focus(1);
                return Action::None;
            }
            KeyCode::Char('p') if ctrl => {
                self.cycle_focus(-1);
                return Action::None;
            }
            KeyCode::Char(c) if alt && c.is_ascii_digit() && c != '0' => {
                let n = c as usize - '1' as usize;
                if n < self.views.len() {
                    self.set_focus(n);
                }
                return Action::None;
            }
            KeyCode::PageUp => {
                self.scroll_by(self.transcript_height.saturating_sub(2) as isize);
                return Action::None;
            }
            KeyCode::PageDown => {
                self.scroll_by(-(self.transcript_height.saturating_sub(2) as isize));
                return Action::None;
            }
            KeyCode::BackTab => {
                self.cycle_effort();
                return Action::None;
            }
            _ => {}
        }

        match self.mode {
            Mode::Help => {
                self.mode = Mode::Input;
                Action::None
            }
            Mode::Navigate => {
                self.nav_key(key);
                Action::None
            }
            Mode::Input => {
                if shift && key.code == KeyCode::Up || ctrl && key.code == KeyCode::Char('g') {
                    self.enter_nav();
                    return Action::None;
                }
                self.input_key(key, ctrl, alt, shift);
                Action::None
            }
        }
    }

    fn ctrl_c(&mut self) {
        if !self.editor.is_empty() {
            self.editor.clear();
            return;
        }
        if self.mode != Mode::Input {
            self.mode = Mode::Input;
            return;
        }
        if self.focused().is_some_and(|v| v.status.busy()) {
            let id = self.focused_id();
            let _ = self.input(&id, Input::Interrupt);
            return;
        }
        self.arm_quit();
    }

    fn arm_quit(&mut self) {
        if let Some(at) = self.quit_armed
            && at.elapsed() < Duration::from_secs(2)
        {
            self.quit = true;
            return;
        }
        self.quit_armed = Some(Instant::now());
        self.toast(
            Level::Info,
            "press ctrl+c again to exit (sessions keep running)",
        );
    }

    fn cycle_focus(&mut self, delta: isize) {
        if self.views.is_empty() {
            return;
        }
        let n = self.views.len() as isize;
        let next = (self.focus as isize + delta).rem_euclid(n) as usize;
        self.set_focus(next);
    }

    pub fn set_focus(&mut self, i: usize) {
        if i >= self.views.len() {
            return;
        }
        self.focus = i;
        self.views[i].unread = false;
        self.suggestion_next = 0;
        if self.mode == Mode::Navigate {
            self.mode = Mode::Input;
        }
    }

    fn cycle_effort(&mut self) {
        let Some(view) = self.focused() else { return };
        let next = view.meta.effort.next();
        let id = view.meta.id.clone();
        let _ = self.input(&id, Input::SetEffort(next));
        self.toast(Level::Info, format!("{id}: effort {}", next.as_str()));
    }

    fn scroll_by(&mut self, delta: isize) {
        if let Some(v) = self.focused_mut() {
            let max = v.last_total;
            let next = (v.scroll as isize + delta).clamp(0, max as isize);
            v.scroll = next as usize;
        }
    }

    /// Slash commands matching what's typed, while a command name is being typed.
    pub fn menu(&self) -> Vec<(&'static str, &'static str)> {
        let text = self.editor.text();
        if self.mode != Mode::Input || !text.starts_with('/') || text.contains(char::is_whitespace)
        {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (name, desc) in COMMANDS {
            if name.starts_with(text) {
                out.push((*name, *desc));
            }
        }
        out
    }

    fn input_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
        let menu = self.menu();
        if !menu.is_empty() {
            match key.code {
                KeyCode::Up => {
                    self.menu_sel = (self.menu_sel + menu.len() - 1) % menu.len();
                    return;
                }
                KeyCode::Down => {
                    self.menu_sel = (self.menu_sel + 1) % menu.len();
                    return;
                }
                KeyCode::Tab => {
                    let (name, _) = menu[self.menu_sel.min(menu.len() - 1)];
                    self.editor.set(&format!("{name} "));
                    return;
                }
                KeyCode::Enter if !alt && !shift => {
                    let (name, _) = menu[self.menu_sel.min(menu.len() - 1)];
                    self.editor.set(name);
                    self.submit();
                    return;
                }
                KeyCode::Esc => {
                    self.editor.clear();
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter if alt || shift => self.editor.insert_char('\n'),
            KeyCode::Char('j') if ctrl => self.editor.insert_char('\n'),
            KeyCode::Enter => self.submit(),
            KeyCode::Left
                if self.screen == Screen::Session && self.editor.is_empty() && !alt && !ctrl =>
            {
                self.leave_session()
            }
            KeyCode::Esc
                if self.screen == Screen::Session
                    && self.editor.is_empty()
                    && self
                        .last_interrupt
                        .is_none_or(|t| t.elapsed() > Duration::from_millis(1500))
                    && !self
                        .focused()
                        .is_some_and(|v| v.status.busy() || v.scroll > 0) =>
            {
                self.leave_session()
            }
            KeyCode::Esc => {
                if self.focused().is_some_and(|v| v.status.busy()) {
                    let id = self.focused_id();
                    self.last_interrupt = Some(Instant::now());
                    let _ = self.input(&id, Input::Interrupt);
                } else if let Some(v) = self.focused_mut() {
                    v.scroll = 0;
                }
            }
            KeyCode::Tab => self.next_suggestion(),
            KeyCode::Char('?') if self.editor.is_empty() => self.mode = Mode::Help,
            KeyCode::Char('a') if ctrl => self.editor.home(),
            KeyCode::Char('e') if ctrl => self.editor.end(),
            KeyCode::Char('k') if ctrl => self.editor.kill_to_end(),
            KeyCode::Char('u') if ctrl => self.editor.kill_to_start(),
            KeyCode::Char('w') if ctrl => self.editor.delete_word_back(),
            KeyCode::Char('y') if ctrl => self.editor.yank(),
            KeyCode::Char('b') if ctrl => self.editor.left(),
            KeyCode::Char('f') if ctrl => self.editor.right(),
            KeyCode::Char('b') if alt => self.editor.word_left(),
            KeyCode::Char('f') if alt => self.editor.word_right(),
            KeyCode::Backspace if alt || ctrl => self.editor.delete_word_back(),
            KeyCode::Left if alt || ctrl => self.editor.word_left(),
            KeyCode::Right if alt || ctrl => self.editor.word_right(),
            KeyCode::Char(c) if !ctrl => {
                self.editor.insert_char(c);
                self.menu_sel = 0;
            }
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::Up if !self.editor.up() => self.history_prev(),
            KeyCode::Down if !self.editor.down() => self.history_next(),
            _ => {}
        }
    }

    pub fn paste(&mut self, text: &str) {
        // Pasting on the list goes to the prompt, like typing.
        if self.screen == Screen::List {
            self.list_sel = None;
        }
        if self.mode != Mode::Input {
            self.mode = Mode::Input;
        }
        self.editor.insert(text);
    }

    fn next_suggestion(&mut self) {
        let Some(view) = self.focused() else { return };
        if view.suggestions.is_empty() {
            return;
        }
        let current = self.editor.text().to_string();
        let mut i = self.suggestion_next % view.suggestions.len();
        // Tab on a filled-in suggestion moves to the next one.
        if view.suggestions[i] == current {
            i = (i + 1) % view.suggestions.len();
        }
        if !current.is_empty() && !view.suggestions.contains(&current) {
            return;
        }
        let s = view.suggestions[i].clone();
        self.editor.set(&s);
        self.suggestion_next = i + 1;
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.hist_pos {
            None => {
                self.draft = self.editor.text().to_string();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.hist_pos = Some(pos);
        let entry = self.history[pos].clone();
        self.editor.set(&entry);
    }

    fn history_next(&mut self) {
        let Some(pos) = self.hist_pos else { return };
        if pos + 1 >= self.history.len() {
            self.hist_pos = None;
            let draft = std::mem::take(&mut self.draft);
            self.editor.set(&draft);
            return;
        }
        self.hist_pos = Some(pos + 1);
        let entry = self.history[pos + 1].clone();
        self.editor.set(&entry);
    }

    fn submit(&mut self) {
        let text = self.editor.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.editor.clear();
        self.hist_pos = None;
        self.menu_sel = 0;
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
            store::append_history(&self.home.join("history"), &text);
        }
        if text.starts_with('/') {
            self.command(&text);
            return;
        }
        if self.screen == Screen::List {
            self.remote.send(Request::Create {
                cwd: self.cwd.clone(),
                prompt: Some(text),
                model: self.new_model.clone(),
                effort: self.new_effort,
                attach: false,
            });
            return;
        }
        let id = self.focused_id();
        if let Some(v) = self.focused_mut() {
            v.scroll = 0;
        }
        if let Err(e) = self.input(&id, Input::User(text)) {
            self.toast(Level::Alert, format!("{e:#}"));
        }
    }

    fn command(&mut self, line: &str) {
        let (cmd, arg) = match line.split_once(char::is_whitespace) {
            Some((c, a)) => (c, a.trim()),
            None => (line, ""),
        };
        let id = self.focused_id();
        let result: anyhow::Result<()> = match cmd {
            "/help" => {
                self.mode = Mode::Help;
                Ok(())
            }
            "/quit" | "/exit" => {
                self.quit = true;
                Ok(())
            }
            "/sessions" => {
                if self.screen == Screen::Session {
                    self.leave_session();
                }
                Ok(())
            }
            "/rm" if self.screen == Screen::List => {
                self.delete_selected();
                Ok(())
            }
            "/stop" if self.screen == Screen::List => {
                if let Some(session) = self.selected_session() {
                    self.remote.send(Request::Stop { session });
                }
                Ok(())
            }
            "/clear" => self.input(&id, Input::Clear),
            "/effort" => match Effort::parse(arg) {
                Some(e) => {
                    self.toast(Level::Info, format!("{id}: effort {}", e.as_str()));
                    self.input(&id, Input::SetEffort(e))
                }
                None => {
                    self.toast(Level::Warn, "effort: low, medium, high, xhigh, or max");
                    Ok(())
                }
            },
            "/model" => {
                if arg.is_empty() {
                    let model = self
                        .focused()
                        .map(|v| v.meta.model.clone())
                        .unwrap_or_default();
                    self.toast(Level::Info, format!("{id}: {model}"));
                    Ok(())
                } else {
                    self.toast(Level::Info, format!("{id}: model {arg}"));
                    self.input(&id, Input::SetModel(arg.to_string()))
                }
            }
            "/spawn" => match arg.split_once(char::is_whitespace) {
                Some((name, task)) => match &self.session {
                    Some(info) => {
                        self.remote.send(Request::Spawn {
                            session: info.id.clone(),
                            parent: id.clone(),
                            name: name.to_string(),
                            task: task.trim().to_string(),
                        });
                        self.toast(Level::Info, format!("starting {name}"));
                        Ok(())
                    }
                    None => Err(anyhow::anyhow!("not in a session")),
                },
                None => {
                    self.toast(Level::Warn, "usage: /spawn <name> <task>");
                    Ok(())
                }
            },
            "/interrupt" => self.input(if arg.is_empty() { &id } else { arg }, Input::Interrupt),
            "/stop" => self.input(if arg.is_empty() { &id } else { arg }, Input::Stop),
            "/retry" => self.input(&id, Input::Retry),
            "/upgrade" => {
                self.remote.send(Request::Upgrade {
                    session: self.session.as_ref().map(|s| s.id.clone()),
                });
                Ok(())
            }
            "/improve" | "/memory" | "/supervisor" => {
                let role = match cmd {
                    "/improve" => Role::Improver,
                    "/memory" => Role::Memory,
                    _ => Role::Supervisor,
                };
                match self.find_role(role) {
                    Some(target) => {
                        if arg.is_empty() {
                            if let Some(i) = self.views.iter().position(|v| v.meta.id == target) {
                                self.set_focus(i);
                            }
                            Ok(())
                        } else {
                            self.toast(Level::Info, format!("sent to {target}"));
                            self.input(&target, Input::User(arg.to_string()))
                        }
                    }
                    None => {
                        self.toast(Level::Warn, format!("no {} agent is running", &cmd[1..]));
                        Ok(())
                    }
                }
            }
            "/focus" => match self.views.iter().position(|v| v.meta.id == arg) {
                Some(i) => {
                    self.set_focus(i);
                    Ok(())
                }
                None => {
                    self.toast(Level::Warn, format!("no agent {arg}"));
                    Ok(())
                }
            },
            "/cost" => {
                let mut total = 0.0;
                let mut parts = Vec::new();
                for v in &self.views {
                    let c = v.meta.usage.cost();
                    total += c;
                    if c > 0.0 {
                        parts.push(format!("{} ${c:.2}", v.meta.id));
                    }
                }
                self.toast(
                    Level::Info,
                    format!("${total:.2} total · {}", parts.join(" · ")),
                );
                Ok(())
            }
            other => {
                self.toast(Level::Warn, format!("unknown command {other}"));
                Ok(())
            }
        };
        if let Err(e) = result {
            self.toast(Level::Alert, format!("{e:#}"));
        }
    }

    // ---- transcript navigation ----

    fn enter_nav(&mut self) {
        let Some(view) = self.focused_mut() else {
            return;
        };
        if view.transcript.entries.is_empty() {
            return;
        }
        let last = view.transcript.entries.len() - 1;
        view.selected = Some(view.selected.unwrap_or(last).min(last));
        self.mode = Mode::Navigate;
    }

    fn nav_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(view) = self.views.get_mut(self.focus) else {
            self.mode = Mode::Input;
            return;
        };
        let n = view.transcript.entries.len();
        if n == 0 {
            self.mode = Mode::Input;
            return;
        }
        let sel = view.selected.unwrap_or(n - 1).min(n - 1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => view.selected = Some(sel.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => {
                if sel + 1 >= n && key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.mode = Mode::Input;
                } else {
                    view.selected = Some((sel + 1).min(n - 1));
                }
            }
            KeyCode::Char('g') if !ctrl => view.selected = Some(0),
            KeyCode::Char('G') => view.selected = Some(n - 1),
            KeyCode::Enter
            | KeyCode::Char(' ')
            | KeyCode::Right
            | KeyCode::Char('l')
            | KeyCode::Left
            | KeyCode::Char('h') => {
                view.transcript.toggle(sel);
            }
            KeyCode::Char('y') | KeyCode::Char('c') => {
                let text = view.transcript.plain(sel);
                let lines = text.lines().count();
                self.copy = Some(text);
                self.toast(
                    Level::Info,
                    format!("copied {lines} line{}", if lines == 1 { "" } else { "s" }),
                );
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => self.mode = Mode::Input,
            KeyCode::Char(c) if !ctrl => {
                // Typing goes back to the prompt.
                self.mode = Mode::Input;
                self.editor.insert_char(c);
            }
            _ => {}
        }
    }

    // ---- mouse ----

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        let inside = |r: Rect| {
            ev.column >= r.x
                && ev.column < r.x + r.width
                && ev.row >= r.y
                && ev.row < r.y + r.height
        };
        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll_by(3),
            MouseEventKind::ScrollDown => self.scroll_by(-3),
            MouseEventKind::Down(MouseButton::Left) if self.screen == Screen::List => {
                let mut target = None;
                for (row, i) in &self.hit.list_rows {
                    if *row == ev.row {
                        target = Some(*i);
                    }
                }
                if let Some(i) = target {
                    // A click on the highlighted row opens it.
                    if self.list_sel == Some(i)
                        && let Some(id) = self.selected_session()
                    {
                        self.enter_session(&id);
                    }
                    self.list_sel = Some(i);
                    self.list_touched = true;
                }
            }
            MouseEventKind::Down(MouseButton::Left) if inside(self.hit.transcript) => {
                let row = (ev.row - self.hit.transcript.y) as usize;
                if let Some(Some(entry)) = self.hit.rows.get(row).copied() {
                    let focus = self.focus;
                    if let Some(view) = self.views.get_mut(focus) {
                        if view.selected == Some(entry) && self.mode == Mode::Navigate {
                            view.transcript.toggle(entry);
                        }
                        view.selected = Some(entry);
                    }
                    self.mode = Mode::Navigate;
                }
            }
            _ => {}
        }
    }
}
