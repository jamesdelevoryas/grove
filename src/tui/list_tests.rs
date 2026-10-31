//! Focus on the session list: sessions or the prompt box.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::Theme;
use super::app::App;
use crate::client::Remote;
use crate::wire::{Reply, SessionSummary};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn list_app(n: usize) -> App {
    let (remote, _rx) = Remote::detached();
    let mut app = App::new(
        remote,
        Theme::dark(),
        "/tmp/grove-test".into(),
        "/tmp".into(),
    );
    let now = chrono::Local::now();
    let mut list = Vec::new();
    for i in 0..n {
        list.push(SessionSummary {
            id: format!("s{i}"),
            cwd: "/tmp".into(),
            created: now,
            title: Some(format!("session {i}")),
            live: true,
            busy: false,
            agents: 1,
            cost: 0.0,
            updated: now,
        });
    }
    app.on_reply(Reply::Sessions { list });
    app
}

#[test]
fn list_focus_moves_between_sessions_and_the_prompt() {
    let mut app = list_app(3);
    // Opens on the newest session, not in the prompt box.
    assert_eq!(app.list_sel, Some(0));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.list_sel, Some(2));
    // Down past the last session goes to the prompt.
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.list_sel, None);
    // Up from an empty prompt comes back to the bottom session; so does Esc.
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.list_sel, Some(2));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.list_sel, Some(2));
    // Typing on the list starts a prompt, keeping the key.
    app.on_key(key(KeyCode::Char('f')));
    assert_eq!(app.list_sel, None);
    assert_eq!(app.editor.text(), "f");
    // With text in the prompt, Up belongs to the editor, not the list.
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.list_sel, None);
}

#[test]
fn empty_list_starts_in_the_prompt() {
    let app = list_app(0);
    assert_eq!(app.list_sel, None);
}
