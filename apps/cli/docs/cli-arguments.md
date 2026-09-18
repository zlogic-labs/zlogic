# zlogic command-line reference

`zlogic` is the Rust TUI front end for zlogic (wired directly to the real engine
in-process). This document lists every argument and mode it supports.

## Modes at a glance

`zlogic` has three modes of operation, distinguished by the first argument, plus one handover:

| Mode | Trigger | Description |
| --- | --- | --- |
| Interactive TUI | no arguments (default) | Full-screen UI, enters raw mode / alt-screen |
| Single-shot chat (oneshot) | `--prompt <text>` | Runs one turn then exits, pipe-friendly, can be combined with `--print json` |
| Markdown rendering | `--render-md [path]` | Renders markdown as ANSI to stdout, no TUI |
| daemon | first word is `daemon` | Hands the rest of the command line to `zlogic-daemon`: run a local or remote daemon, or manage paired devices |

> `daemon` is not a clap subcommand: when the first word is `daemon`, the remaining words go
> through verbatim to the daemon's own command line (see [daemon mode](#daemon-mode)).

## Common arguments

| Argument | Values | Default | Description |
| --- | --- | --- | --- |
| `--prompt <text>` | any text | none | Its presence enters oneshot: exits after one turn, does not start the TUI |
| `--model <id>` | model id | none | Model preference for this turn / session |
| `--theme <id>` | `auto`, `dark`, `light`, `blood`, `cyberpunk`, `god`, `matrix`, `mono`, `monokai` | `auto` | Theme; `auto` probes the terminal background colour |
| `--locale <tag>` | `auto`, `zh-CN`, `en-US` | `auto` | UI language; `auto` follows `ZLOGIC_LANG`/`LC_ALL`/`LC_MESSAGES`/`LANG`, falling back to `en-US` |
| `--print <shape>` | `text`, `json` | `text` | oneshot output shape; `json` emits event JSON-lines |
| `--plan` | — | off | Enters Plan mode (read-only planning) |
| `--cwd <path>` | directory | process cwd | Working directory for the turn / workspace |
| `--session <id>` | session id | new | Continues an existing session by id |
| `--resume [<id>]` | session id, or omitted | new | Resumes a session: without a value it restores the workspace's most recent session, with a value it restores the given id (mutually exclusive with `--session`) |
| `--quiet` | — | off | In oneshot, suppresses thinking / tool output on stderr |
| `--permission <mode>` | `auto`, `deny`, `approve-all` | `auto` | Non-interactive approval posture; in oneshot the interactive events go through a fail-closed responder |
| `--god` | — | off | Full-power mode: the TUI uses the danger theme and permissions map to approve-all (overrides `--permission`) |
| `--render-md [path]` | file path or `-` | `-` (stdin) | Renders markdown as ANSI and exits; the value can be omitted when stdin is a pipe |
| `--width <cols>` | column count | terminal width, otherwise 80 | Render width for `--render-md` |
| `--probe` | — | — | Debug: probes terminal capabilities (env heuristics + live OSC11 / CSI 6n / DECRQM checks) and prints them, then exits. Creates no session and does not enter the TUI; runnable from any shell |
| `-h` / `--help` | — | — | clap help (mentions the `daemon` handover) |

### Workspace on startup

Interactive TUI startup (no `--prompt` / `--resume` / `--session`, and stdin/stdout
are both terminals) **does not register a workspace automatically**:

1. The current directory (or its git root / a registered ancestor directory) already
   belongs to a workspace → use it directly, create nothing;
2. Otherwise a prompt offers two choices: **create a workspace from the current
   directory** / **pick from the list of existing workspaces** (Esc cancels).

All other paths (oneshot, `--resume`, `--session`, non-interactive terminal) keep the
old behaviour: `open_at` registers automatically by the anchoring rules (if cwd / the
git root has never been registered, an entry is created) — scripts cannot answer
questions and must not be blocked by an interactive prompt. Interactive list selection
only opens the chosen workspace and does **not** route the session to another workspace
through the anchoring rules.

While running you can open the workspace panel at any time with `/workspace`
(workspaces in the left column + session preview on the right). `←/→` moves focus
between the two columns: Enter in the left column switches to that workspace and
**restores the most recent session** (creating one if there is none); the first entry
in the right column is "new session", the rest are the session list, and Enter creates
/ restores the selected session.

### Terminal capabilities: `--probe`

