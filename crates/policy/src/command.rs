//! The dialect-independent "command" abstraction.
//! A command, whatever shell runs it, is a string that decomposes into a
//! `Vec<Op>`. That output vocabulary ([`Op`] / [`crate::Zone`]) is the real
//! abstraction — it is already shared by every dialect, so the decision
//! layer never learns which shell produced an op. This module adds the
//! matching INPUT-side seam: a single [`CommandAnalyzer`] trait that the
//! three dialects implement, plus [`analyzer_for`] / [`decompose`] so callers
//! dispatch on a [`Dialect`] value instead of hard-coding which analyzer to
//! construct.
//! What is deliberately NOT abstracted: the internal lexer/parser of each
//! dialect. POSIX (AST), cmd.exe (token stream) and PowerShell (expression
//! tree) have genuinely different grammars — redirect, `cd`, quoting and
//! connector semantics all diverge — so a shared "command AST" would be a
//! leaky abstraction. The common seam is the OUTPUT (`Vec<Op>`) and this
//! dispatch trait, never the internal representation.

use std::path::Path;

use crate::Dialect;
use crate::analyze::Analyzer;
use crate::ops::Op;
use crate::windows::WinAnalyzer;

/// Decompose a command string into atomic operations, independent of which
/// shell dialect implements it. `cwd` is the directory the command starts
/// in, written in that dialect's native path syntax (POSIX path, or a
/// Windows path for cmd/PowerShell).
pub trait CommandAnalyzer {
    fn analyze(&self, input: &str, cwd: &str) -> Vec<Op>;
    fn dialect(&self) -> Dialect;
}

impl CommandAnalyzer for Analyzer {
    fn analyze(&self, input: &str, cwd: &str) -> Vec<Op> {
        // inherent Analyzer::analyze(&str, &Path) shadows this trait method
        // on the concrete type, so this is not recursive
        Analyzer::analyze(self, input, Path::new(cwd))
    }
    fn dialect(&self) -> Dialect {
        Dialect::Posix
    }
}

/// cmd.exe implementation. Newtype over [`WinAnalyzer`] so each Windows
/// dialect is its own named `CommandAnalyzer` (`WinAnalyzer` itself hosts
/// both cmd and PowerShell payload analysis for cross-dialect recursion).
pub struct CmdAnalyzer(pub WinAnalyzer);

impl CommandAnalyzer for CmdAnalyzer {
    fn analyze(&self, input: &str, cwd: &str) -> Vec<Op> {
        self.0.analyze_cmd(input, cwd)
    }
    fn dialect(&self) -> Dialect {
        Dialect::Cmd
    }
}

/// PowerShell implementation.
pub struct PsAnalyzer(pub WinAnalyzer);

impl CommandAnalyzer for PsAnalyzer {
    fn analyze(&self, input: &str, cwd: &str) -> Vec<Op> {
        self.0.analyze_ps(input, cwd)
    }
    fn dialect(&self) -> Dialect {
        Dialect::PowerShell
    }
}

/// Build the analyzer for a dialect. `workspace` / `home` anchor path
/// zoning and are given in that dialect's native path syntax.
pub fn analyzer_for(dialect: Dialect, workspace: &str, home: &str) -> Box<dyn CommandAnalyzer> {
    match dialect {
        Dialect::Posix => Box::new(Analyzer::new(workspace.into(), home.into())),
        Dialect::Cmd => Box::new(CmdAnalyzer(WinAnalyzer::new(workspace, home))),
        Dialect::PowerShell => Box::new(PsAnalyzer(WinAnalyzer::new(workspace, home))),
    }
}

/// One-shot convenience: dispatch on `dialect` and decompose in one call.
/// (Named `decompose`, not `analyze`, to avoid colliding with the [`analyze`]
/// module at the crate root; the per-instance trait method is still
/// [`CommandAnalyzer::analyze`].)
/// [`analyze`]: crate::analyze
pub fn decompose(dialect: Dialect, input: &str, cwd: &str, workspace: &str, home: &str) -> Vec<Op> {
    analyzer_for(dialect, workspace, home).analyze(input, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Access, Op};

    fn has_delete(ops: &[Op]) -> bool {
        ops.iter().any(|o| {
            matches!(
                o,
                Op::Path {
                    access: Access::Delete,
                    ..
                }
            )
        })
    }

    #[test]
    fn dispatches_per_dialect_uniformly() {
        // same intent, three dialects, one call shape
        let posix = decompose(
            Dialect::Posix,
            "rm -rf build",
            "/home/u/ws",
            "/home/u/ws",
            "/home/u",
        );
        assert!(has_delete(&posix));

        let cmd = decompose(
            Dialect::Cmd,
            "del x.txt",
            "C:\\ws",
            "C:\\ws",
            "C:\\Users\\u",
        );
        assert!(has_delete(&cmd));

        let ps = decompose(
            Dialect::PowerShell,
            "Remove-Item -Recurse C:\\ws\\build",
            "C:\\ws",
            "C:\\ws",
            "C:\\Users\\u",
        );
        assert!(has_delete(&ps));
    }

    #[test]
    fn boxed_analyzer_reports_its_dialect() {
        let a = analyzer_for(Dialect::Cmd, "C:\\ws", "C:\\Users\\u");
        assert_eq!(a.dialect(), Dialect::Cmd);
        // reusable across calls
        assert!(has_delete(&a.analyze("del a", "C:\\ws")));
        assert!(has_delete(&a.analyze("erase b", "C:\\ws")));
    }
}
