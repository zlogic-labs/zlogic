---
name: zlogic-guide
description: "zlogic's own manual. Load it whenever zlogic itself comes up — \"zlogic\", \"@zlogic\", \"set zlogic up for me\" — or when the user asks how to configure, extend or run it: skills, plugins, MCP servers, policy.yaml, models, keys, role chains, remote daemon. Read it, do not guess."
descriptions:
  en-US: "zlogic's own manual. Load it whenever zlogic itself comes up — \"zlogic\", \"@zlogic\", \"set zlogic up for me\" — or when the user asks how to configure, extend or run it: skills, plugins, MCP servers, policy.yaml, models, keys, role chains, remote daemon. Read it, do not guess."
  zh-CN: "zlogic 自己的使用与开发手册。凡是用户提到 zlogic ——「zlogic」「@zlogic」「帮我配一下 zlogic」「zlogic 怎么用」——或者问它怎么配置、怎么扩展、怎么接远程 daemon（skill、plugin、MCP server、policy.yaml、模型与密钥、角色链），先加载这份手册再回答，别凭印象猜。"
---

# The zlogic manual

This document is about **zlogic itself**: how to extend it, how to configure it, where its files
live. Treat it as the source of truth when a user asks about zlogic. For anything it does not
cover, do not guess — go read the code or the shipped config template first.

Contents:

