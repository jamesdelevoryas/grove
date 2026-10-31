//! Renders a made-up session to an in-memory terminal. Run
//! `cargo test renders_a_session -- --nocapture` to look at the layout.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use tokio::sync::mpsc;

use super::Theme;
use super::app::App;
use super::view;
use crate::api;
use crate::client::Remote;
use crate::config::Config;
use crate::hub::Hub;
use crate::protocol::*;
use crate::store::Store;
use crate::wire::{Reply, SessionSummary};

fn meta(hub: &Hub, role: Role, name: &str, parent: Option<&str>) -> AgentMeta {
    hub.new_meta(
        role,
        name,
        parent.map(String::from),
        Effort::High,
        std::path::Path::new("/tmp"),
        "",
    )
}

pub fn screen(app: &mut App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| view::draw(f, app)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            text.push_str(buf[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

#[tokio::test]
async fn renders_a_session() {
    let dir = std::env::temp_dir().join(format!("grove-snap-{}", std::process::id()));
    let mut config = Config::load();
    config.home = dir.clone();
    let store = Store::create(&dir, std::path::Path::new("/Users/peterd/code/grove")).unwrap();
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let hub = Hub::new(config, store.clone(), api::Client::new(None), ui_tx, cmd_tx);
    let (remote, _requests) = Remote::detached();
    let mut app = App::new(
        remote,
        Theme::dark(),
        dir.clone(),
        "/Users/peterd/code/grove".into(),
    );
    let mut info = store.session.clone();
    info.title = Some("Make the parser handle trailing commas".into());
    app.on_reply(Reply::Attached { info });
    let ev = |agent: &str, kind: EventKind| Event {
        agent: agent.into(),
        kind,
    };

    let main = meta(&hub, Role::Main, "main", None);
    app.on_event(ev("main", EventKind::Spawned(main)));
    for (role, name) in [
        (Role::Supervisor, "supervisor"),
        (Role::Memory, "memory"),
        (Role::Improver, "improver"),
    ] {
        let m = meta(&hub, role, name, None);
        app.on_event(ev(name, EventKind::Spawned(m)));
    }
    let sub = meta(&hub, Role::Sub, "fix-tests", Some("main"));
    app.on_event(ev("fix-tests", EventKind::Spawned(sub)));
    app.on_event(ev("fix-tests", EventKind::Status(Status::Tools(1))));
    app.on_event(ev("supervisor", EventKind::Status(Status::Thinking)));
    app.on_event(ev(
        "main",
        EventKind::UserText("Make the parser handle trailing commas, and run the tests".into()),
    ));
    app.on_event(ev(
        "main",
        EventKind::Injected {
            from: "memory".into(),
            text: "James prefers plain for-loops over iterator chains (agent-code-style).".into(),
            directive: false,
        },
    ));
    app.on_event(ev(
        "main",
        EventKind::ProgressDelta("Looking at how the parser handles lists first.".into()),
    ));
    app.on_event(ev("main", EventKind::BlockDone(json!({"type":"tool_use","id":"t1","name":"bash","input":{"command":"rg -n 'fn parse_list' src/"}}))));
    app.on_event(ev(
        "main",
        EventKind::ToolDone {
            id: "t1".into(),
            content: "exit code: 0\n\nstdout:\nsrc/parse.rs:88:fn parse_list(&mut self) -> Result<Vec<Expr>> {\nsrc/parse.rs:140:fn parse_list_tail(\nsrc/lex.rs:12:// lists".into(),
            is_error: false,
        },
    ));
    app.on_event(ev(
        "main",
        EventKind::BlockDone(json!({"type":"tool_use","id":"t2","name":"edit_file","input":{"path":"src/parse.rs","old_string":"    if self.eat(Comma) {\n        continue;","new_string":"    if self.eat(Comma) {\n        if self.peek() == CloseParen { break; }\n        continue;"}})),
    ));
    app.on_event(ev(
        "main",
        EventKind::ToolDone {
            id: "t2".into(),
            content: "Edited src/parse.rs (1 replacement).".into(),
            is_error: false,
        },
    ));
    app.on_event(ev(
        "main",
        EventKind::Injected {
            from: "supervisor".into(),
            text: "Don't run `cargo publish`; the human only asked for tests.".into(),
            directive: true,
        },
    ));
    app.on_event(ev(
        "main",
        EventKind::TextDelta("Done. The parser now accepts a **trailing comma** in lists:\n\n- `parse_list` stops at `)` after a comma\n- added a test, `trailing_comma`\n\n```rust\nlet xs = [1, 2, 3,];\n```\n\nAll 48 tests pass.".into()),
    ));
    app.on_event(ev(
        "main",
        EventKind::BlockDone(json!({"type":"text","text":""})),
    ));
    app.on_event(ev(
        "main",
        EventKind::Notice {
            level: Level::Alert,
            text: "supervisor interrupted fix-tests: tried to delete ~/.cargo".into(),
        },
    ));
    app.on_event(ev("main", EventKind::TurnEnd));
    app.on_event(ev(
        "main",
        EventKind::Suggestions(vec![
            "Commit this with a short message".into(),
            "Add a test for nested trailing commas".into(),
        ]),
    ));
    app.toast = None;

    let text = screen(&mut app, 120, 44);
    println!("{text}");
    assert!(text.contains("Make the parser handle trailing commas"));
    assert!(text.contains("Edit(src/parse.rs)"));
    assert!(text.contains("Commit this with a short message"));

    // The frozen view: ctrl+o, then up a few lines and start a selection.
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(app.pager.is_some());
    let _ = screen(&mut app, 120, 24);
    for _ in 0..3 {
        app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    }
    app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    let frozen = screen(&mut app, 120, 24);
    println!("{frozen}");
    assert!(frozen.contains("VISUAL"));
    app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
    assert!(app.pager.is_none());

    // Back to the session list.
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let now = chrono::Local::now();
    let row = |id: &str,
               title: Option<&str>,
               live: bool,
               busy: bool,
               agents: usize,
               cost: f64,
               mins: i64| SessionSummary {
        id: id.into(),
        cwd: "/Users/peterd/code/grove".into(),
        created: now - chrono::Duration::minutes(mins),
        title: title.map(String::from),
        live,
        busy,
        agents,
        cost,
        updated: now - chrono::Duration::minutes(mins),
    };
    app.on_reply(Reply::Sessions {
        list: vec![
            row(
                "20261031-160102-1a2b",
                Some("Make the parser handle trailing commas, and run the tests"),
                true,
                true,
                5,
                0.45,
                0,
            ),
            row(
                "20261031-151500-3c4d",
                Some("Profile the TUI render loop under a 10k-line transcript"),
                true,
                false,
                4,
                1.12,
                12,
            ),
            row(
                "20261031-145859-2e76",
                Some("how does the grove multiplexing work?"),
                false,
                false,
                0,
                0.0,
                70,
            ),
        ],
    });
    app.toast = None;
    let list = screen(&mut app, 120, 16);
    println!("{list}");
    assert!(list.contains("SESSIONS"));
    assert!(list.contains("Profile the TUI render loop"));
    assert!(list.contains("Start a new session in ~/code/grove"));
    let _ = std::fs::remove_dir_all(dir);
}
