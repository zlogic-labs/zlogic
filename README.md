# zlogic (@zlogic.run)

> An AI Agent Harness for developers to build and run coding agents with direct
> access to files, terminals, Git repositories, databases, and cloud services.
> Rust-based and optimized for DeepSeek, with multi-provider AI, local and remote
> agents, MCP, Skills, policy-controlled execution, and end-to-end observability
> of reasoning, execution, and usage.

**[English](README.md) · [简体中文](README.zh.md)**

[![License](https://img.shields.io/badge/license-Apache--2.0-green)](LICENSE)
[![Version](https://img.shields.io/badge/version-v1.0.0--beta.1-blue)](https://zlogic.run)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey)](https://zlogic.run/#download)
[![Website](https://img.shields.io/badge/website-zlogic.run-purple)](https://zlogic.run)

---

## Desktop preview

![zlogic desktop (light theme)](screenshots/desktop.light.svg)

![zlogic desktop (dark theme)](screenshots/desktop.dark.svg)

---

## What is zlogic

zlogic is an AI agent harness: a runtime for building and running coding agents, plus the CLI that
drives it. Instead of only generating code, the agents work directly in your development
environment, under permissions you define.

Every client drives the same local agent runtime: message assembly, context construction, tool
execution, file I/O and conversation persistence all happen on your machine, and requests go only
to the providers you configure.

| Client | Form | Best for |
|---|---|---|
| **CLI (TUI)** | native full-screen terminal UI in Rust — **open source, this repository** | coding, reading logs and running builds where you already work |
| **Desktop** | Tauri 2 app for Windows / macOS / Linux — closed source | managing several projects and watching usage and cost visually |
| **Mobile** (planned) | paired to the runtime on your machine through the daemon — closed source | approving remotely and checking sessions and tasks from your phone |

## What zlogic can do

* **AI Coding Agent** — understand tasks, explore codebases, plan changes, edit files and carry
  multi-step development workflows to completion.
* **Developer Workspace** — projects, sessions, files, tools and AI workflows organised in one
  workspace, with history and approvals shared across clients.
* **Terminal & Shell** — the agent runs commands and development tools for you, behind the
  approval gate, and sessions can move into their own Git worktree.
* **Git Integration** — the agent drives real `git` in your repository (inspect, diff, stage,
  commit); your working tree is never rearranged behind your back.
* **Database Access** *(desktop app)* — connect PostgreSQL, MySQL, SQLite or Redis once and let
  agents work with development data through them.
* **AWS & External Services** *(desktop app)* — object storage and cloud accounts are configured
  once and then authorized per workspace.
* **MCP Support** — connect agents to external tools and services using the Model Context
  Protocol; every capability is declared and approved individually.
* **Skills & Tools** — extend agents with reusable skills and specialised tools.
* **Multi-Provider AI** — connect and switch between multiple AI providers and models from the
  same workspace.
* **Fine-Grained Permissions** — policies control what an agent may access and execute: files,
  commands, tools and external resources.
* **Remote Agent Daemon** *(binary distribution)* — run the agent on another machine and drive it
  from the desktop client, keeping it close to the code, GPUs, databases and services it needs
  while your local desktop stays the interface.
* **Local History & Memory** — sessions, entries and usage live in a local SQLite database;
  durable memory persists across conversations, scoped `global` or `workspace`.
* **Transparent Usage** — calls, tokens and cost are recorded per session and can be filtered by
  date, provider, model and workspace.

Bring your code, tools, services and AI agents into one runtime.

### Supported AI providers

A built-in provider directory ships with the program: a provider appears as soon as its key is
detected, and one that is not listed yet is a few lines of `models.yaml`.

* DeepSeek
* OpenAI
* Anthropic Claude
* Google Gemini
* OpenAI-compatible APIs — enterprise gateways, self-hosted vLLM / SGLang / Ollama
* GLM (Zhipu), DashScope (Qwen), OpenRouter, xAI, Groq
* AWS Bedrock, through the standard `AWS_*` environment variables
* Custom providers — declare any endpoint and its models yourself

DeepSeek, GLM, DashScope and Anthropic each get a dedicated client, with their own
thinking/reasoning and usage handling, instead of being pushed through the generic
OpenAI-compatible path.

## Transparent agent execution

zlogic makes the agent's reasoning and execution visible while tasks are running.

You can see what the agent is doing, which tools it uses, the actions it takes, and how the task
progresses, instead of treating the agent as a black box. In the TUI the reasoning channel and
every tool call stream into the transcript as they happen; `--print json` emits the same events as
JSON-lines, so a script or a CI job can follow along.

## Policy-based access control

zlogic uses a policy-based permission system to give fine-grained control over agent
capabilities.

Policies can define exactly which resources and actions an agent is allowed to use, including
file access, shell commands, tools and external services, which lets you give an agent only the
permissions a particular task or workspace requires.

Every sensitive action passes a permission gate: deterministic rules first, then a **security
classifier** review — it sees a tool's paths, targets and commands, never file bodies — and finally
**you**. The review runs on the cheap model tier, so how strict that gate is depends on the model
you put there. Grant scopes of `once`, `session` or `project` mean an approved project stops asking.
A built-in sensitive-path list (`.env`, SSH private keys, `~/.aws`, `~/.kube`, `.npmrc`, …) always
forces a prompt. Policies live in `<config dir>/policy.yaml` and are tightened per project in
`.zlogic/policy.yaml`.

In the CLI: `--plan` (read-only planning), `--permission auto|deny|approve-all`, and `--god` for
full access — the last two are for local development only.

## Remote agent execution

With daemon mode the agent runtime runs on a separate machine while the zlogic desktop client
stays your local interface: useful when your code, development environment, GPUs, databases or
cloud tooling live somewhere else. The client connects to the remote agent, so you can monitor
conversations, inspect execution, provide input and control the agent without moving your
development environment.

See [Open source, and what is not](#open-source-and-what-is-not) for what this means for the code
in this repository.

## Security & local data

zlogic is designed with a client-direct architecture for AI provider communication.

* **Direct provider connection** — AI requests go straight from the zlogic client to the provider
  you configured. zlogic operates no server that proxies or relays your model requests.
* **Local conversation storage** — conversation history is stored locally on your device, in a
  SQLite database you can inspect, back up or delete. It is not stored on zlogic servers.
* **Protected credentials** — API keys, database passwords and cloud credentials are kept in the
  operating system's credential manager (macOS Keychain, Windows Credential Manager, Linux Secret
  Service) or in an encrypted vault keyed by a master key in your state directory; environment
  variables are supported too.
* **Credentials stay out of model context** — stored credentials are never exposed to the model as
  conversation content or context, and the reasoning channel is never fed back into the next turn.

When you use an AI provider, your requests are sent directly to that provider, and that provider's
own privacy policy and data handling practices apply to them.

## Open source, and what is not

This repository is the **open-source part of zlogic: the CLI and the agent runtime**. It is
Apache-2.0 and complete enough to build, run and modify:

* **the CLI** — `apps/cli`, the Ratatui terminal front end, and
* **the agent runtime** — every crate under `crates/`: the round loop, the provider clients, the
  tool registry and built-in tools, the permission engine, the SQLite store, MCP, hooks and the
  engine that hosts them.

Three parts of the product are **closed source** and are developed in a separate, private
repository:

* **the desktop app** — the Tauri + React client, distributed as binaries only,
* **the daemon** — remote and mobile access, shipped as `zlogic-daemon`,
* **the closed-source tool packs** — database drivers, data analysis, the HTML widget, and the
  bundled Python runtime, which is why *Database Access* and *AWS & External Services* are marked
  *(desktop app)* above.

All of them build on the crates published here: the runtime you can read in this repository is the
same runtime those clients run, and bugs in the runtime or the CLI are fixable in this repository.
A `zlogic` built from source is a complete CLI; the released binary additionally has the daemon
welded in, which is what makes `zlogic daemon …` work in it.

## Quick start

### Install the CLI

```sh
# bash / zsh
curl -fsSL https://install.zlogic.run | sh

# PowerShell
irm https://install.zlogic.run | iex
```

The installer detects your OS and architecture and puts the binary in `/usr/local/bin/zlogic`
(Linux / macOS) or `%LOCALAPPDATA%\zlogic\bin` (Windows).

### Desktop app

Download from [zlogic.run/#download](https://zlogic.run/#download) — binaries only, since the
client is closed source (see [above](#open-source-and-what-is-not)):

| Platform | Format | Download |
|---|---|---|
| Windows 10/11 x64 | NSIS installer (.exe) | [download](https://zlogic.run/dl/zlogic_latest_x64-setup.exe) |
| macOS 12+ Apple Silicon | DMG | [download](https://zlogic.run/dl/zlogic_latest_aarch64.dmg) |
| macOS 12+ Intel (x86_64) | DMG | [download](https://zlogic.run/dl/zlogic_latest_x86_64.dmg) |
| Linux x86_64 | AppImage | [download](https://zlogic.run/dl/zlogic_latest_amd64.AppImage) |

Windows needs the WebView2 runtime (included in Windows 10+); Linux needs WebKitGTK 4.1 + libsoup3.
The macOS app is not signed or notarised yet, so Gatekeeper blocks the first launch — trust it once
with `xattr -dr com.apple.quarantine /Applications/zlogic.app` in Terminal.

Every artefact ships with a SHA-256 checksum and a minisign signature; the machine-readable list is
at <https://update.zlogic.run/checksums.json> (public key id `75AA378991532581`).

### Configure a model

Keys never enter config files — they go to the OS keyring (or the encrypted vault) or an
environment variable:

```sh
export DEEPSEEK_API_KEY=sk-...   # environment keys are recognised as-is
zlogic key set deepseek          # or store one in the system keyring
zlogic key list                  # which providers have a key
```

`zlogic key set`, `zlogic key delete <provider>` and `zlogic key list` work without a configured
model, which is exactly when the TUI cannot start. Declare a provider that is not in the directory
by hand:

```yaml
# ~/.config/zlogic/models.yaml
providers:
  my-provider:
    sdk: openai_chat
    base_url: https://api.openai.com/v1
    models:
      gpt-4o: {}
```

### Run it

```sh
zlogic                                    # interactive TUI: press Enter for a new session
zlogic --prompt "explain this repository"  # headless one-shot
zlogic --prompt "hi" --print json          # the same turn as event JSON-lines
zlogic --session <id>                      # continue a session; --resume reopens the latest
```

Useful flags: `--model`, `--theme`, `--locale`, `--cwd`, `--plan`, `--quiet`,
`--permission auto|deny|approve-all`. Inside the TUI, `/` opens the command palette:
`/help`, `/model`, `/theme`, `/session`, `/replay`, `/workspace`, `/new`, `/compact`, `/stats`,
`/info`, `/plan`, `/approval`, `/lang`, `/view`. Full reference:
[`apps/cli/docs/cli-arguments.md`](apps/cli/docs/cli-arguments.md).

> 💡 Windows users: use **Windows Terminal / WezTerm / Alacritty / VS Code terminal** — the legacy
> console does not support DEC 2026 and has unstable IME composition.

### Build from source

```sh
cargo build --release -p zlogic-cli    # target/release/zlogic
cargo test --workspace                 # the runtime and the CLI test suites
```

Rust 1.85 or newer (the workspace uses edition 2024). The first build pulls the dependency tree
(rustls, rusqlite, git2, …) from crates.io. Nothing here needs the desktop app, a GPU or a network
service.

## Repository layout

| Path | What it is |
|---|---|
| `apps/cli` | the Ratatui TUI, and the `zlogic` binary |
| `crates/protocol` | the shared protocol types at every boundary (core↔llm, UI→engine, engine→UI) |
| `crates/core` | one agent run: build context, drive rounds, execute tools, persist |
| `crates/engine` | the process-singleton dispatch layer: host API, service assembly, event fan-out |
| `crates/llm` | one client per vendor: request serialisation, part lifecycle, usage normalisation |
| `crates/tools` | tool definitions, the registry and the built-in tools |
| `crates/policy` | shell-command decomposition and path zoning for permission review |
| `crates/store` | the SQLite persistence layer: sessions, entries, usage |
| `crates/objects` | content-addressed object storage for large payloads |
| `crates/credential` | credential references, environment resolution, the encrypted vault |
| `crates/config` | directory layout, YAML config, provider detection, model catalog and prices |
| `crates/mcp` | MCP server definitions, connection pool and tool catalog |
| `crates/plugins` | plugins: a manifest in a directory that contributes MCP servers |
| `crates/hooks` | lifecycle hooks and their command runner |
| `crates/task` | the persisted job / run domain model |
| `crates/code-sitter` | code-aware grep: every hit annotated with its enclosing symbol |
| `crates/logging`, `crates/paths` | tracing setup, and path helpers shared across the workspace |

## Data directories

Data is split across four roots (XDG-style on every platform). **`data` and `state` are the only
places your local data lives — deleting them is unrecoverable**; only `cache` is safe to wipe:

| Root | Holds |
|---|---|
| `~/.config/zlogic/` | hand-written config: `config.yaml`, `models.yaml`, `env.yaml`, `policy.yaml` |
| `~/.local/share/zlogic/` | the object store, attachments, and per-workspace history |
| `~/.local/state/zlogic/` | `state.db` (sessions / entries / usage), locks, logs, the credential vault |
| `~/.cache/zlogic/` | regenerable cache, safe to delete |

Per project: `.zlogic/policy.yaml` (project policy, which only tightens the global one) and
`.mcp.json` (workspace MCP servers). Full reference: [zlogic.run/docs](https://zlogic.run/docs/).

## Status

* **v1.0.0-beta.1 (beta)** — the CLI and the desktop app are both downloadable.
* **Open source** — the CLI and the agent runtime are Apache-2.0 in this repository; the desktop
  app, the daemon and the closed-source tool packs are not. Issues and ideas are welcome in
  [Issues](https://github.com/zlogic-labs/zlogic/issues).
* **Daemon** — `zlogic daemon …` serves the engine as an HTTP JSON + SSE endpoint for remote
  desktop and mobile clients, with pairing instead of accounts and no cloud relay.
* **Mobile app** — planned: chat, voice and remote approval from your phone, connected to the
  runtime on your machine.

## Docs & links

* Website — downloads, docs, privacy: **https://zlogic.run**
* Docs: **https://zlogic.run/docs/** (installation / quick start / CLI vs desktop / providers and
  models / key management / configuration reference / data directory / security and approvals /
  data flow and privacy / FAQ / roadmap)
* Install station: **https://install.zlogic.run**
* Update station: **https://update.zlogic.run** (Tauri v2 auto-update; public key id
  `75AA378991532581`)
* Issues and feedback: **https://github.com/zlogic-labs/zlogic/issues**

## License

[Apache-2.0](LICENSE). Third-party open-source components keep their own licenses.