1. [The four roots](#1-the-four-roots)
2. [Skills](#2-skills)
3. [Plugins](#3-plugins)
4. [MCP servers](#4-mcp-servers)
5. [Policy: approvals and budgets](#5-policy-approvals-and-budgets)
6. [Models, providers and keys](#6-models-providers-and-keys)
7. [Remote mode (the daemon)](#7-remote-mode-the-daemon)
8. [Everything else, quick reference](#8-everything-else-quick-reference)
9. [The clients](#9-the-clients)
10. [Troubleshooting](#10-troubleshooting)

---

## 1. The four roots

zlogic splits its state across four root directories. They mean different things — **do not write
to the wrong one**:

| Root | Default path (XDG) | What goes there | What deleting it costs |
|---|---|---|---|
| config | `~/.config/zlogic/` | Hand-written config: `config.yaml`, `models.yaml`, `env.yaml`, `policy.yaml`, `mcp.json`, and the desktop app's `desktop.yaml` | Back to defaults |
| data | `~/.local/share/zlogic/` | Object store, attachments, `extensions/{plugins,mcp,runtimes}/`, `skills/` | ⚠️ All local data is lost |
| state | `~/.local/state/zlogic/` | `state.db` (sessions / entries / usage), locks, logs, `daemon/`, keychain fallback | ⚠️ The whole session store is wiped |
| cache | `~/.cache/zlogic/` | Regenerable caches (MCP tool catalogues, and so on) | Nothing happens |

Precedence: `ZLOGIC_HOME` (folds all four into one) > `XDG_CONFIG_HOME` / `XDG_DATA_HOME` /
`XDG_STATE_HOME` / `XDG_CACHE_HOME` > platform default. On Windows the default is
`%USERPROFILE%\.config\zlogic\`, not `AppData`. Each host uses its own app name, so the matching
home variable is renamed with it (`other-app` claims `OTHER_APP_HOME`).

Anything private to a project lives under that project's `.zlogic/`:

```text
<project>/.zlogic/policy.yaml          # project policy (can only tighten)
<project>/.zlogic/settings.yaml        # project-level shell budgets (tools.shell only)
<project>/.mcp.json                    # workspace MCP servers
<project>/.zlogic/mcp.json
<project>/.zlogic/skills/<name>/SKILL.md
<project>/.agents/skills/<name>/SKILL.md
<project>/.zlogic/extensions/plugins/<name>/
<project>/AGENTS.md                   # project instructions, into the system prompt
```

---

## 2. Skills

A skill is "a written procedure for one kind of task": plain instructions the model follows once
it has read them. **Discovery reads only the frontmatter and one line of description into the
system prompt; the body arrives when the model loads it by name with the `skill` tool.** Get the
description wrong and the skill does not exist — it is skipped and reported.

### 2.1 Format

```markdown
---
name: release-notes          # optional; defaults to the directory name
description: Draft release notes from the changelog between two git tags.
---

# Body

(The procedure the model follows.)
```

- The frontmatter must be `---`-delimited YAML at the very start of the file.
- `description` is **required**, at most 300 characters (longer is trimmed with an ellipsis).
- Fields this build cannot honour yet — `allowed_tools`, `context: fork`, `model`, `effort` — are
  **reported** at load time rather than silently ignored.
- Limits: `SKILL.md` ≤ 256 KiB; at most 100 skills listed at once.
- Names and descriptions may not contain control or invisible characters (rejected outright).

Placeholders available in the body: `${SKILL_DIR}` (the skill directory) and `${SESSION_ID}` (the
current session), expanded when the skill is loaded; `$ARGUMENTS` / `$1` are left for each call to
interpret.

**A skill executes nothing by itself.** If the procedure needs a command, the model calls `shell`
itself, and that call goes through the approval gate like any other.

### 2.2 Where they live, and who wins

| Location | Owner |
|---|---|
| `<data>/skills/<name>/SKILL.md` | The user's own (what the desktop marketplace installs) |
| `<root>/.zlogic/skills/<name>/SKILL.md` | Travels with the repository |
| `<root>/.agents/skills/<name>/SKILL.md` | Travels with the repository (the cross-agent standard location) |
| `<plugin>/skills/<name>/SKILL.md` | Contributed by a plugin, namespaced `<plugin>:<name>` |

On a name collision **the later read wins**: `<data>/skills` < `.zlogic/skills` < `.agents/skills`.
Plugin skills carry a namespace, so they collide with nothing. An override is always reported,
never silent.

A `.zlogic-disabled` marker file in a skill directory skips the whole directory.
**Disabling a plugin takes its skills with it** (the same switch as its MCP servers).

### 2.2.1 The one skill that ships inside the program

`zlogic-guide` — this document — is compiled into the binary. It is the only skill that is, and
that is deliberate: it is the manual, so what you are reading has to describe the build you are
holding. Everything else is maintained separately and reaches users by being installed, which is
why changing it does not wait on an app release.

Two consequences worth knowing:

- **It has no directory**, so no `.zlogic-disabled` marker can sit in one. Its switch is a key in
  the extension state file, visible in **Runtime → Skills** in the desktop app, and it is
  machine-wide rather than per-workspace. **It ships switched off** — you are only reading this
  because it is switched on. Off means out of the model's reach entirely: not listed in the
  system prompt, and `skill` refuses the name.
- **A user or workspace skill named `zlogic-guide` replaces it**, because the bundled entry is
  seeded before everything else and the later read wins. Doing that makes the model read a manual
  written for a different build, so leave it alone unless you mean it.

### 2.3 Invoking one

- The model loads it on its own: the system prompt lists "name — description (path)", and when the
  task matches, the model calls the `skill` tool by name, with `args` if the skill takes any.
- The user calls it directly: in the desktop app, `/<name> <arguments>`, and `/unload <name>` to
  take it back out of the session. `@zlogic` is a shortcut for this guide specifically: the
  composer offers it as an `@` mention and it arrives as an ordinary skill token, so
  `@zlogic how do I reach a remote daemon?` loads this document and asks the question in one line.
- The CLI (TUI) currently lists only its **built-in** commands in the `/` panel — the skill
  catalogue is not wired into `command_catalog` yet. In the CLI, just ask in plain language and the
  model will load the skill.
- A model-initiated load is a **Write-risk** tool call, so policy may ask first; a `/name` the user
  typed does not go through the tool gate.
- **Edits take effect on the next turn, with no restart** (discovery runs every turn).

### 2.4 Writing a new skill

1. **Choose the location.** To share it with a team, put it in the repository at
   `<root>/.zlogic/skills/<name>/`; for personal use, `<data>/skills/<name>/`.
2. **Write the description first.** It is the only thing the model sees, so write *when to reach
   for it*, not *what it is*:
   - Bad: `PDF form processing tool`
   - Good: `Flattens a filled PDF form into a single PDF; use when the user says pdf / form /
     flatten`
3. **Write the body as a procedure**: second person, numbered steps, name the tool to use at each
   step, and say what decides each step. Do not write "see the reference" — the model will not go
   and read it.
4. **Keep the frontmatter tiny** (name / description only) and put the steps in the body. A
   description written as a YAML block scalar (`description: |`) is folded onto one line, so
   wrapping it across several lines is fine; it is truncated to 300 characters.
5. **Verify**: it should appear in the skill catalogue on the next turn; load it once via
   `/<name>` or by asking the model, and confirm the result reports no `unsupported` fields.

---

## 3. Plugins

A plugin is "a directory with a manifest", optionally carrying MCP servers, `skills/` and
`agents/`. **Paths in a manifest are only data: being discovered is not the same as being run.**

```text
<data>/extensions/plugins/<name>/           ← installed by the user
<root>/.zlogic/extensions/plugins/<name>/   ← travels with the repository
    plugin.json | plugin.yaml              ← manifest (looked up in this order: plugin.json → plugin.yaml → plugin.yml → zlogic-plugin.json → zlogic-plugin.yaml → .codex-plugin/plugin.json → .claude-plugin/plugin.json)
    .mcp.json                              ← or keep the servers in their own file
    skills/<skill-name>/SKILL.md
    agents/<agent>.md
```

### 3.1 Manifest fields

```json
{
  "name": "Acme tools",
  "version": "1.2.0",
  "description": "…",
  "enabled": true,
  "mcpServers": {
    "search": { "command": "acme-search", "args": ["--root", "${pluginRoot}"] }
  }
}
```

- The identity is the **directory name**; `name` in the manifest is only a display label.
- The last two manifest locations are where the other agent CLIs keep theirs — Codex writes
  `.codex-plugin/plugin.json` and reads `.claude-plugin/plugin.json` as its own fallback — so a
  package built for either installs here unchanged.
- `skills` names the directories holding the plugin's skills, as a string (`"./skills/"`, Codex) or
  an array (`["./"]`, Claude Code). Either may point at the plugin root itself, where a `SKILL.md`
  sitting directly in it counts as one skill. Without the field, `skills/` is assumed. A path
  climbing out of the plugin root is ignored.
- MCP servers may be inlined in the manifest (the keys `mcpServers` / `mcp.servers` / `servers` are
  all accepted) or kept in the plugin directory as `.mcp.json` / `mcp.json`.
- `${pluginRoot}` / `${ZLOGIC_PLUGIN_ROOT}` point at the plugin's own directory, and may appear in
  `command` / `args` / `env` / `cwd` / `url` / headers. They are substituted **after parsing**, so
  Windows backslashes cannot break the JSON.
- Contributed MCP servers are namespaced `<plugin>.<server>`, and their tools are
  `mcp__<server>__<tool>`. Two plugins can each ship a server called `search`, and a plugin can
  never shadow a server you configured yourself.
- `agents/*.md` is catalogued only and is not loaded or run; `commands/` and `hooks/` are likewise
  only **reported** as having nowhere to live.

### 3.2 Enabling, disabling and trust

- Three ways to disable: `"enabled": false` in the manifest, a `.zlogic-disabled` file in the
  directory, or the switch on the desktop marketplace page (which writes exactly that marker).
- When names collide, **the repository's plugin wins** — the same precedence direction as MCP
  directories: what the user configured explicitly on the left, what the repository pins on the
  right.
- **An MCP server brought by a repository does not start on its own.** It arrives with `git clone`,
  and its manifest can name any command on the machine, so nothing launches until you confirm it;
  the notice says what it would do (`starts a process …` / `can access the network …` / `can read
  …`). The confirmation is bound to a fingerprint of the definition: change the definition — even
  just moving an argument — and it has to be confirmed again.

### 3.3 Installing a plugin

Only the desktop app has a marketplace that installs:

```text
owner/repo
owner/repo@tag-or-branch
https://github.com/owner/repo
owner/repo#subdir
https://github.com/owner/repo/tree/<ref>/subdir
```

You can also pick a local folder: containing `plugin.json` / `plugin.yaml` (or
`.codex-plugin/plugin.json` / `.claude-plugin/plugin.json`) → a plugin; containing
`SKILL.md` → a skill; containing `.mcp.json` / `mcp.json` → an MCP bundle.

- Installing is a deliberate **filesystem** operation, not running the plugin: the source is
  resolved to `owner/repo/ref`, the archive is unpacked with links and path traversal refused, the
  replacement is staged in the managed directory and then renamed atomically, and the origin is
  recorded in `.zlogic-source.json`.
- **Capabilities are reviewed before install**: a source carrying an executable MCP declaration
  spells out the process it would start and the network it would reach. Decline and nothing is
  installed.
- The CLI has no command for installing extensions.
- **A local folder cannot be installed against a remote daemon** (the folder is on the daemon
  host); use a GitHub source instead.

---

## 4. MCP servers

### 4.1 Where they are declared

| Location | Owner |
|---|---|
| `<config>/mcp.json` | Global, hand-written |
| `<data>/extensions/mcp/*.json` | Installed by the desktop marketplace (one file per server) |
| A plugin's `.mcp.json` / inlined manifest | Contributed by the plugin |
| `<root>/.mcp.json`, `<root>/.zlogic/mcp.json` | Workspace |
| `<root>/.zlogic/extensions/mcp/*.json` | Workspace, several files |

Precedence: **global → plugin → workspace**, later reads winning. A repository pins a server
precisely so that everyone on it gets the same one.

### 4.2 Format

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "${env:GITHUB_TOKEN}" }
    },
    "search": {
      "url": "https://mcp.example.com/mcp",
      "auth": { "type": "bearer", "credential": "${keyring:search}" },
      "tools": ["search", "fetch"]
    }
  }
}
```

- `servers`, a nested `mcp.servers`, and a single bare server object (`<id>.json`) are accepted too.
- The transport is inferred from `command` (stdio) or `url` (http), or can be stated with `type`.
  The old `"type": "sse"` is **not supported** and is reported as an error rather than quietly
  treated as http.
- `tools` is an allowlist (only those are exposed); `exclude` / `excludeTools` is a denylist.
- `binding: "session"` is for stateful services (a browser, say); by default the resolved arguments
  decide whether a server is shared.
- Credentials are references only: `${env:VAR}`, `${keyring:entry}`. **A plaintext token is never
  resolved into configuration.**
- Tool names are `mcp__<server>__<tool>`, truncated with a hash past 64 characters.
- **A server that arrived with the repository does not start by default** (`.mcp.json` is someone
  else's code). Turning it on in the workspace's MCP list *is* the confirmation: the engine records
  a fingerprint of the definition and closes the door again if the definition changes. The switch
  is workspace-scoped — it writes `<workspace>/extensions.json` and affects no other project.

### 4.3 Two costs worth knowing

- **MCP tool definitions are resent every turn.** Past roughly 10% of the context window (or 15k
  tokens when the window is unknown) a notice names the biggest offenders; the fix is a `tools`
  allowlist, or switching off what you are not using in the workspace's MCP list.
- A misspelled tool name in a manifest is not ignored — it is reported as a problem.

---

## 5. Policy: approvals and budgets

The approval gate runs before every tool call. The security model is "the model may propose an
action; sensitive actions need local confirmation".

### 5.1 Where the files are

- Global: `<config>/policy.yaml`
- Per project: `<root>/.zlogic/policy.yaml`

**The project copy can only tighten**: the global file loads first, the project second, rules are
unioned, the strictest verdict wins, and budgets take the smaller allowance. A cloned project
therefore cannot widen its own permissions.

### 5.2 Shape

```yaml
policy:
  version: 1
  default: ask          # ask | deny (`allow` is rejected: defaulting to allow leaves a back door for every future operation type)
  commands: []          # command-level rules (POSIX)
  exec: []              # executable-level rules
  paths: []             # path-level rules
  scripts: []           # script rules
  protected_delete: []  # deletions of these paths are always denied
  protected_write: []   # writes to these paths always ask
budget:
  per_turn: 2.00                    # spend cap for one submission (including sub-agents)
  per_task: 0.50                    # one background task; falls back to per_turn when unset
  window: { amount: 50, hours: 720 } # rolling window total
  on_exceeded: ask                  # warn | ask | stop
```

A malformed `budget:` block only disables the budget and produces a visible warning; the `policy:`
half still applies. Amounts must be positive (0 does not mean unlimited).

### 5.3 The four kinds of rule

**`commands` (command level, POSIX shells only)**

```yaml
commands:
  - id: allow-status
    pattern: "git status **"     # must be a single simple command
    effect: allow
  - id: deny-force-push
    pattern: "git push --force **"
    effect: deny
```

- `*` matches **one** word, `**` matches any number of remaining words.
- Redirection (`>`) and a leading `FOO=1` assignment are part of the command: a pattern that omits
  them will not match.
- `require_static_args` (default `true`) restricts wildcards to static words; allow `$VAR`, globs
  and `$(...)` to match and you must turn it off explicitly (`allow_substitutions` defaults to
  `false`).
- `deny` / `ask` patterns are not bound by those restrictions — a tightening rule should not fail
  because an argument was "unreadable".

**`exec` (executable level)**

```yaml
exec:
  - id: allow-cargo-check
    head: cargo            # or "*" for any executable
    args_prefix: [check]   # or args: for an exact match (pick one, never both)
    effect: allow
    require_static_args: true
```

Dynamic arguments (`$VAR`, pipes, `xargs`) do not get an allow by default; `sudo` / `doas` / `su`
can never be allowed by an exec rule — write an explicit `commands` pattern for those.

**`paths` (path level)**

```yaml
paths:
  - id: no-writes-outside-workspace
    zone: workspace        # workspace | home | system | sensitive | network | provider | other | unresolved
    access: [write, delete]
    effect: deny
  - id: allow-logs
    glob: "**/logs/**"
    access: [read]
    effect: allow
```

`*` does not cross a separator, `**` does, `?` matches one non-separator character. The `sensitive`
and `unresolved` zones **cannot** be allowed. Give at least one of `zone` and `glob`.

**`scripts`**: `{ id, zone, glob, effect }`, with the same refusal to allow `sensitive` /
`unresolved`.

### 5.4 Floors that configuration cannot switch off

Whatever the policy says:

- The `sensitive` zone (`.env`, SSH private keys, `~/.aws`, `~/.kube`, `~/.gnupg`,
  `~/.docker/config.json`, `~/.config/gh`, `.npmrc` / `.pypirc` / `.netrc`, `*.pem` / `*.key`,
  `id_rsa` and friends) is **deny** for read, write and delete.
- Writes and deletes in the `system` zone are always **deny**.
- The four XDG roots cannot be deleted; neither `policy.yaml` file can be written with an automatic
  approval; the grant record is protected the same way.

### 5.5 Approval modes and grant scope

- The gate has four layers: deterministic rules (no model involved) → model review (the `approval`
  role chain: session model, `light` as fallback) → deep review (`approval_deep`: session model,
  `main` as fallback) → ask the user. A reviewer sees paths, targets and commands, **never file
  contents**. A review timeout is treated as "ask the user".
- Modes: `ask` (default) / `deny` (refuse everything automatically, right for non-interactive use)
  / `approve-all` (everything passes; local development only).
  - Config: `session.approval_mode: auto | bypass`
  - CLI: `--permission auto|deny|approve-all`, `--god` (same as approve-all), `--plan` (read-only
    planning)
  - TUI: `/approval [auto|deny|all]`
- Grant scope: `once` (this call) / `turn` (this turn) / `session` (this session) / `workspace`
  (persisted per project) / `global` (machine-wide, only the user can pick it). The larger scopes
  are not all pre-ticked in one prompt; the user chooses.

---

## 6. Models, providers and keys

### 6.1 Two files

- `models.yaml`: **providers and models only**. The built-in providers (anthropic / openai / gemini
  / deepseek / glm / dashscope / openrouter / fireworks / bedrock and so on) are compiled into the
  program and **appear on their own once a key is detected** — nothing to write.
- `config.yaml`: everything else (`default_model`, `llm_roles`, `context`, `tools`, `network`,
  `worktree`, `checkpoints`, `retention`, `cost`, `limits`, `log`, `session`).

Both files are `deny_unknown_fields`: writing `context:` into `models.yaml` makes **the whole file
fail to load** rather than being ignored.

### 6.2 Where keys come from

**There is nowhere in the config to put a key**, and config gets printed, logged and pasted into
issues. Lookup runs by id and the first hit wins:

1. `ZLOGIC_<PROVIDER>_API_KEY` — for zlogic only, so it does not collide with other tools in the
   same shell
2. The vendor's own variable: `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GEMINI_API_KEY` /
   `DEEPSEEK_API_KEY` / …
3. `<PROVIDER>_API_KEY` — the general rule for self-hosted providers (`my-gw` → `MY_GW_API_KEY`)
4. The system keychain: service `zlogic`, entry name = the provider id
   - web search is the exception: entry names `web_search:exa` / `web_search:parallel` (they are
     search services and must not fight a provider for the same entry)

`auto_detect_env: false` in `models.yaml` switches off the environment leg entirely, leaving only
the keychain. `<config>/env.yaml` is a flat `KEY: value` table read at startup and used as
environment variables (it too only feeds credential resolution).

Command line:

```sh
zlogic key set                       # interactive: ↑↓ to pick a provider, then type the key
zlogic key set anthropic             # interactively enter one provider's key
zlogic key set anthropic sk-ant-...  # write it directly
zlogic key delete <provider>
zlogic key list                      # which providers have a key, which do not
zlogic auth                          # which subscription providers are logged in
zlogic auth login <provider> [--device]
zlogic auth logout <provider>
zlogic auth refresh <provider>       # re-fetch which models that account can call
```

Subscription tokens (a ChatGPT plan, say) live in a `<provider>_oauth` entry, separate from an
ordinary key — the two never overwrite each other.

`keychain: false` keeps keys out of the system keychain and stores them in
`<state>/security/.key.json` (unencrypted). It defaults to off on macOS (an unsigned build prompts
for every access and the user cannot dismiss it) and to on elsewhere.

### 6.3 Adding a provider or model

```yaml
# models.yaml
providers:
  my-provider:
    sdk: openai_chat      # openai_chat | openai_responses | openai_generic | deepseek | glm | dashscope | qwen_local | openrouter | fireworks | anthropic | gemini | bedrock
    base_url: https://gw.corp.internal/v1
    models:
      internal-70b:
        display_name: Internal 70B
        tier: main           # light | main | thinking
        context_window: 128000
        max_output_tokens: 32000
        vision: false
        thinking:
          supported: true
          can_disable: true
          efforts: [low, medium, high]
        no_think_params:      # how "thinking off" is expressed in the request body (per vendor; never guess)
          chat_template_kwargs: { enable_thinking: false }
        pricing:
          input_per_m: 0.5
          output_per_m: 2.0
```

- A missing `context_window` falls back to a conservative value with a warning: too small only means
  compaction happens earlier, too large means the provider hard-errors.
- `tier` decides whether the model joins the cheap / main / reasoning pool; auxiliary calls
  (titles, approvals, summaries) pick from it.
- `openai_generic` is for self-hosted OpenAI-compatible endpoints, and is the only sdk that accepts
  `generic:` declarations (`reasoning_carrier` / `effort_field` / `thinking_off`).
- An endpoint like Azure — same protocol, different authentication — is expressed with `wiring:`
  (`auth_header: api-key`, `query: {api-version: ...}`).
- The model key is the deployment name; `wire_model` points at the real model name.

### 6.4 The default model and role chains

```yaml
# config.yaml
default_model: deepseek:deepseek-v4-flash

llm_roles:
  title:
    models: [session, light]     # session model first, light as fallback
    thinking: off
  compaction:
    models: [session, main]
  "agent:researcher":
    models: [main]
```

- Each entry in a chain is a `provider:model`, a tier name (`light` / `main` / `thinking`, expanded
  to every model in that tier), or the reserved word `session` (this session's main model).
- **`session` is appended to the end of every chain**, so naming a model with no key configured
  never kills a role outright.
- With nothing configured, the built-in chains all start from `session`: main conversation /
  summaries / sub-agents → `session`; titles / approvals / utility → `session` → `light`; deep
  approval → `session` → `main`.
- Role names: `session` (main conversation), `title`, `compaction`, `approval`, `approval_deep`,
  `utility`, `agent:<name>`. Adding an `agent:<name>` adds one optional sub-agent.

### 6.5 Context compaction

```yaml
context:
  compact_ratio: 0.8      # compact when the previous input reaches this share of the window (0.1–0.95)
  tail_turns: 1           # the last N turns are kept verbatim, out of the summary (minimum 1)
  overflow_retries: 1     # retries when it still overflows after compaction
```

---

## 7. Remote mode (the daemon)

The daemon exposes the engine over HTTP JSON + SSE, so the desktop app (or a phone) can connect and
have the agent run on another machine.

### 7.1 Commands

```sh
zlogic daemon                                   # local; a random free port, recorded in <state>/daemon/listen.json
zlogic daemon --listen 127.0.0.1:7331            # a fixed port
zlogic daemon --listen 0.0.0.0:7331 \
  --public-url https://192.168.1.10:7331 --pair  # remote + pairing
zlogic daemon pair [--scopes read,write] [--daemon PORT]
                                       # give a *running* daemon another device, no restart
zlogic daemon devices                           # paired devices
zlogic daemon revoke <device-id>
zlogic daemon doctor                            # diagnostics: state writability, device count, port, protocol
```

Other options: `--tls-cert/--tls-key` (your own certificate), `--public-certificate-sha256` (the
reverse proxy's certificate fingerprint, baked into the pairing QR code), `--max-upload-mib`
(per-attachment cap, 2048 by default), `--dangerously-skip-permissions` (skip the engine's own
permission prompts).

A CLI built from source has no daemon in it and hands whatever follows `daemon` to a sibling
`zlogic-daemon` binary, or to whatever `$ZLOGIC_DAEMON` points at.

### 7.2 Pairing

A remote start does this: generates and persists a self-signed certificate
(`<state>/daemon/tls/`) → prints the certificate's SHA-256 fingerprint, a **one-time secret valid
for 5 minutes**, and a QR code.

The QR code is `zlogic://pair?url=…&secret=…&fingerprint=…`. The client pins the certificate by
`fingerprint` first, then:

```http
POST /pairings/claim
Content-Type: application/json

{"secret":"<one-time secret>","device_name":"Alice's phone"}
```

The returned `token` **appears exactly once**; the server keeps only its SHA-256, and the client
puts the plaintext in the system keychain. Every later request carries
`Authorization: Bearer <token>`.

- A wrong secret is rate-limited, but it never destroys a still-valid pairing offer for someone
  else.
- A correct secret is consumed immediately on verification, before anything is written to disk, so
  it cannot be replayed.
- **Device scopes are checked per request**: queries need `read`, ordinary changes `write`, and
  config / credentials / extensions / device management `admin`; `full_control` covers all of them.
  Devices expire after 90 days, and an expired token always fails closed.
- **The first remote start must carry `--pair`**: with no paired device the daemon refuses to listen
  externally.

### 7.2b Pairing a running daemon

`--pair` only works at startup, so another device used to mean a restart — which drains the sessions
in flight. `zlogic daemon pair` asks the **running** daemon for an offer over a local channel instead,
and prints the same QR code and secret that `--pair` would have.

macOS / Linux only. The channel is a unix socket in a 0700 directory under `$XDG_RUNTIME_DIR/zlogic/`
(`$TMPDIR` on macOS), named after the listen port, so only the same user can reach it — the same bar as
being able to restart the daemon. It is not HTTP, listens on no TCP port, and carries exactly two
operations (`info`, `pair`) as one JSON line each way, capped at 8 KiB. The offer stays in the
daemon's memory: nothing on disk, no long-lived credential, five-minute expiry, consumed on first use.

Windows has no equivalent "same user only" channel — that needs Win32 DACL calls on a named pipe —
so no channel is created there and pairing a running daemon stays a restart with `--pair`.
`zlogic daemon pair` on Windows prints that reason and exits.

With several daemons on one machine, `zlogic daemon pair` lists them (address + paired device count)
and asks which one; `--daemon <port>` skips the question, and a non-terminal just gets the port list.
`--scopes` defaults to `full_control`, matching `--pair`; pass `--scopes read` for a read-only device.

### 7.3 Connecting the desktop app

Settings → **Run engine** → remote daemon → add: name, address, certificate SHA-256 (comma or space
separated for several), device name, one-time pairing secret.

- The pairing secret is **write-only**: it is handed to the native layer at the moment you press
  connect, and the token from a successful claim goes to the keychain
  (`keyring:desktop-daemon-<url-hash>`, derived from the url, nothing to configure). Hand-writing
  `desktop.yaml` uses snake_case, and `pairing_secret` there is ignored.
- When the daemon refuses a stored token (device revoked, 90 days expired, daemon state reset), the
  desktop app asks for pairing again rather than retrying a claim with the dead secret.
- If an explicitly configured remote is unreachable the app **fails outright** — it never silently
  falls back to the local engine, so that work does not land on the wrong machine.
- The underlying config is the `backend:` block of `~/.config/zlogic/desktop.yaml`
  (`mode` / `active` / `daemons[].url|name|certificate_sha256|device_name`). Prefer the UI.
- What changes when remote: workspace files, skills, plugins, MCP servers and policy all live on
  the **daemon host**; local-folder installs are unavailable; attachments go over HTTP.

### 7.4 Operations

- Behind a reverse proxy: keep the daemon on loopback, point `--public-url` at the proxy's HTTPS
  address, and give `--public-certificate-sha256` the proxy's certificate fingerprint.
- On SIGINT / SIGTERM it drains first: refuse new writes, cancel turns in flight, then allow in-
  flight requests 30 seconds.
- Main endpoints: `GET /health`, `/api/...`, `GET /api/sessions/{id}/events` (SSE, resumed with
  `Last-Event-ID`), `GET /api/events/notices`, `/livez`, `/readyz`, `GET /api/metrics` (paired
  devices only), `POST /api/rpc/{engine_method}`, `POST /api/commands`.
- The local native client can connect without a token; unauthenticated requests carrying a browser
  `Origin` are refused, so that no web page can drive your local daemon.

---

## 8. Everything else, quick reference

All of it is `config.yaml` (`deny_unknown_fields`; a wrong key fails the whole file).

```yaml
tools:
  default_shell: auto        # auto | git_bash | ps7 | powershell | cmd | bash
  max_result_chars: 30000    # an oversized tool result goes to the object store; only head and tail come back
  timeout_secs: 0            # the master stop for any tool; 0 = unlimited (an emergency brake, not a daily limit)
  shell:
    quick_secs: 60           # git status / ls / grep
    test_secs: 600           # tests, deliberately generous
    build_secs: 1200
    wait_secs: 3600          # the ceiling for wait: true
    stall_secs: 600          # no output at all = stuck
    progress_secs: 30        # how often to say "still running" (display only)
  web_search:
    provider: exa            # exa | parallel; unset = use whichever has a key
    timeout_secs: 25

limits:                      # guards against getting stuck, not a budget
  max_rounds: 150             # most model calls per submission
  max_depth: 1                # sub-agent nesting depth
  max_parallel_tools: 16
  task_wait_secs: 60

checkpoints:                 # local history (undo / rollback / diff depend on it)
  enabled: true
  retention_days: 7
  max_snapshots: 500
  max_size_gb: 2
  max_file_mb: 256
  max_files: 200000

retention:
  enabled: true
  cache_days: 7
  logs_days: 30
  session_days: 7            # sessions untouched for longer than this are deleted (not archived)

network:
  proxy: http://127.0.0.1:7890   # protocol required; http / https / socks4 / socks4a / socks5 / socks5h
  no_proxy: [localhost, 127.0.0.1]

worktree:
  dir: "../{workspace}-worktrees"  # relative to the project root; {workspace} = directory name, ~/ expands
  # A new worktree carries over two things: a hardcoded list (.zlogic/settings.yaml, .zlogic/policy.yaml, .mcp.json…)
  # and a .worktreeinclude at the repository root (gitignore syntax, copies only ignored files, never overwrites)

cost:
  display_currency: USD
  rates: [{ from: USD, to: CNY, rate: 7.2 }]

log:
  level: info                # error | warn | info | debug | trace
  to_file: true
```

The traps worth knowing:

- `network.proxy` covers **only the engine's own outbound traffic** (model calls, price tables,
  catalogue fetches). Marketplace downloads, MCP servers, `web_search` and the desktop app talking
  to a remote daemon each have their own client. `web_fetch` deliberately does **not** use the
  proxy (SSRF).
- A project's `<root>/.zlogic/settings.yaml` carries only two blocks: the six numbers under
  `tools.shell.*` (see [Environment variables](#environment-variables) for the other), and `env:`.
  Every other block is ignored. A misspelled key inside `shell` or `env` invalidates that block on
  purpose (otherwise `test_sec` would be ignored silently and the test suite would be killed at the
  default ten minutes).
- An absolute `worktree.dir` means every project shares one pool, and a name clash is refused
  rather than silently reused. Keeping it in the repository means editing `.gitignore`, and
  creating a worktree from inside a worktree nests; the default therefore sits beside the project.
- `checkpoints.enabled: false` writes not one byte: with no snapshots, undo and rollback are gone.
  `node_modules`, `.venv` and the like are excluded unconditionally; build output directories are
  skipped only past the file-count limit.

### When a config change takes effect

- The desktop settings panel writes the file back and reloads when you save.
- The CLI triggers a `config_reload` when you open the `/model` panel.
- Otherwise restart the client.
- Skill changes take effect on the next turn (discovery runs every turn), and policy is re-read
  every turn.

### Environment variables

Every `shell` call runs with its environment rebuilt. The shell reads the user's startup files
(`tools.shell.read_profile`, on by default), so their `PATH` is already there — a `set -e`, an
`EXIT` trap or a blocking prompt in that file applies to your command too, which is the user's
choice to make rather than something to work around. Anything a command needs *beyond* the profile
has to be declared:

```yaml
# <config>/config.yaml — every project
env:
  enabled: true
  variables:
    NODE_ENV: development
    RUST_LOG: { value: debug, enabled: false }
```

```yaml
# <project>/.zlogic/settings.yaml — travels with the repository
env:
  variables:
    CI: "1"
```

Three layers, lowest to highest: **global** (`config.yaml`) → **workspace** (the project's
`settings.yaml`) → **session** (the desktop's Variables tab, in `state.db`). A name declared higher
wins; a higher layer that sets `enabled: false` **masks** the lower one instead of falling back —
the only way to say "not the global one here". The session layer covers the conversation's
sub-agents too.

- A command refers to one as `$NAME`. **zlogic never rewrites the command**, so approvals and policy
  see the command as written.
- The variables reach the **child process** only. A `*_API_KEY` declared here is not a provider key —
  that lookup is `zlogic key list`.
- Built in: `ZLOGIC_WORKSPACE_ROOT`, `ZLOGIC_CWD`, `ZLOGIC_SESSION_ID`, `ZLOGIC_TURN_ID`,
  `ZLOGIC_CACHE_DIR`, `ZLOGIC_OS`, `ZLOGIC_ARCH`, `ZLOGIC_VERSION`. The prefix is reserved.
- Refused at every layer: the dangerous names above (`PATH`, `LD_*`, `NODE_OPTIONS`, `BASH_ENV`, …)
  and the `ZLOGIC_` prefix. A **workspace** variable is also refused if its name looks like a
  credential, since `git clone` delivers it — and the refusal is reported, never silent.
- Values are literals. No `${env:}` / `${keyring:}` expansion here.
- `PATH` is not declarable, and never has to be: one throwaway login shell is asked for its `PATH`
  and nothing else (into a temp file, 3s timeout, cached, `cygpath -w` on Windows), which reaches a
  `cmd` or PowerShell session that reads no profile of its own. A failure is silent and falls back
  to the inherited PATH. Ask bash, so `~/.zshenv` is only picked up where bash is the backend — zsh
  is not a selectable `default_shell`.
- The system prompt gets one `env:` line, values omitted for credential-shaped names.

`docs/env-variables.md` has the whole thing, including what is deliberately not done.

---

## 9. The clients

### CLI

```sh
zlogic                                  # interactive TUI
zlogic --prompt "..."                   # one-shot question
zlogic --prompt "..." --print json       # events as JSON lines
zlogic '**hi**' | zlogic --render-md -  # render markdown
zlogic --model deepseek:deepseek-v4-flash --cwd /path/to/project
zlogic --session <id> | zlogic --resume  # continue a session
zlogic --permission deny --prompt "..."  # non-interactive, fail-closed
zlogic --plan --prompt "..."            # read-only planning
zlogic --probe                          # terminal capability diagnostics, no session
```

The TUI's `/` panel (built-in commands): `help` `model` `theme` `session` `replay` `workspace`
`new` `compact` `stats` `info` `plan` `approval` `lang` `view`. `@` references a file, `Enter`
sends, `Shift+Enter` adds a newline.

When you answer an authorisation prompt in the terminal you can pick the scope: `once` / `session` /
`always`.

### Graphical clients

File browsing, a Git panel, usage and cost, visual configuration, the plugin / skill marketplace,
local history (checkpoints), connection management, and switching to a remote daemon backend. Their
`/` command set differs slightly from the TUI's, and they add `/unload <skill>` and `/clear`.

---

## 10. Troubleshooting

**A skill never shows up**
- No `---` frontmatter, or no `description` → skipped and reported.
- A `.zlogic-disabled` file in the directory.
- Another location overrode it by name (the report says which one was used and which ignored).
- The plugin contributing it is disabled.
- Cut off past 100 skills (also reported).

**A skill loads but looks wrong**
- `unsupported` appears in the result: the frontmatter declares `allowed_tools` / `context: fork` /
  `model` / `effort`, which this build cannot honour.
- Check the spelling of `${SKILL_DIR}` / `${SESSION_ID}` (a wrong case does not expand and is left
  in the body verbatim).

**A plugin or MCP server has no effect**
- The manifest filename is outside the lookup order, or it is not valid JSON / YAML.
- The server was never confirmed: a repository-provided server does not start until it is, and the
  notice says why.
- Tool definitions passed 10% of the context and got named — tighten the `tools` allowlist.
- Name collisions: the workspace overrides the global one; a plugin's server is namespaced
  `<plugin>.` and cannot clash.

**A config change did nothing**
- Nothing reloaded (see §8).
- The key went into the wrong file: both YAML files are `deny_unknown_fields` and fail to load
  whole, with a line number.
- The key was never resolved: `zlogic key list` shows the lookup order; with
  `auto_detect_env: false` the environment does not count.

**An approval blocked you**
- Read the reason in the notice: `deny` is a hard rule (policy or a built-in floor), `ask` is
  asking you to pick a scope.
- A project's `policy.yaml` can only tighten, so "suddenly stricter" is usually it.
- `--permission deny` refuses everything automatically; `--god` lets everything through — both for
  local development only.

**The remote will not connect**
- `zlogic daemon doctor` reports on the state directory, device count and port.
- When an explicitly configured remote is unreachable the desktop app does not fall back to local:
  check the address, the certificate fingerprint, and whether the token was refused (90 days /
  revoked).
- A first remote start without `--pair` is refused permission to listen externally.

**The context is being blown out**
- `/compact` summarises immediately; lower `context.compact_ratio` (lower = earlier).
- MCP tool definitions are a fixed per-turn cost: tighten the `tools` allowlist or switch servers
  off.
- An oversized single tool result goes through `tools.max_result_chars`.
