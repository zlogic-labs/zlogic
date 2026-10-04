# Environment variables

Status: implemented.

Every `shell` call runs with its environment rebuilt from scratch, and by default the shell reads
its startup files: bash is launched as a login shell (`-l`, so `/etc/profile` and `~/.bash_profile`
run), PowerShell runs its `$PROFILE`, cmd runs its AutoRun commands. The PATH a user spent an
afternoon wiring up is therefore already there — see [The startup file](#the-startup-file) for the
other setting and what it costs. Anything a command needs *beyond* that has to be declared.

```yaml
env:
  enabled: true
  variables:
    NODE_ENV: development
    RUST_LOG:
      value: debug
      enabled: false
```

## The three layers

| Layer | Where | Follows you |
|---|---|---|
| `global` | `<config>/config.yaml` → `env:` | every project, every session |
| `workspace` | `<project>/.zlogic/settings.yaml` → `env:` | everyone who clones the repository |
| `session` | `state.db`, table `session_env` | this conversation and its sub-agents |

Precedence runs lowest to highest in that order. All three use the same block, so one loader reads
any of them.

### What "higher wins" means exactly

- A name declared in a higher layer **replaces** the lower one.
- A name declared in a higher layer with `enabled: false` **masks** the lower one. It does not fall
  back. This is the only way to say "do not use the global `FOO` in this project", and it is why a
  disabled entry is a tombstone in the merge rather than a skipped row.
- A name a higher layer does not mention is inherited unchanged.
- `""` is a legitimate empty value and is not the same as being switched off. The configuration
  layer takes no `null`.

A layer's own `enabled: false` means it contributes nothing at all, whatever its variables say —
the difference between "I have no variables here" and "I do not want any from here".

## How a command uses one

As `$NAME`, the way it always has. **zlogic never rewrites the command text.** The variables go
into the child process's environment and the shell expands them, so:

- the approval prompt shows the command the model actually wrote,
- the policy analyzer sees the command it was written to judge,
- quoting behaves exactly as it would in the user's own terminal.

```sh
echo "$NODE_ENV"      # what the model wrote
```

One consequence worth stating: the variables go into the **child's** environment, not into zlogic's
own process. A `MY_API_KEY` declared here is visible to `curl` inside a shell call and is **not** a
model-provider key — that lookup has its own chain (`zlogic key list`, §6 of the guide).

## Built-in variables

| Name | Value |
|---|---|
| `ZLOGIC_WORKSPACE_ROOT` | the workspace root |
| `ZLOGIC_CWD` | where this call runs — the worktree when the session entered one |
| `ZLOGIC_SESSION_ID` | this conversation |
| `ZLOGIC_TURN_ID` | this turn |
| `ZLOGIC_CACHE_DIR` | `<root>/.zlogic/cache/<session_id>` — where to put scratch files |
| `ZLOGIC_OS` / `ZLOGIC_ARCH` | `std::env::consts` |
| `ZLOGIC_VERSION` | the running build |

The `ZLOGIC_` prefix is reserved. A user variable under it is refused rather than silently shadowing
a built-in, because the two would be indistinguishable inside the shell.

There is deliberately **no** `ZLOGIC_GIT_BRANCH`: a session can change branch mid-turn, and a value
frozen into a process environment at spawn time is a snapshot that lies from the next commit
onwards. A shell can ask git itself.

## What the model is told

One line, and only when the user has declared something:

```
env: NODE_ENV=development, RUST_LOG — set in zlogic's variables (global, then this project, then
this session); the shell expands $NAME itself
```

Names only for the built-ins (their values are already in the prompt as prose) and for anything
whose name says it is a secret. A value in the system prompt is a value in the transcript, in every
log that records one, and in the provider's request body. The model can read a variable by running
`echo`, which costs nothing that matters.

The line and the child process are produced by the same function, so they cannot describe different
sets.

## Naming and refusals

Names use shell env syntax: ASCII letters, digits and `_`, not starting with a digit. Two rules
refuse a name at **every** layer:

- the same dangerous list the command analyzer uses to refuse a `PATH=x cmd` prefix — `PATH`,
  `IFS`, `ENV`, `BASH_ENV`, `SHELL`, `CDPATH`, `PYTHONPATH`, `PYTHONSTARTUP`, `NODE_OPTIONS`,
  `PERL5OPT`, `PERL5LIB`, `RUBYOPT`, `GIT_SSH`, `GIT_SSH_COMMAND`, `GIT_ASKPASS`,
  `SSH_ASKPASS`, and anything starting with `LD_` or `DYLD_`. A configured variable is a *standing*
  assignment rather than a one-command prefix, so a name refused there has to be refused here.
- the `ZLOGIC_` prefix.

The `workspace` layer adds one more: a name that looks like a credential (`*_API_KEY`, `*_TOKEN`,
`*_SECRET`, `*_PASSWORD`, or exactly `API_KEY` / `TOKEN` / `SECRET` / `PASSWORD`). A repository's
`settings.yaml` arrives with `git clone`, so a variable in it defines the environment of every
command anyone runs in that repository. A refusal is **reported**, not silent — the turn carries a
`workspace_env_ignored` notice naming the variable and the reason, and the desktop's table shows it
next to the row.

Values are literals. There is no `${env:...}` or `${keyring:...}` expansion here: a repository that
could interpolate a reference would be reading the machine it was cloned onto. Write the value out,
or use the keychain-aware surfaces that already exist (MCP server auth, provider keys).

## The startup file

`tools.shell.read_profile` decides whether a command's shell reads its startup files. It defaults
to **true**, and turning it off is `--noprofile --norc` (`-NoProfile` for PowerShell, `/d` for cmd).

**Why on.** The alternative is a PATH the user cannot extend from the one place they already extend
everything else. `export PATH="$HOME/.local/bin:$PATH"` in `~/.bash_profile` is invisible to an
agent whose shell refuses to read that file, so the toolchain has to be redeclared as a variable,
per project, by whoever is debugging it.

**What it costs.** A startup file is shell code, and it now runs in front of a command the *model*
wrote. `set -e`, an `EXIT` trap, `shopt -s nullglob`, a `ulimit`, or a `read` that blocks forever
all apply to that command too. So a profile that is safe interactively is not automatically safe
non-interactively, and the guard belongs in the profile rather than in the tool:

```bash
case $- in
  *i*) ;;              # interactive: carry on
  *) return ;;         # a script or an agent: no prompts, no readline tweaks
esac
```

**The probe is separate.** One throwaway login shell is still asked for its `PATH` and nothing
else, whatever `read_profile` says — so a `cmd` or PowerShell session inherits the bash PATH, and
one already reading its own profile loses nothing. The answer is written to a temporary file, not
stdout, because a profile's banner prints to stdout and parsing that would mean guessing which line
is the answer. The probe runs on a detached thread with a 3-second timeout, because
`std::process::Command` has no timeout and killing a login shell that is already inside someone's
`.bash_profile` is not something a library should do. The result is cached for the life of the
process; on Windows it goes through `cygpath -w`, because Git Bash reports `/c/Users/...` and a
native program handed that finds nothing. A failure is silent and falls back to the inherited PATH.

The final search path is zlogic's own managed directories, then the probe's entries, then the
inherited ones. The profile's order is what the user meant; an inherited entry it dropped is kept at
the back rather than discarded.

## Editing it

**Settings → Variables** owns the global layer. **The workspace inspector's Variables tab** owns the
project and session layers, because that rail already knows the workspace and the open session. Same
table component in both places, so the two cannot disagree about the format.

The inspector's table also lists the *effective* set with the layer each name won from — without it,
three layers and a precedence rule is a black box, and "why is `FOO` not what I set" has no answer.

## Compatibility

`config.yaml` is `deny_unknown_fields`, so an older zlogic reading a `config.yaml` that has an `env:`
block fails the **whole file** rather than ignoring the block. That is the existing convention for
both YAML files, not something this feature introduces.

## Not done, on purpose

- Rewriting command text to substitute values.
- Value expansion or keychain references in a variable's value.
- Putting these variables into an MCP stdio server's environment.
- A trust prompt for repository-provided variables. The two refusal rules already cover the
  execution-hijacking cases, and the remaining exposure — a repository redirecting a package manager
  with `NPM_CONFIG_REGISTRY` — is something the repository's own `.npmrc` can already do.
- `ZLOGIC_GIT_BRANCH` and `ZLOGIC_SHELL`.
- zsh as a selectable shell backend. `tools.default_shell` is `auto | git_bash | ps7 | powershell |
  cmd | bash`; on macOS and Linux `auto` resolves to bash regardless of what your interactive shell
  is. The profile probe asks bash, so a `~/.zshenv` PATH is only picked up where bash is the
  backend.