`--probe` prints the capability probe results and exits (purely diagnostic: creates no
session, does not enter the TUI, changes no state):

```
$ zlogic --probe
terminal_kind=Windows
color_tier=Rich
icon_tier=Unicode
ambiguous_wide=false
background=Dark
supports_sync=true
legacy_windows_console=false
probe_background=Some(Dark)
probe_ambiguous_wide=Some(false)
probe_supports_sync=Some(true)
```

The first three lines are env heuristics, the three `probe_*` lines are live
measurements taken at startup (OSC 11 background, CSI 6n wide characters, DECRQM
2026), and the measurements win. Typical uses:

- Seeing ASCII icons/logo when you **run cmd inside Windows Terminal**? Run it once and
  check `icon_tier`. Note: when launched from Win+R / the Start menu / a double-click,
  the process does not get `WT_SESSION` even if the window is Windows Terminal
  (terminal takeover happens after process creation), and in that case the live CSI 6n
  check promotes the narrow-rendering terminal back to Unicode.
- A bare conhost (the old-style console window) shows `legacy_windows_console=true` or
  `ambiguous_wide=true`.

## daemon mode

`zlogic daemon [options]` starts a local or remote daemon, or manages paired devices. The words
after `daemon` belong to `zlogic-daemon`'s own command line, so `zlogic daemon --help` prints its
own usage:

```sh
zlogic daemon [--listen <IP[:PORT]>] [--public-url <URL>] [--pair]
zlogic daemon devices
zlogic daemon revoke <device-id>
zlogic daemon doctor
```

### daemon options

| Option | Default | Description |
| --- | --- | --- |
| `--listen <IP[:PORT]>` | `127.0.0.1` | Listen address; the port is saved after the first start and reused, omit it to let the OS pick one |
| `--public-url <URL>` | none | Address reachable from the QR code; required and must be HTTPS when listening remotely |
| `--pair` | off | Generate a one-time secret and QR code valid for 5 minutes |
| `--tls-cert <PEM>` | none | Use the given TLS certificate (requires `--tls-key` as well) |
| `--tls-key <PEM>` | none | Use the given TLS private key |
| `--public-certificate-sha256 <HEX>` | none | Reverse-proxy certificate fingerprint, shown in the pairing QR code |
| `--max-upload-mib <MIB>` | `2048` | Per-attachment upload limit |
| `--dangerously-skip-permissions` | off | Skip engine permission confirmations |
| `-h` / `--help` | — | daemon help |

### daemon subcommands

- `daemon devices` — list paired devices.
- `daemon revoke <device-id>` — revoke one device.
- `daemon doctor` — diagnose: state directory writability, device count, whether the listen port
  is free, protocol version and capabilities.

### Where the daemon comes from

The daemon is **not part of this crate**: this front end is open source, the daemon is not (yet).
The `zlogic` binary released packages ship is built by the closed-source repository with the daemon
welded into it — its `main` dispatches `daemon …` before this code ever runs, so the handover
described below is the from-source path.

A `zlogic` you built yourself has no daemon, so it hands over instead: to a `zlogic-daemon` sitting
next to the executable, or to the binary `ZLOGIC_DAEMON` points at. On Unix the daemon *replaces*
this process (`exec`), elsewhere it is spawned as a child and its exit status becomes ours. Every
word after `daemon` is passed through verbatim — nothing is parsed here. When no daemon binary is
found, the command says so and exits 1 rather than failing obscurely.

## Exit codes

- `0` — success (oneshot completed normally; the TUI exited normally).
- `1` — failure (argument error, engine startup failure, and so on).
- `zlogic daemon …` returns the daemon's own exit status, and `1` when the daemon binary is
  missing.

> In the TUI, Ctrl-C cancels the current turn (it does not kill the process); in
> oneshot a broken pipe (BrokenPipe) returns 0 as success. cli-rs has no separate
> "cancel" exit code.

## Examples

```sh
# Interactive TUI
zlogic

# oneshot: run one turn and write it to stdout
zlogic --prompt "hello"

# oneshot: event JSON-lines
zlogic --prompt hi --print json

# render markdown from a piped stdin
zlogic '**hi** `x`' | zlogic --render-md -
```

## Related files

- Argument definitions: `src/cli/args.rs`
- oneshot path: `src/cli/oneshot.rs`
- markdown rendering: `src/cli/render_md.rs`
