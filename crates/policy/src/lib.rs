//! zlogic-policy — shell-command decomposition + path zoning for permission
//! policy review.
//! Given a command string and the [`Dialect`] that will execute it, this
//! crate produces a flat list of [`ops::Op`] atomic operations with paths
//! resolved to absolute form and classified into a [`Zone`]. That's the
//! normalization layer; the decision layer on top (rule matching, grants,
//! audit — NOT in this crate) merges strictest-wins: any deny → deny; else
//! any ask → ask (every `Op::Unknown` and every unresolved write/delete
//! target asks); else allow.
//! One analyzer per dialect, all emitting the SAME `Op` / `Zone` vocabulary:
//! - [`Dialect::Posix`] → [`Analyzer`] : [`lexer`] (quote/substitution-aware
//!   tokens) → [`parser`] (flat command list; connectors all mean "every
//!   command must pass") → [`analyze`] (simulated-cwd walk). Paths via
//!   [`path`] (physical, symlink-resolving).
//! - [`Dialect::Cmd`] / [`Dialect::PowerShell`] → [`WinAnalyzer`] : the
//!   [`windows`] cmd.exe / PowerShell analyzers. Paths via [`winpath`]
//!   (lexical: case-insensitive, per-drive cwd, UNC/provider/device aware).
//!   Cross-dialect launches (`cmd /c`, `powershell -Command`) recurse into
//!   the other analyzer.
//! The dialect is an INPUT, never inferred from the text — the same string
//! means different operations under different shells (`&` is background in
//! POSIX but sequencing in cmd; `rm` deletes a file in POSIX but is the
//! `Remove-Item` cmdlet in PowerShell).
//! # Threat model
//! This guards against MODEL-GENERATED commands — an agent LLM that is
//! well-intentioned but errs or gets steered by prompt injection. It is NOT
//! a security boundary against a malicious USER: the user is trusted (the
//! fallback action is "ask the user", and an approving user runs the
//! command), and any input deliberately crafted to evade static analysis
//! (self-modifying scripts, `eval`/base64, multi-level indirection) will
//! evade it. Containment against malicious behavior is not what this crate
//! provides: that belongs to a host-supplied sandbox at the execution layer.
//! Policy decides "ask or not"; a sandbox would decide "can't escape even if
//! not asked". The two are not interchangeable.
//! Safety invariant across every module, justified by that model: anything
//! not statically certain (variables, dynamic cwd, script contents, parse
//! failure, recursion depth) collapses to `Op::Unknown` / [`Zone::Unresolved`]
//! so the decision layer asks. The only permitted bias is over-reporting (a
//! spurious ask), never under-reporting — because the fallback is asking a
//! trusted human, not blocking an adversary.

pub mod analyze;
pub mod command;
pub mod command_rule;
pub mod lexer;
pub mod ops;
pub mod parser;
pub mod path;
pub mod policy;
pub(crate) mod specs;
pub mod windows;
pub mod winpath;

pub use analyze::Analyzer;
pub use command::{CmdAnalyzer, CommandAnalyzer, PsAnalyzer, analyzer_for, decompose};
pub use command_rule::CommandRule;
pub use ops::{Access, Op};
pub use path::{Cwd, ResolvedPath, Zone};
pub use policy::{
    Decision, Effect, ExecRule, OpDecision, PathRule, Policy, PolicyError, ScriptRule,
    default_workspace_policy, evaluate_command,
};
pub use windows::WinAnalyzer;

/// Which interpreter will execute the command string. The CALLER must know
/// this (it decides what process to spawn) — never guess it from the text:
/// the same string means different operations in different dialects.
/// Pair with [`analyzer_for`] / [`decompose`] to decompose a command through
/// the [`CommandAnalyzer`] trait without hard-coding a per-dialect analyzer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Posix,
    Cmd,
    PowerShell,
}
