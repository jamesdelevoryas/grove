# grove

A multi-agent coding agent with a fast fullscreen terminal UI, in Rust (tokio + ratatui).
Default model `claude-opus-5-5` at effort `high`.

```sh
cargo build --release
export ANTHROPIC_API_KEY=...   # or: security add-generic-password -s grove-anthropic-api-key -a grove -w
target/release/grove [prompt]  # -c to continue the last session here, -r <id> to resume one
```

## Keys (Claude Code's, mostly)

| key | |
|---|---|
| enter / shift+enter, ctrl+j | send / newline |
| esc | interrupt the focused agent |
| ctrl+c | clear input · interrupt · twice to quit |
| **ctrl+o** | **freeze**: a still snapshot of the transcript while agents keep running. j/k line, ctrl+d/ctrl+u half page, ctrl+f/ctrl+b page, ctrl+e/ctrl+y scroll, gg/G, H/M/L, `/` search with n/N, `v` select lines + `y` copy; the mouse is released for native drag-select. q / esc / ctrl+o resumes |
| shift+tab | cycle effort (low → max) |
| tab | fill a suggested next prompt (again: the other one) |
| ctrl+n / ctrl+p, alt+1…9, click | switch agent |
| shift+↑ or ctrl+g | browse the transcript: ↑↓ select, enter expand, y copy (OSC 52) |
| pgup / pgdn / wheel | scroll (the view stays put while output arrives) |
| ctrl+t | agents panel · `?` help · `/` commands |

Commands: `/clear /effort /model /spawn /interrupt /stop /retry /upgrade /improve /memory /supervisor /focus /cost /quit`.

## Architecture

- **Hub** (`hub.rs`): every agent's mailbox, event fan-out (to the UI, and a no-deltas
  feed to watchers), spawning, messaging, and the upgrade sequence.
- **Agents** (`agent.rs`): one loop for every role. Input that arrives mid-turn is
  queued and joins the next request (steering). Tool calls run concurrently and are
  cancellable. History is append-only JSONL and grows only when the model answers, so
  an interrupted request leaves nothing half-written.
- **Watchers** (`watchers.rs`) are ordinary agents fed batched `[activity]` digests,
  never blocking the agents they watch:
  - *supervisor*: sees prompts and tool calls within ~250 ms; can `interrupt_agent`,
    `stop_agent`, or send operator notes (delivered as mid-conversation `system`
    messages). It replaces permission prompts.
  - *memory*: on each human prompt, searches the memory MCP server (memory.pjd.dev) and
    sends useful notes to the working agent; saves durable facts.
  - *improver*: watches for friction, proposes fixes to grove itself, implements them
    on request, then `request_upgrade`.
- **Self-upgrade** (`/upgrade`): `cargo build --release` runs while everything keeps
  working; then every agent is paused (inference and tools cancelled, queued input
  saved), grove `exec`s the new binary with `--resume`, and paused agents continue.
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
