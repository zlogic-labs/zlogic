# Contributing

## Scope of this repository

This repository is the CLI and the agent runtime (Apache-2.0). The desktop app, the remote-access
daemon and the closed-source tool packs are developed in a separate, private repository and are
built on top of the crates here — nothing in this tree may depend on them. See the
[README](README.md#open-source-and-what-is-not) for the boundary.

## Setup

A recent stable Rust toolchain (edition 2024) is all you need — there is no Node or Python step
in this repository.

```sh
cargo build --release
cargo run -p zlogic-cli
cargo test
```

## Before a pull request

```sh
cargo fmt --all --check
cargo test
```

The tree is kept rustfmt-clean, and the workspace test suite is expected to pass on the host.
Some tests are platform-specific (`cfg(unix)` / `cfg(windows)`), so a green run on one platform
does not prove another.

## Conventions

- **English** in code, comments, commit messages and documentation. The one exception is
  user-facing localization: `apps/cli` ships UI catalogues (`en-US` and `zh-CN`).
- **Comments explain why**, not what. If the code already says it, the comment is noise.
- **Keep files small.** A module that outgrows a screenful or two gets split — tests included.
- **Tests sit with the code** (`#[cfg(test)] mod tests`) or in a crate's `tests/` directory when
  they cover an end-to-end flow.
- **No `unsafe` in the library crates**: the workspace denies it
  (`[workspace.lints.rust] unsafe_code = "forbid"`). `apps/cli` is the single exception, because
  probing terminal capabilities needs the raw console and syscall APIs.
- Crates are `publish = false` — nothing here is released to crates.io.

## Scope of a change

One concern per pull request, so review stays cheap. Behaviour changes are easier to accept when
they come with the reasoning (why the old behaviour was wrong) rather than only the diff.
