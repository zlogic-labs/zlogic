# zlogic

> **An open-source AI Agent Harness — a coding agent runtime built in Rust, and the CLI that drives it.**

**[English](README.md) · [简体中文](README.zh-CN.md)**

[![License](https://img.shields.io/badge/license-Apache--2.0-green)](LICENSE)
[![Version](https://img.shields.io/badge/version-v1.0.0--beta.1-blue)](https://zlogic.run)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey)](https://zlogic.run)
[![Website](https://img.shields.io/badge/website-zlogic.run-purple)](https://zlogic.run)

## What is zlogic

zlogic is an AI Agent Harness: an **agent runtime** plus the terminal client that drives it.

Most coding agents are a product with a runtime buried inside. zlogic is the other way round: **the runtime is the product, and the clients are replaceable.**

The runtime builds context, drives the agent, calls tools in your development environment, handles permissions, and records the whole run — reasoning, tool calls, tokens and cost. The agent works directly on files, your shell and Git repositories; which operations it may perform is decided by policy.

The CLI is one **host** of the runtime, not the runtime itself. The desktop app, the remote daemon (`zlogic daemon`) and a front end you write yourself can all use the same engine: the same `EngineApi`, the same event stream. That is what lets the runtime be open source, built and used on its own, while the clients evolve separately. See [Open source, and what is not](#open-source-and-what-is-not).

## Why zlogic

* **One runtime, many hosts.** The CLI, the desktop app, the remote daemon and custom front ends share one engine. A client owns its own UI and interaction; the runtime does not need to know where it ends up running.
* **Transparent execution.** Reasoning and tool calls stream into the transcript as they happen; `--print json` emits the same turn as JSON Lines, so scripts and CI can consume the events directly.
* **Permissions are handled separately.** A tool executes and reports its result; it does not decide whether it may run. A request goes through deterministic rules, then a risk review, and anything that needs authorization is settled by policy and the user.
* **One provider interface.** Providers share one interface; a vendor-specific implementation is needed only where request format, streaming, thinking or usage actually differ.
* **No relay for model requests.** SQLite, YAML config and credentials are all managed locally, and model requests go straight to the provider you configured.

## Install

### Install the CLI

```sh
# bash / zsh
curl -fsSL https://install.zlogic.run | sh

# PowerShell
irm https://install.zlogic.run | iex
```

Install paths and per-platform dependencies: [Installation](https://zlogic.run/docs/01-installation.html#one-line-cli-install-recommended).
Verifying a download (SHA-256 / minisign): [Verify downloads](https://zlogic.run/docs/01-installation.html#verify-downloads-optional-but-recommended).

> 💡 Windows users: use **Windows Terminal / WezTerm / Alacritty / VS Code terminal**. The legacy Windows
> console does not fully support modern terminal control sequences or IME composition.

### Build from source

```sh
cargo build --release -p zlogic-cli    # target/release/zlogic
cargo test --workspace
```

Rust 1.85+ (edition 2024). The first build pulls dependencies from crates.io. Nothing here needs the desktop app, a GPU or any other service.

### Configure a model

Keys never go into config files — they can live in the OS keyring (or the encrypted vault), or in environment variables:

```sh
export DEEPSEEK_API_KEY=sk-...
zlogic key set deepseek
zlogic key list
```

`zlogic key set`, `zlogic key delete <provider>` and `zlogic key list` work without a configured model.

## 30-second tour

![zlogic CLI (TUI)](screenshots/cli.gif)

```sh
zlogic key set deepseek                                   # 1 · key → OS keyring, never a config file
zlogic                                                     # 2 · TUI: Enter starts a session
zlogic --prompt "where does the execution loop live?"       # 3 · one headless turn → stdout
zlogic --prompt "summarise this diff" --print json          # 4 · the same turn as JSON Lines events
```

The TUI and the JSON event stream run the same runtime — only the host and the output differ.

Type `/` in the TUI to open the command palette: `/model`, `/session`, `/replay`, `/stats`, `/approval`, `/plan` and more.
Full argument reference: [`apps/cli/docs/cli-arguments.md`](apps/cli/docs/cli-arguments.md).

## Architecture

```text
   host ── apps/cli · desktop · daemon · a front end you write
     │  submit turns, steer, answer permission requests      ▲
     ▼                                                        │ events (hub)
   crates/engine ── the in-process dispatch layer ────────────┘
     │  opens sessions and workspaces, resolves the model per turn,
     │  wires policy / grants / memory / skills / tools, fans out events
     ▼
   crates/core ── one agent run: build context → round loop → tools → persist
     │
     ├── crates/llm      LLM provider clients
     ├── crates/tools    the tool registry and the built-in tools
     ├── crates/policy   command decomposition + path zoning → allow / ask / deny
     ├── crates/mcp      MCP servers → entries in the same tool registry
     └── crates/store    SQLite: sessions, entries, usage
```

`crates/protocol` sits between these boundaries: it holds the types shared by the host, the engine, core and the provider clients.

### Layers

| Layer | What it decides | Where |
|---|---|---|
| host | what a person sees: transcript, panels, approvals | `apps/cli` |
| engine | which session, which model, which policy, who receives events | `crates/engine` |
| core | one run: context, rounds, tool batches, persistence, cancellation | `crates/core` |
| providers | how a turn becomes a stream of parts | `crates/llm` |
| tools | what the agent can do, and what it reports back | `crates/tools` |
| authority | `allow` / `ask` / `deny` for paths, commands and tools | `crates/policy` |

A few boundaries matter here: `core` does not care which workspace or UI is using it — it receives the root, the session and an already-resolved model; a tool does not decide permissions; a running tool is stopped by a cancellation request rather than by killing the process.

### Crates in this repository

| Path | What it is |
|---|---|
| `apps/cli` | Ratatui TUI, render loop, themes, i18n, widgets — and the `zlogic` binary |
| `crates/protocol` | the types shared across the boundaries |
| `crates/engine` | host API, service assembly and event fan-out |
| `crates/core` | build context, drive rounds, execute tools, persist |
| `crates/llm` | provider clients, request serialisation, streaming, usage normalisation |
| `crates/tools` | tool definitions, the registry and the built-in tools |
| `crates/policy` | shell-command decomposition and path zoning |
| `crates/mcp` | MCP servers, connection management and the tool catalog |
| `crates/store` | the SQLite persistence layer: sessions, entries, usage |

The remaining crates (`objects`, `credential`, `config`, `task`, `plugins`, `hooks`, `code-sitter`, `logging`, `paths`) are close to self-explanatory by name; the complete layout is in the repository itself.

### Where to start reading the code

For a first pass, in this order:

1. `crates/engine/src/lib.rs` — what a host may do, and why a host does not call the services directly.
2. `crates/core/src/lib.rs` — `Core::run`; then `crates/core/src/round.rs` for the execution loop and
   `crates/core/src/context.rs` for how context is built from stored entries.
3. `crates/tools/src/lib.rs` — the tool contract, and the content / display / object split.
4. `crates/policy/src/lib.rs` — how a shell command becomes atomic operations with zoned paths.
5. `apps/cli/src/session/engine.rs` — a real host: bootstrap, open a session, subscribe, submit.
6. `crates/protocol/src/lib.rs` — once you need to move a boundary.

Starting smaller? Adding a tool is the most direct entry point, adding a provider client the next.

## Development

No Node, no Python, no vendored toolchain — a recent stable Rust is all you need.

```sh
cargo build --release -p zlogic-cli
cargo run -p zlogic-cli -- --prompt "hello"
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked
```

CI runs these checks on Linux, macOS and Windows (`.github/workflows/ci.yml`). The terminal, console events, system keyring and clipboard each have their own per-platform implementation, so passing on one platform says nothing about the others.

What a pull request is expected to carry: [CONTRIBUTING.md](CONTRIBUTING.md). Nothing in this repository may depend on the closed-source products.

## Runtime capabilities

* **Agent execution loop** — understand a task, explore a codebase, plan changes, edit files, and delegate to sub-agents where that helps.
* **Tool execution** — files, search, shell and Git are built-in tools. The agent can work directly in your repository, or in its own Git worktree.
* **Policy-based permissions** — fine-grained control over files, shell commands, tools and external resources. Deterministic rules first, then a risk review, then policy and the user for anything that needs authorization. Grants can be `once` / `session` / `project`; sensitive paths (`.env`, SSH private keys, `~/.aws`, `~/.kube`, `.npmrc`, …) always ask.
* **MCP** — stdio or streamable HTTP servers register as entries in the same tool registry as the built-ins, each gated individually.
* **Skills & plugins** — reusable skills; plugins are discovered through a directory and a manifest, and can contribute MCP servers and skills.
* **Remote execution** — the daemon exposes the engine over HTTP JSON + SSE for remote desktop and mobile clients, connected by pairing with no cloud relay.
* **Observability** — reasoning and tool calls visible in real time; `--print json` emits the same events; sessions and usage live in local SQLite and can be queried by date, provider, model and workspace.

## Providers

zlogic reaches models through one provider interface. Most providers follow the same request and event model; where API format, streaming, thinking, tool calling or usage differ, the vendor's client handles the difference.

Supported today: DeepSeek, OpenAI, Anthropic Claude, Google Gemini, DashScope (Qwen), GLM (Zhipu), OpenRouter, xAI, Groq, AWS Bedrock, and OpenAI-compatible endpoints (vLLM, SGLang, Ollama, enterprise gateways, …).

You can also add your own endpoint in `models.yaml`:

```yaml
# ~/.config/zlogic/models.yaml
providers:
  my-provider:
    sdk: openai_chat
    base_url: https://api.openai.com/v1
    models:
      gpt-4o: {}
```

Provider list: [Providers & Models · Built-in providers](https://zlogic.run/docs/08-providers-and-models.html#built-in-providers).
Where keys are stored: [Key Management · System keyring](https://zlogic.run/docs/09-key-management.html#system-keyring-persistent).

## Security & Privacy

These properties come from the architecture above.

* **Direct provider connection** — model requests go from the machine running the agent straight to the provider you configured; zlogic provides no cloud proxy or relay for model requests.
* **Local storage** — conversation history lives in a SQLite database on your device, which you can inspect, back up and delete. There is no copy on zlogic servers.
* **Protected credentials** — API keys, database passwords and cloud credentials live in the OS credential manager, or encrypted with a master key in the state directory; environment variables are supported too.
* **Credentials stay out of model context** — stored credentials are never exposed to the model as conversation content or context, and the reasoning channel is never fed back into the next turn.
* **No telemetry from the CLI / runtime** — neither sends usage data to zlogic services; the closed-source desktop app sends one anonymous install receipt per launch. See [Data Flow & Privacy · What we collect](https://zlogic.run/docs/12-data-flow-and-privacy.html#what-we-collect).

When you use an AI provider, that provider's own privacy policy and data handling rules apply as well.

## Open source, and what is not

**Open source: the CLI + the agent runtime.** This repository is Apache-2.0, and `apps/cli` plus every crate under `crates/` can be built, run and modified on their own.

**Closed-source clients: desktop + remote daemon + additional tools.** These are built on top of this repository's runtime: the desktop app, the remote daemon, and tools such as database and cloud connections, data analysis, the HTML widget and the Python runtime.

A release ships the OSS runtime together with those closed-source components; a `zlogic` built from source is a complete CLI, just without the daemon.

The runtime you see here is the runtime those clients actually use. Runtime and CLI bugs can be fixed in this repository. Issues and ideas are welcome in [Issues](https://github.com/zlogic-labs/zlogic/issues).

### Desktop app

Download from [zlogic.run/#download](https://zlogic.run/#download). The desktop app is distributed as binaries only.

| Platform | Format | Download |
|---|---|---|
| Windows 10/11 x64 | NSIS installer (.exe) | [download](https://zlogic.run/dl/zlogic_latest_x64-setup.exe) |
| macOS 12+ Apple Silicon | DMG | [download](https://zlogic.run/dl/zlogic_latest_aarch64.dmg) |
| macOS 12+ Intel (x86_64) | DMG | [download](https://zlogic.run/dl/zlogic_latest_x86_64.dmg) |
| Linux x86_64 | AppImage | [download](https://zlogic.run/dl/zlogic_latest_amd64.AppImage) |

The macOS app is not signed or notarised yet, so the first launch may be blocked by Gatekeeper; trust it once with
`xattr -dr com.apple.quarantine /Applications/zlogic.app` in Terminal. System dependencies: [Installation · Desktop app](https://zlogic.run/docs/01-installation.html#desktop-app-closed-source).
Verifying a download: [Verify downloads](https://zlogic.run/docs/01-installation.html#verify-downloads-optional-but-recommended).

## Data directories

Data lives in four roots (XDG style): `config` holds `config.yaml` / `models.yaml` / `env.yaml` / `policy.yaml`,
`data` holds the object store and per-workspace history, `state` holds `state.db`, logs and the credential vault, and `cache` can be wiped at any time.

**`data` and `state` are the only places your local data lives — deleting them is unrecoverable.**

Per project: `.zlogic/policy.yaml` and `.mcp.json`. The former can only tighten the global policy; the latter declares workspace MCP servers.
The actual layout and configuration keys: [Configuration Reference · Data directory](https://zlogic.run/docs/10-configuration.html#data-directory).

## Project status

**v1.0.0-beta.1 (beta)**

The CLI and the desktop app are available. The CLI and the agent runtime are open source under Apache-2.0; the remote daemon and the desktop app currently ship as closed-source components.

The mobile app is still planned.

## Docs & links

* Website (downloads, docs): **https://zlogic.run**
* Docs: **https://zlogic.run/docs/**
* CLI argument reference: [`apps/cli/docs/cli-arguments.md`](apps/cli/docs/cli-arguments.md)
* Contributing: [CONTRIBUTING.md](CONTRIBUTING.md)
* Install station: **https://install.zlogic.run**
* Issues and feedback: **https://github.com/zlogic-labs/zlogic/issues**

## License

[Apache-2.0](LICENSE). Third-party open-source components keep their own licenses.
