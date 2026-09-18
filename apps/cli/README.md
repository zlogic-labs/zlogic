# zlogic-cli — the Rust TUI front end for zlogic

A Ratatui/crossterm terminal front end, wired to the real engine.

The back-end abstraction lives behind the `CoreSession` trait (`src/session/`), and there is
exactly one implementation:

- **`EngineSession`** — the real engine, in-process, following the same flow as every other host:
  `Engine::bootstrap("cli-rs")` → `workspaces.open_at(cwd)` → `session_open`
  → **subscribe to the hub before submitting** → submit → render the stream.
  See `src/session/engine.rs`.

> The older `BridgeSession` (a stdio JSON-lines subprocess) and the scripted mock back end are
> gone: every host talks to the engine **in the same process**.

## Build

This crate is a member of the root workspace. The first build pulls `zlogic-engine` and its
dependencies (rustls, rusqlite, git2, …) from crates.io.

## Run

The built binary is called `zlogic` (the package name is `zlogic-cli`, which is what `-p` takes;
the binary lands in `target/<profile>/zlogic[.exe]`).

```sh
cargo run -p zlogic-cli                          # interactive TUI (needs a real terminal)
cargo run -p zlogic-cli -- --prompt "hello"      # headless one-shot → stdout
cargo run -p zlogic-cli -- --prompt hi --print json   # event JSON-lines
cargo run -p zlogic-cli -- --theme cyberpunk     # pick a theme (8 available)
cargo run -p zlogic-cli -- --session <id>        # continue a specific session
cargo run -p zlogic-cli -- --resume               # continue the most recent session
cargo run -p zlogic-cli -- --resume <id>          # continue a specific session
cargo run -p zlogic-cli -- daemon doctor         # hand over to zlogic-daemon (see below)
```

`--prompt` ⇒ one-shot (no TUI, no alt-screen, no raw mode). Flags:
`--model --theme --print text|json --plan --cwd --session --resume --quiet
--permission auto|deny|approve-all --god`.

### daemon mode

`zlogic daemon …` runs the daemon: it exposes the engine as an HTTP JSON + SSE service, for remote
desktop and mobile clients. The daemon is **not part of this crate** — this front end is open
source, the daemon is not (yet).

- Released packages ship one `zlogic` binary with the daemon built in, and that binary handles
  `daemon …` before this crate's `main` is reached.
- A `zlogic` built from source has no daemon: it hands the command line over to a `zlogic-daemon`
  next to the executable, or to the path in `ZLOGIC_DAEMON`. If neither exists, the command says so
  and exits 1.

Nothing is parsed here: the words after `daemon` are the daemon's own command line — `--listen`,
`--pair`, `devices`, `revoke <id>`, `doctor`, `--help` included. Run `zlogic daemon --help` for its
usage, or see [`docs/cli-arguments.md`](docs/cli-arguments.md#daemon-mode).

The full argument reference is in
[`docs/cli-arguments.md`](docs/cli-arguments.md).

## What is covered

- Render loop: single-writer `Msg` loop, frame coalescing, 50ms stream throttle, DEC 2026
  wrapping, `insert_before` history, RAII plus panic-hook terminal restore.
- Theme-first: 8 themes, 3 colour tiers, the semantic `Sem` colour API.
- The `CoreSession` trait plus its real engine implementation (`EngineSession`).
- The real turn lifecycle: submit, streaming render, permission / confirmation / form replies,
  cancellation, turn summary (synthesised from `TurnEnd` statistics).
- A fail-closed interaction responder for headless one-shot runs.
- The glyph registry (unicode/ascii tiers).
- Workspaces: an interactive start does not register anything by itself — the current directory
  is used directly when it already is a workspace, otherwise you are asked to create one or pick
  from the list. The `/workspace` panel shows workspaces in the left column with a session
  preview on the right, `←/→` moves focus, Enter creates or resumes the selected session. On the
  engine side this added the read-only probe `Workspaces::locate` (an op matching `open_at`'s
  anchoring rules but neither registering nor touching anything).

## Known gaps (EngineSession versus the engine)

- `/plan` is a local UI posture; the engine has no plan op yet.
- `/archive` has no engine op (`session_delete` is the only session mutation) — it is a no-op.
- `usage_timeline` approximates per-turn usage by session-level grouping (the engine has no
  per-turn usage listing op).
- `command_catalog` holds front-end built-in commands only (the engine has no skills directory op).
