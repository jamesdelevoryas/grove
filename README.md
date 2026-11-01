# grove

A multi-agent coding agent with a fast fullscreen terminal UI, in Rust (tokio + ratatui).
Default model `claude-opus-5-5` at effort `high`.

```sh
cargo build --release
export ANTHROPIC_API_KEY=...   # or: security add-generic-password -s grove-anthropic-api-key -a grove -w
grove                  # the session list; type a prompt to start a session, ↑↓ enter to open one
grove <prompt>         # start a session and open it
grove --bg <prompt>    # start one in the background
grove list [--json]    # also: grove agents
grove attach <id>      # also -r <id>; -c opens the latest session in this directory
grove stop <id> · grove rm <id> · grove shutdown
```

Sessions run in a background server (`grove server`, started on demand, socket
`~/.grove/server.sock`, log `~/.grove/server.log`), so they keep running when you close
the terminal. In the list: ↑↓ select, enter open, ctrl+x stop, del twice to delete. In a
session: esc or ← on an empty prompt goes back to the list.

## Keys (Claude Code's, mostly)

| key | |
|---|---|
| enter / shift+enter, ctrl+j | send / newline |
| esc | interrupt the focused agent |
| ctrl+c | clear input · interrupt · twice to quit |
| **ctrl+o** | **freeze**: a still snapshot of the transcript, read like a Helix buffer while agents keep running. Cursor and selection: `hjkl`, `w b e` (`W B E`), `x` line (again to extend), `v` select mode, `;` collapse, `alt-;` flip, `f t F T` + char, `gg ge gh gl gs`, `%`, `/` with `n N`, ctrl-d/u/f/b, ctrl-e/y; `y` or `space y` copies the selection. The mouse is released for native drag-select. `q`, ctrl+o, or esc (with nothing selected) resumes |
| shift+tab | cycle effort (low → max) |
| tab | fill a suggested next prompt (again: the other one) |
| ctrl+n / ctrl+p, alt+1…9, click | switch agent |
| shift+↑ or ctrl+g | browse the transcript: ↑↓ select, enter expand, y copy (OSC 52) |
| pgup / pgdn / wheel | scroll (the view stays put while output arrives) |
| `?` · `/` | help · commands (the header lists every agent; the focused one is highlighted) |

Commands: `/clear /effort /model /spawn /interrupt /stop /retry /upgrade /improve /memory /supervisor /focus /cost /sessions /rm /quit`.

## Architecture

- **Server** (`server.rs`, protocol in `wire.rs`, client in `client.rs`): owns every live
  session's hub. Each session keeps a replay log of its UI events (streaming deltas fold
  away when their block finishes), so a UI attaching later gets the whole transcript, then
  the live stream. The UI (`tui/`) is only a client: it sends `Input`s and renders events.
- **Hub** (`hub.rs`): one session's agent mailboxes, event fan-out (to the server, and a
  no-deltas feed to watchers), spawning, messaging, pause/resume.
- **Agents** (`agent.rs`): one loop for every role. Input that arrives mid-turn is
  queued and joins the next request (steering). Tool calls run concurrently and are
  cancellable. History is append-only JSONL and grows only when the model answers, so
  an interrupted request leaves nothing half-written.
- **Watchers** (`watchers.rs`) are ordinary agents fed batched `[activity]` digests,
  never blocking the agents they watch:
  - *supervisor*: sees prompts and tool calls within ~250 ms; can `interrupt_agent`,
    `stop_agent`, or send operator notes (delivered as mid-conversation `system`
    messages). It replaces permission prompts.
  - *memory*: on each human prompt, searches a memory MCP server and sends useful notes
    to the working agent; saves durable facts. Off unless a server is configured:
    `MEMORY_URL`, or `memory_url = https://…` in `~/.grove/config` (bearer token from the
    keychain item `memory-mcp-token` or `MEMORY_TOKEN`).
  - *improver*: watches for friction, proposes fixes to grove itself, implements them
    on request, then `request_upgrade`.
- **Self-upgrade** (`/upgrade`): the server runs `cargo build --release` while everything
  keeps working; then every agent of every session is paused (inference and tools
  cancelled, queued input saved), the server `exec`s the new binary with the live session
  ids, and paused agents continue. Attached UIs get a `restarting` message, exec the new
  build too, and reattach.
- **UI inference** (`infer.rs`): forks an agent's exact cached request prefix and asks one
  question at low effort. Used for the suggested next prompts after every turn.
- **TUI** (`tui/`): redraws only when something changed, caps streaming at ~120 fps,
  draws keystrokes at once, wraps each transcript entry once and caches the lines, uses
  synchronized output. Colours come from the terminal's ANSI palette, with warm
  background tints for prompts, diffs and notes.

State lives in `~/.grove/sessions/<id>/agents/<agent>/{meta.json,messages.jsonl}`.

Environment: `GROVE_SUPERVISOR=0`, `GROVE_MEMORY=0`, `GROVE_IMPROVER=0` (or
`GROVE_IMPROVER_WATCH=0`), `GROVE_THEME=light|dark`, `GROVE_EFFORT`, `GROVE_MODEL`,
`GROVE_SRC` (the tree `/upgrade` builds), `GROVE_HOME`, `MEMORY_URL`.
